//! Filesystem primitives: regular-file reads, entry kinds, and atomic replacement.

use std::{
    fs,
    io::{self, Read as _, Seek as _, Write as _},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

/// Why a regular file could not be opened or read.
#[derive(Debug, thiserror::Error)]
pub enum RegularFileError {
    #[error("not a regular file")]
    NotRegular,
    #[error("file exceeds the {limit}-byte limit")]
    TooLarge { limit: u64 },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Open `path` for reading without blocking on a FIFO or device, and require a
/// regular file of at most `limit` bytes. The length is an early filter; readers
/// still bound what they consume.
pub(crate) fn open_regular_blocking(path: &Path, limit: u64) -> Result<fs::File, RegularFileError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
    let file = options.open(path)?;
    admit_regular(&file, limit)?;
    Ok(file)
}

/// Require that a file opened with `O_NONBLOCK` is regular and at most `limit`
/// bytes. `O_NONBLOCK` does not affect IO on a regular file, so it stays set.
pub(crate) fn admit_regular(file: &fs::File, limit: u64) -> Result<fs::Metadata, RegularFileError> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(RegularFileError::NotRegular);
    }
    if metadata.len() > limit {
        return Err(RegularFileError::TooLarge { limit });
    }
    Ok(metadata)
}

/// [`open_regular_blocking`] off the async runtime.
pub(crate) async fn open_regular(path: &Path, limit: u64) -> Result<fs::File, RegularFileError> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || open_regular_blocking(&path, limit))
        .await
        .map_err(io::Error::other)?
}

/// Read a whole regular file of at most `limit` bytes, bounded while reading so
/// a file that grows after its length check still cannot exceed the limit.
pub async fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>, RegularFileError> {
    read_to_limit(open_regular(path, limit).await?, limit).await
}

/// The rest of a file admitted by [`open_regular`], under the same bound.
pub(crate) async fn read_to_limit(file: fs::File, limit: u64) -> Result<Vec<u8>, RegularFileError> {
    tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(RegularFileError::TooLarge { limit });
        }
        Ok(bytes)
    })
    .await
    .map_err(io::Error::other)?
}

/// A filesystem entry's own kind; a symlink is never followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl From<fs::FileType> for FileKind {
    fn from(kind: fs::FileType) -> Self {
        if kind.is_symlink() {
            Self::Symlink
        } else if kind.is_dir() {
            Self::Directory
        } else if kind.is_file() {
            Self::File
        } else {
            Self::Other
        }
    }
}

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
pub struct AtomicWriteError {
    pub(crate) stage: AtomicWriteStage,
    pub(crate) source: io::Error,
}

impl AtomicWriteError {
    fn at(stage: AtomicWriteStage) -> impl Fn(io::Error) -> Self {
        move |source| Self { stage, source }
    }
}

impl From<AtomicWriteError> for io::Error {
    fn from(error: AtomicWriteError) -> Self {
        error.source
    }
}

/// Who may access a staged file once it is committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// Ordinary creation (0o666 less umask), keeping an existing destination's permissions.
    Inherit,
    /// Owner read and write only (0o600), whatever the destination had.
    Private,
}

/// How a commit treats an existing destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitMode {
    Replace,
    NoClobber,
}

/// A sibling staging file that atomically becomes its destination on commit.
/// Dropping it uncommitted removes the staging file.
pub struct StagedFile {
    file: tempfile::NamedTempFile,
    destination: PathBuf,
    directory: PathBuf,
    permissions: Option<fs::Permissions>,
}

impl StagedFile {
    pub fn create(destination: &Path, policy: PermissionPolicy) -> Result<Self, AtomicWriteError> {
        use AtomicWriteStage as Stage;
        let directory = destination
            .parent()
            .ok_or_else(|| io::Error::other("path has no parent"))
            .map_err(AtomicWriteError::at(Stage::Prepare))?
            .to_owned();
        let permissions = match policy {
            PermissionPolicy::Private => None,
            PermissionPolicy::Inherit => match fs::metadata(destination) {
                Ok(metadata) => Some(metadata.permissions()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(AtomicWriteError::at(Stage::InspectDestination)(error)),
            },
        };
        let mut staging = tempfile::Builder::new();
        staging.prefix(".skyhook-").suffix(".tmp");
        #[cfg(unix)]
        staging.permissions(std::os::unix::fs::PermissionsExt::from_mode(match policy {
            PermissionPolicy::Inherit => 0o666,
            PermissionPolicy::Private => 0o600,
        }));
        let file = staging
            .tempfile_in(&directory)
            .map_err(AtomicWriteError::at(Stage::CreateStaging))?;
        Ok(Self {
            file,
            destination: destination.to_owned(),
            directory,
            permissions,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<(), AtomicWriteError> {
        self.file
            .write_all(bytes)
            .map_err(AtomicWriteError::at(AtomicWriteStage::WriteStaging))
    }

    pub(crate) fn path(&self) -> &Path {
        self.file.path()
    }

    pub(crate) fn as_file(&self) -> &fs::File {
        self.file.as_file()
    }

    /// Sync the staging file, move it to the destination, and sync the parent
    /// directory. Errors from the directory stages follow a completed commit.
    pub fn commit(self, mode: CommitMode) -> Result<(), AtomicWriteError> {
        self.commit_with(mode, sync_directory)
    }

    fn commit_with(
        self,
        mode: CommitMode,
        sync_directory: impl FnOnce(&Path) -> Result<(), AtomicWriteError>,
    ) -> Result<(), AtomicWriteError> {
        use AtomicWriteStage as Stage;
        if let Some(permissions) = self.permissions {
            self.file
                .as_file()
                .set_permissions(permissions)
                .map_err(AtomicWriteError::at(Stage::SetPermissions))?;
        }
        self.file
            .as_file()
            .sync_all()
            .map_err(AtomicWriteError::at(Stage::SyncStaging))?;
        match mode {
            CommitMode::Replace => self.file.persist(&self.destination),
            CommitMode::NoClobber => self.file.persist_noclobber(&self.destination),
        }
        .map_err(|error| AtomicWriteError::at(Stage::Commit)(error.error))?;
        sync_directory(&self.directory)
    }
}

/// What an atomic replacement writes.
pub(crate) enum Contents {
    Bytes(Vec<u8>),
    /// A whole file, copied from its start in bounded chunks.
    File(fs::File),
}

const COPY_CHUNK_BYTES: usize = 64 * 1024;

/// Replace through a [`StagedFile`] with the [`PermissionPolicy::Inherit`] policy.
///
/// Dropping the awaiter cancels an uncommitted write and cleans its staging file.
/// Neither cancellation nor an error proves the destination is unchanged.
/// Returns the number of bytes written.
pub(crate) async fn atomic_write(path: &Path, contents: Contents) -> Result<u64, AtomicWriteError> {
    let path = path.to_owned();
    let cancellation = CancellationToken::new();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    // Own creation, IO, rename and cleanup in ONE blocking operation. A guard
    // installed after tokio::fs::open().await cannot own a late creation; a
    // dropped rename awaiter likewise cannot safely decide whether to clean up.
    tokio::task::spawn_blocking(move || {
        atomic_write_blocking(&path, contents, &cancellation, sync_directory)
    })
    .await
    .map_err(|error| AtomicWriteError {
        stage: AtomicWriteStage::Wait,
        source: io::Error::other(error),
    })?
}

fn atomic_write_blocking(
    path: &Path,
    contents: Contents,
    cancellation: &CancellationToken,
    sync_directory: impl FnOnce(&Path) -> Result<(), AtomicWriteError>,
) -> Result<u64, AtomicWriteError> {
    let cancelled = AtomicWriteError::at(AtomicWriteStage::Prepare);
    check_cancelled(cancellation).map_err(&cancelled)?;
    let mut staged = StagedFile::create(path, PermissionPolicy::Inherit)?;
    let written = match contents {
        Contents::Bytes(bytes) => staged.write(&bytes).map(|()| bytes.len() as u64),
        Contents::File(source) => copy(source, &mut staged, cancellation),
    }?;
    check_cancelled(cancellation).map_err(cancelled)?;
    // No cancellation check after this point: rename may already be visible.
    staged.commit_with(CommitMode::Replace, sync_directory)?;
    Ok(written)
}

/// Copy a whole file in bounded chunks, stopping promptly when cancelled.
fn copy(
    mut source: fs::File,
    staged: &mut StagedFile,
    cancellation: &CancellationToken,
) -> Result<u64, AtomicWriteError> {
    let failed = AtomicWriteError::at(AtomicWriteStage::WriteStaging);
    source.rewind().map_err(&failed)?;
    let mut buffer = vec![0; COPY_CHUNK_BYTES];
    let mut written = 0;
    loop {
        check_cancelled(cancellation).map_err(&failed)?;
        let read = source.read(&mut buffer).map_err(&failed)?;
        if read == 0 {
            return Ok(written);
        }
        staged.write(&buffer[..read])?;
        written += read as u64;
    }
}

pub(crate) fn sync_directory(directory: &Path) -> Result<(), AtomicWriteError> {
    fs::File::open(directory)
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

    #[tokio::test]
    async fn regular_reads_are_bounded_and_refuse_other_files_without_blocking() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(read_regular(&path, 3).await.unwrap(), b"abc");
        assert!(matches!(
            read_regular(&path, 2).await,
            Err(RegularFileError::TooLarge { limit: 2 })
        ));
        assert!(matches!(
            read_regular(root.path(), u64::MAX).await,
            Err(RegularFileError::NotRegular)
        ));
        #[cfg(unix)]
        {
            let fifo = root.path().join("fifo");
            let status = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap();
            assert!(status.success());
            // Opening a FIFO without a writer would block without O_NONBLOCK.
            let read = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                read_regular(&fifo, u64::MAX),
            );
            assert!(matches!(
                read.await.unwrap(),
                Err(RegularFileError::NotRegular)
            ));
        }
    }

    #[test]
    fn cancellation_before_commit_keeps_destination_and_cleans_staging() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("destination");
        fs::write(&path, b"old").unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = atomic_write_blocking(
            &path,
            Contents::Bytes(b"new".to_vec()),
            &cancellation,
            sync_directory,
        )
        .unwrap_err();
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
        let error = atomic_write(&path, Contents::Bytes(b"new".to_vec()))
            .await
            .unwrap_err();
        assert_eq!(error.stage, AtomicWriteStage::Commit);
        assert_eq!(fs::read(path.join("child")).unwrap(), b"old");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn directory_sync_failure_reports_that_replacement_already_committed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("destination");
        fs::write(&path, b"old").unwrap();
        let error = atomic_write_blocking(
            &path,
            Contents::Bytes(b"new".to_vec()),
            &CancellationToken::new(),
            |_| {
                Err(AtomicWriteError::at(AtomicWriteStage::SyncDirectory)(
                    io::Error::new(io::ErrorKind::PermissionDenied, "sync fixture denied"),
                ))
            },
        )
        .unwrap_err();
        assert_eq!(error.stage, AtomicWriteStage::SyncDirectory);
        assert_eq!(error.source.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(path).unwrap(), b"new");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn file_contents_copy_whole_from_the_start_across_chunks() {
        let root = tempfile::tempdir().unwrap();
        let source_path = root.path().join("source");
        let bytes: Vec<u8> = (0..COPY_CHUNK_BYTES * 2 + 7)
            .map(|i| (i % 251) as u8)
            .collect();
        fs::write(&source_path, &bytes).unwrap();
        let mut source = fs::File::open(&source_path).unwrap();
        source.seek(io::SeekFrom::Start(5)).unwrap();
        let path = root.path().join("copy");
        let written = atomic_write(&path, Contents::File(source)).await.unwrap();
        assert_eq!(written, bytes.len() as u64);
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inherit_keeps_existing_permissions_or_uses_the_umask_and_private_resets_them() {
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
            atomic_write(path, Contents::Bytes(b"new".to_vec()))
                .await
                .unwrap();
            assert_eq!(fs::read(path).unwrap(), b"new");
        }
        assert_eq!(mode(&existing), 0o751);
        // Ordinary creation mode (umask applied), not a private 0o600.
        assert_eq!(mode(&created), mode(&reference));
        let mut private = StagedFile::create(&existing, PermissionPolicy::Private).unwrap();
        private.write(b"secret").unwrap();
        private.commit(CommitMode::Replace).unwrap();
        assert_eq!(mode(&existing), 0o600);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 3);
    }
}
