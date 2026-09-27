//! OpenSSH process lifetime, transport pipes, and shim bootstrap.
use super::{
    AUTH_SOCK,
    config::{SshConfig, shell_quote, ssh_command, wire_path},
};
use crate::{
    remote::{
        artifact::{EmbeddedShimCatalog, Platform, ShimProtocol},
        backend::ProcessEnvironment,
        client::Session,
        error::{DeploymentError, RemoteError, SshError},
        prompt::SensitivePromptHandler,
        transport::{ReportingReader, Transport},
    },
    target::Route,
};
use std::{os::fd::AsFd as _, path::Path, process::Stdio, sync::Arc};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    process::ChildStderr,
};

/// Where shims are installed, relative to the remote user's home directory.
const SHIM_DIR: &str = ".cache/skyhook/shims";
/// The most of SSH's own error output kept to explain its failure.
const DIAGNOSTIC_BYTES: usize = 8192;
/// The most output a bootstrap command may return.
const PROBE_BYTES: u64 = 1024 * 1024;

pub(crate) async fn open(
    route: &Route,
    remote_command: &str,
    environment: &ProcessEnvironment,
    prompts: Arc<dyn SensitivePromptHandler>,
) -> Result<Transport, RemoteError> {
    let external_agent = std::env::var(AUTH_SOCK).ok();
    let config = SshConfig::create(route, environment, external_agent.as_deref(), prompts)?;
    let mut command = ssh_command(&config);
    command
        .arg(remote_command)
        .envs(environment)
        .envs(config.askpass.environment())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.process_group(0);
    let mut child = command.spawn().map_err(RemoteError::start)?;
    let mut group = crate::process_group::ProcessGroup::led_by(&child);
    let input = child.stdin.take().expect("stdin was requested as a pipe");
    let output = child.stdout.take().expect("stdout was requested as a pipe");
    let mut stderr = child.stderr.take().expect("stderr was requested as a pipe");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cancelled = cancellation.clone();
    let (sender, completion) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut diagnostics = Diagnostics::default();
        let exited = async {
            // Only cancellation stops the group; an SSH that exits by itself
            // leaves its group alone.
            tokio::select! {
                status = child.wait() => {
                    group.release();
                    status
                }
                () = cancelled.cancelled() => {
                    group.kill();
                    let _ = child.kill().await;
                    child.wait().await
                }
            }
        };
        tokio::pin!(exited);
        let status = tokio::select! {
            status = &mut exited => status,
            () = diagnostics.drain(&mut stderr) => exited.await,
        };
        // A descendant such as a ProxyCommand can inherit stderr and outlive
        // SSH, so its end of file cannot mark the end of SSH's own output.
        diagnostics.read_buffered(&stderr);
        let result = match status {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(SshError::Exited {
                status,
                stderr: diagnostics.into_text(),
            }
            .into()),
            Err(error) => Err(RemoteError::io(error)),
        };
        let _ = sender.send(result);
        // Keep draining for such descendants until the transport is dropped.
        let mut sink = tokio::io::sink();
        tokio::select! {
            _ = tokio::io::copy(&mut stderr, &mut sink) => {}
            () = cancelled.cancelled() => {}
        }
    });
    Ok(Transport {
        input: Box::new(input),
        output: Box::new(ReportingReader::new(output, completion)),
        owner: Box::new(ProcessOwner {
            cancellation,
            _config: config,
        }),
    })
}

/// The start of SSH's error output, kept to explain its failure.
#[derive(Default)]
struct Diagnostics(Vec<u8>);

impl Diagnostics {
    fn keep(&mut self, bytes: &[u8]) {
        let room = DIAGNOSTIC_BYTES - self.0.len();
        self.0.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    /// Read to end of file, discarding what is not kept so SSH never blocks on
    /// a full pipe. Each read is kept before the next await, so dropping the
    /// future loses nothing already read.
    async fn drain(&mut self, stderr: &mut ChildStderr) {
        let mut buffer = [0; 1024];
        while let Ok(read @ 1..) = stderr.read(&mut buffer).await {
            self.keep(&buffer[..read]);
        }
    }

    /// Everything a process that has exited wrote is already in the pipe; read
    /// what is there without waiting for other writers.
    fn read_buffered(&mut self, stderr: &ChildStderr) {
        // A duplicate shares the pipe's non-blocking mode, which the runtime sets.
        let Ok(pipe) = stderr.as_fd().try_clone_to_owned() else {
            return;
        };
        let mut pipe = std::fs::File::from(pipe);
        let mut buffer = [0; 1024];
        while self.0.len() < DIAGNOSTIC_BYTES {
            match std::io::Read::read(&mut pipe, &mut buffer) {
                Ok(0) => return,
                Ok(read) => self.keep(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                // WouldBlock: the pipe holds nothing more.
                Err(_) => return,
            }
        }
    }

    fn into_text(self) -> String {
        String::from_utf8_lossy(&self.0).trim().to_owned()
    }
}

pub(crate) struct SshLauncher {
    pub origin: Option<Session>,
    pub route: Route,
    pub environment: ProcessEnvironment,
    pub prompts: Arc<dyn SensitivePromptHandler>,
}
impl SshLauncher {
    pub(crate) async fn connect(
        &self,
        workspace: &Path,
        catalog: &EmbeddedShimCatalog,
    ) -> Result<Transport, RemoteError> {
        let root = shell_quote(wire_path(&self.route.destination.workspace)?);
        let workspace = shell_quote(wire_path(workspace)?);
        let probe = self.output("uname -s; uname -m", &[]).await?;
        let mut lines = probe.lines();
        let (Some(os), Some(arch)) = (lines.next(), lines.next()) else {
            return Err(DeploymentError::Probe.into());
        };
        let (os, arch) = (os.trim(), arch.trim());
        let platform = Platform::parse(&os.to_ascii_lowercase(), &arch.to_ascii_lowercase())
            .ok_or_else(|| DeploymentError::UnsupportedPlatform {
                os: os.to_owned(),
                arch: arch.to_owned(),
            })?;
        let shim = catalog.find(ShimProtocol::Ssh, platform)?;
        let hash = shim.sha256();
        let path = format!("{SHIM_DIR}/{hash}/{}", shim.installed_name());
        let check = format!(
            "test -x \"$HOME/{path}\" && \"$HOME/{path}\" --self-check {hash} && echo skyhook-valid"
        );
        if !self
            .output(&check, &[])
            .await
            .is_ok_and(|s| s.trim() == "skyhook-valid")
        {
            let mut random = [0; 8];
            getrandom::fill(&mut random).map_err(DeploymentError::Random)?;
            let temporary = format!(
                "{SHIM_DIR}/{hash}/.upload-{:016x}",
                u64::from_ne_bytes(random)
            );
            let command = format!(
                "umask 077; d=\"$HOME/{SHIM_DIR}/{hash}\"; t=\"$HOME/{temporary}\"; mkdir -p \"$d\" && trap 'rm -f \"$t\"' EXIT HUP INT TERM && cat > \"$t\" && chmod 700 \"$t\" && \"$t\" --self-check {hash} && mv -f \"$t\" \"$HOME/{path}\" && echo skyhook-installed"
            );
            if self.output(&command, &shim.bytes).await?.trim() != "skyhook-installed" {
                return Err(DeploymentError::Install.into());
            }
        }
        let command = format!(
            "r=$(cd -- {root} && pwd -P) && cd -- {workspace} && exec \"$HOME/{path}\" --serve \"$r\""
        );
        self.open(&command).await
    }

    async fn open(&self, command: &str) -> Result<Transport, RemoteError> {
        if let Some(origin) = &self.origin {
            origin
                .open_ssh(self.route.clone(), command.to_owned())
                .await
        } else {
            open(
                &self.route,
                command,
                &self.environment,
                self.prompts.clone(),
            )
            .await
        }
    }
    async fn output(&self, command: &str, bytes: &[u8]) -> Result<String, RemoteError> {
        let Transport {
            mut input,
            mut output,
            owner: _owner,
        } = self.open(command).await?;
        let write = async move {
            input.write_all(bytes).await?;
            input.shutdown().await?;
            drop(input);
            Ok::<(), std::io::Error>(())
        };
        let read = async {
            let mut bytes = Vec::new();
            (&mut output)
                .take(PROBE_BYTES)
                .read_to_end(&mut bytes)
                .await?;
            Ok::<_, std::io::Error>(String::from_utf8_lossy(&bytes).into_owned())
        };
        let (_, output) = tokio::try_join!(write, read)?;
        Ok(output)
    }
}

pub(crate) struct ProcessOwner {
    pub cancellation: tokio_util::sync::CancellationToken,
    pub _config: SshConfig,
}
impl Drop for ProcessOwner {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{remote::RejectSensitivePrompts, target::TargetDefinition, tests::bounded};
    use std::path::PathBuf;

    fn launcher(root: PathBuf, path: &Path) -> SshLauncher {
        SshLauncher {
            origin: None,
            route: Route {
                hops: Vec::new(),
                destination: TargetDefinition::test("remote", root, None),
            },
            environment: ProcessEnvironment::from([("PATH".into(), path.display().to_string())]),
            prompts: Arc::new(RejectSensitivePrompts),
        }
    }

    /// A wrapper or ProxyCommand that SSH starts can inherit its stderr and
    /// outlive it; here it lives until the transport's input closes.
    #[tokio::test]
    async fn ssh_completes_while_a_descendant_holds_its_stderr() {
        use std::os::unix::fs::PermissionsExt as _;
        for (status, expected) in [(255, Some("connection refused")), (0, None)] {
            let bin = tempfile::tempdir().unwrap();
            let ssh = bin.path().join("ssh");
            let script = format!(
                "#!/bin/sh\nexec 3<&0\ncat <&3 >/dev/null &\necho 'connection refused' >&2\nexit {status}\n"
            );
            std::fs::write(&ssh, script).unwrap();
            std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
            let search = format!(
                "{}:{}",
                bin.path().display(),
                std::env::var("PATH").unwrap()
            );
            let launcher = launcher("/work".into(), Path::new(&search));
            let mut transport = launcher.open("ignored").await.unwrap();
            let mut output = Vec::new();
            let result = bounded(transport.output.read_to_end(&mut output)).await;
            match (result.map_err(RemoteError::from), expected) {
                (Ok(_), None) => {}
                (Err(RemoteError::Ssh(SshError::Exited { stderr, .. })), Some(expected)) => {
                    assert_eq!(stderr, expected);
                }
                (result, _) => panic!("exit {status}: {result:?}"),
            }
        }
    }

    /// A descendant that keeps SSH's stderr open without needing its input keeps
    /// the drain alive after SSH exits, until the owner alone is dropped: the
    /// drain's end closes the pipe's only reader.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropping_the_owner_stops_draining_for_a_lingering_descendant() {
        use std::os::unix::fs::PermissionsExt as _;
        use tokio::io::{Interest, unix::AsyncFd};
        struct Descendant(libc::pid_t);
        impl Drop for Descendant {
            fn drop(&mut self) {
                // SAFETY: kill takes integer IDs and accesses no memory; the ID is
                // the fixture's own session leader.
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                }
            }
        }
        let bin = tempfile::tempdir().unwrap();
        let ssh = bin.path().join("ssh");
        let script = "#!/bin/sh\nsetsid sh -c 'echo $$; exec sleep 1000 >/dev/null' </dev/null &\n";
        std::fs::write(&ssh, script).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let search = format!(
            "{}:{}",
            bin.path().display(),
            std::env::var("PATH").unwrap()
        );
        let launcher = launcher("/work".into(), Path::new(&search));
        let Transport {
            input,
            mut output,
            owner,
        } = launcher.open("ignored").await.unwrap();
        let mut pid = String::new();
        bounded(output.read_to_string(&mut pid)).await.unwrap();
        let descendant = Descendant(pid.trim().parse().unwrap());
        let stderr = std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/proc/{}/fd/2", descendant.0))
            .unwrap();
        // A pipe's writer reports an error once no reader is left.
        let stderr = AsyncFd::with_interest(stderr, Interest::ERROR).unwrap();
        drop(owner);
        let closed = bounded(stderr.ready(Interest::ERROR)).await.unwrap();
        assert!(closed.ready().is_error());
        drop((input, output, descendant));
    }

    #[tokio::test]
    async fn connect_rejects_workspaces_ssh_cannot_represent() {
        use std::os::unix::ffi::OsStringExt as _;
        let unrepresentable = PathBuf::from(std::ffi::OsString::from_vec(b"/work-\xff".to_vec()));
        // No SSH on the search path: an accepted path fails to start it instead.
        let empty = tempfile::tempdir().unwrap();
        for (root, workspace) in [
            (unrepresentable.clone(), PathBuf::from("/work")),
            (PathBuf::from("/work"), unrepresentable),
        ] {
            let result = launcher(root, empty.path())
                .connect(&workspace, &EmbeddedShimCatalog::default())
                .await;
            assert!(matches!(
                result,
                Err(RemoteError::Ssh(SshError::UnrepresentablePath))
            ));
        }
    }
}
