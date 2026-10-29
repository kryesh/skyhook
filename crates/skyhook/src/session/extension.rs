//! A product's own tables in the sessions it creates, beside the journal's.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use super::{DATABASE_FILE, Reads, SessionError, SharedDb, db};
use crate::{
    blocking,
    db::{Db, DbError, DbResult, Own, Seeding, Turn, params},
    identity::SessionId,
};

/// Tables a product keeps in every session it creates, under a format of their own.
/// They are mutable, unlike the journal's, and only the product's harness opens them:
/// another host's harness opens the session without them.
#[derive(Clone, Copy, Debug)]
pub struct Extension {
    pub sql: &'static str,
    /// Checked on every open by the product; any change to the tables is a new one.
    pub format: i64,
    /// Fills the tables' dictionaries in a new session, and checks them on an open.
    pub seed: fn(&Db, Seeding) -> DbResult<()>,
}

impl Extension {
    pub(super) fn install(&self, db: &Db) -> DbResult<()> {
        db.atomic(|| {
            db.batch("CREATE TABLE extension(format INTEGER NOT NULL) STRICT;")?;
            db.execute(
                "INSERT INTO extension(format) VALUES (?1)",
                params![self.format],
            )?;
            db.batch(self.sql)?;
            (self.seed)(db, Seeding::Fill)
        })
    }

    pub(super) fn check(&self, db: &Db) -> DbResult<()> {
        // A session created without the tables has no `extension` table to read.
        let installed = db
            .query_row(
                "SELECT 1 FROM sqlite_schema WHERE type='table' AND name='extension'",
                Vec::new(),
                |_| Ok(()),
            )?
            .is_some();
        let format = match installed {
            true => db.query_row("SELECT format FROM extension", Vec::new(), |row| {
                Ok(row.get::<i64>(0)?)
            })?,
            false => None,
        };
        match format {
            Some(format) if format == self.format => (self.seed)(db, Seeding::Check),
            found => Err(DbError::Extension(found.unwrap_or_default())),
        }
    }
}

/// Whether an extension writes through its open session, or only reads a session
/// inspected without being opened, on a connection of its own.
#[derive(Clone)]
enum Access {
    Session {
        db: SharedDb,
        turn: Arc<Turn>,
        /// Writes committed so far, counted once each has committed.
        generation: Arc<AtomicU64>,
    },
    Inspecting,
}

/// A write whose COMMIT failed: whether it committed is unknown, so what it would
/// have recorded may already exist.
#[derive(Debug, thiserror::Error)]
#[error("whether the write committed is unknown: {0}")]
pub struct Unconfirmed(pub DbError);

impl From<Unconfirmed> for DbError {
    fn from(error: Unconfirmed) -> Self {
        error.0
    }
}

impl From<Unconfirmed> for SessionError {
    fn from(error: Unconfirmed) -> Self {
        Self::Unconfirmed(Some(error.0))
    }
}

/// A session's extension tables. An open session's writes share the journal's writer,
/// so they commit in order with it and become durable with its next sync, and its
/// reads share the journal's readers.
#[derive(Clone)]
pub struct ExtensionDb {
    session: SessionId,
    directory: PathBuf,
    reads: Reads,
    access: Access,
}

impl ExtensionDb {
    pub(super) fn new(session: SessionId, directory: PathBuf, db: SharedDb, reads: Reads) -> Self {
        Self {
            session,
            directory,
            reads,
            access: Access::Session {
                db,
                turn: Arc::default(),
                generation: Arc::default(),
            },
        }
    }

    /// The extension tables of a session that may be open elsewhere, read-only.
    pub async fn inspect(
        root: &Path,
        id: SessionId,
        extension: Extension,
    ) -> Result<Self, SessionError> {
        let directory = root.join(id.to_string());
        blocking(move || {
            let path = directory.join(DATABASE_FILE);
            if !path.is_file() {
                return Err(SessionError::NotFound(id));
            }
            let own = Own::new(path, db::opener(db::OpenMode::ReadOnly));
            own.read(|db| extension.check(db))?;
            Ok(Self {
                session: id,
                directory,
                reads: Reads::Own(Arc::new(own)),
                access: Access::Inspecting,
            })
        })
        .await
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    /// The session's directory, where the product may keep files of its own in
    /// subdirectories.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Run `read` in one snapshot: on the reader a [`reading`](Self::reading) holds,
    /// otherwise on the writer's connection.
    pub fn read<T, E: From<DbError>>(
        &self,
        read: impl FnOnce(&Db) -> Result<T, E>,
    ) -> Result<T, E> {
        match (&self.reads, &self.access) {
            (Reads::Pool(pool), _) if pool.holds() => pool.read(read),
            (_, Access::Session { db, .. }) => {
                let db = db.lock();
                db.execute("BEGIN", Vec::new())?;
                let result = read(&db);
                match &result {
                    Ok(_) => db.commit()?,
                    Err(_) => drop(db.rollback()),
                }
                result
            }
            (reads, Access::Inspecting) => reads.read(read),
        }
    }

    /// How many writes have committed to the tables through this session since it
    /// opened. On a session this process owns, what is derived from the tables in a
    /// snapshot begun after reading a generation stays current while the generation
    /// does. An inspected session has none: whoever owns it may change its tables
    /// meanwhile.
    pub fn generation(&self) -> Option<u64> {
        match &self.access {
            Access::Session { generation, .. } => Some(generation.load(Ordering::Acquire)),
            Access::Inspecting => None,
        }
    }

    /// Run `write` in one immediate transaction on the session's writer and commit it.
    /// A failed COMMIT is reported as [`Unconfirmed`], apart from work that rolled back.
    pub fn write<T, E: From<DbError> + From<Unconfirmed>>(
        &self,
        write: impl FnOnce(&Db) -> Result<T, E>,
    ) -> Result<T, E> {
        match &self.access {
            Access::Session { db, generation, .. } => {
                let result = db.write(write);
                // Counted after the commit, and for one whose outcome is unknown, so a
                // generation never names tables older than it.
                if !matches!(result, Err(db::WriteError::Refused(_))) {
                    generation.fetch_add(1, Ordering::Release);
                }
                result.map_err(|error| match error {
                    db::WriteError::Refused(error) => error,
                    db::WriteError::Unconfirmed(error) => Unconfirmed(error).into(),
                })
            }
            Access::Inspecting => {
                Err(DbError::Rejected("the session is only inspected".into()).into())
            }
        }
    }

    /// Run `work`, whose reads go to one reader, on that reader's thread. Waiting for
    /// a reader holds no thread.
    pub async fn reading<T, E>(
        &self,
        work: impl FnOnce() -> Result<T, E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<tokio::task::JoinError> + Send + 'static,
    {
        self.reads.hold(work).await
    }

    /// Run `work`, which may write, once it is this caller's turn among the
    /// extension's writers; waiting holds no thread.
    pub async fn writing<T, E>(
        &self,
        work: impl FnOnce() -> Result<T, E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<tokio::task::JoinError> + Send + 'static,
    {
        match &self.access {
            Access::Session { turn, .. } => turn.take(work).await,
            Access::Inspecting => blocking(work).await,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{db::Dictionaries, session::SessionStore};

    pub(crate) fn seed(db: &Db, seeding: Seeding) -> DbResult<()> {
        Dictionaries(db, seeding).names("fixture_kind", &crate::job::JobState::ALL)
    }

    pub(crate) const TABLES: Extension = Extension {
        sql: "CREATE TABLE fixture_kind(name TEXT PRIMARY KEY) STRICT; \
              CREATE TABLE fixture(id INTEGER PRIMARY KEY, note TEXT NOT NULL) STRICT;",
        format: 1,
        seed,
    };

    /// A product's tables open, and their sessions list for the product, only with their
    /// own format, beside a journal that opens without them; an inspection reads them
    /// while the session is held open.
    #[tokio::test]
    async fn extension_tables_open_and_list_with_their_format_or_not_at_all() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path();
        // The host the fixture's journal records.
        let host: crate::identity::HostName = "test".parse().unwrap();
        let store = SessionStore::create(sessions, host.clone(), Some(TABLES))
            .await
            .unwrap();
        let id = store.id();
        // A session that journaled nothing is discarded as it closes.
        let root_agent = crate::identity::AgentId::root(id);
        store
            .append_all(crate::session::tests::start_events(&root_agent, sessions))
            .await
            .unwrap();
        let tables = store.extension().unwrap().clone();
        let write = tables.clone();
        tables
            .writing(move || {
                write.write(|db| {
                    db.execute("INSERT INTO fixture(note) VALUES ('kept')", Vec::new())?;
                    Ok::<_, SessionError>(())
                })
            })
            .await
            .unwrap();
        // A committed write moves the generation; a refused one leaves it.
        assert_eq!(tables.generation(), Some(1));
        let refused = tables.write(|db| {
            db.execute("INSERT INTO fixture(note) VALUES ('dropped')", Vec::new())?;
            Err::<(), _>(SessionError::NotFound(id))
        });
        assert!(refused.is_err());
        assert_eq!(tables.generation(), Some(1));
        let note = |db: &Db| {
            Ok::<_, SessionError>(db.query_row("SELECT note FROM fixture", Vec::new(), |row| {
                Ok(row.get::<String>(0)?)
            })?)
        };
        let inspected = ExtensionDb::inspect(sessions, id, TABLES).await.unwrap();
        assert_eq!(inspected.read(note).unwrap().as_deref(), Some("kept"));
        assert!(inspected.write(|_| Ok::<_, SessionError>(())).is_err());
        store.close().await.unwrap();
        drop(store);

        let changed = Extension {
            format: 2,
            ..TABLES
        };
        assert!(matches!(
            SessionStore::open(sessions, id, Some((&host, changed))).await,
            Err(SessionError::Database(DbError::Extension(1)))
        ));
        let summary = |host, tables| SessionStore::summary(sessions, id, Some((host, tables)));
        assert!(matches!(
            summary(&host, changed).await,
            Err(SessionError::Database(DbError::Extension(1)))
        ));
        assert!(summary(&host, TABLES).await.is_ok());
        // Another host lists the session and its harness opens it without the tables.
        let other = "other".parse().unwrap();
        assert_eq!(summary(&other, changed).await.unwrap().host, host);
        let plain = SessionStore::open(sessions, id, Some((&other, changed)))
            .await
            .unwrap();
        assert!(plain.extension().is_none());
        plain.close().await.unwrap();
        drop(plain);
        let reopened = SessionStore::open(sessions, id, Some((&host, TABLES)))
            .await
            .unwrap();
        let tables = reopened.extension().unwrap().clone();
        let read = tables.clone();
        let kept = tables.reading(move || read.read(note)).await.unwrap();
        assert_eq!(kept.as_deref(), Some("kept"));
        reopened.close().await.unwrap();

        let journal = SessionStore::create(sessions, host.clone(), None)
            .await
            .unwrap();
        let bare = journal.id();
        let root_agent = crate::identity::AgentId::root(bare);
        journal
            .append_all(crate::session::tests::start_events(&root_agent, sessions))
            .await
            .unwrap();
        journal.close().await.unwrap();
        drop(journal);
        assert!(matches!(
            SessionStore::open(sessions, bare, Some((&host, TABLES))).await,
            Err(SessionError::Database(DbError::Extension(0)))
        ));
    }

    /// A write to the tables alone becomes durable with the next drain, and with the
    /// close, as the journal's own writes do.
    #[tokio::test]
    async fn extension_only_writes_are_synced_by_drain_and_close() {
        let root = tempfile::tempdir().unwrap();
        let host = "test".parse().unwrap();
        let store = SessionStore::create(root.path(), host, Some(TABLES))
            .await
            .unwrap();
        let root_agent = crate::identity::AgentId::root(store.id());
        let start = crate::session::tests::start_events(&root_agent, root.path());
        store.append_all(start).await.unwrap();
        let tables = store.extension().unwrap().clone();
        let insert = || {
            tables.write(|db| {
                db.execute("INSERT INTO fixture(note) VALUES ('kept')", Vec::new())?;
                Ok::<_, SessionError>(())
            })
        };
        let syncs = || store.inner.db.test_syncs();
        let before = syncs();
        insert().unwrap();
        store.drain().await.unwrap();
        assert_eq!(syncs(), before + 1);
        store.drain().await.unwrap();
        assert_eq!(syncs(), before + 1, "nothing new to sync");
        insert().unwrap();
        store.close().await.unwrap();
        assert_eq!(syncs(), before + 2);
    }
}
