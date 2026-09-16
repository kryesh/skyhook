//! Synchronous access to one session database through libsql's local backend.
//!
//! Local libsql futures complete without suspending, so every operation is
//! driven to completion in place and callers own their threading.

use std::path::Path;

use futures_util::FutureExt;
use libsql::{Builder, Connection, OpenFlags, Row, Value};

mod decode;
mod encode;
mod output;
mod state;

pub(super) use decode::{decode_records, u64_of};
pub(super) use encode::Encoder;
pub(super) use output::output_text;
pub(crate) use output::{CaptureExtent, CaptureRow, Presentation};
pub(crate) use state::InterruptedWork;
pub use state::SessionSummary;
pub(super) use state::{interrupted_work, summary};

pub(super) const APPLICATION_ID: i64 = 0x534B_5948;
pub(super) const USER_VERSION: i64 = 4;
const SCHEMA: &str = include_str!("../schema.sql");
/// Payload tables outside the append-only ledger: blob writes, output upserts and pruning.
const MUTABLE_TABLES: [&str; 6] = [
    "blob",
    "job_output",
    "job_capture",
    "job_capture_chunk",
    "job_output_field",
    "job_presentation",
];

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("session database failed: {0}")]
    Sql(#[from] libsql::Error),
    #[error("session database operation did not complete synchronously")]
    Suspended,
    #[error("session database is not a supported skyhook session (version {0})")]
    Unsupported(i64),
    #[error("invalid session database row: {0}")]
    Corrupt(String),
    #[error("session record rejected: {0}")]
    Rejected(String),
}

pub(super) type DbResult<T> = Result<T, DbError>;

fn ready<T>(future: impl Future<Output = libsql::Result<T>>) -> DbResult<T> {
    future
        .now_or_never()
        .ok_or(DbError::Suspended)?
        .map_err(DbError::from)
}

pub(super) fn corrupt(message: impl Into<String>) -> DbError {
    DbError::Corrupt(message.into())
}

pub(super) fn rejected(message: impl Into<String>) -> DbError {
    DbError::Rejected(message.into())
}

/// Bind a Rust value as an SQLite parameter. Unsigned values saturate at `i64::MAX`.
pub(super) trait Sql {
    fn sql(self) -> Value;
}

macro_rules! sql_integer {
    ($($type:ty),*) => {$(
        impl Sql for $type {
            fn sql(self) -> Value {
                Value::Integer(i64::try_from(self).unwrap_or(i64::MAX))
            }
        }
    )*};
}
sql_integer!(i64, u64, u32, u16, usize);

impl Sql for bool {
    fn sql(self) -> Value {
        Value::Integer(i64::from(self))
    }
}

impl Sql for &str {
    fn sql(self) -> Value {
        Value::Text(self.to_owned())
    }
}

impl Sql for String {
    fn sql(self) -> Value {
        Value::Text(self)
    }
}

impl Sql for &String {
    fn sql(self) -> Value {
        Value::Text(self.clone())
    }
}

impl Sql for &[u8] {
    fn sql(self) -> Value {
        Value::Blob(self.to_vec())
    }
}

impl Sql for Vec<u8> {
    fn sql(self) -> Value {
        Value::Blob(self)
    }
}

impl Sql for Value {
    fn sql(self) -> Value {
        self
    }
}

impl<T: Sql> Sql for Option<T> {
    fn sql(self) -> Value {
        self.map_or(Value::Null, Sql::sql)
    }
}

macro_rules! params {
    ($($value:expr),* $(,)?) => {
        vec![$($crate::session::db::Sql::sql($value)),*]
    };
}
pub(super) use params;

pub(super) enum OpenMode {
    Create,
    Open,
    ReadOnly,
    Memory,
}

pub(super) struct Db {
    conn: Connection,
    _database: libsql::Database,
}

impl Db {
    pub(super) fn open(path: &Path, mode: OpenMode) -> DbResult<Self> {
        let builder = match mode {
            OpenMode::Memory => Builder::new_local(":memory:"),
            OpenMode::ReadOnly => Builder::new_local(path).flags(OpenFlags::SQLITE_OPEN_READ_ONLY),
            OpenMode::Create => Builder::new_local(path)
                .flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE),
            OpenMode::Open => Builder::new_local(path).flags(OpenFlags::SQLITE_OPEN_READ_WRITE),
        };
        let database = ready(builder.build())?;
        let conn = database.connect()?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let db = Self {
            conn,
            _database: database,
        };
        match mode {
            OpenMode::Create | OpenMode::Memory => db.initialize()?,
            OpenMode::Open | OpenMode::ReadOnly => db.check_version()?,
        }
        db.batch(match mode {
            OpenMode::ReadOnly => "PRAGMA foreign_keys = ON; PRAGMA query_only = 1;",
            _ => {
                "PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL; \
                 PRAGMA journal_size_limit = 67108864; PRAGMA temp_store = MEMORY;"
            }
        })?;
        Ok(db)
    }

    fn initialize(&self) -> DbResult<()> {
        // WAL cannot change inside a transaction; it persists in the file.
        self.query_row("PRAGMA journal_mode = WAL", Vec::new(), |_| Ok(()))?;
        self.atomic(|| {
            self.batch(SCHEMA)?;
            let tables = self.query(
                "SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name",
                Vec::new(),
                |row| Ok(row.get::<String>(0)?),
            )?;
            for table in tables {
                if MUTABLE_TABLES.contains(&table.as_str()) {
                    continue;
                }
                self.batch(&format!(
                    "CREATE TRIGGER {table}_append_only_update BEFORE UPDATE ON {table} \
                     BEGIN SELECT RAISE(ABORT, '{table}: append-only'); END; \
                     CREATE TRIGGER {table}_append_only_delete BEFORE DELETE ON {table} \
                     BEGIN SELECT RAISE(ABORT, '{table}: append-only'); END;"
                ))?;
            }
            self.batch(&format!(
                "PRAGMA application_id = {APPLICATION_ID}; PRAGMA user_version = {USER_VERSION};"
            ))
        })
    }

    fn check_version(&self) -> DbResult<()> {
        let pragma = |name| {
            self.query_row(&format!("PRAGMA {name}"), Vec::new(), |row| {
                Ok(row.get::<i64>(0)?)
            })
            .map(Option::unwrap_or_default)
        };
        let application = pragma("application_id")?;
        let version = pragma("user_version")?;
        if application != APPLICATION_ID || version != USER_VERSION {
            return Err(DbError::Unsupported(version));
        }
        Ok(())
    }

    pub(super) fn batch(&self, sql: &str) -> DbResult<()> {
        ready(self.conn.execute_batch(sql)).map(drop)
    }

    pub(super) fn execute(&self, sql: &str, params: Vec<Value>) -> DbResult<u64> {
        ready(self.conn.execute(sql, params))
    }

    /// Execute an INSERT and return the new row's rowid.
    pub(super) fn insert(&self, sql: &str, params: Vec<Value>) -> DbResult<i64> {
        self.execute(sql, params)?;
        Ok(self.conn.last_insert_rowid())
    }

    pub(super) fn query<T>(
        &self,
        sql: &str,
        params: Vec<Value>,
        mut map: impl FnMut(&Row) -> DbResult<T>,
    ) -> DbResult<Vec<T>> {
        let mut rows = ready(self.conn.query(sql, params))?;
        let mut values = Vec::new();
        while let Some(row) = ready(rows.next())? {
            values.push(map(&row)?);
        }
        Ok(values)
    }

    pub(super) fn query_row<T>(
        &self,
        sql: &str,
        params: Vec<Value>,
        map: impl FnOnce(&Row) -> DbResult<T>,
    ) -> DbResult<Option<T>> {
        let mut rows = ready(self.conn.query(sql, params))?;
        ready(rows.next())?.as_ref().map(map).transpose()
    }

    /// Run `work` inside `BEGIN IMMEDIATE`; any error rolls back.
    pub(super) fn transaction<T>(&self, work: impl FnOnce() -> DbResult<T>) -> DbResult<T> {
        self.batch("BEGIN IMMEDIATE")?;
        let result = work();
        if result.is_err() {
            let _ = self.rollback();
        }
        result
    }

    /// Run `work` in one immediate transaction and commit it.
    pub(super) fn atomic<T>(&self, work: impl FnOnce() -> DbResult<T>) -> DbResult<T> {
        let value = self.transaction(work)?;
        self.commit().inspect_err(|_| drop(self.rollback()))?;
        Ok(value)
    }

    pub(super) fn commit(&self) -> DbResult<()> {
        self.batch("COMMIT")
    }

    pub(super) fn rollback(&self) -> DbResult<()> {
        self.batch("ROLLBACK")
    }
}

/// The session's one read-write connection, shared by the ledger writer and job output
/// streams. Ledger transactions hold it from BEGIN through COMMIT; output operations
/// hold it for one statement group, so neither observes the other's open transaction.
#[derive(Clone)]
pub(crate) struct SharedDb(std::sync::Arc<std::sync::Mutex<Db>>);

impl SharedDb {
    pub(super) fn new(db: Db) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(db)))
    }

    /// A holder that panicked may have left a transaction open; roll it back so
    /// later statements cannot join it.
    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, Db> {
        self.0.lock().unwrap_or_else(|poisoned| {
            self.0.clear_poison();
            let db = poisoned.into_inner();
            let _ = db.rollback();
            db
        })
    }
}

#[cfg(test)]
mod tests;
