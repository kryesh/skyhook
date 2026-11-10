//! Atomic replacement of a file through a synced sibling staging file.

use std::{
    fs,
    io::{self, Seek as _, Write as _},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

use super::{
    LiveDatabase,
    guarded::{OpenFile, is_live},
};
use crate::op::{OsError, TargetPath};

/// The operation owning a failed atomic replacement. Commit and later stages
/// must not be treated as proof that the destination is unchanged. The directory
/// stages name the destination's directory, as its machine splits the path.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum AtomicWriteStage {
    Prepare,
    InspectDestination,
    CreateStaging,
    WriteStaging,
    SetPermissions,
    SyncStaging,
    Commit,
    OpenDirectory(TargetPath),
    SyncDirectory(TargetPath),
}

/// A failed atomic replacement: the stage that failed, and how.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct AtomicWriteError {
    pub stage: AtomicWriteStage,
    pub error: OsError,
}

impl AtomicWriteError {
    fn at(stage: AtomicWriteStage) -> impl Fn(io::Error) -> Self {
        move |error| Self {
            stage: stage.clone(),
            error: error.into(),
        }
    }
}

impl From<AtomicWriteError> for io::Error {
    fn from(error: AtomicWriteError) -> Self {
        io::Error::from(&error.error)
    }
}

/// Who may access a staged file once it is committed. On Windows the file takes its
/// directory's inherited access control, and no attributes of the destination, either way.
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
        if is_live(destination) {
            return Err(AtomicWriteError::at(Stage::InspectDestination)(
                io::Error::other(LiveDatabase),
            ));
        }
        // Windows permissions are a file's attributes, such as being a directory, which
        // are not the replacement's to copy.
        let permissions = match policy {
            PermissionPolicy::Inherit if cfg!(unix) => match fs::metadata(destination) {
                Ok(metadata) => Some(metadata.permissions()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(AtomicWriteError::at(Stage::InspectDestination)(error)),
            },
            PermissionPolicy::Inherit | PermissionPolicy::Private => None,
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

    pub fn as_file(&self) -> &fs::File {
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
        // Decided and done under the registry's lock: the destination is checked again,
        // and the staging descriptor, now the destination's, closes before a database
        // can open there.
        let registry = crate::fs::live::lock();
        if registry.holds(&crate::fs::live::Probe::path(&self.destination)) {
            return Err(AtomicWriteError::at(Stage::Commit)(io::Error::other(
                LiveDatabase,
            )));
        }
        let committed = match mode {
            CommitMode::Replace => self.file.persist(&self.destination),
            CommitMode::NoClobber => self.file.persist_noclobber(&self.destination),
        }
        .map_err(|error| AtomicWriteError::at(Stage::Commit)(error.error))?;
        drop(committed);
        drop(registry);
        sync_directory(&self.directory)
    }
}

/// A [`StagedFile`] written as its bytes arrive. A write's bytes are taken when it is
/// first polled and reach the file in the order taken, whatever becomes of what awaited
/// it: every later step waits for them first, and fails as their write did. Each step
/// is blocking work its connection counts, so one still running when this drops is
/// waited for before the connection says its work is done.
pub(crate) struct StagedWriter {
    /// Before the staged file, so ordinary drop closes it first.
    file: std::sync::Arc<fs::File>,
    staged: StagedFile,
    /// The bytes taken that no write has begun on, and the write under way.
    taken: Vec<u8>,
    writing: Option<tokio::task::JoinHandle<io::Result<()>>>,
}

impl StagedWriter {
    pub(crate) async fn create(
        destination: &Path,
        policy: PermissionPolicy,
    ) -> Result<Self, OsError> {
        let destination = destination.to_owned();
        skyhook_base::blocking(move || {
            let staged = StagedFile::create(&destination, policy).map_err(|error| error.error)?;
            let file = std::sync::Arc::new(staged.as_file().try_clone()?);
            Ok(Self {
                file,
                staged,
                taken: Vec::new(),
                writing: None,
            })
        })
        .await
    }

    pub(crate) fn path(&self) -> &Path {
        self.staged.path()
    }

    pub(crate) async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.taken.extend_from_slice(bytes);
        self.written().await
    }

    /// Once the file holds every byte taken.
    async fn written(&mut self) -> io::Result<()> {
        loop {
            if let Some(writing) = &mut self.writing {
                let ended = writing.await;
                self.writing = None;
                skyhook_base::joined(ended).await?;
            }
            if self.taken.is_empty() {
                return Ok(());
            }
            let (file, bytes) = (self.file.clone(), std::mem::take(&mut self.taken));
            let write = move || (&*file).write_all(&bytes);
            self.writing = Some(skyhook_base::spawn_blocking(write));
        }
    }

    pub(crate) async fn sync(&mut self) -> io::Result<()> {
        self.written().await?;
        let file = self.file.clone();
        skyhook_base::blocking(move || file.sync_all()).await
    }

    /// [`StagedFile::commit`], which runs to its end once started.
    pub(crate) async fn commit(mut self, mode: CommitMode) -> Result<(), AtomicWriteError> {
        let written = self.written().await;
        written.map_err(AtomicWriteError::at(AtomicWriteStage::WriteStaging))?;
        let Self { file, staged, .. } = self;
        drop(file);
        skyhook_base::blocking(move || staged.commit(mode)).await
    }
}

/// What an atomic replacement writes.
pub(crate) enum Contents {
    Bytes(Vec<u8>),
    /// A whole file, copied from its start in bounded chunks.
    File(OpenFile),
    /// Bytes read to their end in bounded chunks.
    Reader(Box<dyn io::Read + Send>),
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
    skyhook_base::blocking(move || {
        atomic_write_blocking(&path, contents, &cancellation, sync_directory)
    })
    .await
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
        Contents::File(mut source) => {
            (source.rewind()).map_err(AtomicWriteError::at(AtomicWriteStage::WriteStaging))?;
            copy(source, &mut staged, cancellation)
        }
        Contents::Reader(source) => copy(source, &mut staged, cancellation),
    }?;
    check_cancelled(cancellation).map_err(cancelled)?;
    // No cancellation check after this point: rename may already be visible.
    staged.commit_with(CommitMode::Replace, sync_directory)?;
    Ok(written)
}

/// Copy `source` to its end in bounded chunks, stopping promptly when cancelled.
fn copy(
    mut source: impl io::Read,
    staged: &mut StagedFile,
    cancellation: &CancellationToken,
) -> Result<u64, AtomicWriteError> {
    let failed = AtomicWriteError::at(AtomicWriteStage::WriteStaging);
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

/// Make the entries of `directory` durable. Windows offers no directory sync: NTFS
/// journals a rename's metadata itself, so there this does nothing.
pub fn sync_directory(directory: &Path) -> Result<(), AtomicWriteError> {
    if cfg!(windows) {
        return Ok(());
    }
    // The parent of a destination named in text is text.
    let named = TargetPath::new(directory.to_string_lossy());
    fs::File::open(directory)
        .map_err(AtomicWriteError::at(AtomicWriteStage::OpenDirectory(
            named.clone(),
        )))?
        .sync_all()
        .map_err(AtomicWriteError::at(AtomicWriteStage::SyncDirectory(named)))
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
    use crate::op::IoKind;

    impl StagedWriter {
        /// A writer for `destination` whose bytes go to `file` instead, the other end
        /// of which decides when a write ends and how.
        pub(crate) fn writing_to(file: fs::File, destination: &Path) -> Self {
            Self {
                file: file.into(),
                staged: StagedFile::create(destination, PermissionPolicy::Private).unwrap(),
                taken: Vec::new(),
                writing: None,
            }
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
        assert_eq!(error.error.kind, IoKind::Interrupted);
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
                Err(AtomicWriteError::at(AtomicWriteStage::SyncDirectory(
                    "dir".into(),
                ))(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "sync fixture denied",
                )))
            },
        )
        .unwrap_err();
        assert!(matches!(error.stage, AtomicWriteStage::SyncDirectory(_)));
        assert_eq!(error.error.kind, IoKind::PermissionDenied);
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
        let mut source = crate::fs::open_regular_blocking(&source_path, u64::MAX).unwrap();
        source.seek(io::SeekFrom::Start(5)).unwrap();
        let path = root.path().join("copy");
        let written = atomic_write(&path, Contents::File(source)).await.unwrap();
        assert_eq!(written, bytes.len() as u64);
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

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

    /// A write whose awaiter is gone is still the work of the scope that started it:
    /// once that work has finished, the staging file holds the bytes.
    #[tokio::test]
    async fn a_staged_write_is_its_scopes_work_after_its_awaiter_is_dropped() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("destination");
        let staged = StagedWriter::create(&destination, PermissionPolicy::Private);
        let mut staged = staged.await.unwrap();
        let work = skyhook_base::Work::default();
        work.scope(async {
            let write = staged.write(b"kept");
            tokio::pin!(write);
            let _ = futures_util::poll!(write);
        })
        .await;
        crate::tests::bounded(work.finished()).await;
        assert_eq!(fs::read(staged.path()).unwrap(), b"kept");
    }
}
