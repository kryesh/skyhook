//! OpenSSH process lifetime, transport pipes, and shim bootstrap.
use super::config::{SshConfig, shell_quote, ssh_command};
use crate::{
    remote::{
        DeploymentError, Platform, RemoteError, SensitivePromptHandler, ShimProtocol,
        backend::ProcessEnvironment, transport::Transport,
    },
    target::TargetDefinition,
};
use std::{
    future::Future as _,
    path::Path,
    pin::Pin,
    process::Stdio,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, ReadBuf};

pub(crate) async fn open(
    route: &[TargetDefinition],
    remote_command: &str,
    environment: &ProcessEnvironment,
    prompts: Arc<dyn SensitivePromptHandler>,
) -> Result<Transport, RemoteError> {
    let target = route.last().ok_or(RemoteError::EmptyRoute)?;
    let prompts = Arc::new(ContextPrompts {
        inner: prompts,
        target: target.name.to_string(),
    });
    let external_agent = std::env::var("SSH_AUTH_SOCK").ok();
    let config = SshConfig::create(route, environment, external_agent.as_deref(), prompts)?;
    let mut command = ssh_command(&config, &config.destination);
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
    let process_group = child.id().and_then(|id| i32::try_from(id).ok());
    let input = child.stdin.take().expect("stdin was requested as a pipe");
    let output = child.stdout.take().expect("stdout was requested as a pipe");
    let mut stderr = child.stderr.take().expect("stderr was requested as a pipe");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cancelled = cancellation.clone();
    let (sender, completion) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let diagnostics = async {
            let mut diagnostics = Vec::new();
            let mut bytes = [0; 4096];
            while let Ok(count) = stderr.read(&mut bytes).await {
                if count == 0 {
                    break;
                }
                let keep = count.min(8192usize.saturating_sub(diagnostics.len()));
                diagnostics.extend_from_slice(&bytes[..keep]);
            }
            diagnostics
        };
        let process = async {
            tokio::select! {
                status = child.wait() => status,
                () = cancelled.cancelled() => {
                    if let Some(group) = process_group {
                        // SAFETY: this SSH process was launched into its own process group.
                        unsafe { libc::kill(-group, libc::SIGKILL); }
                    }
                    let _ = child.kill().await; child.wait().await
                }
            }
        };
        let (status, diagnostics) = tokio::join!(process, diagnostics);
        let result = match status {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!(
                "SSH exited with {status}: {}",
                String::from_utf8_lossy(&diagnostics).trim()
            )),
            Err(error) => Err(error.to_string()),
        };
        let _ = sender.send(result);
    });
    Ok(Transport {
        input: Box::new(input),
        output: Box::new(ProcessReader {
            output,
            completion,
            ended: false,
        }),
        owner: Box::new(ProcessOwner {
            cancellation,
            _config: config,
        }),
    })
}

pub(crate) struct SshLauncher {
    pub origin: Option<crate::remote::client::Session>,
    pub route: Vec<TargetDefinition>,
    pub environment: ProcessEnvironment,
    pub prompts: Arc<dyn SensitivePromptHandler>,
}
impl SshLauncher {
    pub(crate) async fn connect(
        &self,
        destination: &TargetDefinition,
        workspace: &Path,
        catalog: &crate::remote::EmbeddedShimCatalog,
    ) -> Result<Transport, RemoteError> {
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
        let protocol = ShimProtocol::Ssh;
        let shim = catalog
            .find(protocol, platform)
            .ok_or(if catalog.is_empty() {
                DeploymentError::NoShims
            } else {
                DeploymentError::NoShim { protocol, platform }
            })?;
        let hash = shim.sha256();
        let path = format!(".cache/skyhook/shims/{hash}/{}", shim.installed_name());
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
                ".cache/skyhook/shims/{hash}/.upload-{:016x}",
                u64::from_ne_bytes(random)
            );
            let command = format!(
                "umask 077; d=\"$HOME/.cache/skyhook/shims/{hash}\"; t=\"$HOME/{temporary}\"; mkdir -p \"$d\" && trap 'rm -f \"$t\"' EXIT HUP INT TERM && cat > \"$t\" && chmod 700 \"$t\" && \"$t\" --self-check {hash} && mv -f \"$t\" \"$HOME/{path}\" && echo skyhook-installed"
            );
            if self.output(&command, &shim.bytes).await?.trim() != "skyhook-installed" {
                return Err(DeploymentError::Install.into());
            }
        }
        let command = format!(
            "r=$(cd -- {} && pwd -P) && cd -- {} && exec \"$HOME/{path}\" --serve \"$r\"",
            shell_quote(&destination.workspace.to_string_lossy()),
            shell_quote(&workspace.to_string_lossy())
        );
        self.open(&command).await
    }

    async fn open(&self, command: &str) -> Result<Transport, RemoteError> {
        if let Some(origin) = &self.origin {
            origin
                .clone()
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
                .take(1024 * 1024)
                .read_to_end(&mut bytes)
                .await?;
            Ok::<_, std::io::Error>(String::from_utf8_lossy(&bytes).into_owned())
        };
        let (_, output) = tokio::try_join!(write, read)?;
        Ok(output)
    }
}

struct ContextPrompts {
    inner: Arc<dyn SensitivePromptHandler>,
    target: String,
}
impl SensitivePromptHandler for ContextPrompts {
    fn prompt(
        &self,
        mut prompt: crate::remote::SensitivePrompt,
    ) -> crate::remote::SensitivePromptFuture {
        prompt.message = format!("[target={}] {}", self.target, prompt.message);
        self.inner.prompt(prompt)
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

pub(crate) struct ProcessReader {
    pub output: tokio::process::ChildStdout,
    pub completion: tokio::sync::oneshot::Receiver<Result<(), String>>,
    pub ended: bool,
}
impl AsyncRead for ProcessReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.ended || buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let before = buffer.filled().len();
        match Pin::new(&mut self.output).poll_read(cx, buffer) {
            Poll::Ready(Ok(())) if before == buffer.filled().len() => {
                match Pin::new(&mut self.completion).poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(result) => {
                        self.ended = true;
                        Poll::Ready(match result {
                            Ok(Ok(())) => Ok(()),
                            Ok(Err(error)) => Err(std::io::Error::other(error)),
                            Err(_) => Err(std::io::Error::other("SSH process supervisor stopped")),
                        })
                    }
                }
            }
            result => result,
        }
    }
}
