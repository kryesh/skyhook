//! Where a session's reads run, on the shared read connections.

use std::sync::Arc;

use super::{Db, DbError, SharedDb};
use crate::db::{Own, ReadPool, Turn};

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

impl Reads {
    /// The reads of a session held in memory, on its writer's connection.
    pub(crate) fn writer(db: SharedDb) -> Self {
        Self::Writer(db, Arc::default())
    }

    /// Run `read` against one snapshot of the committed session. It blocks; a pool
    /// reads only inside [`Reads::hold`], on the held connection.
    pub(in crate::session) fn read<T, E: From<DbError>>(
        &self,
        read: impl FnOnce(&Db) -> Result<T, E>,
    ) -> Result<T, E> {
        match self {
            Self::Pool(pool) => pool.read(read),
            Self::Own(own) => own.read(read),
            Self::Writer(db, _) => read(&db.lock()),
        }
    }

    /// Run `work`, which reads through these reads, on one of their reader threads,
    /// with its connection serving all of its reads. Waiting for a reader holds no
    /// thread. `work` must not wait on reads made on another thread, and its panic is
    /// raised again in the caller, as in any blocking task.
    pub(crate) async fn hold<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        match self {
            Self::Pool(pool) => pool.hold(work).await,
            Self::Own(own) => own.hold(work).await,
            Self::Writer(_, turn) => turn.take(work).await,
        }
    }
}
