//! Synchronous access to one session database through libsql's local backend.
//!
//! Local libsql futures complete without suspending, so every operation is
//! driven to completion in place and callers own their threading.

use std::path::Path;

use futures_util::FutureExt;
use libsql::{Builder, Connection, OpenFlags, Row, Value};

use crate::{
    agent::{FailureKind, FaultKind},
    named_enum::NamedEnum,
    session::{MessageSeq, RecordSeq, RequestSeq},
};

mod approval;
mod contract;
mod decode;
mod diagnostic;
mod encode;
mod job;
mod message;
mod output;
mod request;
mod state;

pub(super) use decode::{decode_records, u64_of};
pub(super) use encode::Encoder;
pub(crate) use output::{CaptureExtent, CaptureRow};
pub use state::{SessionSummary, SessionTitle};
pub(super) use state::{stopped_turn, summary, title};

pub(super) const APPLICATION_ID: i64 = 0x534B_5948;
pub(super) const USER_VERSION: i64 = 19;
const SCHEMA: &str = include_str!("../schema.sql");
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Bytes of write-ahead log kept after a checkpoint.
const JOURNAL_SIZE_LIMIT: u64 = 64 * 1024 * 1024;
/// Payload tables outside the append-only ledger: blob writes and output upserts.
const MUTABLE_TABLES: [&str; 6] = [
    "blob",
    "job_output",
    "job_capture",
    "job_capture_chunk",
    "job_output_field",
    "job_complete_field",
];

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("session database failed: {0}")]
    Sql(#[from] libsql::Error),
    #[error("session database operation did not complete synchronously")]
    Suspended,
    #[error("session database is not a supported skyhook session (version {0})")]
    Unsupported(i64),
    #[error("session database dictionary {0} differs from this build's")]
    Dictionary(&'static str),
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
pub(crate) trait Sql {
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

macro_rules! sql_sequence {
    ($($type:ty),*) => {$(
        impl Sql for $type {
            fn sql(self) -> Value {
                self.get().sql()
            }
        }
    )*};
}
sql_sequence!(RecordSeq, RequestSeq, MessageSeq);

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

macro_rules! sql_text {
    ($($type:ty),*) => {$(
        impl Sql for &$type {
            fn sql(self) -> Value {
                self.as_str().sql()
            }
        }
    )*};
}
sql_text!(
    String,
    crate::target::TargetRef,
    crate::newtype::Prose,
    crate::tool::policy::ModeName,
    crate::tool::registry::JobName,
    crate::tool::output::FieldPointer,
    crate::provider::profile::ProviderName,
    crate::provider::profile::ModelName,
    crate::provider::protocol::WireModel,
    crate::provider::protocol::ItemId,
    crate::provider::protocol::BlockId,
    crate::provider::protocol::Scope
);

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
pub(super) fn enum_column<T: NamedEnum>(row: &Row, index: i32) -> DbResult<T> {
    parse_enum(row.get::<String>(index)?)
}

pub(super) fn optional_enum_column<T: NamedEnum>(row: &Row, index: i32) -> DbResult<Option<T>> {
    row.get::<Option<String>>(index)?
        .map(parse_enum)
        .transpose()
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
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let db = Self {
            conn,
            _database: database,
        };
        match mode {
            OpenMode::Create | OpenMode::Memory => db.initialize()?,
            OpenMode::Open | OpenMode::ReadOnly => {
                db.check_version()?;
                Dictionaries(&db, Seeding::Check).seed()?;
            }
        }
        db.batch(&match mode {
            OpenMode::ReadOnly => "PRAGMA foreign_keys = ON; PRAGMA query_only = 1;".to_owned(),
            _ => format!(
                "PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL; \
                 PRAGMA journal_size_limit = {JOURNAL_SIZE_LIMIT}; PRAGMA temp_store = MEMORY;"
            ),
        })?;
        Ok(db)
    }

    fn initialize(&self) -> DbResult<()> {
        // WAL cannot change inside a transaction; it persists in the file.
        self.query_row("PRAGMA journal_mode = WAL", Vec::new(), |_| Ok(()))?;
        self.atomic(|| {
            self.batch(SCHEMA)?;
            Dictionaries(self, Seeding::Fill).seed()?;
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
}

/// Whether [`Dictionaries::seed`] fills a new journal's dictionaries or checks that an
/// opened one holds exactly the same rows, so that none decodes under another meaning.
#[derive(Clone, Copy)]
enum Seeding {
    Fill,
    Check,
}

struct Dictionaries<'a>(&'a Db, Seeding);

impl Dictionaries<'_> {
    /// Seed each dictionary with its enum's spellings and flags, and each subset
    /// dictionary with the values its columns may take.
    fn seed(&self) -> DbResult<()> {
        use crate::{
            agent::TodoStatus,
            job::{CaptureKind, JobRole, JobState},
            media::ImageFormat,
            provider::{
                profile::StateMode,
                protocol::{Binding, HistoryLifetime, ItemKind, ReplayFormat},
            },
            session::{EntryKind as E, ModelPurpose, TitleSource, Truncation},
            target::{SshAuth, TargetSource},
            tool::{
                diagnostic::{Effects, IoKind, Operation, PathRole},
                policy::{ApprovalCoverage, Capability, ResourceKind},
            },
        };
        self.names("capability", &Capability::ALL)?;
        self.flagged("entry_kind", ["activity"], &E::ALL, |kind| {
            [kind.is_activity()]
        })?;
        // The entry kinds each subset dictionary admits to the subtype table it narrows.
        for (table, kinds) in [
            (
                "targets_entry_kind",
                &[E::SessionStarted, E::TargetsUpserted][..],
            ),
            ("mode_entry_kind", &[E::AgentStarted, E::ModeChanged]),
            ("todos_entry_kind", &[E::TodosReplaced, E::Compaction]),
            ("failure_entry_kind", &[E::AgentFailed, E::ModelFailed]),
            (
                "attempt_outcome_kind",
                &[
                    E::Compaction,
                    E::ModelFailed,
                    E::ModelAttemptInterrupted,
                    E::ResponseCompleted,
                ],
            ),
            (
                "delivery_kind",
                &[E::JobClaimed, E::JobInjected, E::JobMessageDelivered],
            ),
        ] {
            self.names(table, kinds)?;
        }
        self.names("title_source", &TitleSource::ALL)?;
        self.names("target_source", &TargetSource::ALL)?;
        self.names("ssh_auth", &SshAuth::ALL)?;
        self.names("state_mode", &StateMode::ALL)?;
        self.names("model_purpose", &ModelPurpose::ALL)?;
        self.names("message_role", &message::MessageRole::ALL)?;
        self.names("image_format", &ImageFormat::ALL)?;
        self.flagged(
            "user_part_kind",
            ["text"],
            &message::UserPartKind::ALL,
            |kind| [kind.text()],
        )?;
        self.names("job_event_kind", &message::JobEventKind::ALL)?;
        self.names("item_kind", &ItemKind::ALL)?;
        self.names("block_item_kind", &[ItemKind::Text, ItemKind::Reasoning])?;
        self.names("replay_binding", &Binding::ALL)?;
        self.names("replay_format", &ReplayFormat::ALL)?;
        self.names("todo_status", &TodoStatus::ALL)?;
        self.names("history_lifetime", &HistoryLifetime::ALL)?;
        self.names("response_outcome", &request::ResponseOutcome::ALL)?;
        self.names("cut_reason", &Truncation::ALL)?;
        self.names("job_role", &JobRole::ALL)?;
        self.names("capture_kind", &CaptureKind::ALL)?;
        self.names("capture_detection", &crate::job::output::Detection::ALL)?;
        self.names("diagnostic_slot", &diagnostic::Slot::ALL)?;
        self.names("diagnostic_operation", &Operation::ALL)?;
        self.flagged(
            "diagnostic_subject",
            ["path", "text"],
            &diagnostic::SubjectKind::ALL,
            diagnostic::SubjectKind::stores,
        )?;
        self.names("diagnostic_site", &diagnostic::SiteKind::ALL)?;
        self.names("diagnostic_effects", &Effects::ALL)?;
        self.flagged(
            "diagnostic_cause",
            ["text", "job"],
            &diagnostic::CauseKind::ALL,
            diagnostic::CauseKind::stores,
        )?;
        self.names("diagnostic_io_kind", &IoKind::ALL)?;
        self.names("diagnostic_path_role", &PathRole::ALL)?;
        self.names("approval_coverage", &ApprovalCoverage::ALL)?;
        self.flagged("resource_kind", ["target"], &ResourceKind::ALL, |kind| {
            [approval::targeted(kind)]
        })?;
        self.flagged("job_state", ["terminal"], &JobState::ALL, |state| {
            [state.is_terminal()]
        })?;
        self.flagged("failure_kind", ["detailed"], &FailureKind::ALL, |kind| {
            [kind.detailed()]
        })?;
        self.names(
            "provider_error_kind",
            &crate::provider::ProviderErrorKind::ALL,
        )?;
        self.flagged("compaction_fault", ["detailed"], &FaultKind::ALL, |fault| {
            [fault.detailed()]
        })?;
        self.names("checkpoint_error", &crate::session::CheckpointError::ALL)?;
        Ok(())
    }

    fn names<T: NamedEnum>(&self, table: &'static str, values: &[T]) -> DbResult<()> {
        self.flagged(table, [], values, |_| [])
    }

    /// `table` holds `values`, each with its `flags`, a column of `columns` each.
    fn flagged<T: NamedEnum, const N: usize>(
        &self,
        table: &'static str,
        columns: [&str; N],
        values: &[T],
        flags: impl Fn(T) -> [bool; N],
    ) -> DbResult<()> {
        let slots: String = (2..=N + 1).map(|slot| format!(", ?{slot}")).collect();
        let columns: String = columns.map(|column| format!(", {column}")).concat();
        let (columns, slots) = (format!("(name{columns})"), format!("(?1{slots})"));
        let Self(db, seeding) = *self;
        let sql = match seeding {
            Seeding::Fill => format!("INSERT INTO {table} {columns} VALUES {slots}"),
            Seeding::Check => format!("SELECT count(*) FROM {table} WHERE {columns} = {slots}"),
        };
        let count = |sql: &str, params| {
            let count = db.query_row(sql, params, |row| Ok(row.get::<u64>(0)?))?;
            DbResult::Ok(count.unwrap_or_default())
        };
        let mut held = 0;
        for value in values {
            let mut params = params![*value];
            params.extend(flags(*value).map(Sql::sql));
            held += match seeding {
                Seeding::Fill => db.execute(&sql, params)?,
                Seeding::Check => count(&sql, params)?,
            };
        }
        let rows = count(&format!("SELECT count(*) FROM {table}"), Vec::new())?;
        if held != values.len() as u64 || rows != held {
            return Err(DbError::Dictionary(table));
        }
        Ok(())
    }
}

impl Db {
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

    /// Freeze the shared connection before relinquishing ownership. SQLite also
    /// rejects writes made through queries with RETURNING; retained output handles
    /// keep reading but cannot mutate this or a later owner's output generation.
    pub(super) fn close_writes(&self) -> DbResult<()> {
        self.batch("PRAGMA query_only = ON")
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
mod tests {
    // Schema-level rejection of inconsistent rows. The fixture also drives the
    // encode -> decode round trips in encode.rs.
    use serde_json::json;

    use super::{Db, DbError, Encoder, OpenMode, decode_records};
    use crate::{
        execution::ExecutionLocation,
        identity::{AgentId, EventId, JobId, SessionId},
        job::{JobEnd, JobRole, JobTransition},
        media::{AttachmentRef, BlobRef, ImageFormat, ImageRef},
        provider::protocol::{AssistantItem, ToolCall, ToolResult},
        session::{
            EventRecord, Message, ModelCallOrigin, RecordSeq, SessionEvent, UserPart,
            tests::{child_started, start_events},
        },
    };

    pub(super) struct Fixture {
        pub(super) db: Db,
        encoder: Encoder,
        pub(super) records: Vec<EventRecord>,
        session: SessionId,
    }

    impl Fixture {
        pub(super) fn new() -> Self {
            Self {
                db: Db::open(std::path::Path::new(""), OpenMode::Memory).unwrap(),
                encoder: Encoder::default(),
                records: Vec::new(),
                session: SessionId::from_bytes([3; 16]),
            }
        }

        fn root(&self) -> AgentId {
            AgentId::root(self.session)
        }

        pub(super) fn blob(&self, bytes: &[u8]) -> BlobRef {
            let blob = BlobRef::of(bytes);
            self.db
                .execute(
                    "INSERT INTO blob (sha256, bytes) VALUES (?1, ?2)",
                    params![blob.sha256.to_bytes().to_vec(), bytes.to_vec()],
                )
                .unwrap();
            blob
        }

        /// Commit events as one transaction; returns their sequences.
        pub(super) fn commit(
            &mut self,
            events: Vec<(AgentId, SessionEvent)>,
        ) -> Result<Vec<RecordSeq>, super::DbError> {
            let first = self.records.len() as u64 + 1;
            let records: Vec<_> = events
                .into_iter()
                .enumerate()
                .map(|(offset, (agent, event))| EventRecord {
                    id: EventId::generate().unwrap(),
                    sequence: (first + offset as u64).into(),
                    timestamp_millis: 1_700_000_000_000 + offset as i64,
                    agent,
                    event,
                })
                .collect();
            let (db, encoder) = (&self.db, &mut self.encoder);
            let result = db.transaction(|| encoder.records(db, &records));
            match result {
                Ok(()) => db.commit().unwrap(),
                Err(error) => {
                    encoder.reset();
                    return Err(error);
                }
            }
            let sequences = records.iter().map(|record| record.sequence).collect();
            self.records.extend(records);
            Ok(sequences)
        }

        /// Start the session and its root agent.
        pub(super) fn start(&mut self, workspace: &str) -> AgentId {
            let root = self.root();
            let events = start_events(&root, std::path::Path::new(workspace));
            self.commit(events).unwrap();
            root
        }

        pub(super) fn one(&mut self, agent: AgentId, event: SessionEvent) -> RecordSeq {
            self.commit(vec![(agent, event)]).unwrap()[0]
        }

        pub(super) fn reject(&mut self, agent: AgentId, event: SessionEvent) {
            let before = self.records.len();
            assert!(
                self.commit(vec![(agent, event.clone())]).is_err(),
                "accepted {event:?}"
            );
            assert_eq!(self.records.len(), before);
        }

        pub(super) fn assert_round_trip(&self) {
            let decoded = decode_records(&self.db, self.session).unwrap();
            assert_eq!(decoded.len(), self.records.len());
            for (decoded, expected) in decoded.iter().zip(&self.records) {
                assert_eq!(decoded, expected);
            }
        }
    }

    pub(super) fn user(text: &str) -> Message {
        Message::User(vec![UserPart::Text { text: text.into() }])
    }

    fn call(id: &str, name: &str) -> AssistantItem {
        AssistantItem::tool_call(
            format!("item-{id}"),
            0,
            ToolCall::new(id, name, json!({"path": "file"})).unwrap(),
        )
    }

    pub(super) fn result(id: &str, name: &str, images: Vec<ImageRef>) -> SessionEvent {
        SessionEvent::MessageCommitted {
            message: Message::Tool(vec![ToolResult {
                call_id: id.into(),
                name: name.into(),
                result: json!({"ok": true, "nested": [1, null]}),
                images,
                is_error: false,
            }]),
        }
    }

    #[test]
    fn inconsistent_rows_are_rejected_without_partial_writes() {
        let mut fixture = Fixture::new();
        let root = fixture.root();
        let workspace = ExecutionLocation::root("/workspace".into());
        // Nothing references an agent before it starts.
        fixture.reject(root.clone(), SessionEvent::AgentCompleted);
        fixture.start("/workspace");
        macro_rules! one {
            ($event:expr $(,)?) => {
                fixture.one(root.clone(), $event)
            };
        }
        // A second root agent.
        fixture.reject(root.clone(), child_started(None, workspace));
        // Results need an open committed call; messages carry one result.
        fixture.reject(root.clone(), result("missing", "read", Vec::new()));
        let assistant = one!(SessionEvent::MessageCommitted {
            message: Message::Assistant(vec![call("a", "read")]),
        });
        fixture.reject(root.clone(), result("a", "write", Vec::new()));
        one!(result("a", "read", Vec::new()));
        fixture.reject(root.clone(), result("a", "read", Vec::new()));
        // Unknown jobs and duplicate finishes.
        let job = JobId::new(1).unwrap();
        fixture.reject(
            root.clone(),
            SessionEvent::JobStateChanged {
                job,
                state: JobTransition::Running,
            },
        );
        one!(SessionEvent::JobCreated {
            job,
            parent: None,
            origin: Some(ModelCallOrigin {
                message: assistant.message(),
                call_id: "a".into(),
            }),
            tool: "read".into(),
            role: JobRole::Tool,
            name: None,
            arguments: json!({}),
            output_schema: None,
            accepts_input: false,
            background: false,
            location: ExecutionLocation::root("/workspace".into()),
        });
        let finished = |state| SessionEvent::JobFinished {
            job,
            state,
            diagnostic: None,
            output_diagnostic: None,
            images: Vec::new(),
        };
        one!(finished(JobEnd::Completed));
        fixture.reject(root.clone(), finished(JobEnd::Failed));
        one!(SessionEvent::JobStateChanged {
            job,
            state: JobTransition::Running,
        });
        one!(finished(JobEnd::Failed));
        // Unstored blobs cannot be referenced.
        let image = ImageRef {
            file: None,
            format: ImageFormat::Png,
            blob: BlobRef::of(b"never stored"),
        };
        fixture.reject(
            root.clone(),
            SessionEvent::MessageCommitted {
                message: Message::User(vec![UserPart::Attachment {
                    attachment: AttachmentRef::Image(image),
                }]),
            },
        );
        // A failed batch leaves nothing behind, including rows of its valid events.
        let batch = fixture.commit(vec![
            (
                root.clone(),
                SessionEvent::Status {
                    message: "kept?".into(),
                },
            ),
            (root.clone(), finished(JobEnd::Completed)),
        ]);
        assert!(batch.is_err());
        fixture.assert_round_trip();
        let foreign_keys = fixture
            .db
            .query("PRAGMA foreign_key_check", Vec::new(), |_| Ok(()))
            .unwrap();
        assert!(foreign_keys.is_empty());
    }

    #[test]
    fn an_opened_journal_must_hold_this_builds_dictionaries() {
        for (table, tamper) in [
            (
                "todo_status",
                "INSERT INTO todo_status (name) VALUES ('abandoned')",
            ),
            (
                "job_state",
                "DROP TRIGGER job_state_append_only_update; \
                 UPDATE job_state SET terminal = 1 - terminal WHERE name = 'running'",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("session.db");
            Db::open(&path, OpenMode::Create)
                .unwrap()
                .batch(tamper)
                .unwrap();
            for mode in [OpenMode::Open, OpenMode::ReadOnly] {
                let opened = Db::open(&path, mode);
                assert!(matches!(opened, Err(DbError::Dictionary(name)) if name == table));
            }
        }
    }

    #[test]
    fn ledger_rows_are_append_only() {
        let mut fixture = Fixture::new();
        fixture.start("/w");
        for sql in [
            "UPDATE entry SET created_millis = 0",
            "DELETE FROM agent_capability",
            "UPDATE model_profile SET name = 'other'",
        ] {
            assert!(fixture.db.execute(sql, Vec::new()).is_err(), "{sql}");
        }
        fixture.assert_round_trip();
    }

    /// A model failure is its attempt's outcome; an agent failure has none.
    #[test]
    fn only_agent_failures_stand_without_an_attempt_outcome() {
        let mut fixture = Fixture::new();
        fixture.start("/w");
        for (seq, kind, accepted) in [
            (100_i64, "agent_failed", true),
            (101, "model_failed", false),
        ] {
            let entry = "INSERT INTO entry (seq, public_id, agent, created_millis, kind) \
                         VALUES (?1, randomblob(16), (SELECT min(id) FROM agent), 0, ?2)";
            fixture.db.execute(entry, params![seq, kind]).unwrap();
            let failure = "INSERT INTO failure (entry, kind, failure) VALUES (?1, ?2, 'empty')";
            let inserted = fixture.db.execute(failure, params![seq, kind]);
            assert_eq!(inserted.is_ok(), accepted, "{kind}");
        }
    }

    /// Only a run that restarts a finished job starts a new output generation; a
    /// question answered mid-run, or a cancel after an interrupt, does not.
    #[test]
    fn job_generations_count_restarts_after_a_finish() {
        let mut fixture = Fixture::new();
        let root = fixture.start("/w");
        let job = JobId::new(1).unwrap();
        fixture.one(
            root.clone(),
            SessionEvent::JobCreated {
                job,
                parent: None,
                origin: None,
                tool: "agent".into(),
                role: JobRole::Agent,
                name: None,
                arguments: json!({}),
                output_schema: None,
                accepts_input: true,
                background: false,
                location: ExecutionLocation::root("/w".into()),
            },
        );
        let finished = |state| SessionEvent::JobFinished {
            job,
            state,
            diagnostic: None,
            output_diagnostic: None,
            images: Vec::new(),
        };
        let state = |state| SessionEvent::JobStateChanged { job, state };
        let generation =
            |fixture: &Fixture| super::output::generation(&fixture.db, job.get()).unwrap();
        for (event, expected) in [
            (state(JobTransition::Running), 0),
            (state(JobTransition::WaitingInput), 0),
            (state(JobTransition::Running), 0),
            (finished(JobEnd::Completed), 0),
            (state(JobTransition::Running), 1),
            (state(JobTransition::WaitingInput), 1),
            (state(JobTransition::Running), 1),
            (finished(JobEnd::Interrupted), 1),
            (finished(JobEnd::Cancelled), 1),
            (state(JobTransition::Running), 2),
        ] {
            fixture.one(root.clone(), event);
            assert_eq!(generation(&fixture), expected);
        }
    }

    /// Reopening a session, and closing it, which interrupts its agents, leave its
    /// last activity where it was; a message moves it on.
    #[test]
    fn last_activity_ignores_reopening_and_closing() {
        let mut fixture = Fixture::new();
        let root = fixture.start("/w");
        let text = "prompt".into();
        let message = SessionEvent::MessageCommitted {
            message: Message::User(vec![UserPart::Text { text }]),
        };
        let (reopened, interrupted) = (
            SessionEvent::SessionReopened,
            SessionEvent::AgentInterrupted,
        );
        let mut last_after = |events: Vec<SessionEvent>, activity: usize| {
            let events = events.into_iter().map(|event| (root.clone(), event));
            let sequences = fixture.commit(events.collect()).unwrap();
            // A commit's records are a millisecond apart, in order.
            let at = |index: usize| {
                let sequence = sequences[index];
                let record = fixture.records.iter().find(|r| r.sequence == sequence);
                record.unwrap().timestamp_millis
            };
            let last = super::state::summary(&fixture.db).unwrap().last_millis;
            assert_eq!(last, at(activity));
            last
        };
        let before = last_after(vec![message.clone(), reopened.clone(), interrupted], 0);
        let after = last_after(vec![reopened, message], 1);
        assert!(after > before);
    }
}
