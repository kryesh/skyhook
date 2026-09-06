//! Session-owned OpenSSH agent and per-process authentication environment.
use super::{RemoteError, SensitivePromptHandler, askpass::AskpassServer};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, sync::Arc};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};

pub(crate) type ProcessEnvironment = BTreeMap<String, String>;

pub(crate) struct Authentication {
    prompts: Arc<dyn SensitivePromptHandler>,
    agent: Mutex<Option<Agent>>,
}

struct Agent {
    child: Child,
    _directory: tempfile::TempDir,
    _askpass: AskpassServer,
    environment: ProcessEnvironment,
}
impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl Authentication {
    pub fn new(prompts: Arc<dyn SensitivePromptHandler>) -> Self {
        Self {
            prompts,
            agent: Mutex::new(None),
        }
    }
    pub async fn environment(&self) -> Result<ProcessEnvironment, RemoteError> {
        let mut agent = self.agent.lock().await;
        if agent
            .as_mut()
            .is_some_and(|a| a.child.try_wait().ok().flatten().is_some())
        {
            *agent = None;
        }
        if agent.is_none() {
            let directory = tempfile::Builder::new()
                .prefix("skyhook-agent-")
                .tempdir()?;
            let socket = directory.path().join("agent.sock");
            let askpass = AskpassServer::start(self.prompts.clone())?;
            let mut environment = askpass.environment();
            environment.insert(
                "SSH_AUTH_SOCK".into(),
                socket.to_string_lossy().into_owned(),
            );
            let mut child = Command::new("ssh-agent")
                .arg("-D")
                .arg("-a")
                .arg(&socket)
                .envs(&environment)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(RemoteError::start)?;
            let ready = async {
                loop {
                    if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                        return Ok(());
                    }
                    if child.try_wait()?.is_some() {
                        return Err(std::io::Error::other(
                            "managed ssh-agent exited during startup",
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), ready)
                .await
                .map_err(|_| {
                    RemoteError::ConnectionTask("managed ssh-agent startup timed out".into())
                })??;
            *agent = Some(Agent {
                child,
                _directory: directory,
                _askpass: askpass,
                environment,
            });
        }
        Ok(agent
            .as_ref()
            .expect("agent initialized")
            .environment
            .clone())
    }
    pub async fn shutdown(&self) {
        if let Some(mut agent) = self.agent.lock().await.take() {
            let _ = agent.child.kill().await;
            let _ = agent.child.wait().await;
        }
    }
}

/// A socket owned by this worker, backed by the SSH-forwarded root agent.
pub(crate) struct AgentRelay {
    pub socket: PathBuf,
    _directory: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
}
impl AgentRelay {
    pub fn start(upstream: PathBuf) -> Result<Self, std::io::Error> {
        let directory = tempfile::Builder::new()
            .prefix("skyhook-agent-relay-")
            .tempdir()?;
        let socket = directory.path().join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let task = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((mut client, _)) = accepted else { break };
                        let upstream = upstream.clone();
                        tasks.spawn(async move {
                            if let Ok(mut server) = tokio::net::UnixStream::connect(upstream).await {
                                let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                            }
                        });
                    }
                    _ = tasks.join_next(), if !tasks.is_empty() => {}
                }
            }
        });
        Ok(Self {
            socket,
            _directory: directory,
            task,
        })
    }
}
impl Drop for AgentRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}
