//! Atomic file replacement.

use std::{fs, io, io::Write as _, path::Path};
use tokio_util::sync::CancellationToken;

/// The operation owning a failed atomic replacement. Commit and later stages
/// must not be treated as proof that the destination is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AtomicWriteStage {
    Prepare,
    InspectDestination,
    CreateStaging,
    WriteStaging,
    SetPermissions,
    SyncStaging,
    Commit,
    OpenDirectory,
    SyncDirectory,
    Wait,
}

#[derive(Debug)]
pub(crate) struct AtomicWriteError {
    pub stage: AtomicWriteStage,
    pub source: io::Error,
}

impl AtomicWriteError {
    fn at(stage: AtomicWriteStage) -> impl FnOnce(io::Error) -> Self {
        move |source| Self { stage, source }
    }
}

/// Replace through a synced sibling staging file, keeping an existing file's
/// permissions and syncing the parent directory.
///
/// Dropping the awaiter cancels an uncommitted write and cleans its staging file.
/// Neither cancellation nor an error proves the destination is unchanged.
pub(crate) async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), AtomicWriteError> {
    let path = path.to_owned();
    let bytes = bytes.to_owned();
    let cancellation = CancellationToken::new();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    // Own creation, IO, rename and cleanup in ONE blocking operation. A guard
    // installed after tokio::fs::open().await cannot own a late creation; a
    // dropped rename awaiter likewise cannot safely decide whether to clean up.
    tokio::task::spawn_blocking(move || {
        atomic_write_blocking(&path, &bytes, &cancellation, sync_directory)
    })
    .await
    .map_err(|error| AtomicWriteError {
        stage: AtomicWriteStage::Wait,
        source: io::Error::other(error),
    })?
}

fn atomic_write_blocking(
    path: &Path,
    bytes: &[u8],
    cancellation: &CancellationToken,
    sync_directory: impl FnOnce(&Path) -> Result<(), AtomicWriteError>,
) -> Result<(), AtomicWriteError> {
    use AtomicWriteStage as Stage;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))
        .map_err(AtomicWriteError::at(Stage::Prepare))?;
    check_cancelled(cancellation).map_err(AtomicWriteError::at(Stage::Prepare))?;
    let permissions = match fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(AtomicWriteError::at(Stage::InspectDestination)(error)),
    };
    // The staging file removes itself unless persisted. Match ordinary file
    // creation (0o666 less umask) rather than tempfile's private 0o600 default.
    let mut staging = tempfile::Builder::new();
    staging.prefix(".skyhook-").suffix(".tmp");
    #[cfg(unix)]
    staging.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o666));
    let mut file = staging
        .tempfile_in(parent)
        .map_err(AtomicWriteError::at(Stage::CreateStaging))?;
    file.write_all(bytes)
        .map_err(AtomicWriteError::at(Stage::WriteStaging))?;
    if let Some(permissions) = permissions {
        file.as_file()
            .set_permissions(permissions)
            .map_err(AtomicWriteError::at(Stage::SetPermissions))?;
    }
    file.as_file()
        .sync_all()
        .map_err(AtomicWriteError::at(Stage::SyncStaging))?;
    check_cancelled(cancellation).map_err(AtomicWriteError::at(Stage::Prepare))?;
    // No cancellation check after this point: rename may already be visible.
    file.persist(path)
        .map_err(|error| AtomicWriteError::at(Stage::Commit)(error.error))?;
    sync_directory(parent)
}

fn sync_directory(parent: &Path) -> Result<(), AtomicWriteError> {
    fs::File::open(parent)
        .map_err(AtomicWriteError::at(AtomicWriteStage::OpenDirectory))?
        .sync_all()
        .map_err(AtomicWriteError::at(AtomicWriteStage::SyncDirectory))
}

fn check_cancelled(cancellation: &CancellationToken) -> io::Result<()> {
    if cancellation.is_cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "atomic write awaiter dropped",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_before_commit_keeps_destination_and_cleans_staging() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("destination");
        fs::write(&path, b"old").unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error =
            atomic_write_blocking(&path, b"new", &cancellation, sync_directory).unwrap_err();
        assert_eq!(error.stage, AtomicWriteStage::Prepare);
        assert_eq!(error.source.kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn rename_failure_cleans_uncommitted_staging() {
        let root = tempfile::tempdir().unwrap();
        // Renaming a file over a non-empty directory fails after the staging write.
        let path = root.path().join("destination");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("child"), b"old").unwrap();
        let error = atomic_write(&path, b"new").await.unwrap_err();
        assert_eq!(error.stage, AtomicWriteStage::Commit);
        assert_eq!(fs::read(path.join("child")).unwrap(), b"old");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn directory_sync_failure_reports_that_replacement_already_committed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("destination");
        fs::write(&path, b"old").unwrap();
        let error = atomic_write_blocking(&path, b"new", &CancellationToken::new(), |_| {
            Err(AtomicWriteError::at(AtomicWriteStage::SyncDirectory)(
                io::Error::new(io::ErrorKind::PermissionDenied, "sync fixture denied"),
            ))
        })
        .unwrap_err();
        assert_eq!(error.stage, AtomicWriteStage::SyncDirectory);
        assert_eq!(error.source.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(path).unwrap(), b"new");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn success_keeps_existing_permissions_and_otherwise_uses_the_umask() {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let root = tempfile::tempdir().unwrap();
        let (existing, created, reference) = (
            root.path().join("existing"),
            root.path().join("created"),
            root.path().join("reference"),
        );
        fs::write(&existing, b"old").unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o751)).unwrap();
        fs::write(&reference, b"").unwrap();
        for path in [&existing, &created] {
            atomic_write(path, b"new").await.unwrap();
            assert_eq!(fs::read(path).unwrap(), b"new");
        }
        assert_eq!(mode(&existing), 0o751);
        // Ordinary creation mode (umask applied), not a private 0o600.
        assert_eq!(mode(&created), mode(&reference));
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 3);
    }
}
