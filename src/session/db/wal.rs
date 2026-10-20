//! The write-ahead log's durability and checkpoints, kept off the core's commits. The
//! writer commits without syncing, and the log is synced once for every group of
//! commands; a thread of its own copies the log into the database on its own
//! connection every `CHECKPOINT_COMMITS` commits.

use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
};

use super::{Db, DbResult, OpenMode, SharedDb};

/// Commits after which the next checkpoint runs.
const CHECKPOINT_COMMITS: u32 = 128;

/// Frames a checkpoint may leave uncopied for the log to start over after it.
pub(in crate::session) const CHECKPOINT_FRAMES: u32 = 1000;

/// Frames past which the log is made to start over though commits never pause.
const RESTART_FRAMES: u32 = 4 * CHECKPOINT_FRAMES;

/// A durable session's log file, synced apart from the commits that write it: a sync
/// makes every transaction written to it so far durable, as a commit under
/// `synchronous = FULL` would.
pub(in crate::session) struct Log {
    path: PathBuf,
    /// Held across a sync, so that what it reports is recorded before another begins.
    file: Mutex<LogFile>,
    /// Wakes the checkpointer, while one runs.
    checkpointer: Mutex<Option<mpsc::SyncSender<()>>>,
    /// Commits since the checkpointer last woke.
    commits: AtomicU32,
    /// Where the log stood after the latest checkpoint.
    #[cfg(test)]
    passed: Mutex<Option<Frames>>,
}

enum LogFile {
    /// Opened at the first sync, once the log exists. SQLite removes the log only when
    /// the last connection closes, which the writer is not while it syncs.
    Unopened,
    Open(File),
    /// A sync failed. The kernel reports a failed write-back once per open file, so
    /// what it was to make durable may be lost though a later sync succeeds: every
    /// later sync reports this one.
    Failed(io::ErrorKind),
}

impl Log {
    /// The log of the database at `database`.
    pub(in crate::session) fn of(database: &Path) -> Self {
        Self {
            path: suffixed(database, "-wal"),
            file: Mutex::new(LogFile::Unopened),
            checkpointer: Mutex::new(None),
            commits: AtomicU32::new(0),
            #[cfg(test)]
            passed: Mutex::new(None),
        }
    }

    pub(in crate::session) fn sync(&self) -> io::Result<()> {
        let mut file = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        let synced = match &*file {
            LogFile::Failed(kind) => return Err((*kind).into()),
            LogFile::Open(file) => file.sync_data(),
            LogFile::Unopened => {
                let opened = File::open(&self.path)?;
                let synced = opened.sync_data();
                *file = LogFile::Open(opened);
                synced
            }
        };
        if let Err(error) = &synced {
            *file = LogFile::Failed(error.kind());
        }
        synced
    }

    /// Count a commit that grew the log, waking the checkpointer once enough have.
    pub(super) fn wake(&self) {
        if self.commits.fetch_add(1, Ordering::Relaxed) + 1 >= CHECKPOINT_COMMITS
            && let Some(wake) = &*self.checkpointer()
        {
            let _ = wake.try_send(());
        }
    }

    fn checkpointer(&self) -> MutexGuard<'_, Option<mpsc::SyncSender<()>>> {
        (self.checkpointer.lock()).unwrap_or_else(PoisonError::into_inner)
    }
}

fn suffixed(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// A thread with its own connection that checkpoints the log as commits grow it. Its
/// passive passes never block the writer or a reader: frames a reader's snapshot still
/// needs stay in the log until a pass after the reader ends copies them. The writer
/// starts the log over at a commit that begins once a pass copied it all, which commits
/// that never pause would deny; so once the log passes `RESTART_FRAMES` and a pass left
/// at most `CHECKPOINT_FRAMES` uncopied, as frames committed during it are, the next
/// pass copies those holding the writer's connection. Dropping it ends the thread.
pub(in crate::session) struct Checkpointer {
    log: Arc<Log>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Checkpointer {
    /// Checkpoint `log`, which `writer` writes, into the database at `database`.
    pub(in crate::session) fn start(
        database: &Path,
        log: Arc<Log>,
        writer: SharedDb,
    ) -> DbResult<Self> {
        let db = Db::open(database, OpenMode::Checkpoint)?;
        let checkpoint = move || {
            let frames = |row: &libsql::Row| {
                let (logged, copied) = (row.get(1)?, row.get(2)?);
                Ok(Frames { logged, copied })
            };
            let passive = "PRAGMA wal_checkpoint(PASSIVE)";
            db.query_row(passive, Vec::new(), frames).ok().flatten()
        };
        // One pending wake covers every commit before the thread looks again.
        let (wake, woken) = mpsc::sync_channel(1);
        let passes = log.clone();
        let thread = std::thread::Builder::new()
            .name("session-checkpoint".into())
            .spawn(move || {
                for () in woken {
                    passes.commits.store(0, Ordering::Relaxed);
                    let Some(after) = checkpoint() else { continue };
                    if after.logged >= RESTART_FRAMES && after.uncopied() <= CHECKPOINT_FRAMES {
                        let _writer = writer.lock();
                        checkpoint();
                    }
                    #[cfg(test)]
                    let _ = passes.passed.lock().map(|mut passed| *passed = Some(after));
                }
            })?;
        *log.checkpointer() = Some(wake);
        Ok(Self {
            log,
            thread: Some(thread),
        })
    }

    /// Stop checkpointing, then copy the log into the database and empty it through
    /// `writer`, as the session closes and the writer commits nothing more. It does not
    /// wait for a reader: a reader's snapshot leaves its frames in the log, which the
    /// next open recovers.
    pub(in crate::session) fn close(self, writer: &SharedDb) {
        drop(self);
        let db = writer.lock();
        if db.batch("PRAGMA busy_timeout = 0").is_ok() {
            let truncate = "PRAGMA wal_checkpoint(TRUNCATE)";
            let _ = db.query_row(truncate, Vec::new(), |_| Ok(()));
        }
    }
}

impl Drop for Checkpointer {
    fn drop(&mut self) {
        // The thread's handle on the writer, like any output stream's, keeps the log and
        // so its sender: clearing the sender is what ends the thread.
        drop(self.log.checkpointer().take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Where the log stands after a checkpoint: the frames it holds and how many of them a
/// checkpoint copied.
#[derive(Clone, Copy, Debug)]
pub(in crate::session) struct Frames {
    pub(in crate::session) logged: u32,
    pub(in crate::session) copied: u32,
}

impl Frames {
    pub(in crate::session) fn uncopied(self) -> u32 {
        self.logged.saturating_sub(self.copied)
    }
}

/// Fail every later sync of the log, as a failed write-back does.
#[cfg(test)]
impl SharedDb {
    pub(in crate::session) fn fail_syncs(&self) {
        let log = self.log.as_ref().expect("a durable session's connection");
        *log.file.lock().unwrap() = LogFile::Failed(io::ErrorKind::Other);
    }

    /// Where the log stood after the latest checkpoint, once one ran.
    pub(in crate::session) fn test_passed(&self) -> Option<Frames> {
        *self.log.as_ref()?.passed.lock().unwrap()
    }
}
