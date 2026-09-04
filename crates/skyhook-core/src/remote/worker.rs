use base64::Engine as _;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{
        Arc, OnceLock,
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
    remote::protocol::{RemoteToolError, Request, Response, read_frame, write_frame},
    session::SessionStore,
    tool::{
        ToolRegistryBuilder,
        builtins::{install_script_tool_weak, register_worker_tools},
        executor::{ExecutionError, ExecutionResult, ToolExecutor},
        policy::{AuthorizationRequest, Policy, PolicyDecision, PolicyFuture},
    },
};

pub async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::fs::canonicalize(".")?;
    serve_io_at(tokio::io::stdin(), tokio::io::stdout(), root).await
}

pub async fn serve_with_authorization_root(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let root = std::fs::canonicalize(root)?;
    serve_io_at(tokio::io::stdin(), tokio::io::stdout(), root).await
}

#[cfg(test)]
async fn serve_io<R, W>(input: R, output: W) -> Result<(), Box<dyn std::error::Error>>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let root = std::fs::canonicalize(".")?;
    serve_io_at(input, output, root).await
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
        Some(Request::Hello) => write_frame(&mut output, &Response::Ready).await?,
        Some(request) => return Err(format!("expected hello, received {request:?}").into()),
        None => return Ok(()),
    }

    let output = Arc::new(Mutex::new(output));
    let authorizations = Arc::new(Mutex::new(HashMap::new()));
    let temporary = tempfile::Builder::new()
        .prefix("skyhook-worker-")
        .tempdir()?;
    let store = SessionStore::create_ephemeral(temporary.path()).await?;
    let worker_agent = AgentId::root(store.id());
    let jobs = JobManager::new(store.clone());
    let slot = Arc::new(OnceLock::new());
    let mut builder = ToolRegistryBuilder::default();
    register_worker_tools(&mut builder, store.clone(), jobs.clone())?;
    install_script_tool_weak(&mut builder, Arc::downgrade(&slot))?;
    let executor = ToolExecutor::new(
        builder.build(),
        Arc::new(ForwardPolicy {
            output: output.clone(),
            authorizations: authorizations.clone(),
            next_id: AtomicU64::new(1),
        }),
        jobs.clone(),
        std::fs::canonicalize(".")?,
    )
    .with_authorization_root(authorization_root);
    slot.set(executor.clone())
        .map_err(|_| "worker executor already initialized")?;
    let (requests, mut incoming) = mpsc::channel(32);
    let (started_jobs, mut started) = mpsc::channel(32);
    let reader = tokio::spawn(read_requests(input, requests));
    let mut tasks = JoinSet::new();
    let mut active = HashMap::new();
    let mut cancelled = HashSet::new();
    loop {
        tokio::select! {
            request = incoming.recv() => {
                let Some(request) = request else {
                    jobs.cancel_all(&worker_agent).await;
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    return match reader.await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(error)) => Err(error.into()),
                        Err(error) => Err(error.into()),
                    };
                };
                match request {
                    Request::Tool { request_id, name, arguments } => {
                        if request_id == 0 || active.insert(request_id, None).is_some() {
                            reader.abort();
                            return Err(format!("invalid or duplicate request ID {request_id}").into());
                        }
                        let executor = executor.clone();
                        let store = store.clone();
                        let output = output.clone();
                        let worker_agent = worker_agent.clone();
                        let started_jobs = started_jobs.clone();
                        tasks.spawn(async move {
                            let result = match executor
                                .start_scoped(worker_agent, &name, arguments, None, request_id)
                                .await
                            {
                                Ok(started) => {
                                    let _ = started_jobs.send((request_id, started.job)).await;
                                    executor.collect_started(started).await
                                }
                                Err(error) => Err(error),
                            };
                            let result = externalize_result(result, &store).await;
                            let response = Response::Tool { request_id, result };
                            let result = write_frame(&mut *output.lock().await, &response).await;
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
                    Request::Hello => {
                        reader.abort();
                        return Err("received a second hello".into());
                    }
                }
            }
            completed = tasks.join_next(), if !tasks.is_empty() => {
                let (request_id, result) = match completed {
                    Some(Ok(completed)) => completed,
                    Some(Err(error)) => {
                        reader.abort();
                        tasks.abort_all();
                        return Err(error.into());
                    }
                    None => {
                        reader.abort();
                        return Err("remote request set ended unexpectedly".into());
                    }
                };
                active.remove(&request_id);
                cancelled.remove(&request_id);
                if let Err(error) = result {
                    reader.abort();
                    tasks.abort_all();
                    return Err(error.into());
                }
                if tasks.is_empty()
                    && let Err(error) = jobs.prune_claimed().await
                {
                    reader.abort();
                    return Err(error.into());
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

struct ForwardPolicy<W> {
    output: Arc<Mutex<W>>,
    authorizations: PendingAuthorizations,
    next_id: AtomicU64,
}

type PendingAuthorizations = Arc<Mutex<HashMap<(u64, u64), oneshot::Sender<PolicyDecision>>>>;

impl<W> Policy for ForwardPolicy<W>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    fn authorize(&self, mut request: AuthorizationRequest) -> PolicyFuture<'_> {
        let Some(request_id) = request.scope else {
            return Box::pin(async {
                PolicyDecision::Deny {
                    reason: "remote authorization scope is unavailable".to_owned(),
                }
            });
        };
        if request.parent.is_none() {
            request
                .permissions
                .retain(|permission| permission.resource.namespace == "path");
            if request.permissions.is_empty() {
                return Box::pin(async { PolicyDecision::allow() });
            }
        }
        let authorization_id = self.next_id.fetch_add(1, Ordering::Relaxed);
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
                output,
            })
        }
    }
}

fn remote_error(error: impl ToString) -> RemoteToolError {
    RemoteToolError {
        message: error.to_string(),
        output: None,
    }
}

async fn externalize_images(
    output: crate::tool::ToolOutput,
    store: &SessionStore,
) -> Result<crate::remote::protocol::RemoteToolOutput, Box<dyn std::error::Error>> {
    let mut images = Vec::new();
    for mut image in output.images {
        let data_base64 = if let Some(data) = image.data_base64.take() {
            data
        } else {
            base64::engine::general_purpose::STANDARD.encode(store.read_blob(&image).await?)
        };
        images.push(crate::remote::protocol::RemoteImage {
            reference: image,
            data_base64,
        });
    }
    Ok(crate::remote::protocol::RemoteToolOutput {
        value: output.value,
        images,
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

    use super::*;
    use crate::remote::protocol::RemoteToolOutput;

    #[tokio::test]
    async fn tool_requests_execute_concurrently_and_reply_on_completion() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (mut client_input, mut client_output) = tokio::io::split(client);
        let (server_input, server_output) = tokio::io::split(server);
        let worker = tokio::spawn(async move {
            serve_io(server_input, server_output)
                .await
                .map_err(|error| error.to_string())
        });

        write_frame(&mut client_output, &Request::Hello)
            .await
            .unwrap();
        assert!(matches!(
            read_frame::<_, Response>(&mut client_input).await.unwrap(),
            Some(Response::Ready)
        ));
        write_frame(
            &mut client_output,
            &Request::Tool {
                request_id: 1,
                name: "shell".to_owned(),
                arguments: serde_json::json!({"command":"sleep 1; printf slow"}),
            },
        )
        .await
        .unwrap();
        write_frame(
            &mut client_output,
            &Request::Tool {
                request_id: 2,
                name: "shell".to_owned(),
                arguments: serde_json::json!({"command":"printf fast"}),
            },
        )
        .await
        .unwrap();

        let first = read_frame::<_, Response>(&mut client_input)
            .await
            .unwrap()
            .unwrap();
        let second = read_frame::<_, Response>(&mut client_input)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            first,
            Response::Tool {
                request_id: 2,
                result: Ok(RemoteToolOutput { value, .. }),
            } if value["stdout"] == "fast"
        ));
        assert!(matches!(
            second,
            Response::Tool {
                request_id: 1,
                result: Ok(RemoteToolOutput { value, .. }),
            } if value["stdout"] == "slow"
        ));

        write_frame(
            &mut client_output,
            &Request::Tool {
                request_id: 3,
                name: "shell".to_owned(),
                arguments: serde_json::json!({"command":"sleep 10; printf cancelled"}),
            },
        )
        .await
        .unwrap();
        write_frame(
            &mut client_output,
            &Request::Tool {
                request_id: 4,
                name: "shell".to_owned(),
                arguments: serde_json::json!({"command":"sleep 0.1; printf sibling"}),
            },
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        write_frame(&mut client_output, &Request::Cancel { request_id: 3 })
            .await
            .unwrap();

        let mut cancelled = None;
        let mut sibling = None;
        for _ in 0..2 {
            let response = read_frame::<_, Response>(&mut client_input)
                .await
                .unwrap()
                .unwrap();
            match response {
                Response::Tool {
                    request_id: 3,
                    result,
                } => cancelled = Some(result),
                Response::Tool {
                    request_id: 4,
                    result,
                } => sibling = Some(result),
                response => panic!("unexpected response: {response:?}"),
            }
        }
        assert!(matches!(
            cancelled,
            Some(Err(RemoteToolError { message, .. })) if message.contains("cancelled")
        ));
        assert!(matches!(
            sibling,
            Some(Ok(RemoteToolOutput { value, .. })) if value["stdout"] == "sibling"
        ));

        write_frame(
            &mut client_output,
            &Request::Tool {
                request_id: 5,
                name: "shell".to_owned(),
                arguments: serde_json::json!({"command":"printf reusable"}),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<_, Response>(&mut client_input).await.unwrap(),
            Some(Response::Tool {
                request_id: 5,
                result: Ok(RemoteToolOutput { value, .. }),
            }) if value["stdout"] == "reusable"
        ));

        drop(client_output);
        drop(client_input);
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .expect("worker should stop on EOF")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn external_paths_request_host_authorization() {
        let authorization_root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("outside.txt");
        std::fs::write(&path, "visible after approval").unwrap();
        let authorization_root = std::fs::canonicalize(authorization_root.path()).unwrap();
        let path = std::fs::canonicalize(path).unwrap();
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (mut client_input, mut client_output) = tokio::io::split(client);
        let (server_input, server_output) = tokio::io::split(server);
        let worker = tokio::spawn(async move {
            serve_io_at(server_input, server_output, authorization_root)
                .await
                .map_err(|error| error.to_string())
        });

        write_frame(&mut client_output, &Request::Hello)
            .await
            .unwrap();
        assert!(matches!(
            read_frame::<_, Response>(&mut client_input).await.unwrap(),
            Some(Response::Ready)
        ));
        write_frame(
            &mut client_output,
            &Request::Tool {
                request_id: 10,
                name: "read".to_owned(),
                arguments: serde_json::json!({"path": path}),
            },
        )
        .await
        .unwrap();

        let authorization = read_frame::<_, Response>(&mut client_input)
            .await
            .unwrap()
            .unwrap();
        let authorization_id = match authorization {
            Response::Authorization {
                request_id: 10,
                authorization_id,
                permissions,
                ..
            } => {
                assert_eq!(permissions.len(), 1);
                assert_eq!(
                    permissions[0].capability,
                    crate::tool::policy::Capability::Read
                );
                assert_eq!(
                    permissions[0].resource,
                    crate::tool::policy::ResourceId::path("root", &path),
                );
                authorization_id
            }
            response => panic!("unexpected response: {response:?}"),
        };
        write_frame(
            &mut client_output,
            &Request::AuthorizationDecision {
                request_id: 10,
                authorization_id,
                allowed: true,
                reason: None,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<_, Response>(&mut client_input).await.unwrap(),
            Some(Response::Tool {
                request_id: 10,
                result: Ok(RemoteToolOutput { value, .. }),
            }) if value["content"] == "visible after approval"
        ));

        drop(client_output);
        drop(client_input);
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .expect("worker should stop on EOF")
            .unwrap()
            .unwrap();
    }
}
