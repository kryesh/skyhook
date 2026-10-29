//! OpenSSH backend: configuration, authentication, process transport, and bootstrap.
mod askpass;
mod authentication;
mod config;
mod process;
mod requester;

pub use askpass::askpass_main;
pub(crate) use askpass::{AskpassServer, apply_environment};
pub(crate) use authentication::{Authentication, worker_authentication};
pub(crate) use config::SshOption;
pub(crate) use process::open;

use crate::remote::{
    artifact::EmbeddedShimCatalog,
    backend::{
        ConnectionFactory, ConnectionRequest, ProcessAuthentication, ProcessEnvironment, Transport,
    },
    error::RemoteError,
    prompt::SensitivePromptHandler,
};
use futures_util::future::BoxFuture;
use std::sync::Arc;

const AUTH_SOCK: &str = "SSH_AUTH_SOCK";

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

/// Session-owned OpenSSH implementation of the common transport factory.
/// Authentication stays lazy and shared across every connection in the session.
pub(crate) struct Backend {
    catalog: EmbeddedShimCatalog,
    prompts: Arc<dyn SensitivePromptHandler>,
    authentication: Authentication,
}

impl Backend {
    pub fn new(
        catalog: EmbeddedShimCatalog,
        prompts: Arc<dyn SensitivePromptHandler>,
        jobs: Option<crate::job::JobManager>,
    ) -> Self {
        Self {
            catalog,
            authentication: Authentication::new(prompts.clone(), jobs),
            prompts,
        }
    }
}

impl ConnectionFactory for Backend {
    fn connect(&self, request: ConnectionRequest) -> BoxFuture<'_, Result<Transport, RemoteError>> {
        Box::pin(async move {
            // A remote origin's shim chooses agents for the SSH process it starts.
            let environment = if request.origin.is_some() {
                ProcessEnvironment::new()
            } else {
                self.authentication
                    .route_environment(&request.route)
                    .await?
            };
            process::SshLauncher {
                origin: request.origin,
                route: request.route,
                environment,
                prompts: self.prompts.clone(),
            }
            .connect(&request.workspace, &self.catalog)
            .await
        })
    }

    fn environment(
        &self,
        authentication: ProcessAuthentication,
    ) -> BoxFuture<'_, Result<ProcessEnvironment, RemoteError>> {
        Box::pin(self.authentication.process_environment(authentication))
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.authentication.shutdown())
    }
}
