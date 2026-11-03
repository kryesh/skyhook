//! Synchronous access to a local SQLite database through libsql, shared by every
//! Skyhook store.
//!
//! Local libsql futures complete without suspending, so every operation is
//! driven to completion in place and callers own their threading.

use std::{cell::RefCell, collections::HashMap, path::Path};

use futures_util::FutureExt;
use libsql::{Builder, Connection, OpenFlags, Row, Statement, Value};

use crate::{media::BlobDigest, named_enum::NamedEnum};

pub(crate) mod live;
#[cfg(test)]
mod memory;
mod reads;
#[cfg(test)]
pub(crate) use reads::CONNECTIONS;
pub(crate) use reads::{Opener, Own, ReadPool, Turn};

const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database failed: {0}")]
    Sql(#[from] libsql::Error),
    #[error("database operation did not complete synchronously")]
    Suspended,
    #[error("database is not a supported Skyhook database (version {0})")]
    Unsupported(i64),
    #[error("session product data is not this build's (format {0})")]
    Extension(i64),
    #[error("database dictionary {0} differs from this build's")]
    Dictionary(&'static str),
    #[error("invalid database row: {0}")]
    Corrupt(String),
    #[error("record rejected: {0}")]
    Rejected(String),
    #[error("database file failed: {0}")]
    Io(#[from] std::io::Error),
}

pub type DbResult<T> = Result<T, DbError>;

/// Drive a local libsql future, which never suspends, to its result.
pub fn ready<T>(future: impl Future<Output = libsql::Result<T>>) -> DbResult<T> {
    future
        .now_or_never()
        .ok_or(DbError::Suspended)?
        .map_err(DbError::from)
}

pub fn corrupt(message: impl Into<String>) -> DbError {
    DbError::Corrupt(message.into())
}

pub fn rejected(message: impl Into<String>) -> DbError {
    DbError::Rejected(message.into())
}

/// Bind a Rust value as an SQLite parameter. Unsigned values saturate at `i64::MAX`.
pub trait Sql {
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
        self.as_str().sql()
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

impl Sql for BlobDigest {
    fn sql(self) -> Value {
        Value::Blob(self.to_bytes().to_vec())
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

impl<T: NamedEnum> Sql for T {
    fn sql(self) -> Value {
        Value::Text(self.as_str().to_owned())
    }
}

fn parse_enum<T: NamedEnum>(text: String) -> DbResult<T> {
    T::parse(&text).ok_or_else(|| {
        corrupt(format!(
            "{text:?} is not a {}",
            std::any::type_name::<T>()
                .rsplit("::")
                .next()
                .unwrap_or_default()
        ))
    })
}

/// Column `index` of `row` as one of `T`'s spellings.
pub fn enum_column<T: NamedEnum>(row: &Row, index: i32) -> DbResult<T> {
    parse_enum(row.get::<String>(index)?)
}

pub fn optional_enum_column<T: NamedEnum>(row: &Row, index: i32) -> DbResult<Option<T>> {
    row.get::<Option<String>>(index)?
        .map(parse_enum)
        .transpose()
}

/// A fixed-width blob column: an identifier or digest.
pub fn bytes<const N: usize>(value: Vec<u8>) -> DbResult<[u8; N]> {
    value
        .try_into()
        .map_err(|_| corrupt(format!("stored value is not {N} bytes")))
}

/// Bind each value with [`Sql`], in order.
#[macro_export]
macro_rules! params {
    ($($value:expr),* $(,)?) => {
        vec![$($crate::db::Sql::sql($value)),*]
    };
}
pub use params;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenMode {
    #[cfg(test)]
    Create,
    Open,
    ReadOnly,
}

/// Where [`Db::create`] puts a new database.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Place<'a> {
    File(&'a Path),
    /// Held in memory for its process; nothing of it survives.
    #[cfg(test)]
    Memory,
}

/// Whether dictionaries are being filled into a new database or checked in an
/// existing one, so that no row decodes under another meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seeding {
    Fill,
    Check,
}

/// Seeds or checks dictionary tables from the enums they spell.
pub struct Dictionaries<'a>(pub &'a Db, pub Seeding);

impl Dictionaries<'_> {
    /// `table` holds exactly `values`, by name.
    pub fn names<T: NamedEnum>(&self, table: &'static str, values: &[T]) -> DbResult<()> {
        self.flagged(table, [], values, |_| [])
    }

    /// `table` holds `values`, each with its `flags`, a column of `columns` each.
    pub fn flagged<T: NamedEnum, const N: usize>(
        &self,
        table: &'static str,
        columns: [&str; N],
        values: &[T],
        flags: impl Fn(T) -> [bool; N],
    ) -> DbResult<()> {
        self.columns(table, columns, values, |value| {
            flags(value).map(|flag| Value::Integer(flag.into()))
        })
    }

    /// `table` holds `values`, each with its position among them as its `rank`, so SQL
    /// orders them as the enum does.
    pub fn ranked<T: NamedEnum>(&self, table: &'static str, values: &[T]) -> DbResult<()> {
        let rank = |value: T| {
            values
                .iter()
                .position(|other| other.as_str() == value.as_str())
        };
        self.columns(table, ["rank"], values, |value| {
            [Value::Integer(
                rank(value).expect("ranks values it lists") as i64
            )]
        })
    }

    /// `table` holds `values`, each with its `cells`, one per column of `columns`.
    pub fn columns<T: NamedEnum, const N: usize>(
        &self,
        table: &'static str,
        columns: [&str; N],
        values: &[T],
        cells: impl Fn(T) -> [Value; N],
    ) -> DbResult<()> {
        let columns: String = columns.map(|column| format!(", {column}")).concat();
        let columns = format!("name{columns}");
        let Self(db, seeding) = *self;
        let mut expected: std::collections::BTreeMap<_, _> = values
            .iter()
            .map(|value| (value.as_str(), cells(*value)))
            .collect();
        if let Seeding::Fill = seeding {
            let slots: String = (2..=N + 1).map(|slot| format!(", ?{slot}")).collect();
            let sql = format!("INSERT INTO {table} ({columns}) VALUES (?1{slots})");
            let statement = db.prepare(sql)?;
            for value in values {
                let mut params = params![*value];
                params.extend(expected[value.as_str()].clone());
                statement.execute(params)?;
            }
        }
        // Each row consumes its expected entry, so a repeat or unknown row fails.
        db.query(
            format!("SELECT {columns} FROM {table}"),
            Vec::new(),
            |row| {
                let Value::Text(name) = row.get_value(0)? else {
                    return Err(DbError::Dictionary(table));
                };
                let Some(cells) = expected.remove(name.as_str()) else {
                    return Err(DbError::Dictionary(table));
                };
                for (column, cell) in cells.into_iter().enumerate() {
                    if row.get_value(column as i32 + 1)? != cell {
                        return Err(DbError::Dictionary(table));
                    }
                }
                Ok(())
            },
        )?;
        if !expected.is_empty() {
            return Err(DbError::Dictionary(table));
        }
        Ok(())
    }
}

pub struct Db {
    /// Statements of static SQL, prepared once and kept reset between uses. They come
    /// first so that they are finalized before the connection closes.
    statements: RefCell<HashMap<Kept, Statement>>,
    conn: Connection,
    _database: libsql::Database,
    /// Last, so the file counts as held until the connection has closed it.
    _held: Option<live::Held>,
}

/// The text of one SQL statement. Static text is prepared once per connection and
/// kept with it; text built at run time is prepared for each use, since what it
/// interpolates can differ every time.
pub enum SqlText {
    Static(&'static str),
    /// Static text prepared for each use: one binding a large payload, as a kept
    /// statement keeps a copy of the values it last bound until it binds again, or
    /// one a connection runs once, as a kept statement holds memory the connection's
    /// later statements would allocate from.
    Once(&'static str),
    Built(String),
}

impl From<&'static str> for SqlText {
    fn from(sql: &'static str) -> Self {
        Self::Static(sql)
    }
}

impl From<String> for SqlText {
    fn from(sql: String) -> Self {
        Self::Built(sql)
    }
}

/// Static SQL by where its text lies, which hashing need not read: the same text at
/// two places is kept twice, never two texts as one.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Kept(usize, usize);

impl Kept {
    fn of(sql: &'static str) -> Self {
        Self(sql.as_ptr() as usize, sql.len())
    }
}

/// A statement out for use, reset when dropped and, if its SQL is static, kept.
struct Prepared<'a> {
    db: &'a Db,
    kept: Option<&'static str>,
    /// Present from prepare until drop.
    statement: Option<Statement>,
}

impl std::ops::Deref for Prepared<'_> {
    type Target = Statement;

    fn deref(&self) -> &Statement {
        self.statement
            .as_ref()
            .expect("a prepared statement is held until dropped")
    }
}

impl Prepared<'_> {
    /// Run it to completion and reset it, so it binds afresh.
    fn execute(&self, params: Vec<Value>) -> DbResult<u64> {
        let changes = ready((**self).execute(params));
        self.reset();
        changes.map(|changes| changes as u64)
    }
}

impl Drop for Prepared<'_> {
    fn drop(&mut self) {
        let statement = self.statement.take();
        if let (Some(sql), Some(statement)) = (self.kept, statement) {
            // A statement stepped but not run to its end holds its snapshot until reset.
            statement.reset();
            self.db
                .statements
                .borrow_mut()
                .insert(Kept::of(sql), statement);
        }
    }
}

/// Turn off SQLite's allocation statistics for the whole process, before its first
/// connection. Counting every allocation takes one global mutex, which serialises
/// connections that otherwise read in parallel; Skyhook reads none of the counts. This
/// also disables SQLite's memory accounting and heap limits for every other SQLite user
/// in the process. SQLite refuses it once initialised, which libsql does as it opens the
/// first database.
fn configure_process() {
    static CONFIGURED: std::sync::Once = std::sync::Once::new();
    CONFIGURED.call_once(|| {
        // SAFETY: every connection opens through `Db::at`, which this blocks until it
        // returns, so nothing else uses SQLite meanwhile.
        unsafe {
            libsql::ffi::sqlite3_config(
                libsql::ffi::SQLITE_CONFIG_MEMSTATUS,
                0 as std::os::raw::c_int,
            );
        }
    });
}

impl Db {
    /// A connection in `mode`, before any schema work.
    pub(crate) fn connect(path: &Path, mode: OpenMode) -> DbResult<Self> {
        // Registered before SQLite opens the file, so it never becomes live under a
        // handle another in-process opener holds.
        let held = live::Held::new(path, BUSY_TIMEOUT)?;
        let flags = match mode {
            OpenMode::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
            #[cfg(test)]
            OpenMode::Create => OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
            OpenMode::Open => OpenFlags::SQLITE_OPEN_READ_WRITE,
        };
        Self::at(path, flags, Some(held))
    }

    fn at(path: &Path, flags: OpenFlags, held: Option<live::Held>) -> DbResult<Self> {
        configure_process();
        let database = ready(Builder::new_local(path).flags(flags).build())?;
        let conn = database.connect()?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        Ok(Self {
            statements: RefCell::default(),
            conn,
            _database: database,
            _held: held,
        })
    }

    /// A new database at `place` holding `image`, a serialized database, on which
    /// `init` then runs in one transaction.
    pub(crate) fn create(
        place: Place<'_>,
        image: &'static [u8],
        init: impl FnOnce(&Self) -> DbResult<()>,
    ) -> DbResult<Self> {
        let db = match place {
            Place::File(path) => {
                let held = live::Held::new(path, BUSY_TIMEOUT)?;
                // The mode SQLite creates a database with; `create_new` alone would ask 0666.
                let mut file = std::os::unix::fs::OpenOptionsExt::mode(
                    std::fs::OpenOptions::new().write(true).create_new(true),
                    0o644,
                )
                .open(path)?;
                std::io::Write::write_all(&mut file, image)?;
                file.sync_all()?;
                drop(file);
                let db = Self::at(path, OpenFlags::SQLITE_OPEN_READ_WRITE, Some(held))?;
                // WAL cannot change inside a transaction; it persists in the file.
                db.query_row("PRAGMA journal_mode = WAL", Vec::new(), |_| Ok(()))?;
                db
            }
            #[cfg(test)]
            Place::Memory => memory::filled(image)?,
        };
        db.atomic(|| init(&db))?;
        Ok(db)
    }

    /// Settings for a connection that writes: foreign keys enforced, temporary tables
    /// in memory, 64 MiB of log kept after a checkpoint, and commits that sync at checkpoints rather than each
    /// commit. A write-ahead database stays consistent across a power loss under that
    /// setting, though its last commits can be lost.
    pub(crate) fn configure_writer(&self) -> DbResult<()> {
        self.batch(concat!(
            "PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL; PRAGMA temp_store = MEMORY; ",
            "PRAGMA journal_size_limit = 67108864;"
        ))
    }

    /// Whether no transaction is open on this connection.
    pub fn is_autocommit(&self) -> bool {
        self.conn.is_autocommit()
    }

    pub fn batch(&self, sql: &str) -> DbResult<()> {
        ready(self.conn.execute_batch(sql)).map(drop)
    }

    /// `sql`, prepared: kept from an earlier use if it is static and none is in use.
    fn prepare(&self, sql: impl Into<SqlText>) -> DbResult<Prepared<'_>> {
        let (kept, statement) = match sql.into() {
            SqlText::Static(sql) => {
                let statement = self.statements.borrow_mut().remove(&Kept::of(sql));
                let statement = match statement {
                    Some(statement) => statement,
                    None => ready(self.conn.prepare(sql))?,
                };
                (Some(sql), statement)
            }
            SqlText::Once(sql) => (None, ready(self.conn.prepare(sql))?),
            SqlText::Built(sql) => (None, ready(self.conn.prepare(&sql))?),
        };
        Ok(Prepared {
            db: self,
            kept,
            statement: Some(statement),
        })
    }

    pub fn execute(&self, sql: impl Into<SqlText>, params: Vec<Value>) -> DbResult<u64> {
        self.prepare(sql)?.execute(params)
    }

    /// Execute an INSERT and return the new row's rowid.
    pub fn insert(&self, sql: impl Into<SqlText>, params: Vec<Value>) -> DbResult<i64> {
        self.execute(sql, params)?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn query<T>(
        &self,
        sql: impl Into<SqlText>,
        params: Vec<Value>,
        map: impl FnMut(&Row) -> DbResult<T>,
    ) -> DbResult<Vec<T>> {
        self.query_as(sql, params, map)
    }

    pub fn query_row<T>(
        &self,
        sql: impl Into<SqlText>,
        params: Vec<Value>,
        map: impl FnOnce(&Row) -> DbResult<T>,
    ) -> DbResult<Option<T>> {
        self.query_row_as(sql, params, map)
    }

    /// [`Db::query`] under the caller's error type, for extensions with their own errors.
    pub fn query_as<T, E: From<DbError>>(
        &self,
        sql: impl Into<SqlText>,
        params: Vec<Value>,
        map: impl FnMut(&Row) -> Result<T, E>,
    ) -> Result<Vec<T>, E> {
        self.query_up_to(sql, params, usize::MAX, map)
    }

    /// The first `limit` rows of `sql`. A page's size is never bound into its SQL as a
    /// `LIMIT` or `OFFSET`: SQLite recompiles a statement whenever such a value changes.
    pub fn query_up_to<T, E: From<DbError>>(
        &self,
        sql: impl Into<SqlText>,
        params: Vec<Value>,
        limit: usize,
        mut map: impl FnMut(&Row) -> Result<T, E>,
    ) -> Result<Vec<T>, E> {
        let statement = self.prepare(sql)?;
        let mut rows = ready(statement.query(params))?;
        let mut values = Vec::new();
        while values.len() < limit
            && let Some(row) = ready(rows.next())?
        {
            values.push(map(&row)?);
        }
        Ok(values)
    }

    /// [`Db::query_row`] under the caller's error type.
    pub fn query_row_as<T, E: From<DbError>>(
        &self,
        sql: impl Into<SqlText>,
        params: Vec<Value>,
        map: impl FnOnce(&Row) -> Result<T, E>,
    ) -> Result<Option<T>, E> {
        let statement = self.prepare(sql)?;
        let mut rows = ready(statement.query(params))?;
        ready(rows.next())?.as_ref().map(map).transpose()
    }

    /// Run `work` inside `BEGIN IMMEDIATE`; any error rolls back.
    pub fn transaction<T, E: From<DbError>>(
        &self,
        work: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        self.execute("BEGIN IMMEDIATE", Vec::new())?;
        let result = work();
        if result.is_err() {
            let _ = self.rollback();
        }
        result
    }

    /// Run `work` in one immediate transaction and commit it.
    pub fn atomic<T>(&self, work: impl FnOnce() -> DbResult<T>) -> DbResult<T> {
        let value = self.transaction(work)?;
        self.commit().inspect_err(|_| drop(self.rollback()))?;
        Ok(value)
    }

    pub fn commit(&self) -> DbResult<()> {
        self.execute("COMMIT", Vec::new()).map(drop)
    }

    pub fn rollback(&self) -> DbResult<()> {
        self.execute("ROLLBACK", Vec::new()).map(drop)
    }
}
