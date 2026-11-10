//! Shim deployment at a route's destination: start an installed shim, or probe the
//! platform, install its shim and start that, each a command the destination's shell
//! runs over the route's channel.
use skyhook_worker::op::TargetPath;
use skyhook_worker::origin::{
    ProcessEnvironment,
    ssh::{self, ConnectionSecret, SshRoute},
    transport::Transport,
    winrm::{self, Client, WinrmRoute},
};
use skyhook_worker::protocol::PREAMBLE;
use skyhook_worker::target::{Os, UPLOAD_PREFIX};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::bootstrap::{self, INSTALLED};
use crate::remote::{
    artifact::EmbeddedShimCatalog,
    client::PooledConnection,
    error::{DeploymentError, RemoteError},
};
use skyhook_worker::origin::Leg;

/// The most output a bootstrap command may return.
const PROBE_BYTES: u64 = 1024 * 1024;

/// One connection attempt to a destination, which may run several commands there.
pub(crate) struct Launcher {
    /// The destination's authorization root, as its definition spells it.
    pub root: TargetPath,
    /// The destination's declared OS, whose shell runs the deployment.
    pub os: Os,
    pub channel: Channel,
}

/// How a connection attempt runs its commands at the destination.
pub(crate) enum Channel {
    /// SSH started on this machine, with its agent environment, reading the route's
    /// secret once for the whole attempt, each process supervised by a task of `work`.
    Ssh {
        route: SshRoute,
        environment: ProcessEnvironment,
        secret: ConnectionSecret,
        work: skyhook_base::Work,
    },
    /// WinRM from this machine, over one client the attempt authenticated once.
    Winrm { route: WinrmRoute, client: Client },
    /// SSH or WinRM started on a remote origin, whose shim chooses agents and reads
    /// the secret for each command it starts.
    Origin {
        leg: Leg,
        origin: std::sync::Arc<PooledConnection>,
    },
}

/// What a start command did: started a shim, whose stream follows, or printed.
enum Started {
    Serving(Transport),
    Printed(String),
}

impl Launcher {
    /// Start the destination's shim in one command when an intact one is installed;
    /// otherwise install the one its platform needs, then start that.
    pub(crate) async fn connect(
        &self,
        catalog: &EmbeddedShimCatalog,
    ) -> Result<Transport, RemoteError> {
        let (os, root) = (self.os, self.root.as_str());
        let start = bootstrap::start(os, &catalog.installed(os), root);
        let printed = match started(self.open(&start).await?).await? {
            Started::Serving(transport) => return Ok(transport),
            Started::Printed(printed) => printed,
        };
        let platform = bootstrap::platform(os, &printed)?;
        let shim = catalog.find(platform)?;
        let name = shim.installed_name();
        let mut random = [0; 8];
        getrandom::fill(&mut random).map_err(DeploymentError::Random)?;
        // Windows runs a file only by its executable suffix.
        let suffix = platform.os.executable_suffix();
        let upload = format!("{UPLOAD_PREFIX}{:016x}{suffix}", u64::from_ne_bytes(random));
        let install = bootstrap::install(os, &name, &upload);
        let installing = self.open(&install).await?;
        if output(installing, &shim.bytes).await?.trim() != INSTALLED {
            return Err(DeploymentError::Install.into());
        }
        match started(self.open(&bootstrap::serve(os, &name, root)).await?).await? {
            Started::Serving(transport) => Ok(transport),
            Started::Printed(_) => Err(DeploymentError::Install.into()),
        }
    }

    async fn open(&self, command: &str) -> Result<Transport, RemoteError> {
        let command = command.to_owned();
        match &self.channel {
            Channel::Ssh {
                route,
                environment,
                secret,
                work,
            } => Ok(ssh::open(route, &command, environment, secret, work).await?),
            Channel::Winrm { route, client } => Ok(winrm::open(route, client, &command).await?),
            Channel::Origin { leg, origin } => origin.open(leg.clone(), command).await,
        }
    }
}

/// What the command behind `transport` prints once it has read all of `bytes`. A
/// command that ends early closes its input, and its output ends with why or shows
/// that it did not finish: the write it broke is never the failure.
async fn output(transport: Transport, bytes: &[u8]) -> Result<String, RemoteError> {
    let Transport {
        mut input,
        output,
        owner: _owner,
    } = transport;
    let write = async move {
        input.write_all(bytes).await?;
        input.shutdown().await
    };
    let read = async {
        let mut printed = Vec::new();
        output.take(PROBE_BYTES).read_to_end(&mut printed).await?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&printed).into_owned())
    };
    let (_, printed) = tokio::join!(write, read);
    Ok(printed?)
}

/// What a start command's `transport` carries: a shim's stream after the preamble it
/// writes first, or all the command printed.
async fn started(transport: Transport) -> Result<Started, RemoteError> {
    let Transport {
        input,
        mut output,
        owner,
    } = transport;
    let mut printed = Vec::new();
    loop {
        let found = (printed.windows(PREAMBLE.len())).position(|bytes| bytes == PREAMBLE);
        if let Some(at) = found {
            let output = Box::new(
                std::io::Cursor::new(printed.split_off(at + PREAMBLE.len())).chain(output),
            );
            return Ok(Started::Serving(Transport {
                input,
                output,
                owner,
            }));
        }
        if printed.len() as u64 > PROBE_BYTES {
            return Err(DeploymentError::Probe.into());
        }
        if output.read_buf(&mut printed).await? == 0 {
            return Ok(Started::Printed(String::from_utf8_lossy(&printed).into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::bounded;

    fn printing(output: &'static [u8]) -> Transport {
        Transport {
            input: Box::new(tokio::io::sink()),
            output: Box::new(output),
            owner: Box::new(()),
        }
    }

    /// A start that reaches a shim hands on its stream after the preamble, whatever a
    /// login script printed before it; one that ends first printed its probe.
    #[tokio::test]
    async fn a_start_serves_from_the_preamble_or_returns_what_it_printed() {
        let serving = printing(b"Last login: today\nSKYHOOK\0frames");
        let Started::Serving(mut transport) = bounded(started(serving)).await.unwrap() else {
            panic!("expected the shim's stream");
        };
        let mut stream = Vec::new();
        transport.output.read_to_end(&mut stream).await.unwrap();
        assert_eq!(stream, b"frames");
        let probed = printing(b"skyhook-probe\nLinux\nx86_64\n");
        let Started::Printed(printed) = bounded(started(probed)).await.unwrap() else {
            panic!("expected the probe");
        };
        assert_eq!(printed, "skyhook-probe\nLinux\nx86_64\n");
    }

    /// A command that ends without reading what it is sent breaks the write, and its
    /// output ends with how it exited: that is the failure reported. One that exited
    /// cleanly reports what it printed.
    #[tokio::test]
    async fn a_command_that_ends_early_fails_with_how_it_exited() {
        use skyhook_worker::origin::{
            error::{SshError, TransportError},
            transport::ReportingReader,
        };
        let unread = || {
            let (input, closed) = tokio::io::duplex(1);
            drop(closed);
            Box::new(input)
        };
        let (exit, exited) = tokio::sync::oneshot::channel();
        let exit_status = SshError::Exited {
            status: skyhook_worker::op::ExitStatus::Exited(1),
            stderr: "No space left on device".into(),
        };
        exit.send(Err(TransportError::from(exit_status))).unwrap();
        let transport = Transport {
            input: unread(),
            output: Box::new(ReportingReader::new(tokio::io::empty(), exited)),
            owner: Box::new(()),
        };
        let failed = bounded(output(transport, b"shim")).await.unwrap_err();
        std::assert_matches!(
            failed.fault,
            crate::remote::RemoteFault::Transport(TransportError::Ssh(SshError::Exited { .. }))
        );
        let transport = Transport {
            input: unread(),
            ..printing(b"usage")
        };
        let printed = bounded(output(transport, b"shim")).await.unwrap();
        assert_eq!(printed, "usage");
    }
}
