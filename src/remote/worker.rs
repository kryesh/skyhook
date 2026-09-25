use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex as StdMutex, PoisonError},
};

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Mutex, mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use super::{
    flow::{CHUNK_BYTES, WINDOW},
    payload::PayloadSender,
    protocol::{
        AuthorizationId, RemoteToolError, Request, RequestId, Response, read_frame,
        spawn_owned_write, write_frame,
    },
};
use crate::tool::{
    diagnostic::{Operation, Subject},
    invocation::{
        AdmissionError, CANCELLATION_GRACE, LocalAuthorizer, LocalCatalog, LocalContext, LocalError,
    },
    output::ProducedOutput,
    policy::{Capability, PermissionUse, ResourceId},
    source::{Source, Spool, open_on_worker},
};

type WorkerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub async fn serve_with_authorization_root(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let root = std::fs::canonicalize(root)?;
    serve_io_at(tokio::io::stdin(), tokio::io::stdout(), root)
        .await
        .map_err(|error| error as Box<dyn std::error::Error>)
}

async fn serve_io_at<R, W>(
    mut input: R,
    mut output: W,
    authorization_root: std::path::PathBuf,
) -> WorkerResult
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    match read_frame::<_, Request>(&mut input).await? {
        Some(Request::Hello) => write_frame(&mut output, &Response::Ready).await?,
        Some(request) => return Err(format!("expected hello, received {request:?}").into()),
        None => return Ok(()),
    }
    let output = Arc::new(Mutex::new(output));
    let services = super::service::WorkerServices::new(output.clone())?;
    let catalog = Arc::new(LocalCatalog::builtins()?);
    let workspace = crate::execution::ExecutionLocation::root(std::fs::canonicalize(".")?);
    let (payloads, receiver) = PayloadSender::new();
    let credits = receiver.credits();
    let mut tasks = JoinSet::new();
    let writer = output.clone();
    tasks.spawn(async move { (WorkerTask::Output, receiver.forward(writer).await) });
    let mut worker = Worker {
        output,
        services,
        catalog,
        workspace,
        authorization_root: Arc::new(authorization_root),
        payloads,
        credits,
        tasks,
        active: HashMap::new(),
        uploads: HashMap::new(),
    };
    let result = async {
        loop {
            let reading = read_frame::<_, Request>(&mut input);
            tokio::pin!(reading);
            let request = loop {
                tokio::select! {
                    request = &mut reading => break request?,
                    completed = worker.services.tasks.join_next(), if !worker.services.tasks.is_empty() => {
                        completed.expect("nonempty service task set")??;
                    }
                    completed = worker.tasks.join_next() => {
                        let (task, result) = completed.expect("live output task")?;
                        result?;
                        match task {
                            WorkerTask::Request(id) => { worker.active.remove(&id); }
                            WorkerTask::Output => return Err("shim output stopped unexpectedly".into()),
                        }
                    }
                }
            };
            let Some(request) = request else { return Ok(()); };
            worker.handle(request).await?;
        }
    }.await;
    worker.active.clear();
    worker.credits.close();
    worker.tasks.abort_all();
    while worker.tasks.join_next().await.is_some() {}
    result
}

struct Worker<W> {
    output: Arc<Mutex<W>>,
    services: super::service::WorkerServices<W>,
    catalog: Arc<LocalCatalog>,
    workspace: crate::execution::ExecutionLocation,
    authorization_root: Arc<std::path::PathBuf>,
    payloads: PayloadSender,
    credits: super::flow::Credits,
    tasks: JoinSet<(WorkerTask, std::io::Result<()>)>,
    active: HashMap<RequestId, ActiveRequest>,
    /// Calls whose source is still arriving; each starts at its `SourceEnd`.
    uploads: HashMap<RequestId, Upload>,
}

struct Upload {
    capabilities: Vec<Capability>,
    name: String,
    arguments: Value,
    chunks: mpsc::Sender<Vec<u8>>,
    /// Spools the chunks off the request loop; dropping the upload stops it.
    spooled: Spooled,
}

type Spooled = AbortOnDropHandle<std::io::Result<Source>>;

impl<W: AsyncWrite + Unpin + Send + 'static> Worker<W> {
    async fn handle(&mut self, request: Request) -> WorkerResult {
        match request {
            Request::Tool {
                request_id,
                name,
                arguments,
                capabilities,
                source,
            } => {
                if !source {
                    let work = Work::Tool {
                        name,
                        arguments,
                        source: None,
                    };
                    return self.start(request_id, capabilities, work);
                }
                self.ensure_new(request_id)?;
                let (chunks, received) = mpsc::channel(WINDOW);
                let spool = spool_upload(request_id, received, self.output.clone());
                let upload = Upload {
                    capabilities,
                    name,
                    arguments,
                    chunks,
                    spooled: AbortOnDropHandle::new(tokio::spawn(spool)),
                };
                self.uploads.insert(request_id, upload);
            }
            Request::SourceData { request_id, data } => {
                if data.len() > CHUNK_BYTES {
                    return Err("oversized source chunk".into());
                }
                let upload = self
                    .uploads
                    .get(&request_id)
                    .ok_or("source data for an unknown request")?;
                // A closed spool has already failed; the call reports it when it starts.
                if let Err(mpsc::error::TrySendError::Full(_)) = upload.chunks.try_send(data) {
                    return Err("source upload flow-control overflow".into());
                }
            }
            Request::SourceEnd { request_id } => {
                let Upload {
                    capabilities,
                    name,
                    arguments,
                    chunks,
                    spooled,
                } = self
                    .uploads
                    .remove(&request_id)
                    .ok_or("source end for an unknown request")?;
                // Closing the channel lets the spool finish after its queued chunks.
                drop(chunks);
                self.start(
                    request_id,
                    capabilities,
                    Work::Tool {
                        name,
                        arguments,
                        source: Some(spooled),
                    },
                )?;
            }
            Request::ReadSource {
                request_id,
                tool,
                path,
                capabilities,
            } => self.start(request_id, capabilities, Work::ReadSource { tool, path })?,
            Request::Cancel { request_id } => {
                if let Some(request) = self.active.get(&request_id) {
                    request.cancellation.cancel();
                } else if self.uploads.remove(&request_id).is_some() {
                    // The call never started; it still owes the host a terminal result.
                    let sender = self.payloads.request(request_id);
                    self.tasks.spawn(async move {
                        let result = Err(remote_error(LocalError::cancelled()));
                        (WorkerTask::Request(request_id), sender.finish(result).await)
                    });
                }
            }
            Request::PayloadAck => self.credits.acknowledge()?,
            Request::AuthorizationDecision {
                request_id,
                authorization_id,
                decision,
            } => {
                if let Some(request) = self.active.get(&request_id)
                    && let Some(sender) = request
                        .authorizations
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&authorization_id)
                {
                    let _ = sender.send(decision.into_result().map_err(AdmissionError::from));
                }
            }
            control @ (Request::OpenSsh { .. }
            | Request::StreamData { .. }
            | Request::StreamEnd { .. }
            | Request::StreamClose { .. }
            | Request::StreamAck { .. }
            | Request::SensitiveAnswer { .. }) => {
                self.services.handle(control).await?;
            }
            Request::Hello => return Err("received a second hello".into()),
        }
        Ok(())
    }

    fn ensure_new(&self, id: RequestId) -> WorkerResult {
        if self.active.contains_key(&id) || self.uploads.contains_key(&id) {
            return Err(format!("duplicate request ID {}", id.get()).into());
        }
        Ok(())
    }

    fn start(&mut self, id: RequestId, capabilities: Vec<Capability>, work: Work) -> WorkerResult {
        self.ensure_new(id)?;
        let sender = self.payloads.request(id);
        let produced = sender.context();
        let cancellation = CancellationToken::new();
        let authorizations = Arc::new(StdMutex::new(HashMap::new()));
        let context = LocalContext::new(
            self.workspace.clone(),
            capabilities.into_iter().collect(),
            self.services.environment.clone(),
            cancellation.clone(),
            produced.clone(),
            Arc::new(ForwardAuthorization {
                request_id: id,
                tool: work.name().to_owned(),
                output: self.output.clone(),
                pending: authorizations.clone(),
                next: StdMutex::new(Some(AuthorizationId(0))),
            }),
            work.arguments(),
        );
        self.active.insert(
            id,
            ActiveRequest {
                cancellation: cancellation.clone(),
                authorizations,
            },
        );
        let catalog = self.catalog.clone();
        let root = self.authorization_root.clone();
        let source_output = sender.clone();
        self.tasks.spawn(async move {
            let execution = async {
                let call: BoxFuture<'_, _> = match work {
                    Work::Tool {
                        name,
                        arguments,
                        source,
                    } => Box::pin(async move {
                        let source = match source {
                            Some(spooled) => Some(
                                spooled
                                    .await
                                    .map_err(std::io::Error::other)
                                    .and_then(|spooled| spooled)
                                    .map_err(source_error(Operation::Receive))?,
                            ),
                            None => None,
                        };
                        catalog
                            .run(&name, arguments, context.with_source(source), &root)
                            .await
                    }),
                    Work::ReadSource { tool, path } => Box::pin(async move {
                        let file = open_on_worker(&tool, &path, &context, &root).await?;
                        source_output
                            .send_source(file)
                            .await
                            .map_err(source_error(Operation::Send))?;
                        Ok(ProducedOutput::new(Value::Null))
                    }),
                };
                tokio::pin!(call);
                let result = tokio::select! {
                    result = &mut call => result,
                    () = cancellation.cancelled() => {
                        let _ = tokio::time::timeout(CANCELLATION_GRACE, &mut call).await;
                        Err(LocalError::cancelled())
                    }
                };
                if cancellation.is_cancelled() {
                    Err(LocalError::cancelled())
                } else {
                    result
                }
            }
            .await;
            let result = match produced.settle().await {
                Ok(()) => execution.map(Into::into).map_err(remote_error),
                Err(error) => Err(remote_error(LocalError::io(error))),
            };
            (WorkerTask::Request(id), sender.finish(result).await)
        });
        Ok(())
    }
}

/// Spool an upload's chunks, acknowledging each so the host may send another.
/// A spool failure is reported when the call starts; later chunks are still
/// acknowledged so the upload reaches its end.
async fn spool_upload<W: AsyncWrite + Unpin + Send + 'static>(
    request_id: RequestId,
    mut chunks: mpsc::Receiver<Vec<u8>>,
    output: Arc<Mutex<W>>,
) -> std::io::Result<Source> {
    let mut spool = Spool::new().await;
    while let Some(data) = chunks.recv().await {
        if let Ok(file) = &mut spool
            && let Err(error) = file.append(&data).await
        {
            spool = Err(error);
        }
        let acknowledgement = Response::SourceAck { request_id };
        spawn_owned_write(output.clone().lock_owned().await, acknowledgement).await?;
    }
    spool?.finish().await
}

fn source_error(operation: Operation) -> impl FnOnce(std::io::Error) -> LocalError {
    move |error| LocalError::io(error).operation(operation, Subject::Label("source".into()))
}

/// What a request asks the worker to run.
enum Work {
    Tool {
        name: String,
        arguments: Value,
        /// A source still being received.
        source: Option<Spooled>,
    },
    ReadSource {
        /// The tool consuming the source on another machine.
        tool: String,
        path: String,
    },
}

impl Work {
    /// The operation named in forwarded authorization requests.
    fn name(&self) -> &str {
        match self {
            Self::Tool { name, .. } | Self::ReadSource { tool: name, .. } => name,
        }
    }

    fn arguments(&self) -> Value {
        match self {
            Self::Tool { arguments, .. } => arguments.clone(),
            Self::ReadSource { path, .. } => serde_json::json!({ "path": path }),
        }
    }
}

type PendingAuthorizations =
    Arc<StdMutex<HashMap<AuthorizationId, oneshot::Sender<Result<(), AdmissionError>>>>>;

enum WorkerTask {
    Output,
    Request(RequestId),
}

struct ActiveRequest {
    cancellation: CancellationToken,
    authorizations: PendingAuthorizations,
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.authorizations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

struct ForwardAuthorization<W> {
    request_id: RequestId,
    tool: String,
    output: Arc<Mutex<W>>,
    pending: PendingAuthorizations,
    next: StdMutex<Option<AuthorizationId>>,
}

struct PendingAuthorization {
    id: AuthorizationId,
    pending: PendingAuthorizations,
}

impl Drop for PendingAuthorization {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

impl<W: AsyncWrite + Unpin + Send + 'static> LocalAuthorizer for ForwardAuthorization<W> {
    fn authorize(
        &self,
        mut permissions: Vec<PermissionUse>,
        arguments: Value,
    ) -> BoxFuture<'static, Result<(), AdmissionError>> {
        // The host already admitted static workspace access; destination-derived
        // paths and network origins still require its policy decision.
        permissions.retain(|permission| {
            !(matches!(permission.resource, ResourceId::Workspace { .. })
                && matches!(
                    permission.capability,
                    Capability::Read | Capability::Write | Capability::Exec
                ))
        });
        if permissions.is_empty() {
            return Box::pin(async { Ok(()) });
        }
        let id = {
            let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
            let id = *next;
            *next = id.and_then(|id| id.0.checked_add(1).map(AuthorizationId));
            id
        };
        let pending = self.pending.clone();
        let output = self.output.clone();
        let request_id = self.request_id;
        let tool = self.tool.clone();
        Box::pin(async move {
            let id =
                id.ok_or_else(|| AdmissionError::failed("authorization ID space exhausted"))?;
            let (sender, receiver) = oneshot::channel();
            pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(id, sender);
            let _registration = PendingAuthorization { id, pending };
            spawn_owned_write(
                output.lock_owned().await,
                Response::Authorization {
                    request_id,
                    authorization_id: id,
                    tool,
                    permissions,
                    arguments,
                },
            )
            .await?;
            receiver
                .await
                .map_err(|_| AdmissionError::denied("host authorization channel closed"))?
        })
    }
}

pub(super) fn remote_error(error: LocalError) -> RemoteToolError {
    let (diagnostic, output) = error.into_parts();
    RemoteToolError {
        diagnostic: Box::new(diagnostic),
        output: output.map(|output| Box::new(output.into())),
    }
}

pub async fn self_check(expected: &str) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = tokio::fs::read(std::env::current_exe()?).await?;
    let actual = crate::sha256_hex(bytes);
    if actual != expected {
        return Err(format!("shim hash mismatch: expected {expected}, got {actual}").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        execution::ExecutionLocation,
        identity::JobId,
        job::{JobEnvelope, JobSpec, JobState},
        remote::{
            RejectSensitivePrompts, RemoteError, client::PooledConnection, transport::Transport,
        },
        tests::{RecordingPolicy, TestRuntime},
        tool::{
            ToolContext, ToolOutput,
            authorization::{AuthorizationCoordinator, AuthorizationSubject},
            policy::{AuthorizationRequest, CapabilitySet, Policy, PolicyDecision, PolicyFuture},
        },
    };
    use std::{future::Future, time::Duration};
    use tokio::sync::mpsc;

    async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(20), future)
            .await
            .expect("worker operation stalled")
    }

    struct Harness {
        runtime: TestRuntime,
        connection: Arc<PooledConnection>,
        authorization: AuthorizationCoordinator,
        worker: tokio::task::JoinHandle<Result<(), String>>,
    }

    impl Harness {
        async fn start(root: std::path::PathBuf, policy: Arc<dyn Policy>) -> Self {
            let runtime = TestRuntime::new().await;
            let authorization = AuthorizationCoordinator::new(policy);
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (output, input) = tokio::io::split(client);
            let (server_input, server_output) = tokio::io::split(server);
            let worker = tokio::spawn(async move {
                serve_io_at(server_input, server_output, root)
                    .await
                    .map_err(|error| error.to_string())
            });
            let connection = PooledConnection::from_transport(
                Transport {
                    input: Box::new(input),
                    output: Box::new(output),
                    owner: Box::new(()),
                },
                &"remote".parse().unwrap(),
                authorization.clone(),
                Arc::new(RejectSensitivePrompts),
                &tokio_util::task::TaskTracker::new(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            Self {
                runtime,
                connection: Arc::new(connection),
                authorization,
                worker,
            }
        }

        async fn tool(&self, capabilities: CapabilitySet, name: &str, arguments: Value) -> JobId {
            self.tool_with_source(capabilities, name, arguments, None)
                .await
        }

        async fn tool_with_source(
            &self,
            capabilities: CapabilitySet,
            name: &str,
            arguments: Value,
            source: Option<Source>,
        ) -> JobId {
            let tool = name.to_owned();
            let call_arguments = arguments.clone();
            self.job(
                capabilities,
                name,
                arguments,
                source,
                move |connection, context| async move {
                    let destination = context.execution_location().clone();
                    connection
                        .execute(tool, call_arguments, &context, destination)
                        .await
                },
            )
            .await
        }

        async fn read_source(&self, capabilities: CapabilitySet, path: String) -> JobId {
            let arguments = serde_json::json!({"path": path});
            self.job(
                capabilities,
                "write",
                arguments,
                None,
                move |connection, context| async move {
                    let destination = context.execution_location().clone();
                    let source =
                        (connection.read_source("write".to_owned(), path, &context, destination))
                            .await?;
                    let mut bytes = Vec::new();
                    std::io::Read::read_to_end(&mut source.reader().unwrap(), &mut bytes).unwrap();
                    Ok(ToolOutput::new(serde_json::json!(bytes)))
                },
            )
            .await
        }

        /// Run one remote call as a job, recording its result like the executor.
        async fn job<F, Fut>(
            &self,
            capabilities: CapabilitySet,
            name: &str,
            arguments: Value,
            source: Option<Source>,
            call: F,
        ) -> JobId
        where
            F: FnOnce(Arc<PooledConnection>, ToolContext) -> Fut + Send + 'static,
            Fut: Future<Output = Result<ToolOutput, RemoteError>> + Send,
        {
            let mut spec = JobSpec::test(self.runtime.agent.clone(), name);
            spec.arguments = arguments.clone();
            spec.location = ExecutionLocation {
                target: "remote".parse().unwrap(),
                workspace: std::fs::canonicalize(".").unwrap(),
            };
            let location = spec.location.clone();
            let lease = self.runtime.jobs.create(spec).await.unwrap();
            let lease = lease.test_run().await;
            let job = lease.id();
            let cancellation = lease.cancellation_token();
            let (input, worker) = lease.split();
            let context = ToolContext::new(
                AuthorizationSubject {
                    agent: self.runtime.agent.clone(),
                    job,
                    parent: None,
                    capabilities,
                    cancellation,
                },
                location,
                ExecutionLocation::root(self.runtime.root.path().to_owned()),
                input,
                self.runtime.jobs.clone(),
            )
            .with_invocation_authority(self.authorization.clone(), name.to_owned(), arguments)
            .with_source(source);
            let connection = self.connection.clone();
            worker
                .start_supervised(async move {
                    let result = call(connection, context.clone()).await;
                    if context.is_cancelled() {
                        Err(crate::tool::ToolError::cancelled())
                    } else {
                        result.map_err(RemoteError::into_tool_error)
                    }
                })
                .await
                .unwrap();
            job
        }

        async fn result(&self, job: JobId) -> JobEnvelope {
            bounded(self.runtime.jobs.wait_settled(job)).await.unwrap();
            let mut envelope = self.runtime.jobs.metadata(job).await.unwrap();
            self.runtime
                .jobs
                .hydrate_envelope(&mut envelope)
                .await
                .unwrap();
            envelope
        }

        async fn finish(self) {
            drop(self.connection);
            bounded(self.worker).await.unwrap().unwrap();
        }
    }

    /// A call cancelled while its source is still arriving never starts, still
    /// answers with a terminal result, and releases its upload.
    #[tokio::test]
    async fn cancelled_uploads_finish_without_starting_the_call() {
        use crate::remote::protocol::{PayloadEvent, PayloadId, Response, read_frame, write_frame};
        let root = tempfile::tempdir().unwrap();
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (mut responses, mut requests) = tokio::io::split(client);
        let (server_input, server_output) = tokio::io::split(server);
        let root_path = root.path().to_owned();
        let worker = tokio::spawn(serve_io_at(server_input, server_output, root_path.clone()));
        let id = RequestId::FIRST;
        let destination = root_path.join("never-written");
        for request in [
            Request::Hello,
            Request::Tool {
                request_id: id,
                name: "write".into(),
                arguments: serde_json::json!({"path": destination, "source": {"path": "x"}}),
                capabilities: vec![Capability::Write],
                source: true,
            },
            Request::SourceData {
                request_id: id,
                data: b"partial".to_vec(),
            },
            Request::Cancel { request_id: id },
        ] {
            write_frame(&mut requests, &request).await.unwrap();
        }
        let mut result = Vec::new();
        loop {
            match bounded(read_frame::<_, Response>(&mut responses))
                .await
                .unwrap()
            {
                Some(Response::Payload { event, .. }) => {
                    if let PayloadEvent::Data {
                        id: PayloadId::Result,
                        data,
                    } = event
                    {
                        result.extend(data);
                    }
                    write_frame(&mut requests, &Request::PayloadAck)
                        .await
                        .unwrap();
                }
                Some(Response::Tool { request_id }) if request_id == id => break,
                Some(_) => {}
                None => panic!("worker closed before the terminal result"),
            }
        }
        let result: crate::remote::protocol::RemoteToolResult =
            serde_json::from_slice(&result).unwrap();
        assert!(result.is_err());
        assert!(!destination.exists());
        drop((requests, responses));
        bounded(worker).await.unwrap().unwrap();
    }

    /// Spooling an upload never holds up the request loop: while the upload's
    /// acknowledgements are stuck behind an unread output, the worker still
    /// starts other calls, and a Cancel stops the upload before its call starts.
    #[tokio::test]
    async fn requests_are_served_while_an_upload_is_stalled() {
        use crate::remote::protocol::{PayloadEvent, PayloadId, RemoteToolResult};
        let root = tempfile::tempdir().unwrap();
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        // Room for a few acknowledgements only: the rest wait for the host to read.
        let (client, server) = tokio::io::duplex(256);
        let (mut responses, mut requests) = tokio::io::split(client);
        let (server_input, server_output) = tokio::io::split(server);
        let worker = tokio::spawn(serve_io_at(server_input, server_output, root_path.clone()));
        write_frame(&mut requests, &Request::Hello).await.unwrap();
        assert!(matches!(
            bounded(read_frame(&mut responses)).await.unwrap(),
            Some(Response::Ready)
        ));
        let (upload, sibling) = (RequestId::FIRST, RequestId::FIRST.next().unwrap());
        let never_written = root_path.join("never-written");
        let written = root_path.join("sibling");
        let chunks = (0..WINDOW).map(|_| Request::SourceData {
            request_id: upload,
            data: b"chunk".to_vec(),
        });
        let requests_in_order = std::iter::once(Request::Tool {
            request_id: upload,
            name: "write".into(),
            arguments: serde_json::json!({"path": never_written, "source": {"path": "x"}}),
            capabilities: vec![Capability::Write],
            source: true,
        })
        .chain(chunks)
        .chain(std::iter::once(Request::Tool {
            request_id: sibling,
            name: "write".into(),
            arguments: serde_json::json!({"path": written, "content": "sibling"}),
            capabilities: vec![Capability::Write],
            source: false,
        }));
        bounded(async {
            for request in requests_in_order {
                write_frame(&mut requests, &request).await.unwrap();
            }
            while !written.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        write_frame(&mut requests, &Request::Cancel { request_id: upload })
            .await
            .unwrap();
        let mut results = HashMap::<RequestId, Vec<u8>>::new();
        let (mut acknowledged, mut finished) = (0, 0);
        while finished < 2 {
            match bounded(read_frame::<_, Response>(&mut responses))
                .await
                .unwrap()
                .expect("worker closed before the terminal results")
            {
                Response::Payload { request_id, event } => {
                    if let PayloadEvent::Data {
                        id: PayloadId::Result,
                        data,
                    } = event
                    {
                        results.entry(request_id).or_default().extend(data);
                    }
                    write_frame(&mut requests, &Request::PayloadAck)
                        .await
                        .unwrap();
                }
                Response::SourceAck { request_id } if request_id == upload => acknowledged += 1,
                Response::Tool { .. } => finished += 1,
                other => panic!("unexpected response: {other:?}"),
            }
        }
        let result = |id| serde_json::from_slice::<RemoteToolResult>(&results[&id]).unwrap();
        assert!(result(upload).is_err());
        assert!(result(sibling).is_ok());
        assert!(acknowledged <= WINDOW);
        assert!(!never_written.exists());
        drop((requests, responses));
        bounded(worker).await.unwrap().unwrap();
    }

    /// Sources stream across the protocol in both directions, in more chunks
    /// than one credit window: read on the machine holding the file, and
    /// delivered with the call that consumes them.
    #[tokio::test]
    async fn sources_are_read_on_the_worker_and_delivered_with_calls() {
        let root = tempfile::tempdir().unwrap();
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        let binary: Vec<u8> = (0..CHUNK_BYTES * (WINDOW + 2) + 11)
            .map(|index| (index % 253) as u8)
            .collect();
        std::fs::write(root_path.join("asset.bin"), &binary).unwrap();
        let worker = Harness::start(root_path.clone(), RecordingPolicy::allowing()).await;
        let read = [Capability::Read].into_iter().collect::<CapabilitySet>();
        let asset = root_path.join("asset.bin").to_str().unwrap().to_owned();
        let job = worker.read_source(read, asset.clone()).await;
        let output = worker.result(job).await.output.unwrap();
        assert_eq!(serde_json::from_value::<Vec<u8>>(output).unwrap(), binary);
        // Without the read capability the worker refuses the source as the consuming tool.
        let job = worker.read_source(CapabilitySet::empty(), asset).await;
        let refused = worker.result(job).await;
        assert_eq!(refused.state, JobState::Failed);
        assert_eq!(
            refused.diagnostic.unwrap().cause,
            crate::tool::diagnostic::Cause::Message(
                "tool `write` is unavailable in this context".into()
            )
        );

        let write = [Capability::Write].into_iter().collect::<CapabilitySet>();
        let arguments = serde_json::json!({
            "path": root_path.join("copy.bin"),
            "source": {"path": "held-by-another-machine"},
        });
        let host = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(host.path(), &binary).unwrap();
        let source = Source::open(host.path()).await.unwrap();
        let job = worker
            .tool_with_source(write, "write", arguments, Some(source))
            .await;
        assert_eq!(worker.result(job).await.state, JobState::Completed);
        assert_eq!(std::fs::read(root_path.join("copy.bin")).unwrap(), binary);
        worker.finish().await;
    }

    #[tokio::test]
    async fn worker_enforces_exact_capabilities_and_noninteractive_exec_sessions() {
        let worker = Harness::start(
            std::fs::canonicalize(".").unwrap(),
            RecordingPolicy::allowing(),
        )
        .await;
        for capabilities in [
            vec![],
            vec![Capability::Read],
            vec![Capability::Exec],
            vec![],
        ] {
            let allowed = capabilities.contains(&Capability::Exec);
            let job = worker
                .tool(
                    capabilities.into_iter().collect(),
                    "exec",
                    serde_json::json!({"command":["/bin/sh", "-c", "printf exact"]}),
                )
                .await;
            let result = worker.result(job).await;
            assert_eq!(result.state == JobState::Completed, allowed);
            if allowed {
                assert_eq!(result.output.unwrap()["stdout"], "exact");
            }
        }
        #[cfg(target_os = "linux")]
        for interactive in [false, true, false] {
            let mut capabilities: CapabilitySet = [Capability::Exec].into_iter().collect();
            if interactive {
                capabilities.insert(Capability::Interactive);
            }
            let stat = "read pid comm state ppid pgrp sid rest < /proc/self/stat; printf '%s %s' \"$pid\" \"$sid\"";
            let job = worker
                .tool(
                    capabilities,
                    "exec",
                    serde_json::json!({"command":["/bin/sh", "-c", stat]}),
                )
                .await;
            let result = worker.result(job).await.output.unwrap();
            let ids: Vec<_> = result["stdout"]
                .as_str()
                .unwrap()
                .split_whitespace()
                .collect();
            assert_eq!(ids.len(), 2);
            assert_eq!(ids[0] == ids[1], !interactive);
        }
        worker.finish().await;
    }

    #[tokio::test]
    async fn remote_captures_and_images_are_persisted_on_the_host() {
        let root = tempfile::tempdir().unwrap();
        let text = "aé🦀\n".repeat(20_000);
        let path = root.path().join("text");
        std::fs::write(&path, &text).unwrap();
        let image_path = root.path().join("image.png");
        let image = crate::tests::png(b"remote source bytes");
        std::fs::write(&image_path, image.bytes()).unwrap();
        let policy = RecordingPolicy::allowing();
        let worker = Harness::start(std::fs::canonicalize(".").unwrap(), policy.clone()).await;
        let job = worker
            .tool(
                CapabilitySet::default(),
                "read",
                serde_json::json!({"path":path}),
            )
            .await;
        let result = worker.result(job).await;
        assert_eq!(result.state, JobState::Completed);
        assert_eq!(result.output.unwrap()["content"], text);
        assert_eq!(
            worker
                .runtime
                .jobs
                .output(job)
                .test_bytes("/result/content")
                .unwrap(),
            text.as_bytes()
        );
        let expected = ResourceId::path(
            &"remote".parse().unwrap(),
            &crate::tool::policy::PathText::new(&path).unwrap(),
        );
        assert!(
            policy
                .requests
                .lock()
                .unwrap()
                .iter()
                .flat_map(|request| &request.permissions)
                .any(|permission| permission.resource == expected)
        );

        let job = worker
            .tool(
                CapabilitySet::default(),
                "read",
                serde_json::json!({"path":image_path}),
            )
            .await;
        assert_eq!(worker.result(job).await.state, JobState::Completed);
        let images = worker.runtime.jobs.images(job).await.unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].file.as_deref(), image_path.to_str());
        let bytes = worker
            .runtime
            .store
            .read_blob(&images[0].blob, crate::media::MAX_IMAGE_BYTES as usize)
            .await
            .unwrap();
        assert_eq!(bytes, image.bytes());
        assert!(
            worker
                .runtime
                .jobs
                .output(job)
                .test_bytes("/result/content")
                .is_none()
        );

        let job = worker
            .tool(
                CapabilitySet::default(),
                "read",
                serde_json::json!({"path":root.path().join("missing")}),
            )
            .await;
        let result = worker.result(job).await;
        assert_eq!(result.state, JobState::Completed);
        assert_eq!(result.output.unwrap()["kind"], "error");
        // The completed read error is bound to the invocation's trusted location.
        assert_eq!(
            result.output_diagnostic.unwrap().context.site,
            crate::tool::diagnostic::FailureSite::Execution(ExecutionLocation::named(
                "remote".parse().unwrap(),
                std::fs::canonicalize(".").unwrap(),
            )),
        );
        worker.finish().await;
    }

    struct GatedPolicy {
        requests: mpsc::UnboundedSender<AuthorizationRequest>,
        release: Arc<tokio::sync::Semaphore>,
    }

    impl Policy for GatedPolicy {
        fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
            self.requests.send(request).unwrap();
            Box::pin(async {
                let _permit = self.release.acquire().await.unwrap();
                PolicyDecision::allow()
            })
        }
    }

    #[tokio::test]
    async fn worker_cancellation_during_authorization_does_not_block_siblings() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let (requests, mut observed) = mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let policy = Arc::new(GatedPolicy {
            requests,
            release: release.clone(),
        });
        let worker = Harness::start(std::fs::canonicalize(".").unwrap(), policy).await;
        let blocked = worker
            .tool(
                CapabilitySet::default(),
                "read",
                serde_json::json!({"path":file.path()}),
            )
            .await;
        let request = bounded(observed.recv()).await.unwrap();
        assert!(
            request
                .permissions
                .iter()
                .any(|permission| permission.resource
                    == ResourceId::path(
                        &"remote".parse().unwrap(),
                        &crate::tool::policy::PathText::new(file.path()).unwrap(),
                    ))
        );
        let sibling = worker
            .tool(
                CapabilitySet::default(),
                "exec",
                serde_json::json!({"command":["/bin/sh", "-c", "printf sibling"]}),
            )
            .await;
        assert_eq!(
            worker.result(sibling).await.output.unwrap()["stdout"],
            "sibling"
        );
        worker.runtime.jobs.cancel(blocked).await.unwrap();
        assert_eq!(worker.result(blocked).await.state, JobState::Cancelled);
        release.add_permits(1);
        let followup = worker
            .tool(
                CapabilitySet::default(),
                "exec",
                serde_json::json!({"command":["/bin/sh", "-c", "printf reusable"]}),
            )
            .await;
        assert_eq!(
            worker.result(followup).await.output.unwrap()["stdout"],
            "reusable"
        );
        assert_eq!(
            worker.runtime.jobs.metadata(blocked).await.unwrap().state,
            JobState::Cancelled
        );
        worker.finish().await;
    }

    #[tokio::test]
    async fn remote_output_is_live_and_timeout_preserves_cut_streams() {
        let worker = Harness::start(
            std::fs::canonicalize(".").unwrap(),
            RecordingPolicy::allowing(),
        )
        .await;
        let job = worker
            .tool(
                CapabilitySet::default(),
                "exec",
                serde_json::json!({"command":["/bin/sh", "-c", "printf before; sleep 3600"]}),
            )
            .await;
        bounded(async {
            loop {
                if worker
                    .runtime
                    .jobs
                    .output(job)
                    .test_bytes("/result/stdout")
                    .as_deref()
                    == Some(b"before")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            !worker
                .runtime
                .jobs
                .metadata(job)
                .await
                .unwrap()
                .state
                .is_terminal()
        );
        worker.runtime.jobs.cancel(job).await.unwrap();
        assert_eq!(worker.result(job).await.state, JobState::Cancelled);
        let job = worker.tool(CapabilitySet::default(), "exec", serde_json::json!({"command":["/bin/sh", "-c", "printf before; sleep 30"], "timeout":1})).await;
        let result = worker.result(job).await;
        assert_eq!(result.state, JobState::Failed);
        assert_eq!(result.output.unwrap()["timed_out"], true);
        let saved = worker.runtime.jobs.output(job);
        assert_eq!(saved.test_captures_complete(), Some(false));
        assert_eq!(
            worker
                .runtime
                .jobs
                .output(job)
                .test_bytes("/result/stdout")
                .unwrap(),
            b"before"
        );
        worker.finish().await;
    }
}
