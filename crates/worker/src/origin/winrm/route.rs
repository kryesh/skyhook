//! WinRM connections as byte streams: a command in a WinRS shell carries the protocol
//! on its standard input and output, and its error output explains how it failed.
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;

use super::{Auth, Client, Endpoint, Process, Tls, WinrmError};
use crate::{
    op::ExitStatus,
    origin::{
        error::{TransportError, WinrmStreamError},
        transport::{ReportingReader, Transport},
    },
    secret::Credential,
};

/// The most of a command's error output kept to explain its failure.
const DIAGNOSTIC_BYTES: u64 = 8192;

/// A WinRM listener as the machine connecting to it receives it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WinrmRoute {
    /// Which target failures name.
    #[serde(rename = "name")]
    pub target: String,
    pub endpoint: Endpoint,
    pub auth: Auth<Credential>,
    pub tls: Tls,
}

impl WinrmRoute {
    /// Connect from this machine, reading the password here, the shells the client
    /// drops deleted as tasks of `work`.
    pub async fn connect(&self, work: &skyhook_base::Work) -> Result<Client, TransportError> {
        let target = self.target.clone();
        let auth = (self.auth.clone().resolve().await)
            .map_err(|error| WinrmStreamError::Password { target, error })?;
        let (endpoint, tls) = (self.endpoint.clone(), self.tls.clone());
        let connected = super::connect(endpoint, auth, tls, work.clone()).await;
        connected.map_err(|error| self.failed(error).into())
    }

    fn failed(&self, error: WinrmError) -> WinrmStreamError {
        let target = self.target.clone();
        WinrmStreamError::Connection { target, error }
    }
}

/// Run `command` on `client`'s listener as a stream: its input and output are the
/// command's, and once the output ends it reports how the command ended. Dropping the
/// stream's owner deletes the command's shell, at once.
pub async fn open(
    route: &WinrmRoute,
    client: &Client,
    command: &str,
) -> Result<Transport, TransportError> {
    let Process {
        stdin,
        stdout,
        mut stderr,
        handle,
    } = (client.run(command).await).map_err(|error| route.failed(error))?;
    let (sender, completion) = tokio::sync::oneshot::channel();
    let (failed, exit) = (route.clone(), handle.exit());
    let ended = tokio::spawn(async move {
        let diagnostics = async {
            let mut kept = Vec::new();
            let _ = (&mut stderr)
                .take(DIAGNOSTIC_BYTES)
                .read_to_end(&mut kept)
                .await;
            let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
            String::from_utf8_lossy(&kept).trim().to_owned()
        };
        let (status, stderr) = tokio::join!(exit, diagnostics);
        let result: Result<(), TransportError> = match status {
            Ok(ExitStatus::Exited(0)) => Ok(()),
            Ok(status) => Err(WinrmStreamError::Exited { status, stderr }.into()),
            Err(error) => Err(failed.failed(error).into()),
        };
        let _ = sender.send(result);
    });
    let ended = tokio_util::task::AbortOnDropHandle::new(ended);
    Ok(Transport {
        input: Box::new(stdin),
        output: Box::new(ReportingReader::new(stdout, completion)),
        owner: Box::new((ended, handle)),
    })
}
