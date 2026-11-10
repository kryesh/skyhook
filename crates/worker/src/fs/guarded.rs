//! Opening regular files, never a live database's, and reading them to a bound.

use std::{
    fs,
    io::{self, Read as _},
    path::Path,
};

use super::LiveDatabase;
use crate::op::FsError;

/// A file opened by an in-process opener other than SQLite, registered with the open
/// databases for as long as it is open: no database opens beneath it, and it is the only
/// way such a file is handed out.
#[derive(Debug)]
pub struct OpenFile {
    /// First, so the descriptor closes before the hold ends.
    file: fs::File,
    _hold: crate::fs::live::Hold,
}

impl OpenFile {
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
                    this.state = AsyncState::Reading(skyhook_base::spawn_blocking(move || {
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

/// Open what `probe` names with `open`, refusing a live database's file. The file the
/// probe reached is checked and held under the registry's lock and opened outside it, so
/// concurrent opens never wait on each other's syscalls, while no database whose file it
/// is becomes live until the descriptor closes. A descriptor that reached another file is
/// checked again under the lock as that file. Only a database renamed onto the path in
/// between could slip past, which SQLite never does. The descriptor's metadata comes back
/// with it.
fn open_guarded(
    probe: &crate::fs::live::Probe,
    open: impl FnOnce() -> io::Result<fs::File>,
) -> Result<(OpenFile, fs::Metadata), FsError> {
    use crate::fs::live::{Probe, Reached, identity, lock};
    let held = {
        let mut registry = lock();
        if registry.holds(probe) {
            return Err(LiveDatabase.into());
        }
        probe.file.as_ref().map(|file| registry.hold(file.identity))
    };
    let file = open()?;
    let hold = match (held, identity(&file)) {
        (Some(hold), Ok(reached)) if hold.file() == reached => hold,
        (stale, reached) => {
            drop(stale);
            let probe = Probe {
                file: (reached.as_ref().ok()).map(|&identity| Reached {
                    identity,
                    aliased: true,
                }),
                directory: probe.directory,
                name: probe.name.clone(),
            };
            let mut registry = lock();
            if registry.holds(&probe) {
                registry.park(&probe, file);
                return Err(LiveDatabase.into());
            }
            match reached {
                Ok(identity) => registry.hold(identity),
                Err(error) => {
                    // Closed under the lock: its file is unknown, so no database may
                    // become live around it first.
                    drop(file);
                    return Err(error.into());
                }
            }
        }
    };
    let file = OpenFile { file, _hold: hold };
    let metadata = file.metadata()?;
    Ok((file, metadata))
}

/// Open `path` for reading without blocking on a FIFO or device, and require a
/// regular file of at most `limit` bytes, never a live database's file. The length is an
/// early filter; readers still bound what they consume. Whatever the file is, it opens and
/// its metadata decides: Windows opens a directory only with backup semantics.
pub fn open_regular_blocking(path: &Path, limit: u64) -> Result<OpenFile, FsError> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::custom_flags(
        &mut options,
        windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS,
    );
    let probe = crate::fs::live::Probe::path(path);
    let (file, metadata) = open_guarded(&probe, || options.open(path))?;
    admit_regular(metadata, limit)?;
    Ok(file)
}

/// Whether `path` is one of the open databases' files, as of now.
pub(crate) fn is_live(path: &Path) -> bool {
    crate::fs::live::lock().holds(&crate::fs::live::Probe::path(path))
}

/// Remove the file or symbolic link `path` unless it is a live database's file. The
/// decision and the removal happen under the registry's lock, so no database opens there
/// meanwhile; a link is checked as itself, never as its target.
pub(crate) fn remove_file_unless_live(path: &Path) -> Result<(), FsError> {
    let registry = crate::fs::live::lock();
    let kind = fs::symlink_metadata(path)?.file_type();
    if !kind.is_symlink() && registry.holds(&crate::fs::live::Probe::path(path)) {
        return Err(LiveDatabase.into());
    }
    // Windows removes a link to a directory, symbolic or a junction, as a directory.
    #[cfg(windows)]
    if std::os::windows::fs::FileTypeExt::is_symlink_dir(&kind) {
        return Ok(fs::remove_dir(path)?);
    }
    Ok(fs::remove_file(path)?)
}

/// Remove the directory `path` with everything below it unless a live database lives
/// there; nothing is removed when one does. The tree is reserved across the removal, so
/// no database opens inside it meanwhile, while other files open and close beside it.
pub(crate) fn remove_tree_unless_live(path: &Path) -> Result<(), FsError> {
    let tree = fs::canonicalize(path)?;
    let reservation = crate::fs::live::lock().reserve(tree).ok_or(LiveDatabase)?;
    fs::remove_dir_all(path)?;
    drop(reservation);
    Ok(())
}

/// Require that a file opened with `O_NONBLOCK`, as `metadata` describes it, is regular
/// and at most `limit` bytes. `O_NONBLOCK` does not affect IO on a regular file, so it
/// stays set.
pub fn admit_regular(metadata: fs::Metadata, limit: u64) -> Result<fs::Metadata, FsError> {
    if !metadata.is_file() {
        return Err(FsError::NotRegular);
    }
    if metadata.len() > limit {
        return Err(FsError::TooLarge { limit });
    }
    Ok(metadata)
}

/// [`open_regular_blocking`] off the async runtime.
pub async fn open_regular(path: &Path, limit: u64) -> Result<OpenFile, FsError> {
    let path = path.to_owned();
    skyhook_base::blocking(move || open_regular_blocking(&path, limit)).await
}

/// Read a whole regular file of at most `limit` bytes, bounded while reading so
/// a file that grows after its length check still cannot exceed the limit.
pub async fn read_regular(path: &Path, limit: u64) -> Result<Vec<u8>, FsError> {
    read_to_limit(open_regular(path, limit).await?, limit).await
}

/// The rest of a file admitted by [`open_regular`], under the same bound.
pub async fn read_to_limit(file: OpenFile, limit: u64) -> Result<Vec<u8>, FsError> {
    skyhook_base::blocking(move || {
        let mut bytes = Vec::new();
        (&*file)
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(FsError::TooLarge { limit });
        }
        Ok(bytes)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{AtomicWriteStage, CommitMode, PermissionPolicy, StagedFile};
    use std::assert_matches;

    /// A live database's file is neither read nor replaced by the in-process openers.
    #[tokio::test]
    async fn live_databases_are_neither_read_replaced_nor_removed() {
        use crate::fs::live::Held;
        let connect = |path: &Path| Held::new(path, std::time::Duration::ZERO).unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("live.db");
        fs::write(&path, b"").unwrap();
        let db = connect(&path);
        assert_matches!(read_regular(&path, u64::MAX).await, Err(FsError::Live(_)));
        let staged = StagedFile::create(&path, PermissionPolicy::Inherit);
        assert!(staged.is_err_and(|error| error.stage == AtomicWriteStage::InspectDestination));
        // A destination that became live while staged is refused at commit.
        let later = root.path().join("later.db");
        let mut staged = StagedFile::create(&later, PermissionPolicy::Inherit).unwrap();
        staged.write(b"replacement").unwrap();
        let opened = connect(&later);
        let committed = staged.commit(CommitMode::Replace);
        assert!(committed.is_err_and(|error| error.stage == AtomicWriteStage::Commit));
        // A tree holding a live database is removed not at all.
        let tree = root.path().join("tree");
        fs::create_dir_all(tree.join("session")).unwrap();
        fs::write(tree.join("notes"), b"").unwrap();
        fs::write(tree.join("session/session.db"), b"").unwrap();
        let nested = connect(&tree.join("session/session.db"));
        assert_matches!(remove_tree_unless_live(&tree), Err(FsError::Live(_)));
        assert!(tree.join("notes").exists() && tree.join("session/session.db").exists());
        drop((db, opened, nested));
        remove_tree_unless_live(&tree).unwrap();
        assert!(!tree.exists());
    }

    /// A name that reaches a database opened after it was checked is refused. A descriptor
    /// of that database's file stays open until the database closes, since closing it
    /// would release SQLite's locks; one of another file, the name having moved on, is
    /// closed at once rather than tied to that database. Opening outside the registry's
    /// lock is what lets the database open in between.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_open_that_reaches_a_database_opened_meanwhile_is_parked_on_its_owner() {
        use crate::fs::live::Held;
        let root = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(root.path()).unwrap();
        let descriptors = |path: &Path| {
            let open = fs::read_dir("/proc/self/fd").unwrap().flatten();
            open.filter(|fd| fs::read_link(fd.path()).is_ok_and(|target| target == path))
                .count()
        };
        let other = root.join("other");
        fs::write(&other, b"").unwrap();
        for (name, reached) in [("own.db", None), ("moved.db", Some(&other))] {
            let path = root.join(name);
            fs::write(&path, b"").unwrap();
            let mut db = None;
            let opened = open_guarded(&crate::fs::live::Probe::path(&path), || {
                fs::remove_file(&path).unwrap();
                fs::write(&path, b"").unwrap();
                let opened = Held::new(&path, std::time::Duration::ZERO).unwrap();
                db = Some((opened, descriptors(&path)));
                fs::File::open(reached.unwrap_or(&path))
            });
            assert_matches!(opened, Err(FsError::Live(_)));
            let (db, by_sqlite) = db.unwrap();
            let parked = usize::from(reached.is_none());
            assert_eq!(descriptors(&path), by_sqlite + parked, "{name}");
            assert_eq!(descriptors(&other), 0, "{name}");
            drop(db);
            assert_eq!(descriptors(&path), 0, "{name}");
        }
    }

    #[tokio::test]
    async fn regular_reads_are_bounded_and_refuse_other_files_without_blocking() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(read_regular(&path, 3).await.unwrap(), b"abc");
        assert_matches!(
            read_regular(&path, 2).await,
            Err(FsError::TooLarge { limit: 2 })
        );
        assert_matches!(
            read_regular(root.path(), u64::MAX).await,
            Err(FsError::NotRegular)
        );
        let fifo = root.path().join("fifo");
        crate::tests::fifo(&fifo);
        // Opening a FIFO without a writer would block without O_NONBLOCK.
        let read = crate::tests::bounded(read_regular(&fifo, u64::MAX));
        assert_matches!(read.await, Err(FsError::NotRegular));
    }

    /// A read still running as its async handle drops keeps the file registered until
    /// the read returns and its descriptor closes.
    #[tokio::test]
    async fn an_async_read_in_flight_keeps_its_file_registered() {
        use std::io::Write as _;
        use tokio::io::AsyncReadExt as _;
        let root = tempfile::tempdir().unwrap();
        let fifo = root.path().join("fifo");
        crate::tests::fifo(&fifo);
        // Holding both ends, a read waits for data and never sees the end.
        let both = || fs::OpenOptions::new().read(true).write(true).open(&fifo);
        let (opened, _) = open_guarded(&crate::fs::live::Probe::path(&fifo), both).unwrap();
        let file = crate::fs::live::identity(&opened).unwrap();
        let mut reader = opened.into_async();
        let mut buffer = [0; 1];
        let mut read = Box::pin(reader.read(&mut buffer));
        assert!(futures_util::poll!(&mut read).is_pending());
        drop(read);
        drop(reader);
        assert!(crate::fs::live::tests::held_after(
            file,
            std::time::Duration::ZERO
        ));
        let mut writer = fs::OpenOptions::new().write(true).open(&fifo).unwrap();
        writer.write_all(b"x").unwrap();
        let released =
            move || crate::fs::live::tests::held_after(file, std::time::Duration::from_secs(20));
        assert!(!tokio::task::spawn_blocking(released).await.unwrap());
    }
}
