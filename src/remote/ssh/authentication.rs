//! Session-owned OpenSSH agent and per-process authentication environment.
use super::{AUTH_SOCK, askpass::AskpassServer};
use crate::remote::{
    backend::{ProcessAuthentication, ProcessEnvironment},
    error::{RemoteError, SshError},
    prompt::SensitivePromptHandler,
};
use crate::{job::JobManager, target::Route};
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};

/// How long the managed ssh-agent may take to accept connections.
const AGENT_START_TIMEOUT: Duration = Duration::from_secs(5);
/// ssh-agent announces nothing when its socket is ready, so startup polls it.
const AGENT_READY_POLL: Duration = Duration::from_millis(10);

pub(crate) struct Authentication {
    prompts: Arc<dyn SensitivePromptHandler>,
    jobs: Option<JobManager>,
    askpass: Mutex<Askpass>,
    agent: Mutex<Option<Agent>>,
}

/// The session askpass server, started on first use; once shut down it is never restarted.
enum Askpass {
    Idle,
    Running(AskpassServer),
    ShutDown,
}

struct Agent {
    child: Child,
    _directory: tempfile::TempDir,
    environment: ProcessEnvironment,
}
impl Authentication {
    pub fn new(prompts: Arc<dyn SensitivePromptHandler>, jobs: Option<JobManager>) -> Self {
        Self::with_askpass(prompts, jobs, Askpass::Idle)
    }

    fn with_askpass(
        prompts: Arc<dyn SensitivePromptHandler>,
        jobs: Option<JobManager>,
        askpass: Askpass,
    ) -> Self {
        Self {
            prompts,
            jobs,
            askpass: Mutex::new(askpass),
            agent: Mutex::new(None),
        }
    }

    /// The session askpass helper's environment, shared by every process and the agent.
    async fn prompt_environment(&self) -> Result<ProcessEnvironment, RemoteError> {
        let mut askpass = self.askpass.lock().await;
        if matches!(*askpass, Askpass::Idle) {
            let server = AskpassServer::start(self.prompts.clone(), None, self.jobs.clone())?;
            *askpass = Askpass::Running(server);
        }
        match &*askpass {
            Askpass::Running(server) => Ok(server.environment()),
            Askpass::Idle | Askpass::ShutDown => Err(RemoteError::Cancelled),
        }
    }

    pub async fn process_environment(
        &self,
        authentication: ProcessAuthentication,
    ) -> Result<ProcessEnvironment, RemoteError> {
        match authentication {
            ProcessAuthentication::Prompts => self.prompt_environment().await,
            ProcessAuthentication::PromptsAndAgent => self.environment().await,
        }
    }

    async fn environment(&self) -> Result<ProcessEnvironment, RemoteError> {
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
            let mut environment = self.prompt_environment().await?;
            environment.insert(
                AUTH_SOCK.into(),
                super::config::wire_path(std::path::Path::new(&socket))?.to_owned(),
            );
            let mut command = Command::new("ssh-agent");
            command
                .arg("-D")
                .arg("-a")
                .arg(&socket)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            super::apply_environment(command.as_std_mut(), &environment);
            let mut child = command.spawn().map_err(RemoteError::start)?;
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
                    tokio::time::sleep(AGENT_READY_POLL).await;
                }
            };
            tokio::time::timeout(AGENT_START_TIMEOUT, ready)
                .await
                .map_err(|_| SshError::AgentTimeout)??;
            *agent = Some(Agent {
                child,
                _directory: directory,
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
        route: &Route,
    ) -> Result<ProcessEnvironment, RemoteError> {
        if route.iter().all(|hop| hop.ssh.external_agent) {
            return Ok(ProcessEnvironment::new());
        }
        self.environment().await
    }

    /// The askpass server stops first, so an agent startup that already holds the
    /// agent lock either fails to get its environment or is stopped here after it.
    pub async fn shutdown(&self) {
        *self.askpass.lock().await = Askpass::ShutDown;
        if let Some(mut agent) = self.agent.lock().await.take() {
            let _ = agent.child.kill().await;
            let _ = agent.child.wait().await;
        }
    }
}

/// Worker-side OpenSSH authentication: the environment for worker processes,
/// with the forwarded agent and askpass server, and a private agent for SSH
/// processes this worker starts. Both share one askpass server, which the
/// private agent's state owns.
pub(crate) fn worker_authentication(
    prompts: Arc<dyn SensitivePromptHandler>,
) -> Result<(ProcessEnvironment, Arc<Authentication>), std::io::Error> {
    let askpass = AskpassServer::start(prompts.clone(), None, None)?;
    let mut environment = askpass.environment();
    // OpenSSH already owns a private forwarded socket for this connection.
    // Pass it through; a second forwarding listener adds no isolation or lifetime.
    // A socket path SSH configuration cannot represent only disables agent
    // authentication; password and key authentication still work.
    if let Some(socket) = std::env::var_os(AUTH_SOCK).and_then(|socket| socket.into_string().ok()) {
        environment.insert(AUTH_SOCK.into(), socket);
    }
    let agent = Authentication::with_askpass(prompts, None, Askpass::Running(askpass));
    Ok((environment, Arc::new(agent)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn external_agent_routes_and_prompt_only_processes_never_start_the_private_agent() {
        let authentication =
            Authentication::new(Arc::new(crate::remote::RejectSensitivePrompts), None);
        let mut destination = crate::target::TargetDefinition::test("external", ".", None);
        destination.ssh.external_agent = true;
        let route = Route {
            hops: Vec::new(),
            destination,
        };
        let environment = authentication.route_environment(&route).await.unwrap();
        assert!(environment.is_empty());
        assert!(authentication.agent.lock().await.is_none());
        // Prompt-only processes keep their inherited agent and share the session helper.
        let prompts = (authentication.process_environment(ProcessAuthentication::Prompts))
            .await
            .unwrap();
        assert!(prompts.contains_key("SUDO_ASKPASS") && !prompts.contains_key(AUTH_SOCK));
        assert!(authentication.agent.lock().await.is_none());
        let again = (authentication.process_environment(ProcessAuthentication::Prompts))
            .await
            .unwrap();
        assert_eq!(prompts, again);
    }

    #[tokio::test]
    async fn shut_down_authentication_never_restarts_its_askpass_server() {
        let authentication =
            Authentication::new(Arc::new(crate::remote::RejectSensitivePrompts), None);
        let running = (authentication.process_environment(ProcessAuthentication::Prompts))
            .await
            .unwrap();
        let socket = std::path::PathBuf::from(&running["SKYHOOK_ASKPASS_SOCKET"]);
        authentication.shutdown().await;
        assert!(!socket.exists());
        for request in [
            ProcessAuthentication::Prompts,
            ProcessAuthentication::PromptsAndAgent,
        ] {
            let refused = authentication.process_environment(request).await;
            assert!(matches!(refused, Err(RemoteError::Cancelled)));
        }
        assert!(authentication.agent.lock().await.is_none());
        assert!(matches!(
            *authentication.askpass.lock().await,
            Askpass::ShutDown
        ));
    }

    #[tokio::test]
    async fn worker_processes_and_the_private_agent_share_one_broker() {
        let (forwarded, agent) =
            worker_authentication(Arc::new(crate::remote::RejectSensitivePrompts)).unwrap();
        let private = agent
            .process_environment(ProcessAuthentication::Prompts)
            .await
            .unwrap();
        for key in ["SSH_ASKPASS", "SUDO_ASKPASS", "SKYHOOK_ASKPASS_SOCKET"] {
            assert_eq!(forwarded[key], private[key], "{key}");
        }
        assert!(std::path::Path::new(&private["SKYHOOK_ASKPASS_SOCKET"]).exists());
        // Worker processes keep OpenSSH's forwarded agent; the private agent's
        // socket only ever comes from its own startup.
        assert_eq!(
            forwarded.get(AUTH_SOCK),
            std::env::var(AUTH_SOCK).ok().as_ref()
        );
        assert!(!private.contains_key(AUTH_SOCK));
    }
}
