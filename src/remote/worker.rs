use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex as StdMutex, PoisonError},
};

use futures_util::future::BoxFuture;
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Mutex, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

use super::{
    payload::PayloadSender,
    protocol::{
        AuthorizationId, RemoteToolError, Request, RequestId, Response, read_frame,
        spawn_owned_write, write_frame,
    },
};
use crate::tool::{
    invocation::{
        AdmissionError, CANCELLATION_GRACE, LocalAuthorizer, LocalCatalog, LocalContext, LocalError,
    },
    policy::{Capability, PermissionUse, ResourceId},
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
}

impl<W: AsyncWrite + Unpin + Send + 'static> Worker<W> {
    async fn handle(&mut self, request: Request) -> WorkerResult {
        match request {
            Request::Tool {
                request_id,
                name,
                arguments,
                capabilities,
            } => {
                self.start(request_id, name, arguments, capabilities)?;
            }
            Request::Cancel { request_id } => {
                if let Some(request) = self.active.get(&request_id) {
                    request.cancellation.cancel();
                }
            }
            Request::PayloadAck => self.credits.acknowledge()?,
            Request::AuthorizationDecision {
                request_id,
                authorization_id,
                allowed,
                reason,
            } => {
                if let Some(request) = self.active.get(&request_id)
                    && let Some(sender) = request
                        .authorizations
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&authorization_id)
                {
                    let decision = if allowed {
                        Ok(())
                    } else {
                        Err(AdmissionError::Denied(
                            reason.unwrap_or_else(|| "denied by host".into()),
                        ))
                    };
                    let _ = sender.send(decision);
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

    fn start(
        &mut self,
        id: RequestId,
        name: String,
        arguments: Value,
        capabilities: Vec<Capability>,
    ) -> WorkerResult {
        if self.active.contains_key(&id) {
            return Err(format!("duplicate request ID {}", id.get()).into());
        }
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
                tool: name.clone(),
                output: self.output.clone(),
                pending: authorizations.clone(),
                next: StdMutex::new(Some(AuthorizationId(0))),
            }),
            arguments.clone(),
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
        self.tasks.spawn(async move {
            let execution = async {
                let call = catalog.run(&name, arguments, context, &root);
                tokio::pin!(call);
                let result = tokio::select! {
                    result = &mut call => result,
                    () = cancellation.cancelled() => {
                        let _ = tokio::time::timeout(CANCELLATION_GRACE, &mut call).await;
                        Err(LocalError::Cancelled)
                    }
                };
                if cancellation.is_cancelled() {
                    Err(LocalError::Cancelled)
                } else {
                    result
                }
            }
            .await;
            let result = match produced.settle().await {
                Ok(()) => execution.map(Into::into).map_err(remote_error),
                Err(error) => Err(remote_error(LocalError::Io(error))),
            };
            (WorkerTask::Request(id), sender.finish(result).await)
        });
        Ok(())
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
            let id = id
                .ok_or_else(|| AdmissionError::Failed("authorization ID space exhausted".into()))?;
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
                .map_err(|_| AdmissionError::Denied("host authorization channel closed".into()))?
        })
    }
}

fn remote_error(error: LocalError) -> RemoteToolError {
    match error {
        LocalError::Denied(message) => RemoteToolError {
            message,
            denial: Some(crate::tool::Denial::permission_denied()),
            output: None,
        },
        LocalError::FailedWithOutput { message, output } => RemoteToolError {
            message,
            denial: None,
            output: Some(Box::new((*output).into())),
        },
        error => RemoteToolError {
            message: error.to_string(),
            denial: None,
            output: None,
        },
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
        remote::{RejectSensitivePrompts, client::PooledConnection, transport::Transport},
        tests::{RecordingPolicy, TestRuntime},
        tool::{
            ToolContext,
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
                "remote",
                authorization.clone(),
                Arc::new(RejectSensitivePrompts),
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
            let mut spec = JobSpec::test(self.runtime.agent.clone(), name);
            spec.arguments = arguments.clone();
            spec.location = ExecutionLocation {
                target: "remote".into(),
                workspace: std::fs::canonicalize(".").unwrap(),
            };
            let location = spec.location.clone();
            let mut lease = self.runtime.jobs.create(spec).await.unwrap();
            let job = lease.id();
            self.runtime
                .jobs
                .transition(job, JobState::Running)
                .await
                .unwrap();
            let context = ToolContext::new(
                AuthorizationSubject {
                    agent: self.runtime.agent.clone(),
                    job,
                    parent: None,
                    capabilities,
                    cancellation: lease.cancellation_token(),
                },
                location,
                ExecutionLocation::root(self.runtime.root.path().to_owned()),
                lease.take_input(),
                self.runtime.jobs.clone(),
            )
            .with_invocation_authority(
                self.authorization.clone(),
                name.to_owned(),
                arguments.clone(),
            );
            let connection = self.connection.clone();
            let name = name.to_owned();
            lease
                .start_supervised(async move {
                    let result = connection.execute(name, arguments, &context).await;
                    if context.is_cancelled() {
                        Err(crate::tool::ToolError::Cancelled)
                    } else {
                        result.map_err(crate::remote::RemoteError::into_tool_error)
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
                    serde_json::json!({"argv":["/bin/sh", "-c", "printf exact"]}),
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
                    serde_json::json!({"argv":["/bin/sh", "-c", stat]}),
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
        let expected = ResourceId::path("remote", &path);
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
                .any(|permission| permission.resource == ResourceId::path("remote", file.path()))
        );
        let sibling = worker
            .tool(
                CapabilitySet::default(),
                "exec",
                serde_json::json!({"argv":["/bin/sh", "-c", "printf sibling"]}),
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
                serde_json::json!({"argv":["/bin/sh", "-c", "printf reusable"]}),
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
                serde_json::json!({"argv":["/bin/sh", "-c", "printf before; sleep 3600"]}),
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
        let job = worker.tool(CapabilitySet::default(), "exec", serde_json::json!({"argv":["/bin/sh", "-c", "printf before; sleep 30"], "timeout":1})).await;
        let result = worker.result(job).await;
        assert_eq!(result.state, JobState::Failed);
        assert_eq!(result.output.unwrap()["timed_out"], true);
        let saved = worker.runtime.jobs.output(job).test_document().unwrap();
        assert_eq!(saved["capture_complete"], false);
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
