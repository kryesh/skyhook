//! Shared atomic file replacement; callers select their durability guarantees.

use std::{fs, io, io::Write as _, path::Path};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(crate) struct AtomicWriteOptions {
    pub preserve_permissions: bool,
    pub sync_parent: bool,
}

/// Replace through a sibling staging file, syncing its contents before rename.
///
/// Dropping the awaiter requests cancellation, but does not interrupt an OS call.
/// The blocking owner resolves in-flight work and cleans uncommitted staging
/// before exiting. Once rename is underway it may commit despite cancellation;
/// after commit the owner also finishes the requested parent sync. Neither
/// cancellation nor an error (notably parent-sync failure) proves the destination
/// is unchanged or makes retry safe. Cleanup is best-effort under filesystem
/// errors, and does not cover process termination or undo a committed rename.
pub(crate) async fn atomic_write(
    path: &Path,
    bytes: &[u8],
    options: AtomicWriteOptions,
) -> io::Result<()> {
    let path = path.to_owned();
    let bytes = bytes.to_owned();
    let cancellation = CancellationToken::new();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    // Own creation, IO, rename and cleanup in ONE blocking operation. A guard
    // installed after tokio::fs::open().await cannot own a late creation; a
    // dropped rename awaiter likewise cannot safely decide whether to clean up.
    tokio::task::spawn_blocking(move || {
        atomic_write_blocking(&path, &bytes, options, &cancellation)
    })
    .await
    .map_err(io::Error::other)?
}

fn atomic_write_blocking(
    path: &Path,
    bytes: &[u8],
    options: AtomicWriteOptions,
    cancellation: &CancellationToken,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    check_cancelled(cancellation)?;
    let permissions = if options.preserve_permissions {
        match fs::metadata(path) {
            Ok(metadata) => Some(metadata.permissions()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    // The staging file removes itself unless persisted. Match ordinary file
    // creation (0o666 less umask) rather than tempfile's private 0o600 default.
    let mut staging = tempfile::Builder::new();
    staging.prefix(".skyhook-").suffix(".tmp");
    #[cfg(unix)]
    staging.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o666));
    let mut file = staging.tempfile_in(parent)?;
    file.write_all(bytes)?;
    if let Some(permissions) = permissions {
        file.as_file().set_permissions(permissions)?;
    }
    file.as_file().sync_all()?;
    check_cancelled(cancellation)?;
    // No cancellation check after this point: rename may already be visible.
    file.persist(path).map_err(|error| error.error)?;
    if options.sync_parent {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
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

    fn options(preserve_permissions: bool, sync_parent: bool) -> AtomicWriteOptions {
        AtomicWriteOptions {
            preserve_permissions,
            sync_parent,
        }
    }

    #[test]
    fn cancellation_before_commit_keeps_destination_and_cleans_staging() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("destination");
        fs::write(&path, b"old").unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error =
            atomic_write_blocking(&path, b"new", options(true, true), &cancellation).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
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
        atomic_write(&path, b"new", options(false, true))
            .await
            .unwrap_err();
        assert_eq!(fs::read(path.join("child")).unwrap(), b"old");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn success_preserves_options_and_atomic_visibility() {
        for (preserve_permissions, sync_parent, existing) in [
            (false, false, true),
            (false, true, true),
            (true, false, true),
            (true, true, true),
            (true, true, false),
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("destination");
            if existing {
                fs::write(&path, b"old").unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o751)).unwrap();
                }
            }
            atomic_write(&path, b"new", options(preserve_permissions, sync_parent))
                .await
                .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                if preserve_permissions && existing {
                    assert_eq!(mode, 0o751);
                } else {
                    // Ordinary creation mode (umask applied), not a private 0o600.
                    let reference = tempfile::tempdir().unwrap();
                    fs::write(reference.path().join("file"), b"").unwrap();
                    let metadata = fs::metadata(reference.path().join("file")).unwrap();
                    assert_eq!(mode, metadata.permissions().mode() & 0o777);
                }
            }
            assert_eq!(fs::read(&path).unwrap(), b"new");
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        }
    }
}
