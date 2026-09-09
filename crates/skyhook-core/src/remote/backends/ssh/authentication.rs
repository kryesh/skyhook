//! Session-owned OpenSSH agent and per-process authentication environment.
use super::askpass::AskpassServer;
use crate::remote::backend::ProcessEnvironment;
use crate::remote::{RemoteError, SensitivePromptHandler};
use std::{process::Stdio, sync::Arc};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};

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

/// Worker-side OpenSSH authentication state, retaining the askpass server.
pub(crate) struct WorkerAuthentication {
    _askpass: AskpassServer,
    environment: ProcessEnvironment,
}

impl WorkerAuthentication {
    pub(crate) fn new(prompts: Arc<dyn SensitivePromptHandler>) -> Result<Self, std::io::Error> {
        let askpass = AskpassServer::start(prompts)?;
        let mut environment = askpass.environment();
        // OpenSSH already owns a private forwarded socket for this connection.
        // Pass it through; a second forwarding listener adds no isolation or lifetime.
        if let Some(socket) = std::env::var_os("SSH_AUTH_SOCK") {
            environment.insert(
                "SSH_AUTH_SOCK".into(),
                socket.to_string_lossy().into_owned(),
            );
        }
        Ok(Self {
            _askpass: askpass,
            environment,
        })
    }

    pub(crate) fn environment(&self) -> &ProcessEnvironment {
        &self.environment
    }
}
