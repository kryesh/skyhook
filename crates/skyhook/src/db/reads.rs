//! Reads that never wait on the writer: each runs in one snapshot on a read-only
//! connection, which a write-ahead log lets proceed beside the writer's
//! transactions, fsync included. The database's owner supplies how a reader
//! connection opens.

use std::{
    cell::RefCell,
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    thread::JoinHandle,
};

use super::{Db, DbError, DbResult};

/// How a reader connection opens the database at a path: read-only, with whatever
/// checks and settings its owner gives every reader.
pub(crate) type Opener = Arc<dyn Fn(&Path) -> DbResult<Db> + Send + Sync>;

/// Reader threads a pool starts at most, each with a connection of its own. A read
/// beyond them waits for one rather than opening another: an open parses the whole
/// schema, and more connections reading at once cost more of SQLite's process-wide
/// page-cache and shared-memory locks than they gain.
pub(crate) const CONNECTIONS: usize = 4;

/// A turn on a connection that serves one caller at a time, so that a caller
/// waiting for it holds no thread.
#[derive(Default)]
pub(crate) struct Turn(Arc<tokio::sync::Mutex<()>>);

impl Turn {
    /// Run `work` on a blocking thread once it is this caller's turn. The work holds
    /// the turn until it returns: a caller that stops waiting cannot stop work already
    /// started, so the next turn must not begin beside it.
    pub(crate) async fn take<T, E>(
        &self,
        work: impl FnOnce() -> Result<T, E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<tokio::task::JoinError> + Send + 'static,
    {
        let turn = Arc::clone(&self.0).lock_owned().await;
        crate::blocking(move || {
            let _turn = turn;
            work()
        })
        .await
    }
}

/// One read-only connection, opened when first read, for a reader that must never
/// wait behind other reads.
pub(crate) struct Own {
    path: PathBuf,
    opener: Opener,
    db: Mutex<Option<Db>>,
    turn: Turn,
}

impl Own {
    pub(crate) fn new(path: PathBuf, opener: Opener) -> Self {
        Self {
            path,
            opener,
            db: Mutex::default(),
            turn: Turn::default(),
        }
    }

    /// Run `read` against one snapshot of the committed database. It blocks.
    pub(crate) fn read<T, E: From<DbError>>(
        &self,
        read: impl FnOnce(&Db) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut db = self.db.lock().unwrap_or_else(PoisonError::into_inner);
        snapshot(&mut db, &self.path, &self.opener, read)
    }

    /// Run `work`, which reads through this connection, once it is its turn.
    pub(crate) async fn hold<T, E>(
        &self,
        work: impl FnOnce() -> Result<T, E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<tokio::task::JoinError> + Send + 'static,
    {
        self.turn.take(work).await
    }
}

/// Read-only connections to one database, each owned by a reader thread that runs
/// the work queued for it, started as the work needs them.
pub(crate) struct ReadPool {
    shared: Arc<Shared>,
    readers: Mutex<Vec<JoinHandle<()>>>,
}

struct Shared {
    path: PathBuf,
    opener: Opener,
    queue: Mutex<Queue>,
    queued: Condvar,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    /// Readers waiting for a job.
    idle: usize,
    readers: usize,
    closed: bool,
}

type Job = Box<dyn FnOnce() + Send>;

/// A reader's connection, and the pool it serves.
struct Held {
    pool: *const Shared,
    /// Opened at the first read.
    db: Option<Db>,
}

thread_local! {
    /// The connection of the reader running on this thread.
    static HELD: RefCell<Option<Held>> = const { RefCell::new(None) };
}

impl ReadPool {
    pub(crate) fn new(path: PathBuf, opener: Opener) -> Self {
        Self {
            shared: Arc::new(Shared {
                path,
                opener,
                queue: Mutex::default(),
                queued: Condvar::new(),
            }),
            readers: Mutex::default(),
        }
    }

    /// Whether this runs inside one of this pool's holds, where [`ReadPool::read`]
    /// reads.
    pub(crate) fn holds(&self) -> bool {
        let pool = Arc::as_ptr(&self.shared);
        HELD.with_borrow(|held| held.as_ref().is_some_and(|held| held.pool == pool))
    }

    /// Read in one snapshot on the connection of the reader this runs on: only work
    /// a hold runs reads the pool.
    pub(crate) fn read<T, E: From<DbError>>(
        &self,
        read: impl FnOnce(&Db) -> Result<T, E>,
    ) -> Result<T, E> {
        let shared = &self.shared;
        let pool = Arc::as_ptr(shared);
        // A read never reads again, so the connection stays borrowed for the whole read.
        HELD.with_borrow_mut(|held| {
            let held = held.as_mut().filter(|held| held.pool == pool);
            let held = held.expect("a read pool reads only inside a hold");
            snapshot(&mut held.db, &shared.path, &shared.opener, read)
        })
    }

    /// Run `work`, which reads through this pool, on one of its reader threads, with
    /// its connection serving all of its reads. Waiting for a reader holds no thread.
    /// `work` must not wait on reads made on another thread, and a panic in it is an
    /// error, as in any blocking task.
    pub(crate) async fn hold<T, E>(
        &self,
        work: impl FnOnce() -> Result<T, E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<tokio::task::JoinError> + Send + 'static,
    {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.push(Box::new(move || {
            // A caller that has gone needs nothing read.
            if !reply.is_closed() {
                let _ = reply.send(catch_unwind(AssertUnwindSafe(work)));
            }
        }));
        match result.await.expect("a reader runs every queued job") {
            Ok(done) => done,
            // Only a task makes a join error, so the panic is raised again in one.
            Err(panic) => Err(tokio::task::spawn_blocking(move || resume_unwind(panic))
                .await
                .expect_err("re-raised")
                .into()),
        }
    }

    fn push(&self, job: Job) {
        let mut queue = self.shared.queue();
        queue.jobs.push_back(job);
        let start = queue.jobs.len() > queue.idle && queue.readers < CONNECTIONS;
        if start {
            queue.readers += 1;
            queue.idle += 1;
        }
        drop(queue);
        self.shared.queued.notify_one();
        if start {
            let shared = self.shared.clone();
            let reader = std::thread::Builder::new()
                .name("read-pool".into())
                .spawn(move || shared.serve())
                .expect("start a reader thread");
            self.readers().push(reader);
        }
    }

    fn readers(&self) -> MutexGuard<'_, Vec<JoinHandle<()>>> {
        self.readers.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Shared {
    /// Run queued jobs until the pool closes and nothing is left queued.
    fn serve(&self) {
        HELD.set(Some(Held {
            pool: self,
            db: None,
        }));
        let mut queue = self.queue();
        loop {
            let waiting = |queue: &mut Queue| queue.jobs.is_empty() && !queue.closed;
            queue =
                (self.queued.wait_while(queue, waiting)).unwrap_or_else(PoisonError::into_inner);
            let Some(job) = queue.jobs.pop_front() else {
                break;
            };
            queue.idle -= 1;
            drop(queue);
            job();
            queue = self.queue();
            queue.idle += 1;
        }
        drop(queue);
        drop(HELD.take());
    }

    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for ReadPool {
    fn drop(&mut self) {
        self.shared.queue().closed = true;
        self.shared.queued.notify_all();
        // The last handle can drop on a reader, with the work it ran.
        let current = std::thread::current().id();
        for reader in self.readers().drain(..) {
            if reader.thread().id() != current {
                let _ = reader.join();
            }
        }
    }
}

/// Run `read` in one snapshot on `slot`'s connection, opening it first if there is
/// none. A read that fails or panics inside its snapshot closes the connection, so
/// none is handed on inside an abandoned transaction.
fn snapshot<T, E: From<DbError>>(
    slot: &mut Option<Db>,
    path: &Path,
    opener: &Opener,
    read: impl FnOnce(&Db) -> Result<T, E>,
) -> Result<T, E> {
    struct Unfinished<'a>(&'a mut Option<Db>);
    impl Drop for Unfinished<'_> {
        fn drop(&mut self) {
            if self.0.as_ref().is_some_and(|db| !db.is_autocommit()) {
                *self.0 = None;
            }
        }
    }
    if slot.is_none() {
        *slot = Some(opener(path)?);
    }
    let unfinished = Unfinished(slot);
    let db = unfinished.0.as_ref().expect("opened above");
    db.execute("BEGIN", Vec::new())?;
    let value = read(db);
    db.commit()?;
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db::OpenMode, tests::bounded};

    fn read_only() -> Opener {
        Arc::new(|path| Db::connect(path, OpenMode::ReadOnly))
    }

    /// A caller that stops waiting leaves its started work holding the turn, so the
    /// next caller's work runs after it, never beside it.
    #[tokio::test]
    async fn cancelled_work_keeps_the_turn_until_it_returns() {
        let turn = Arc::new(Turn::default());
        let log = Arc::new(Mutex::new(Vec::new()));
        let (started, running) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let first = {
            let (turn, log) = (turn.clone(), log.clone());
            tokio::spawn(async move {
                turn.take(move || {
                    log.lock().unwrap().push("first started");
                    started.send(()).unwrap();
                    gate.recv().unwrap();
                    log.lock().unwrap().push("first ended");
                    Ok::<_, tokio::task::JoinError>(())
                })
                .await
            })
        };
        bounded(tokio::task::spawn_blocking(move || running.recv().unwrap()))
            .await
            .unwrap();
        first.abort();
        assert!(bounded(first).await.unwrap_err().is_cancelled());
        assert!(
            turn.0.try_lock().is_err(),
            "the started work still holds the turn"
        );
        release.send(()).unwrap();
        let second = log.clone();
        let next = move || {
            second.lock().unwrap().push("second");
            Ok::<_, tokio::task::JoinError>(())
        };
        bounded(turn.take(next)).await.unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            ["first started", "first ended", "second"]
        );
    }

    #[tokio::test]
    async fn a_read_that_fails_inside_its_snapshot_frees_its_connection() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.db");
        Db::connect(&path, OpenMode::Create)
            .unwrap()
            .batch("CREATE TABLE t (x)")
            .unwrap();
        let pool = Arc::new(ReadPool::new(path, read_only()));
        for _ in 0..=CONNECTIONS {
            let held = pool.clone();
            let read = move || {
                let read = || held.read(|_| -> DbResult<()> { panic!("a read that fails") });
                let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(read));
                Ok::<_, tokio::task::JoinError>(failed.is_err())
            };
            assert!(pool.hold(read).await.unwrap());
        }
        let held = pool.clone();
        let read = move || Ok::<_, tokio::task::JoinError>(held.read(|db| db.batch("SELECT 1")));
        pool.hold(read).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_panic_in_held_work_is_an_error() {
        let pool = ReadPool::new("never-read.db".into(), read_only());
        let held = pool.hold(|| -> Result<(), tokio::task::JoinError> { panic!("held work") });
        assert!(held.await.unwrap_err().is_panic());
    }
}
