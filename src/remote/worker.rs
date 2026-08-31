use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};
use tokio::{io::AsyncReadExt as _, process::Command};

use crate::{
    identity::AgentId,
    job::JobManager,
    provider::protocol::{
        AssistantContent, Message, ModelRequest, ResponseChunk, ToolResult, Usage,
    },
    remote::protocol::{
        PROTOCOL_VERSION, ProcessRequest, RemoteAgentSpec, RemoteAgentStep, RemoteClock, Request,
        Response, read_frame, write_frame,
    },
    session::SessionStore,
    tool::builtins::ProcessOutput,
    tool::{
        ToolRegistryBuilder,
        builtins::{install_script_tool_weak, register_worker_tools},
        executor::ToolExecutor,
        policy::AllowAll,
    },
};

const MAX_OUTPUT: usize = 1024 * 1024;

pub async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = tokio::io::stdin();
    let mut output = tokio::io::stdout();
    let mut agents = HashMap::<String, RemoteAgentState>::new();
    let temporary = tempfile::Builder::new()
        .prefix("skyhook-worker-")
        .tempdir()?;
    let store = SessionStore::create(temporary.path()).await?;
    let worker_agent = AgentId::root(store.id());
    let jobs = JobManager::new(store.clone());
    let slot = Arc::new(OnceLock::new());
    let mut builder = ToolRegistryBuilder::default();
    register_worker_tools(&mut builder, store.clone(), jobs.clone())?;
    install_script_tool_weak(&mut builder, Arc::downgrade(&slot))?;
    let executor = ToolExecutor::new(
        builder.build(),
        Arc::new(AllowAll),
        jobs,
        std::fs::canonicalize(".")?,
    );
    slot.set(executor.clone())
        .map_err(|_| "worker executor already initialized")?;
    while let Some(request) = read_frame::<_, Request>(&mut input).await? {
        let response = match request {
            Request::Hello { version } if version == PROTOCOL_VERSION => Response::Hello {
                version: PROTOCOL_VERSION,
                arch: std::env::consts::ARCH.to_owned(),
                os: std::env::consts::OS.to_owned(),
            },
            Request::Hello { version } => Response::Error {
                message: format!("unsupported protocol version {version}"),
            },
            Request::Execute(request) => Response::Process {
                result: execute(request).await.map_err(|error| error.to_string()),
            },
            Request::Tool { name, arguments } => {
                let result = match executor
                    .execute(worker_agent.clone(), &name, arguments, None)
                    .await
                {
                    Ok(result) => externalize_images(result.output, &store)
                        .await
                        .map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                Response::Tool { result }
            }
            Request::AgentStart { id, spec } => {
                let result = start_agent(&mut agents, id, spec);
                Response::Agent { result }
            }
            Request::AgentProvider {
                id,
                message,
                chunks,
            } => Response::Agent {
                result: continue_provider(&mut agents, &id, message, chunks),
            },
            Request::AgentTools { id, results } => Response::Agent {
                result: continue_tools(&mut agents, &id, results),
            },
            Request::AgentAbort { id } => {
                agents.remove(&id);
                Response::Accepted
            }
            Request::Shutdown => {
                write_frame(&mut output, &Response::Accepted).await?;
                break;
            }
        };
        write_frame(&mut output, &response).await?;
    }
    Ok(())
}

async fn externalize_images(
    output: crate::tool::ToolOutput,
    store: &SessionStore,
) -> Result<crate::remote::protocol::RemoteToolOutput, Box<dyn std::error::Error>> {
    let mut images = Vec::new();
    for image in output.images {
        let data_base64 = if let Some(data) = &image.data_base64 {
            data.clone()
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

struct RemoteAgentState {
    spec: RemoteAgentSpec,
    history: Vec<Message>,
    text: String,
    awaiting_initial_message: bool,
}

fn start_agent(
    agents: &mut HashMap<String, RemoteAgentState>,
    id: String,
    spec: RemoteAgentSpec,
) -> Result<RemoteAgentStep, String> {
    if agents.contains_key(&id) {
        return Err(format!("remote agent `{id}` already exists"));
    }
    let state = RemoteAgentState {
        history: spec.history.clone(),
        spec,
        text: String::new(),
        awaiting_initial_message: true,
    };
    let step = started_step(&id, &state);
    agents.insert(id, state);
    Ok(step)
}

fn continue_provider(
    agents: &mut HashMap<String, RemoteAgentState>,
    id: &str,
    message: Option<Message>,
    chunks: Vec<ResponseChunk>,
) -> Result<RemoteAgentStep, String> {
    let state = agents
        .get_mut(id)
        .ok_or_else(|| format!("unknown remote agent `{id}`"))?;
    match (state.awaiting_initial_message, message) {
        (true, Some(message)) => {
            state.history.push(message);
            state.awaiting_initial_message = false;
        }
        (true, None) => {
            return Err("initial remote provider response omitted its user message".to_owned());
        }
        (false, Some(_)) => {
            return Err("remote user message was synchronized more than once".to_owned());
        }
        (false, None) => {}
    }
    let mut blocks = Vec::new();
    let mut streamed = String::new();
    let mut usage = Usage::default();
    for chunk in chunks {
        match chunk {
            ResponseChunk::TextDelta { text } => streamed.push_str(&text),
            ResponseChunk::Block { block } => blocks.push(block),
            ResponseChunk::Usage { usage: value } => usage = value,
            ResponseChunk::MessageStart { .. }
            | ResponseChunk::ReasoningDelta { .. }
            | ResponseChunk::ToolInputDelta { .. }
            | ResponseChunk::Diagnostic { .. }
            | ResponseChunk::Done { .. } => {}
        }
    }
    if !streamed.is_empty()
        && !blocks
            .iter()
            .any(|block| matches!(block, AssistantContent::Text { .. }))
    {
        blocks.insert(0, AssistantContent::Text { text: streamed });
    }
    if blocks.is_empty() {
        return Err("provider returned no assistant content".to_owned());
    }
    for block in &blocks {
        if let AssistantContent::Text { text } = block {
            state.text.push_str(text);
        }
    }
    state.history.push(Message::Assistant(blocks.clone()));
    let calls = blocks
        .iter()
        .filter_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            AssistantContent::Text { .. } | AssistantContent::Reasoning { .. } => None,
        })
        .collect::<Vec<_>>();
    if calls.is_empty() {
        let text = state.text.clone();
        agents.remove(id);
        Ok(RemoteAgentStep::Complete {
            blocks,
            usage,
            text,
        })
    } else {
        Ok(RemoteAgentStep::Tools {
            blocks,
            usage,
            calls,
        })
    }
}

fn continue_tools(
    agents: &mut HashMap<String, RemoteAgentState>,
    id: &str,
    results: Vec<ToolResult>,
) -> Result<RemoteAgentStep, String> {
    let state = agents
        .get_mut(id)
        .ok_or_else(|| format!("unknown remote agent `{id}`"))?;
    state.history.push(Message::Tool(results));
    Ok(RemoteAgentStep::Provider {
        request: model_request(id, state),
    })
}

fn started_step(id: &str, state: &RemoteAgentState) -> RemoteAgentStep {
    let now = chrono::Local::now();
    RemoteAgentStep::Started {
        request: model_request(id, state),
        clock: RemoteClock {
            date: now.format("%Y-%m-%d").to_string(),
            timezone: iana_time_zone::get_timezone()
                .unwrap_or_else(|_| now.format("%Z").to_string()),
            utc_offset: now.format("%:z").to_string(),
        },
    }
}

fn model_request(id: &str, state: &RemoteAgentState) -> ModelRequest {
    ModelRequest {
        model: state.spec.model.clone(),
        system: state.spec.system.clone(),
        messages: state.history.clone(),
        tools: state.spec.tools.clone(),
        reasoning: state.spec.reasoning.clone(),
        max_output_tokens: state.spec.max_output_tokens,
        correlation: Some(id.to_owned()),
    }
}

pub async fn self_check(expected: &str) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = tokio::fs::read(std::env::current_exe()?).await?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        return Err(format!("shim hash mismatch: expected {expected}, got {actual}").into());
    }
    Ok(())
}

async fn execute(request: ProcessRequest) -> Result<ProcessOutput, std::io::Error> {
    let (mut command, cwd, timeout) = match request {
        ProcessRequest::Exec { argv, cwd, timeout } => {
            let (program, args) = argv.split_first().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "argv cannot be empty")
            })?;
            let mut command = Command::new(program);
            command.args(args);
            (command, cwd, timeout)
        }
        ProcessRequest::Shell {
            command,
            cwd,
            timeout,
        } => {
            let mut process = Command::new("/bin/sh");
            process.arg("-lc").arg(command);
            (process, cwd, timeout)
        }
    };
    if !(1..=3600).contains(&timeout) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "timeout must be 1 through 3600",
        ));
    }
    let cwd = resolve_cwd(&std::env::current_dir()?, &cwd).await?;
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("stdout unavailable"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("stderr unavailable"))?;
    let stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let timed = tokio::time::timeout(Duration::from_secs(timeout), child.wait()).await;
    let (status, timed_out) = if let Ok(status) = timed {
        (status?, false)
    } else {
        child.kill().await?;
        (child.wait().await?, true)
    };
    let stdout = stdout_task.await.map_err(std::io::Error::other)??;
    let stderr = stderr_task.await.map_err(std::io::Error::other)??;
    let (stdout, stdout_truncated) = output_text(stdout);
    let (stderr, stderr_truncated) = output_text(stderr);
    Ok(ProcessOutput {
        exit_code: status.code(),
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        timed_out,
    })
}

async fn resolve_cwd(root: &Path, relative: &str) -> Result<PathBuf, std::io::Error> {
    let path = Path::new(relative);
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cwd must be workspace-relative and cannot contain `..`",
        ));
    }
    let root = tokio::fs::canonicalize(root).await?;
    let path = tokio::fs::canonicalize(root.join(path)).await?;
    if !path.starts_with(&root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cwd escapes workspace",
        ));
    }
    Ok(path)
}

fn output_text(mut bytes: Vec<u8>) -> (String, bool) {
    let truncated = bytes.len() > MAX_OUTPUT;
    bytes.truncate(MAX_OUTPUT);
    (String::from_utf8_lossy(&bytes).into_owned(), truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::{AssistantContent, ResponseChunk, ToolCall, ToolResult};

    #[test]
    fn remote_agent_state_machine_pauses_for_host_services() {
        let mut agents = HashMap::new();
        let RemoteAgentStep::Started {
            request: initial_request,
            ..
        } = start_agent(
            &mut agents,
            "agent".to_owned(),
            RemoteAgentSpec {
                model: "test".to_owned(),
                provider: "host".to_owned(),
                system: Vec::new(),
                tools: Vec::new(),
                reasoning: None,
                max_output_tokens: None,
                history: Vec::new(),
            },
        )
        .unwrap()
        else {
            panic!("remote agent must expose its clock before the first provider call");
        };
        assert_eq!(initial_request.messages, Vec::<Message>::new());
        let initial_message = Message::User(vec![
            crate::provider::protocol::UserContent::Text {
                text: "work".to_owned(),
            },
            crate::provider::protocol::UserContent::Runtime {
                text: "<skyhook_state>{}</skyhook_state>".to_owned(),
            },
        ]);
        let call = ToolCall {
            id: "call".to_owned(),
            name: "read".to_owned(),
            arguments: serde_json::json!({"path":"README.md"}),
        };
        let step = continue_provider(
            &mut agents,
            "agent",
            Some(initial_message.clone()),
            vec![ResponseChunk::Block {
                block: AssistantContent::ToolCall(call.clone()),
            }],
        )
        .unwrap();
        assert!(matches!(step, RemoteAgentStep::Tools { .. }));
        let RemoteAgentStep::Provider { request } = continue_tools(
            &mut agents,
            "agent",
            vec![ToolResult {
                call_id: call.id,
                name: call.name,
                result: serde_json::json!({"ok":true}),
                images: Vec::new(),
                is_error: false,
            }],
        )
        .unwrap() else {
            panic!("tool results must resume the provider");
        };
        assert_eq!(request.messages.first(), Some(&initial_message));
        assert_eq!(request.messages.len(), 3);
        assert_eq!(
            request
                .messages
                .iter()
                .filter_map(|message| match message {
                    Message::User(content) => Some(content),
                    Message::Assistant(_) | Message::Tool(_) => None,
                })
                .flatten()
                .filter(|content| matches!(
                    content,
                    crate::provider::protocol::UserContent::Runtime { text }
                        if text.contains("<skyhook_state>")
                ))
                .count(),
            1
        );
        let step = continue_provider(
            &mut agents,
            "agent",
            None,
            vec![ResponseChunk::TextDelta {
                text: "done".to_owned(),
            }],
        )
        .unwrap();
        assert!(matches!(step, RemoteAgentStep::Complete { text, .. } if text == "done"));
        assert!(!agents.contains_key("agent"));
    }
}
