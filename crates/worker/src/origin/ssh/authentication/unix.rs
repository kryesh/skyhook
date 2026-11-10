//! A unix origin's agents: the one it inherited, else a private agent it starts.
use super::{AUTH_SOCK, inherited};
use crate::origin::ssh::route::{HopAuth, SshRoute};
use crate::origin::{
    ProcessEnvironment,
    error::{SshError, TransportError},
};
use std::{process::Stdio, time::Duration};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};

/// The agent this process inherited in SSH_AUTH_SOCK: on a remote origin, the one
/// forwarded to its shim.
pub(in crate::origin::ssh) fn user_agent() -> Option<String> {
    inherited()
}

/// How long the managed ssh-agent may take to accept connections.
const AGENT_START_TIMEOUT: Duration = Duration::from_secs(5);
/// ssh-agent announces nothing when its socket is ready, so startup polls it.
const AGENT_READY_POLL: Duration = Duration::from_millis(10);

#[derive(Default)]
pub struct Authentication {
    agent: Mutex<AgentState>,
}

/// The agent starts on first use and restarts if it exits; once shut down it never restarts.
#[derive(Default)]
enum AgentState {
    #[default]
    Idle,
    Running(Box<Agent>),
    ShutDown,
}

impl AgentState {
    /// The running agent's environment.
    fn running(&mut self) -> Option<&ProcessEnvironment> {
        let Self::Running(agent) = self else {
            return None;
        };
        let exited = agent.child.try_wait().ok().flatten().is_some();
        (!exited).then_some(&agent.environment)
    }
}

struct Agent {
    child: Child,
    _directory: tempfile::TempDir,
    environment: ProcessEnvironment,
}

impl Agent {
    async fn start() -> Result<Self, TransportError> {
        let (directory, socket) = socket_directory("skyhook-agent-", "agent.sock")?;
        let environment = ProcessEnvironment::from([(
            AUTH_SOCK.into(),
            // The forwarded environment is a UTF-8 protocol.
            (socket.to_str().ok_or(SshError::UnrepresentablePath)?).to_owned(),
        )]);
        let mut child = Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(TransportError::start)?;
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
        Ok(Self {
            child,
            _directory: directory,
            environment,
        })
    }
}

impl Authentication {
    /// The agent environment local processes receive: none when this process
    /// inherited an agent, which they inherit too, otherwise the private agent's.
    pub async fn agent_environment(&self) -> Result<ProcessEnvironment, TransportError> {
        if user_agent().is_some() {
            return Ok(ProcessEnvironment::new());
        }
        self.private_environment().await
    }

    /// The environment naming the private agent, started if need be.
    async fn private_environment(&self) -> Result<ProcessEnvironment, TransportError> {
        let mut state = self.agent.lock().await;
        if matches!(*state, AgentState::ShutDown) {
            return Err(TransportError::Cancelled);
        }
        if let Some(environment) = state.running() {
            return Ok(environment.clone());
        }
        let agent = Agent::start().await?;
        let environment = agent.environment.clone();
        *state = AgentState::Running(Box::new(agent));
        Ok(environment)
    }

    /// Environment for an SSH process started on this machine. Without an inherited
    /// agent, the private agent starts once a hop loads a key into it or forwards it;
    /// agent keys alone use it only once it runs.
    pub async fn route_environment(
        &self,
        route: &SshRoute,
    ) -> Result<ProcessEnvironment, TransportError> {
        if user_agent().is_some() {
            return Ok(ProcessEnvironment::new());
        }
        let needed = (route.hops()).any(|(hop, auth)| {
            hop.forward_agent || matches!(auth, HopAuth::Key(_) | HopAuth::EncryptedKey(_))
        });
        if needed {
            return self.private_environment().await;
        }
        Ok((self.agent.lock().await.running().cloned()).unwrap_or_default())
    }

    pub async fn shutdown(&self) {
        let state = std::mem::replace(&mut *self.agent.lock().await, AgentState::ShutDown);
        if let AgentState::Running(mut agent) = state {
            let _ = agent.child.kill().await;
            let _ = agent.child.wait().await;
        }
    }
}

/// The longest path a socket binds to: `sun_path` holds it and a terminating NUL.
const SOCKET_PATH_BYTES: usize =
    size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path) - 1;

/// A new owner-only directory for the socket `socket`, private from creation even under
/// umask 000, with the socket's path in it. That path must fit `sun_path`, which a deep
/// `TMPDIR` overruns, so the per-user runtime directory, which exists for sockets, comes
/// first, then the temporary directory, then `/tmp`, where any socket name fits.
fn socket_directory(
    prefix: &str,
    socket: &str,
) -> std::io::Result<(tempfile::TempDir, std::path::PathBuf)> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut builder = tempfile::Builder::new();
    builder
        .prefix(prefix)
        .permissions(std::fs::Permissions::from_mode(0o700));
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|directory| directory.is_absolute());
    // Restore owner bits if an unusually restrictive umask removed them, before a candidate
    // that does not fit is dropped: removing it reads it.
    let create = |parent: std::path::PathBuf| {
        let directory = builder.tempdir_in(parent)?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        std::io::Result::Ok(directory)
    };
    let fits = |directory: &tempfile::TempDir| {
        directory.path().join(socket).as_os_str().len() <= SOCKET_PATH_BYTES
    };
    let directory = (runtime.into_iter().chain([std::env::temp_dir()]))
        .filter_map(|parent| create(parent).ok())
        .find(fits);
    let directory = match directory {
        Some(directory) => directory,
        None => create("/tmp".into())?,
    };
    let socket = directory.path().join(socket);
    Ok((directory, socket))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[tokio::test]
    async fn shut_down_authentication_refuses_its_agent() {
        let authentication = Authentication::default();
        authentication.shutdown().await;
        let refused = authentication.private_environment().await;
        assert_matches!(refused, Err(TransportError::Cancelled));
    }

    #[test]
    fn socket_directories_ignore_umask_and_a_deep_tmpdir() {
        const CHILD: &str = "SKYHOOK_SOCKET_DIRECTORY_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Only the isolated child changes umask; other test threads are unaffected.
            let name = format!(
                "{}::socket_directories_ignore_umask_and_a_deep_tmpdir",
                module_path!().split_once("::").unwrap().1
            );
            // Sockets cannot fit under this TMPDIR, so they use the runtime directory, or
            // `/tmp` without one.
            let root = tempfile::tempdir().unwrap();
            let deep = root.path().join("deep/".repeat(20));
            std::fs::create_dir_all(&deep).unwrap();
            let runtime = std::env::var_os("XDG_RUNTIME_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            // Both umask extremes, and both directory choices.
            for (mask, runtime) in [("000", Some(&runtime)), ("777", None)] {
                let mut command = std::process::Command::new("/bin/sh");
                command
                    .arg("-c")
                    .arg(format!("umask {mask}; exec \"$@\""))
                    .arg("socket-directory-test")
                    .arg(std::env::current_exe().unwrap())
                    .args(["--exact", &name, "--nocapture"])
                    .env(CHILD, "1")
                    .env("TMPDIR", &deep);
                match runtime {
                    Some(runtime) => command.env("XDG_RUNTIME_DIR", runtime),
                    None => command.env_remove("XDG_RUNTIME_DIR"),
                };
                let output = command.output().unwrap();
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(output.status.success(), "umask {mask}: {stdout}\n{stderr}");
                assert!(stdout.contains("1 passed"));
            }
            // No directory the socket did not fit is left behind, whatever the umask.
            assert_eq!(std::fs::read_dir(&deep).unwrap().count(), 0);
            return;
        }

        use std::os::unix::fs::PermissionsExt as _;
        let (directory, socket) = socket_directory("skyhook-test-", "test.sock").unwrap();
        if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
            assert!(socket.starts_with("/tmp"), "{}", socket.display());
        }
        let mode = std::fs::metadata(directory.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o700);
        std::os::unix::net::UnixListener::bind(&socket).unwrap();
    }
}
