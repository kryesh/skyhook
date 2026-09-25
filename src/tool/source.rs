//! Source files: a path on any target whose contents a tool consumes. The
//! executor opens it where it lives; remote contents stream into an anonymous
//! spool file, so sources of any size never need to fit in memory.
use std::{
    io::{self, Seek as _},
    path::Path,
    sync::Arc,
};

use tokio::io::AsyncWriteExt as _;

use crate::tool::{
    PathKind,
    builtins::workspace::{resolve_for_authorization, source_file},
    invocation::{AdmissionError, LocalContext, LocalError},
    policy::Capability,
};

/// An opened source, read from its start by the handler that consumes it.
#[derive(Clone, Debug)]
pub(crate) struct Source(Arc<std::fs::File>);

impl Source {
    /// Open an authorized source path on this machine.
    pub(crate) async fn open(path: &Path) -> Result<Self, AdmissionError> {
        let file = crate::fs::open_regular(path, u64::MAX)
            .await
            .map_err(source_file(path))?;
        Ok(Self(Arc::new(file)))
    }

    /// A handle positioned at the start. Handles share one file offset, so a
    /// source has one reader at a time.
    pub(crate) fn reader(&self) -> io::Result<std::fs::File> {
        let mut file = self.0.try_clone()?;
        file.rewind()?;
        Ok(file)
    }
}

/// An anonymous temporary file receiving contents streamed from another machine.
pub(crate) struct Spool(tokio::fs::File);

impl Spool {
    pub(crate) async fn new() -> io::Result<Self> {
        let file = tokio::task::spawn_blocking(tempfile::tempfile)
            .await
            .map_err(io::Error::other)??;
        Ok(Self(tokio::fs::File::from_std(file)))
    }

    pub(crate) async fn append(&mut self, data: &[u8]) -> io::Result<()> {
        self.0.write_all(data).await
    }

    /// The received contents, as a source read from its start.
    pub(crate) async fn finish(mut self) -> io::Result<Source> {
        self.0.flush().await?;
        Ok(Source(Arc::new(self.0.into_std().await)))
    }
}

/// Open a source for a remote read by `tool`: resolve the path in this worker's
/// workspace and authorize it if it is outside the authorized root.
pub(crate) async fn open_on_worker(
    tool: &str,
    path: &str,
    context: &LocalContext,
    authorization_root: &Path,
) -> Result<std::fs::File, LocalError> {
    if !context.capabilities().contains(Capability::Read) {
        return Err(LocalError::unavailable(tool));
    }
    let location = context.execution_location();
    let resolved = resolve_for_authorization(&location.workspace, path, PathKind::Existing).await?;
    if let Some(permission) =
        resolved.permission_outside(authorization_root, Capability::Read, &location.target)
    {
        context.authorize(vec![permission]).await?;
    }
    Ok(crate::fs::open_regular(resolved.path.as_path(), u64::MAX)
        .await
        .map_err(source_file(resolved.path.as_path()))?)
}
