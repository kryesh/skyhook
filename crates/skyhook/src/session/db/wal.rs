//! The write-ahead log's durability, kept off the core's commits. The writer commits
//! without syncing, and the log is synced once for every group of commands.

use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use super::Db;

/// A durable session's log file, synced apart from the commits that write it: a sync
/// makes every transaction written to it so far durable, as a commit under
/// `synchronous = FULL` would, whichever of the connection's writers committed it.
pub(in crate::session) struct Log {
    path: PathBuf,
    /// Held across a sync, so that what it reports is recorded before another begins.
    file: Mutex<Synced>,
    /// Commits through the connection, each counted once it committed.
    written: AtomicU64,
    #[cfg(test)]
    syncs: AtomicU64,
}

struct Synced {
    file: LogFile,
    /// The commits the latest sync made durable.
    through: u64,
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
            file: Mutex::new(Synced {
                file: LogFile::Unopened,
                through: 0,
            }),
            written: AtomicU64::new(0),
            #[cfg(test)]
            syncs: AtomicU64::new(0),
        }
    }

    /// Make every transaction committed so far durable; with none since the last sync,
    /// there is nothing to do.
    pub(in crate::session) fn sync(&self) -> io::Result<()> {
        let mut synced = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        let written = self.written.load(Ordering::Acquire);
        let result = match &synced.file {
            LogFile::Failed(kind) => return Err((*kind).into()),
            _ if synced.through == written => return Ok(()),
            LogFile::Open(file) => file.sync_data(),
            LogFile::Unopened => {
                let opened = File::open(&self.path)?;
                let result = opened.sync_data();
                synced.file = LogFile::Open(opened);
                result
            }
        };
        match &result {
            Ok(()) => synced.through = written,
            Err(error) => synced.file = LogFile::Failed(error.kind()),
        }
        #[cfg(test)]
        self.syncs.fetch_add(1, Ordering::Relaxed);
        result
    }

    /// Count a commit, which the next sync makes durable. It is counted on the
    /// connection that made it, still held: a close takes the connection before its
    /// last sync, so it never syncs short of a commit it has not counted.
    pub(super) fn committed(&self, _held: &Db) {
        self.written.fetch_add(1, Ordering::Release);
    }
}

fn suffixed(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// Fail every later sync of the log, as a failed write-back does.
#[cfg(test)]
impl super::SharedDb {
    pub(in crate::session) fn fail_syncs(&self) {
        let log = self.log.as_ref().expect("a durable session's connection");
        log.file.lock().unwrap().file = LogFile::Failed(io::ErrorKind::Other);
    }

    /// How many syncs reached the log file.
    pub(in crate::session) fn test_syncs(&self) -> u64 {
        let log = self.log.as_ref().expect("a durable session's connection");
        log.syncs.load(Ordering::Relaxed)
    }
}
