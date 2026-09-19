use base64::Engine as _;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Mutex, mpsc, oneshot},
    task::JoinSet,
};

use crate::{
    identity::AgentId,
    job::JobManager,
    remote::protocol::{
        AuthorizationId, PROTOCOL_VERSION, RemoteToolError, Request, RequestId, Response,
        read_frame, write_frame,
    },
    session::SessionStore,
    tool::{
        ToolRegistryBuilder,
        builtins::register_worker_tools,
        executor::{ExecutionError, ExecutionResult, ToolExecutor},
        policy::{AuthorizationRequest, Policy, PolicyDecision, PolicyFuture},
    },
};

pub async fn serve_with_authorization_root(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let root = std::fs::canonicalize(root)?;
    serve_io_at(tokio::io::stdin(), tokio::io::stdout(), root).await
}

async fn serve_io_at<R, W>(
    mut input: R,
    mut output: W,
    authorization_root: std::path::PathBuf,
) -> Result<(), Box<dyn std::error::Error>>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    match read_frame::<_, Request>(&mut input).await? {
        Some(Request::Hello {
            version: PROTOCOL_VERSION,
        }) => {
            write_frame(
                &mut output,
                &Response::Ready {
                    version: PROTOCOL_VERSION,
                },
            )
            .await?
        }
        Some(request) => return Err(format!("expected hello, received {request:?}").into()),
        None => return Ok(()),
    }

    let output = Arc::new(Mutex::new(output));
    let authorizations = Arc::new(Mutex::new(HashMap::new()));
    let mut services = super::service::WorkerServices::new(output.clone())?;
    let temporary = tempfile::Builder::new()
        .prefix("skyhook-worker-")
        .tempdir()?;
    let store = SessionStore::create_ephemeral(temporary.path()).await?;
    let worker_agent = AgentId::root(store.id());
    let workspace = std::fs::canonicalize(".")?;
    // Worker jobs belong to a tool-only root agent; each request carries its exact capabilities.
    store
        .append_all(vec![
            (
                worker_agent.clone(),
                crate::session::SessionEvent::SessionStarted {
                    targets: Vec::new(),
                    capabilities: crate::tool::policy::Capability::ALL.to_vec(),
                    max_child_depth: 0,
                },
            ),
            (
                worker_agent.clone(),
                crate::session::SessionEvent::AgentStarted {
                    parent: None,
                    owner_job: None,
                    profile: None,
                    available_depth: 0,
                    capabilities: crate::tool::policy::Capability::ALL.to_vec(),
                    location: crate::execution::ExecutionLocation::root(workspace.clone()),
                },
            ),
        ])
        .await?;
    let jobs = JobManager::new(store.clone());
    let mut builder = ToolRegistryBuilder::default();
    register_worker_tools(&mut builder, store.clone())?;
    crate::tool::builtins::skill_transfer::register_worker(&mut builder)?;
    let executor = ToolExecutor::new(
        builder.build(),
        Arc::new(ForwardPolicy {
            output: output.clone(),
            authorizations: authorizations.clone(),
            next_id: AtomicU64::new(1),
        }),
        jobs.clone(),
        workspace,
    )
    .with_authorization_root(authorization_root)
    .with_process_environment(services.environment.clone());
    let (requests, mut incoming) = mpsc::channel(32);
    let (started_jobs, mut started) = mpsc::channel(32);
    let mut reader = tokio::spawn(read_requests(input, requests));
    // Dropping the task set on return aborts every running request.
    let mut tasks = JoinSet::new();
    let mut active = HashMap::new();
    let mut cancelled = HashSet::new();
    let result = async {
    loop {
        tokio::select! {
            request = incoming.recv() => {
                let Some(request) = request else {
                    jobs.cancel_all(&worker_agent).await;
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    let ended: Result<(), Box<dyn std::error::Error>> = match (&mut reader).await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(error)) => Err(error.into()),
                        Err(error) => Err(error.into()),
                    };
                    return ended;
                };
                match request {
                    Request::Tool { request_id, name, arguments, capabilities } => {
                        if active.insert(request_id, None).is_some() {
                            return Err(format!("duplicate request ID {}", request_id.get()).into());
                        }
                        // Collect an exact set: defaults would restore capabilities the caller lacks.
                        let executor = executor.clone().with_capabilities(capabilities.into_iter().collect());
                        let store = store.clone();
                        let output = output.clone();
                        let worker_agent = worker_agent.clone();
                        let started_jobs = started_jobs.clone();
                        tasks.spawn(async move {
                            let mut captured_job = None;
                            let result = match executor
                                .start_scoped(worker_agent, &name, arguments, None, request_id.get())
                                .await
                            {
                                Ok(started) => {
                                    let _ = started_jobs.send((request_id, started.job)).await;
                                    captured_job = Some(started.job);
                                    executor.collect_for_transfer(started).await
                                }
                                Err(error) => Err(error),
                            };
                            let mut result = externalize_result(result, &store).await;
                            if let Some(job) = captured_job {
                                let saved = executor.jobs().output(job);
                                match tokio::task::spawn_blocking(move || crate::job::output::transfer_fields(&saved)).await.map_err(|error| crate::tool::ToolError::Failed(error.to_string())).and_then(|fields| fields) {
                                    Ok(fields) => for (field, kind, source) in fields {
                                        if let Err(error) = super::protocol::write_artifact(&output, request_id, field, kind, source).await {
                                            return (request_id, Err(error));
                                        }
                                    },
                                    Err(error) => result = Err(remote_error(error)),
                                }
                            }
                            let result = super::protocol::write_tool_result(&output, request_id, &result).await;
                            if result.is_ok() && let Some(job) = captured_job { let _ = executor.jobs().claim(job).await; }
                            (request_id, result)
                        });
                    }
                    Request::Cancel { request_id } => {
                        match active.get(&request_id) {
                            Some(Some(job)) => {
                                let _ = jobs.cancel(*job).await;
                            }
                            Some(None) => {
                                cancelled.insert(request_id);
                            }
                            None => {}
                        }
                    }
                    Request::AuthorizationDecision {
                        request_id,
                        authorization_id,
                        allowed,
                        reason,
                    } => {
                        if let Some(sender) = authorizations
                            .lock()
                            .await
                            .remove(&(request_id, authorization_id))
                        {
                            let decision = if allowed {
                                PolicyDecision::allow()
                            } else {
                                PolicyDecision::Deny {
                                    reason: reason.unwrap_or_else(|| "denied by host".to_owned()),
                                }
                            };
                            let _ = sender.send(decision);
                        }
                    }
                    control @ (Request::OpenSsh { .. } | Request::StreamData { .. } | Request::StreamEnd { .. } | Request::StreamClose { .. } | Request::StreamAck { .. } | Request::SensitiveAnswer { .. }) => services.handle(control).await?,
                    Request::Hello { .. } => return Err("received a second hello".into()),
                }
            }
            completed = services.tasks.join_next(), if !services.tasks.is_empty() => {
                let error: Option<Box<dyn std::error::Error>> = match completed {
                    Some(Ok(Ok(()))) => None,
                    Some(Ok(Err(error))) => Some(error.into()),
                    Some(Err(error)) => Some(error.into()),
                    None => unreachable!("nonempty service task set"),
                };
                if let Some(error) = error {
                    return Err(error);
                }
            }
            completed = tasks.join_next(), if !tasks.is_empty() => {
                let (request_id, result) = match completed {
                    Some(Ok(completed)) => completed,
                    Some(Err(error)) => return Err(error.into()),
                    None => return Err("remote request set ended unexpectedly".into()),
                };
                active.remove(&request_id);
                cancelled.remove(&request_id);
                result?;
                if tasks.is_empty() {
                    jobs.prune_claimed().await?;
                }
            }
            started_job = started.recv() => {
                if let Some((request_id, job)) = started_job
                    && let Some(active_job) = active.get_mut(&request_id)
                {
                    *active_job = Some(job);
                    if cancelled.remove(&request_id) {
                        let _ = jobs.cancel(job).await;
                    }
                }
            }
        }
    }
    }
    .await;
    reader.abort();
    result
}

struct ForwardPolicy<W> {
    output: Arc<Mutex<W>>,
    authorizations: PendingAuthorizations,
    next_id: AtomicU64,
}

type PendingAuthorizations =
    Arc<Mutex<HashMap<(RequestId, AuthorizationId), oneshot::Sender<PolicyDecision>>>>;

impl<W> Policy for ForwardPolicy<W>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    fn authorize(&self, mut request: AuthorizationRequest) -> PolicyFuture<'_> {
        let Some(request_id) = request.scope.and_then(RequestId::new) else {
            return Box::pin(async {
                PolicyDecision::Deny {
                    reason: "remote authorization scope is unavailable".to_owned(),
                }
            });
        };
        if request.parent.is_none() {
            // Only the ordinary static workspace permissions were approved by
            // the caller before dispatch. Preserve destination-derived resources
            // (network origins, paths, and other namespaces), including redirects
            // from a top-level worker invocation whose parent remains None.
            request.permissions.retain(|permission| {
                !(matches!(
                    permission.resource,
                    crate::tool::policy::ResourceId::Workspace { .. }
                ) && matches!(
                    permission.capability,
                    crate::tool::policy::Capability::Read
                        | crate::tool::policy::Capability::Write
                        | crate::tool::policy::Capability::Exec
                ))
            });
            if request.permissions.is_empty() {
                return Box::pin(async { PolicyDecision::allow() });
            }
        }
        let authorization_id = AuthorizationId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let output = self.output.clone();
        let authorizations = self.authorizations.clone();
        Box::pin(async move {
            let (sender, receiver) = oneshot::channel();
            authorizations
                .lock()
                .await
                .insert((request_id, authorization_id), sender);
            let response = Response::Authorization {
                request_id,
                authorization_id,
                tool: request.tool,
                permissions: request.permissions,
                arguments: request.arguments,
            };
            if let Err(error) = write_frame(&mut *output.lock().await, &response).await {
                authorizations
                    .lock()
                    .await
                    .remove(&(request_id, authorization_id));
                return PolicyDecision::Deny {
                    reason: format!("could not request host authorization: {error}"),
                };
            }
            receiver.await.unwrap_or_else(|_| PolicyDecision::Deny {
                reason: "host authorization channel closed".to_owned(),
            })
        })
    }
}

async fn read_requests<R>(
    mut input: R,
    requests: mpsc::Sender<Request>,
) -> Result<(), std::io::Error>
where
    R: AsyncRead + Unpin,
{
    while let Some(request) = read_frame(&mut input).await? {
        if requests.send(request).await.is_err() {
            break;
        }
    }
    Ok(())
}

async fn externalize_result(
    result: Result<ExecutionResult, ExecutionError>,
    store: &SessionStore,
) -> Result<crate::remote::protocol::RemoteToolOutput, RemoteToolError> {
    match result {
        Ok(result) => externalize_images(result.output, store)
            .await
            .map_err(remote_error),
        Err(error) => {
            let failure = error.into_failure();
            let output = match failure.output {
                Some(output) => Some(
                    externalize_images(output, store)
                        .await
                        .map_err(remote_error)?,
                ),
                None => None,
            };
            Err(RemoteToolError {
                message: failure.message,
                denial: failure.denial,
                output: output.map(Box::new),
            })
        }
    }
}

fn remote_error(error: impl ToString) -> RemoteToolError {
    RemoteToolError {
        message: error.to_string(),
        denial: None,
        output: None,
    }
}

async fn externalize_images(
    output: crate::tool::ToolOutput,
    store: &SessionStore,
) -> Result<crate::remote::protocol::RemoteToolOutput, Box<dyn std::error::Error>> {
    let mut images = Vec::new();
    for image in output.images {
        let bytes = store
            .read_blob(&image.blob, crate::media::MAX_IMAGE_BYTES as usize)
            .await?;
        images.push(crate::remote::protocol::RemoteImage {
            file: image.file,
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }
    Ok(crate::remote::protocol::RemoteToolOutput {
        value: output.value,
        images,
        streams: output.streams,
    })
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
    use std::time::Duration;
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

    use super::*;
    use crate::remote::protocol::RemoteToolOutput;
    use crate::tool::policy::{Capability, CapabilitySet};

    fn id(value: u64) -> RequestId {
        RequestId::new(value).unwrap()
    }

    /// A handshaken client connection to a worker serving over an in-memory duplex.
    struct Harness {
        input: ReadHalf<DuplexStream>,
        output: WriteHalf<DuplexStream>,
        worker: tokio::task::JoinHandle<Result<(), String>>,
    }

    impl Harness {
        async fn start(root: std::path::PathBuf) -> Self {
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (input, output) = tokio::io::split(client);
            let (server_input, server_output) = tokio::io::split(server);
            let worker = tokio::spawn(async move {
                serve_io_at(server_input, server_output, root)
                    .await
                    .map_err(|error| error.to_string())
            });
            let mut harness = Self {
                input,
                output,
                worker,
            };
            harness
                .send(Request::Hello {
                    version: PROTOCOL_VERSION,
                })
                .await;
            assert!(matches!(
                harness.recv().await,
                Response::Ready {
                    version: PROTOCOL_VERSION
                }
            ));
            harness
        }

        async fn send(&mut self, request: Request) {
            write_frame(&mut self.output, &request).await.unwrap();
        }

        async fn tool(
            &mut self,
            request_id: u64,
            capabilities: Vec<Capability>,
            name: &str,
            arguments: serde_json::Value,
        ) {
            self.send(Request::Tool {
                request_id: id(request_id),
                capabilities,
                name: name.to_owned(),
                arguments,
            })
            .await;
        }

        async fn recv(&mut self) -> Response {
            tokio::time::timeout(Duration::from_secs(10), read_frame(&mut self.input))
                .await
                .unwrap()
                .unwrap()
                .unwrap()
        }

        async fn finish(self) {
            drop((self.input, self.output));
            tokio::time::timeout(Duration::from_secs(2), self.worker)
                .await
                .expect("worker should stop on EOF")
                .unwrap()
                .unwrap();
        }
    }

    fn defaults() -> Vec<Capability> {
        CapabilitySet::default().iter().collect()
    }

    #[tokio::test]
    async fn tool_image_externalization_reads_verified_bounded_blobs() {
        use crate::{media::MAX_IMAGE_BYTES, tool::ToolOutput};
        use base64::engine::general_purpose::STANDARD;
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::create(directory.path()).await.unwrap();
        let png = crate::tests::png(b"image bytes");
        let image = store
            .store_image(Some("image.png".into()), &png)
            .await
            .unwrap();
        let output = |image| ToolOutput::default().with_images(vec![image]);
        let result = externalize_images(output(image.clone()), &store)
            .await
            .unwrap();
        assert_eq!(result.images[0].data_base64, STANDARD.encode(png.bytes()));
        assert_eq!(result.images[0].file.as_deref(), Some("image.png"));
        let corruptions: [fn(&mut crate::media::ImageRef); 3] = [
            |image| image.blob.sha256 = crate::media::BlobDigest::of(b"wrong bytes"),
            |image| image.blob.bytes += 1,
            |image| image.blob.bytes = MAX_IMAGE_BYTES + 1,
        ];
        for corrupt in corruptions {
            let mut corrupted = image.clone();
            corrupt(&mut corrupted);
            assert!(externalize_images(output(corrupted), &store).await.is_err());
        }
    }

    #[tokio::test]
    async fn incompatible_client_handshakes_are_rejected() {
        for hello in [
            serde_json::json!({"type":"hello"}),
            serde_json::json!({"type":"hello", "version": PROTOCOL_VERSION - 1}),
        ] {
            let (mut client, server) = tokio::io::duplex(4096);
            write_frame(&mut client, &hello).await.unwrap();
            let (input, output) = tokio::io::split(server);
            let root = std::fs::canonicalize(".").unwrap();
            assert!(serve_io_at(input, output, root).await.is_err());
            let response = read_frame::<_, Response>(&mut client).await.unwrap();
            assert!(response.is_none());
        }
    }

    #[tokio::test]
    async fn worker_enforces_exact_capabilities_and_noninteractive_exec_sessions() {
        let mut worker = Harness::start(std::fs::canonicalize(".").unwrap()).await;
        let exact = serde_json::json!({"argv":["/bin/sh", "-c", "printf exact"]});
        for (index, capabilities) in [
            vec![],
            vec![Capability::Read],
            vec![Capability::Exec],
            vec![],
        ]
        .into_iter()
        .enumerate()
        {
            let allowed = capabilities.contains(&Capability::Exec);
            worker
                .tool(index as u64 + 1, capabilities, "exec", exact.clone())
                .await;
            let response = worker.recv().await;
            let Response::Tool { request_id, result } = response else {
                panic!("expected tool result, got {response:?}")
            };
            assert_eq!(request_id, id(index as u64 + 1));
            if allowed {
                assert_eq!(result.unwrap().value["stdout"], "exact");
            } else {
                assert_eq!(
                    result.unwrap_err().message,
                    "invalid tool arguments: tool `exec` is unavailable",
                    "missing Exec must be rejected locally, not forwarded to host"
                );
            }
        }
        #[cfg(target_os = "linux")]
        for (index, interactive) in [false, true, false].into_iter().enumerate() {
            let mut capabilities = vec![Capability::Exec];
            if interactive {
                capabilities.push(Capability::Interactive);
            }
            let stat = "read pid comm state ppid pgrp sid rest < /proc/self/stat; printf '%s %s' \"$pid\" \"$sid\"";
            let argv = serde_json::json!({"argv":["/bin/sh", "-c", stat]});
            worker
                .tool(index as u64 + 10, capabilities, "exec", argv)
                .await;
            let Response::Tool { result, .. } = worker.recv().await else {
                panic!("expected tool result")
            };
            let result = result.unwrap();
            let ids: Vec<_> = result.value["stdout"]
                .as_str()
                .unwrap()
                .split_whitespace()
                .collect();
            assert_eq!(ids.len(), 2);
            assert_eq!(
                ids[0] == ids[1],
                !interactive,
                "only noninteractive exec should be a session leader: {ids:?}"
            );
        }
        worker.finish().await;
    }

    #[tokio::test]
    async fn workspace_suppression_preserves_dynamic_and_nested_permissions() {
        use crate::{
            identity::{JobId, SessionId},
            tool::policy::{PermissionUse, ResourceId},
        };
        let (mut host, worker) = tokio::io::duplex(16 * 1024);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let policy = Arc::new(ForwardPolicy {
            output: Arc::new(Mutex::new(worker)),
            authorizations: pending.clone(),
            next_id: AtomicU64::new(0),
        });
        for (origin, allow, parent) in [
            ("https://initial.test", true, None),
            ("https://redirect.test:8443", false, None),
            ("https://nested.test", true, Some(JobId::new(2).unwrap())),
        ] {
            let workspace = ResourceId::workspace("root", std::path::Path::new("/workspace"));
            let mut permissions: Vec<_> = [Capability::Read, Capability::Write, Capability::Exec]
                .into_iter()
                .map(|capability| PermissionUse::new(capability, workspace.clone()))
                .collect();
            // Workspace permissions are suppressed only for top-level jobs.
            let mut expected_permissions = if parent.is_some() {
                permissions.clone()
            } else {
                vec![]
            };
            let dynamic = vec![
                PermissionUse::new(Capability::Network, ResourceId::network("root", origin)),
                PermissionUse::new(Capability::Interactive, workspace),
                PermissionUse::new(
                    Capability::Read,
                    ResourceId::path("root", "/outside".as_ref()),
                ),
                PermissionUse::new(Capability::Mcp, ResourceId::mcp("root", "tool")),
            ];
            permissions.extend(dynamic.clone());
            expected_permissions.extend(dynamic);
            let arguments = serde_json::json!({"url":"https://initial.test", "network_origin":origin, "insecure":true});
            let request = AuthorizationRequest {
                agent: crate::identity::AgentId::root(SessionId::from_bytes([1; 16])),
                job: JobId::new(1).unwrap(),
                parent,
                scope: Some(7),
                tool: "fetch".to_owned(),
                permissions,
                arguments: arguments.clone(),
            };
            let policy = policy.clone();
            let decision = tokio::spawn(async move { policy.authorize(request).await });
            let response =
                tokio::time::timeout(Duration::from_secs(2), read_frame::<_, Response>(&mut host))
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            let Response::Authorization {
                request_id,
                authorization_id,
                tool,
                permissions,
                arguments: forwarded,
            } = response
            else {
                panic!("expected network authorization");
            };
            assert_eq!(
                (request_id.get(), tool.as_str(), &permissions, &forwarded),
                (7, "fetch", &expected_permissions, &arguments)
            );
            let expected = if allow {
                PolicyDecision::allow()
            } else {
                PolicyDecision::Deny {
                    reason: "redirect denied".to_owned(),
                }
            };
            let sender = pending.lock().await.remove(&(request_id, authorization_id));
            sender.unwrap().send(expected.clone()).unwrap();
            assert_eq!(decision.await.unwrap(), expected);
        }
        assert!(pending.lock().await.is_empty());
    }

    fn stdout(response: &Response) -> (u64, Result<&str, &str>) {
        match response {
            Response::Tool { request_id, result } => (
                request_id.get(),
                result
                    .as_ref()
                    .map(|RemoteToolOutput { value, .. }| value["stdout"].as_str().unwrap())
                    .map_err(|RemoteToolError { message, .. }| message.as_str()),
            ),
            response => panic!("unexpected response: {response:?}"),
        }
    }

    #[tokio::test]
    async fn tool_requests_execute_concurrently_and_reply_on_completion() {
        let mut worker = Harness::start(std::fs::canonicalize(".").unwrap()).await;
        let shell = |command: &str| serde_json::json!({ "command": command });
        // The first request can only finish while the second one runs.
        let flag = tempfile::tempdir().unwrap();
        let flag = flag.path().join("flag").display().to_string();
        let wait = format!("while [ ! -e '{flag}' ]; do sleep 0.01; done; printf slow");
        worker.tool(1, defaults(), "shell", shell(&wait)).await;
        let touch = format!("touch '{flag}'; printf fast");
        worker.tool(2, defaults(), "shell", shell(&touch)).await;
        let mut responses = [worker.recv().await, worker.recv().await];
        responses.sort_by_key(|response| stdout(response).0);
        assert_eq!(
            responses.each_ref().map(stdout),
            [(1, Ok("slow")), (2, Ok("fast"))]
        );

        worker
            .tool(3, defaults(), "shell", shell("sleep 10; printf cancelled"))
            .await;
        worker
            .tool(4, defaults(), "shell", shell("printf sibling"))
            .await;
        assert_eq!(stdout(&worker.recv().await), (4, Ok("sibling")));
        worker.send(Request::Cancel { request_id: id(3) }).await;
        let cancelled = worker.recv().await;
        assert!(matches!(stdout(&cancelled), (3, Err(message)) if message.contains("cancelled")));

        worker
            .tool(5, defaults(), "shell", shell("printf reusable"))
            .await;
        assert_eq!(stdout(&worker.recv().await), (5, Ok("reusable")));
        worker.finish().await;
    }

    #[tokio::test]
    async fn external_paths_request_host_authorization() {
        let authorization_root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("outside.txt");
        std::fs::write(&path, "visible after approval").unwrap();
        let path = std::fs::canonicalize(path).unwrap();
        let mut worker =
            Harness::start(std::fs::canonicalize(authorization_root.path()).unwrap()).await;
        worker
            .tool(10, defaults(), "read", serde_json::json!({"path": path}))
            .await;
        let Response::Authorization {
            request_id,
            authorization_id,
            permissions,
            ..
        } = worker.recv().await
        else {
            panic!("expected authorization request");
        };
        assert_eq!(request_id, id(10));
        let resource = crate::tool::policy::ResourceId::path("root", &path);
        assert_eq!(
            permissions
                .iter()
                .map(|permission| (permission.capability, &permission.resource))
                .collect::<Vec<_>>(),
            [(Capability::Read, &resource)]
        );
        worker
            .send(Request::AuthorizationDecision {
                request_id,
                authorization_id,
                allowed: true,
                reason: None,
            })
            .await;
        assert!(matches!(
            worker.recv().await,
            Response::Tool {
                result: Ok(RemoteToolOutput { value, .. }),
                ..
            } if value["content"] == "visible after approval"
        ));
        worker.finish().await;
    }
}
