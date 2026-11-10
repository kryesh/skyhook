//! OpenSSH process lifetime and transport pipes.
#[cfg(unix)]
mod unix;
#[cfg(windows)]
pub(super) mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

use super::route::SshRoute;
use super::{
    config::{OriginAgent, SshConfig},
    terminal::{ConnectionSecret, Terminal},
};
use crate::origin::{
    ProcessEnvironment,
    error::{SshError, TransportError},
    transport::{Reader, ReportingReader, Transport, Writer},
};
use crate::process::ProcessGroup;
use tokio::io::{AsyncRead, AsyncReadExt as _};

/// The most of SSH's own error output kept to explain its failure.
const DIAGNOSTIC_BYTES: usize = 8192;

/// A started process, its tree and its standard streams.
pub(in crate::origin::ssh) struct Spawned {
    pub child: platform::Child,
    pub group: ProcessGroup,
    pub input: Writer,
    pub output: Reader,
    pub errors: platform::Errors,
}

/// Start SSH along `route`, typing its hop's `secret`. A task of `work` supervises
/// the process: it ends once SSH has exited by itself, or once dropping the
/// transport's owner has killed SSH and its group and SSH has been waited for.
pub async fn open(
    route: &SshRoute,
    remote_command: &str,
    environment: &ProcessEnvironment,
    secret: &ConnectionSecret,
    work: &skyhook_base::Work,
) -> Result<Transport, TransportError> {
    let config = SshConfig::create(route, &OriginAgent::of(environment))?;
    let mut terminal = match route.secret() {
        Some((hop, credential)) => {
            let secret = secret.read(&hop.target, credential).await?;
            Some(Terminal::open(hop.target.clone(), secret)?)
        }
        None => None,
    };
    // With a terminal SSH reads its secret there, never through askpass.
    let mut environment = environment.clone();
    if terminal.is_some() {
        environment.insert("SSH_ASKPASS_REQUIRE".into(), "never".into());
    }
    let console = terminal.as_ref().map(|terminal| &terminal.console);
    let Spawned {
        mut child,
        mut group,
        input,
        output,
        mut errors,
    } = platform::spawn(
        "ssh",
        &config.arguments(remote_command),
        &environment,
        console,
    )
    .map_err(TransportError::start)?;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cancelled = cancellation.clone();
    let (sender, completion) = tokio::sync::oneshot::channel();
    let supervise = async move {
        let mut diagnostics = Diagnostics::default();
        let exited = async {
            let answering = async {
                match &mut terminal {
                    Some(terminal) => terminal.answer().await,
                    None => std::future::pending().await,
                }
            };
            // Only cancellation or a failed prompt stops the group; an SSH that
            // exits by itself leaves its group alone.
            let status = tokio::select! {
                status = child.wait() => {
                    group.release();
                    Ok(status)
                }
                () = cancelled.cancelled() => {
                    group.kill();
                    let _ = child.kill().await;
                    Ok(child.wait().await)
                }
                failure = answering => {
                    group.kill();
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    Err(failure)
                }
            };
            drop(terminal.take());
            status
        };
        tokio::pin!(exited);
        let status = tokio::select! {
            status = &mut exited => status,
            () = diagnostics.drain(&mut errors) => exited.await,
        };
        // A descendant such as a ProxyCommand can inherit stderr and outlive
        // SSH, so its end of file cannot mark the end of SSH's own output.
        platform::written(&mut errors, &mut diagnostics).await;
        let result = match status {
            Err(failure) => Err(SshError::Prompt(failure).into()),
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => Err(SshError::Exited {
                status: status.into(),
                stderr: diagnostics.into_text(),
            }
            .into()),
            Ok(Err(error)) => Err(TransportError::Io(error.into())),
        };
        let _ = sender.send(result);
        // Keep draining for such descendants until the transport is dropped.
        let mut sink = tokio::io::sink();
        tokio::select! {
            _ = tokio::io::copy(&mut errors, &mut sink) => {}
            () = cancelled.cancelled() => {}
        }
    };
    work.spawn_on(supervise, &tokio::runtime::Handle::current());
    Ok(Transport {
        input,
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
    async fn drain(&mut self, errors: &mut (impl AsyncRead + Unpin)) {
        let mut buffer = [0; 1024];
        while let Ok(read @ 1..) = errors.read(&mut buffer).await {
            self.keep(&buffer[..read]);
        }
    }

    fn into_text(self) -> String {
        String::from_utf8_lossy(&self.0).trim().to_owned()
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
pub(super) mod tests {
    use super::*;
    use crate::tests::bounded;

    /// SSH along `route` supervised by `work`, `script` standing in for it, and the
    /// directory holding the script.
    pub(in super::super) async fn start(
        script: &str,
        route: &SshRoute,
        work: &skyhook_base::Work,
    ) -> (tempfile::TempDir, Transport) {
        use std::os::unix::fs::PermissionsExt as _;
        let bin = tempfile::tempdir().unwrap();
        let ssh = bin.path().join("ssh");
        std::fs::write(&ssh, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = std::env::var("PATH").unwrap();
        let search = format!("{}:{path}", bin.path().display());
        let environment = ProcessEnvironment::from([("PATH".into(), search)]);
        let secret = ConnectionSecret::default();
        let transport = open(route, "ignored", &environment, &secret, work).await;
        (bin, transport.unwrap())
    }

    fn route() -> SshRoute {
        SshRoute::Plain(vec![super::super::config::tests::hop(
            "remote",
            serde_json::json!({}),
        )])
    }

    /// A wrapper or ProxyCommand that SSH starts can inherit its stderr and
    /// outlive it; here it lives until the transport's input closes.
    #[tokio::test]
    async fn ssh_completes_while_a_descendant_holds_its_stderr() {
        for (status, expected) in [(255, Some("connection refused")), (0, None)] {
            let script = format!(
                "exec 3<&0\ncat <&3 >/dev/null &\necho 'connection refused' >&2\nexit {status}"
            );
            let (_bin, mut transport) = start(&script, &route(), &Default::default()).await;
            let mut output = Vec::new();
            let result = bounded(transport.output.read_to_end(&mut output)).await;
            match (result.map_err(TransportError::from), expected) {
                (Ok(_), None) => {}
                (Err(TransportError::Ssh(SshError::Exited { stderr, .. })), Some(expected)) => {
                    assert_eq!(stderr, expected);
                }
                (result, _) => panic!("exit {status}: {result:?}"),
            }
        }
    }

    /// Dropping a transport kills its SSH, and the work supervising it has finished
    /// only once SSH has been waited for: the process is gone by then.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_dropped_transports_work_finishes_once_its_ssh_is_gone() {
        use tokio::io::AsyncBufReadExt as _;
        let work = skyhook_base::Work::default();
        let (_bin, mut transport) = start("echo $$; exec sleep 60", &route(), &work).await;
        let mut pid = String::new();
        let mut output = tokio::io::BufReader::new(&mut transport.output);
        bounded(output.read_line(&mut pid)).await.unwrap();
        let pid: u32 = pid.trim().parse().unwrap();
        drop(transport);
        bounded(work.finished()).await;
        let process = std::path::Path::new("/proc").join(pid.to_string());
        assert!(!process.exists(), "SSH outlived the work supervising it");
    }

    /// A descendant that keeps SSH's stderr open without needing its input keeps
    /// the drain alive after SSH exits, until the owner alone is dropped: the
    /// drain's end closes the pipe's only reader.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropping_the_owner_stops_draining_for_a_lingering_descendant() {
        use tokio::{io::Interest, net::unix::pipe};
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
        let script = "setsid sh -c 'echo $$; exec sleep 1000 >/dev/null' </dev/null &";
        let (_bin, transport) = start(script, &route(), &Default::default()).await;
        let Transport {
            input,
            mut output,
            owner,
        } = transport;
        let mut pid = String::new();
        bounded(output.read_to_string(&mut pid)).await.unwrap();
        let descendant = Descendant(pid.trim().parse().unwrap());
        let stderr = std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/proc/{}/fd/2", descendant.0))
            .unwrap();
        // A pipe's writer reports an error once no reader is left.
        let stderr = pipe::Sender::from_file(stderr).unwrap();
        drop(owner);
        let closed = bounded(stderr.ready(Interest::ERROR)).await.unwrap();
        assert!(closed.is_error());
        drop((input, output, descendant));
    }
}
