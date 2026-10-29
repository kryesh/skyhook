//! Opening regular files, never a live database's, and reading them to a bound.

use std::{
    fs,
    io::{self, Read as _},
    path::Path,
};

use super::{BeneathError, LiveDatabase, RegularFileError};

/// A file opened by an in-process opener other than SQLite, registered with the open
/// databases for as long as it is open: no database opens beneath it, and it is the only
/// way such a file is handed out. Every duplicate shares the registration, so it cannot
/// end while any descriptor of the file is still open here.
#[derive(Debug)]
pub struct OpenFile {
    /// First, so the descriptor closes before the hold ends.
    file: fs::File,
    _hold: Option<std::sync::Arc<crate::db::live::Hold>>,
}

impl OpenFile {
    /// A file without a name, such as an anonymous spool, which no database can be.
    pub(crate) fn unnamed(file: fs::File) -> Self {
        Self { file, _hold: None }
    }

    /// Another descriptor of the file, under the same registration.
    pub(crate) fn duplicate(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            _hold: self._hold.clone(),
        })
    }

    /// The same descriptor for async readers, still under its registration.
    pub(crate) fn into_async(self) -> AsyncFile {
        AsyncFile {
            file: std::sync::Arc::new(self),
            state: AsyncState::Idle {
                buffer: Vec::new(),
                start: 0,
            },
        }
    }
}

impl std::ops::Deref for OpenFile {
    type Target = fs::File;

    fn deref(&self) -> &fs::File {
        &self.file
    }
}

impl io::Read for OpenFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl io::Seek for OpenFile {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}

/// An [`OpenFile`] read on blocking threads. Each read owns the file, registration and
/// all, so one still running when this drops keeps the file registered until its
/// descriptor closes.
pub(crate) struct AsyncFile {
    file: std::sync::Arc<OpenFile>,
    state: AsyncState,
}

enum AsyncState {
    /// What the last read returned that is not yet taken: `buffer[start..]`.
    Idle {
        buffer: Vec<u8>,
        start: usize,
    },
    Reading(tokio::task::JoinHandle<(Vec<u8>, io::Result<usize>)>),
}

/// The most one blocking read takes.
const ASYNC_READ_BYTES: usize = 1 << 21;

impl tokio::io::AsyncRead for AsyncFile {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        out: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        use std::task::Poll;
        let this = self.get_mut();
        loop {
            match &mut this.state {
                AsyncState::Idle { buffer, start } if *start < buffer.len() => {
                    let taken = (buffer.len() - *start).min(out.remaining());
                    out.put_slice(&buffer[*start..*start + taken]);
                    *start += taken;
                    return Poll::Ready(Ok(()));
                }
                AsyncState::Idle { .. } if out.remaining() == 0 => return Poll::Ready(Ok(())),
                AsyncState::Idle { buffer, .. } => {
                    let mut buffer = std::mem::take(buffer);
                    buffer.resize(out.remaining().min(ASYNC_READ_BYTES), 0);
                    let file = this.file.clone();
                    this.state = AsyncState::Reading(tokio::task::spawn_blocking(move || {
                        let read = (&file.file).read(&mut buffer);
                        (buffer, read)
                    }));
                }
                AsyncState::Reading(task) => {
                    let joined = std::task::ready!(std::pin::Pin::new(task).poll(context));
                    let (mut buffer, read) =
                        joined.unwrap_or_else(|error| (Vec::new(), Err(io::Error::other(error))));
                    buffer.truncate(*read.as_ref().unwrap_or(&0));
                    let ended = buffer.is_empty();
                    this.state = AsyncState::Idle { buffer, start: 0 };
                    if read.is_err() || ended {
                        return Poll::Ready(read.map(drop));
                    }
                }
            }
        }
    }
}

/// Open what `probe` names with `open`, refusing a live database's file, and register the
/// handle under the same lock, so no database becomes live between the check and the
/// registration. Only a database renamed onto the path in between could slip past, which
/// SQLite never does. The descriptor's metadata, read to register it, comes back with it.
pub(super) fn open_guarded(
    probe: &crate::db::live::Probe,
    open: impl FnOnce() -> Result<fs::File, BeneathError>,
) -> Result<(OpenFile, fs::Metadata), BeneathError> {
    let mut registry = crate::db::live::lock();
    if registry.holds(probe) {
        return Err(RegularFileError::Live(LiveDatabase).into());
    }
    let file = open()?;
    let metadata = file.metadata().map_err(RegularFileError::Io)?;
    let hold = registry.hold(crate::db::live::identity(&metadata));
    drop(registry);
    let file = OpenFile {
        file,
        _hold: Some(std::sync::Arc::new(hold)),
    };
    Ok((file, metadata))
}

/// Open `path` for reading without blocking on a FIFO or device, and require a
/// regular file of at most `limit` bytes, never a live database's file. The length is an
/// early filter; readers still bound what they consume.
pub(crate) fn open_regular_blocking(path: &Path, limit: u64) -> Result<OpenFile, RegularFileError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
    let probe = crate::db::live::Probe::path(path);
    let (file, metadata) = open_guarded(&probe, || {
        Ok(options.open(path).map_err(RegularFileError::Io)?)
    })
    .map_err(|error| match error {
        BeneathError::File(error) => error,
        BeneathError::Symlink => RegularFileError::NotRegular,
    })?;
    admit_regular(metadata, limit)?;
    Ok(file)
}

/// Whether `path` is one of the open databases' files, as of now.
pub(crate) fn is_live(path: &Path) -> bool {
    crate::db::live::lock().holds(&crate::db::live::Probe::path(path))
}

/// Remove the file or symbolic link `path` unless it is a live database's file. The
/// decision and the removal happen under the registry's lock, so no database opens there
/// meanwhile; a link is checked as itself, never as its target.
pub(crate) fn remove_file_unless_live(path: &Path) -> Result<(), RegularFileError> {
    let registry = crate::db::live::lock();
    let link = fs::symlink_metadata(path)?.file_type().is_symlink();
    if !link && registry.holds(&crate::db::live::Probe::path(path)) {
        return Err(LiveDatabase.into());
    }
    Ok(fs::remove_file(path)?)
}

/// Remove the directory `path` with everything below it unless a live database lives
/// there; nothing is removed when one does. The tree is reserved across the removal, so
/// no database opens inside it meanwhile, while other files open and close beside it.
pub(crate) fn remove_tree_unless_live(path: &Path) -> Result<(), RegularFileError> {
    let tree = fs::canonicalize(path)?;
    let reservation = crate::db::live::lock().reserve(tree).ok_or(LiveDatabase)?;
    fs::remove_dir_all(path)?;
    drop(reservation);
    Ok(())
}

/// Require that a file opened with `O_NONBLOCK`, as `metadata` describes it, is regular
/// and at most `limit` bytes. `O_NONBLOCK` does not affect IO on a regular file, so it
/// stays set.
pub(crate) fn admit_regular(
    metadata: fs::Metadata,
    limit: u64,
) -> Result<fs::Metadata, RegularFileError> {
    if !metadata.is_file() {
        return Err(RegularFileError::NotRegular);
    }
    if metadata.len() > limit {
        return Err(RegularFileError::TooLarge { limit });
    }
    Ok(metadata)
}

/// [`open_regular_blocking`] off the async runtime.
pub(crate) async fn open_regular(path: &Path, limit: u64) -> Result<OpenFile, RegularFileError> {
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
pub(crate) async fn read_to_limit(file: OpenFile, limit: u64) -> Result<Vec<u8>, RegularFileError> {
    tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        (&*file)
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(RegularFileError::TooLarge { limit });
        }
        Ok(bytes)
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{AtomicWriteStage, CommitMode, PermissionPolicy, StagedFile};
    use std::io::Write as _;

    /// A live database's file is neither read nor replaced by the in-process openers.
    #[tokio::test]
    async fn live_databases_are_neither_read_replaced_nor_removed() {
        use crate::db::{Db, OpenMode};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("live.db");
        let db = Db::connect(&path, OpenMode::Create).unwrap();
        db.batch("CREATE TABLE t(x)").unwrap();
        assert!(matches!(
            read_regular(&path, u64::MAX).await,
            Err(RegularFileError::Live(_))
        ));
        let staged = StagedFile::create(&path, PermissionPolicy::Inherit);
        assert!(staged.is_err_and(|error| error.stage == AtomicWriteStage::InspectDestination));
        // A destination that became live while staged is refused at commit.
        let later = root.path().join("later.db");
        let mut staged = StagedFile::create(&later, PermissionPolicy::Inherit).unwrap();
        staged.write(b"replacement").unwrap();
        let opened = Db::connect(&later, OpenMode::Create).unwrap();
        let committed = staged.commit(CommitMode::Replace);
        assert!(committed.is_err_and(|error| error.stage == AtomicWriteStage::Commit));
        // A tree holding a live database is removed not at all.
        let tree = root.path().join("tree");
        fs::create_dir_all(tree.join("session")).unwrap();
        fs::write(tree.join("notes"), b"").unwrap();
        let nested = Db::connect(&tree.join("session/session.db"), OpenMode::Create).unwrap();
        assert!(matches!(
            remove_tree_unless_live(&tree),
            Err(RegularFileError::Live(_))
        ));
        assert!(tree.join("notes").exists() && tree.join("session/session.db").exists());
        drop((db, opened, nested));
        remove_tree_unless_live(&tree).unwrap();
        assert!(!tree.exists());
    }

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
        let fifo = root.path().join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        // Opening a FIFO without a writer would block without O_NONBLOCK.
        let read = crate::tests::bounded(read_regular(&fifo, u64::MAX));
        assert!(matches!(read.await, Err(RegularFileError::NotRegular)));
    }

    /// A read still running as its async handle drops keeps the file registered until
    /// the read returns and its descriptor closes.
    #[tokio::test]
    async fn an_async_read_in_flight_keeps_its_file_registered() {
        use tokio::io::AsyncReadExt as _;
        let root = tempfile::tempdir().unwrap();
        let fifo = root.path().join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        // Holding both ends, a read waits for data and never sees the end.
        let both = || {
            let opened = fs::OpenOptions::new().read(true).write(true).open(&fifo);
            Ok(opened.map_err(RegularFileError::Io)?)
        };
        let (opened, metadata) = open_guarded(&crate::db::live::Probe::path(&fifo), both).unwrap();
        let file = crate::db::live::identity(&metadata);
        let mut reader = opened.into_async();
        let mut buffer = [0; 1];
        let mut read = Box::pin(reader.read(&mut buffer));
        assert!(futures_util::poll!(&mut read).is_pending());
        drop(read);
        drop(reader);
        assert!(crate::db::live::held_after(file, std::time::Duration::ZERO));
        let mut writer = fs::OpenOptions::new().write(true).open(&fifo).unwrap();
        writer.write_all(b"x").unwrap();
        let released =
            move || crate::db::live::held_after(file, std::time::Duration::from_secs(20));
        assert!(!tokio::task::spawn_blocking(released).await.unwrap());
    }
}
