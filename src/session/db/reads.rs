//! Reads that never wait on the writer: each runs in one snapshot on a read-only
//! connection, which a write-ahead log lets proceed beside the writer's
//! transactions, fsync included.

use std::{
    cell::RefCell,
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    thread::JoinHandle,
};

use super::{Db, DbResult, OpenMode, SharedDb};

/// Reader threads a pool starts at most, each with a connection of its own. A read
/// beyond them waits for one rather than opening another: an open parses the whole
/// schema, and more connections reading at once cost more of SQLite's process-wide
/// page-cache and shared-memory locks than they gain.
pub(in crate::session) const CONNECTIONS: usize = 4;

/// Where a session's reads run: its read pool; a connection of their own, for a
/// reader that must never wait behind other reads; or, for a session held in
/// memory, which no other connection can open, its writer's connection, whose holds
/// take turns.
#[derive(Clone)]
pub(crate) enum Reads {
    Pool(Arc<ReadPool>),
    Own(Arc<Own>),
    Writer(SharedDb, Arc<Turn>),
}

/// A hold's turn on a connection that serves one read at a time, so that a hold
/// waiting for it holds no thread.
#[derive(Default)]
pub(crate) struct Turn(tokio::sync::Mutex<()>);

impl Reads {
    /// The reads of a session held in memory, on its writer's connection.
    pub(crate) fn writer(db: SharedDb) -> Self {
        Self::Writer(db, Arc::default())
    }

    /// Run `read` against one snapshot of the committed session. It blocks; a pool
    /// reads only inside [`Reads::hold`], on the held connection.
    pub(in crate::session) fn read<T>(&self, read: impl FnOnce(&Db) -> DbResult<T>) -> DbResult<T> {
        match self {
            Self::Pool(pool) => pool.read(read),
            Self::Own(own) => snapshot(&mut own.db(), &own.path, own.mode, read),
            Self::Writer(db, _) => read(&db.lock()),
        }
    }

    /// Run `work`, which reads through these reads, on one of their reader threads,
    /// with its connection serving all of its reads. Waiting for a reader holds no
    /// thread. `work` must not wait on reads made on another thread, and a panic in
    /// it is an error, as in any blocking task.
    pub(crate) async fn hold<T, E>(
        &self,
        work: impl FnOnce() -> Result<T, E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<tokio::task::JoinError> + Send + 'static,
    {
        match self {
            // Only a task makes a join error, so the panic is raised again in one.
            Self::Pool(pool) => match pool.hold(work).await {
                Ok(done) => done,
                Err(panic) => Err(tokio::task::spawn_blocking(move || resume_unwind(panic))
                    .await
                    .expect_err("re-raised")
                    .into()),
            },
            Self::Own(own) => own.turn.take(work).await,
            Self::Writer(_, turn) => turn.take(work).await,
        }
    }
}

/// One read-only connection, opened when first read.
pub(crate) struct Own {
    path: PathBuf,
    mode: OpenMode,
    db: Mutex<Option<Db>>,
    turn: Turn,
}

impl Own {
    pub(in crate::session) fn new(path: PathBuf, mode: OpenMode) -> Self {
        Self {
            path,
            mode,
            db: Mutex::default(),
            turn: Turn::default(),
        }
    }

    fn db(&self) -> MutexGuard<'_, Option<Db>> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Turn {
    async fn take<T, E>(&self, work: impl FnOnce() -> Result<T, E> + Send + 'static) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<tokio::task::JoinError> + Send + 'static,
    {
        let _turn = self.0.lock().await;
        crate::blocking(work).await
    }
}

/// Read-only connections to one session database, each owned by a reader thread
/// that runs the work queued for it, started as the work needs them.
pub(crate) struct ReadPool {
    shared: Arc<Shared>,
    readers: Mutex<Vec<JoinHandle<()>>>,
}

struct Shared {
    path: PathBuf,
    mode: OpenMode,
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
    pub(in crate::session) fn new(path: PathBuf, mode: OpenMode) -> Self {
        Self {
            shared: Arc::new(Shared {
                path,
                mode,
                queue: Mutex::default(),
                queued: Condvar::new(),
            }),
            readers: Mutex::default(),
        }
    }

    /// Read on the connection of the reader this runs on: only work a hold runs
    /// reads the pool.
    fn read<T>(&self, read: impl FnOnce(&Db) -> DbResult<T>) -> DbResult<T> {
        let (pool, path) = (Arc::as_ptr(&self.shared), &self.shared.path);
        // A read never reads again, so the connection stays borrowed for the whole read.
        HELD.with_borrow_mut(|held| {
            let held = held.as_mut().filter(|held| held.pool == pool);
            let held = held.expect("a read pool reads only inside a hold");
            snapshot(&mut held.db, path, self.shared.mode, read)
        })
    }

    async fn hold<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> std::thread::Result<T> {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.push(Box::new(move || {
            // A caller that has gone needs nothing read.
            if !reply.is_closed() {
                let _ = reply.send(catch_unwind(AssertUnwindSafe(work)));
            }
        }));
        result.await.expect("a reader runs every queued job")
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
fn snapshot<T>(
    slot: &mut Option<Db>,
    path: &Path,
    mode: OpenMode,
    read: impl FnOnce(&Db) -> DbResult<T>,
) -> DbResult<T> {
    struct Unfinished<'a>(&'a mut Option<Db>);
    impl Drop for Unfinished<'_> {
        fn drop(&mut self) {
            if self.0.as_ref().is_some_and(|db| !db.conn.is_autocommit()) {
                *self.0 = None;
            }
        }
    }
    if slot.is_none() {
        *slot = Some(Db::open(path, mode)?);
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

    #[tokio::test]
    async fn a_read_that_fails_inside_its_snapshot_frees_its_connection() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.db");
        drop(Db::open(&path, OpenMode::Create).unwrap());
        let reads = Reads::Pool(Arc::new(ReadPool::new(path, OpenMode::ReadOnly)));
        for _ in 0..=CONNECTIONS {
            let held = reads.clone();
            let read = move || {
                let read = || held.read(|_| -> DbResult<()> { panic!("a read that fails") });
                let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(read));
                Ok::<_, tokio::task::JoinError>(failed.is_err())
            };
            assert!(reads.hold(read).await.unwrap());
        }
        let held = reads.clone();
        let read = move || Ok::<_, tokio::task::JoinError>(held.read(|db| db.batch("SELECT 1")));
        reads.hold(read).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_panic_in_held_work_is_an_error() {
        let reads = Reads::Pool(Arc::new(ReadPool::new(
            "never-read.db".into(),
            OpenMode::ReadOnly,
        )));
        let held = reads.hold(|| -> Result<(), tokio::task::JoinError> { panic!("held work") });
        assert!(held.await.unwrap_err().is_panic());
    }
}
