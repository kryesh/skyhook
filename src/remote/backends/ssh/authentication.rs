//! Session-owned OpenSSH agent and per-process authentication environment.
use super::askpass::AskpassServer;
use crate::remote::backend::ProcessEnvironment;
use crate::remote::{RemoteError, SensitivePromptHandler};
use crate::target::TargetDefinition;
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
                super::config::wire_path(std::path::Path::new(&socket))?.to_owned(),
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
    /// Environment for an SSH process started on this machine. The private agent
    /// starts only when some hop uses it; external_agent hops use this process's
    /// own SSH_AUTH_SOCK.
    pub async fn route_environment(
        &self,
        route: &[TargetDefinition],
    ) -> Result<ProcessEnvironment, RemoteError> {
        if route.iter().all(|hop| hop.ssh.external_agent) {
            return Ok(ProcessEnvironment::new());
        }
        self.environment().await
    }

    pub async fn shutdown(&self) {
        if let Some(mut agent) = self.agent.lock().await.take() {
            let _ = agent.child.kill().await;
            let _ = agent.child.wait().await;
        }
    }
}

/// Worker-side OpenSSH authentication: the forwarded agent and askpass server for
/// worker processes, and a private agent for SSH processes this worker starts.
pub(crate) struct WorkerAuthentication {
    _askpass: AskpassServer,
    environment: ProcessEnvironment,
    agent: Arc<Authentication>,
}

impl WorkerAuthentication {
    pub(crate) fn new(prompts: Arc<dyn SensitivePromptHandler>) -> Result<Self, std::io::Error> {
        let agent = Arc::new(Authentication::new(prompts.clone()));
        let askpass = AskpassServer::start(prompts)?;
        let mut environment = askpass.environment();
        // OpenSSH already owns a private forwarded socket for this connection.
        // Pass it through; a second forwarding listener adds no isolation or lifetime.
        // A socket path SSH configuration cannot represent only disables agent
        // authentication; password and key authentication still work.
        if let Some(socket) =
            std::env::var_os("SSH_AUTH_SOCK").and_then(|socket| socket.into_string().ok())
        {
            environment.insert("SSH_AUTH_SOCK".into(), socket);
        }
        Ok(Self {
            _askpass: askpass,
            environment,
            agent,
        })
    }

    pub(crate) fn environment(&self) -> &ProcessEnvironment {
        &self.environment
    }

    pub(crate) fn agent(&self) -> Arc<Authentication> {
        self.agent.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn routes_using_only_external_agents_never_start_the_private_agent() {
        let authentication = Authentication::new(Arc::new(crate::remote::RejectSensitivePrompts));
        let mut hop = TargetDefinition::test("external", ".", None);
        hop.ssh.external_agent = true;
        let environment = authentication.route_environment(&[hop]).await.unwrap();
        assert!(environment.is_empty());
        assert!(authentication.agent.lock().await.is_none());
    }
}
