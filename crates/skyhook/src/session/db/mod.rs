//! The session journal's database: its schema, connections and shared writer.

use std::path::Path;

use libsql::{Row, Value};

use crate::{
    db::{Db, DbError, DbResult, Dictionaries, Schema, Seeding, Sql, SqlText, params},
    media::BlobDigest,
    session::{BlobError, MessageSeq, RecordSeq, RequestSeq},
};

mod codes;
mod contract;
mod decode;
mod diagnostic;
mod encode;
pub(super) mod history;
mod job;
mod message;
mod output;
mod pages;
mod reads;
mod request;
mod resident;
mod state;
pub(super) mod stats;
mod wal;

pub(crate) use crate::db::{bytes, corrupt, enum_column, optional_enum_column, rejected};
pub(super) use decode::{Entries, agent_paths, decode_records, u64_of};
pub(super) use encode::Encoder;
pub use encode::agent_row;
pub(crate) use job::JobRow;
pub(in crate::session) use job::agent_job;
pub(super) use job::{creations, job_rows, write_status};
pub(super) use message::Scope;
pub(super) use output::remove_staged;
pub(crate) use output::{CaptureExtent, CaptureRow, OutputRows, SavedRows, StagedCapture};
pub(super) use pages::{
    agent_summaries, child_jobs, failed_after, newest_jobs, root_jobs, transcript,
};
pub(crate) use reads::Reads;
pub(super) use request::requests;
pub(crate) use resident::AgentRecords;
pub(super) use resident::{Opening, agent, created_by, opening};
pub use state::{SessionSummary, SessionTitle};
pub(super) use state::{counters, host, root_prompt, summary, title};
#[cfg(test)]
pub(super) use wal::{CHECKPOINT_FRAMES, Frames};
pub(super) use wal::{Checkpointer, Log};

pub(super) const SCHEMA: Schema = Schema {
    sql: include_str!("../schema.sql"),
    application_id: 0x534B_5948,
    user_version: 29,
};
/// Store `bytes` under `digest`, their digest, once.
pub(super) fn insert_blob(db: &Db, digest: BlobDigest, bytes: &[u8]) -> DbResult<()> {
    db.execute(
        SqlText::Once(
            "INSERT INTO blob (sha256, bytes) VALUES (?1, ?2) ON CONFLICT (sha256) DO NOTHING",
        ),
        params![digest, bytes],
    )
    .map(drop)
}

/// The bytes stored under `digest`, checked against it.
pub(super) fn verified_blob(db: &Db, digest: BlobDigest) -> DbResult<Result<Vec<u8>, BlobError>> {
    let bytes = db.query_row(
        "SELECT bytes FROM blob WHERE sha256 = ?1",
        params![digest],
        |row| Ok(row.get::<Vec<u8>>(0)?),
    )?;
    Ok(match bytes {
        None => Err(BlobError::Missing),
        Some(bytes) if BlobDigest::of(&bytes) != digest => Err(BlobError::HashMismatch),
        Some(bytes) => Ok(bytes),
    })
}

macro_rules! sql_sequence {
    ($($type:ty),*) => {$(
        impl Sql for $type {
            fn sql(self) -> Value {
                self.get().sql()
            }
        }
    )*};
}
sql_sequence!(RecordSeq, RequestSeq, MessageSeq, crate::identity::JobId);

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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum OpenMode {
    Create,
    Open,
    ReadOnly,
    /// Read-only, to a database its store opened: the store's writer checked it.
    Reader,
    Memory,
    /// A connection that only checkpoints the write-ahead log.
    Checkpoint,
}

/// Tables outside the append-only ledger: blobs, captures, which are written before
/// the output that references them is saved with its finish, and the jobs', requests'
/// and agents' current state, which their folds write.
const MUTABLE_TABLES: [&str; 9] = [
    "blob",
    "job_capture",
    "job_capture_chunk",
    "job_capture_staged",
    "job_status",
    "job_reply",
    "request_state",
    "agent_state",
    "agent_context",
];

/// Open the journal at `path`, filling or checking its dictionaries.
pub(super) fn open(path: &Path, mode: OpenMode) -> DbResult<Db> {
    use crate::db::OpenMode as Shared;
    let db = match mode {
        OpenMode::Create => Db::open(path, Shared::Create, &SCHEMA, initialize)?,
        OpenMode::Memory => Db::open(path, Shared::Memory, &SCHEMA, initialize)?,
        OpenMode::Open => Db::open(path, Shared::Open, &SCHEMA, initialize)?,
        OpenMode::ReadOnly => Db::open(path, Shared::ReadOnly, &SCHEMA, initialize)?,
        OpenMode::Reader => {
            let db = Db::connect(path, Shared::ReadOnly)?;
            db.check_version(&SCHEMA)?;
            db
        }
        OpenMode::Checkpoint => {
            let db = Db::connect(path, Shared::Open)?;
            db.check_version(&SCHEMA)?;
            return Ok(db);
        }
    };
    match mode {
        OpenMode::ReadOnly | OpenMode::Reader => {
            db.batch("PRAGMA foreign_keys = ON; PRAGMA query_only = 1;")?
        }
        // The writer does not checkpoint either: the core syncs once per group of
        // commands, and a thread of its own checkpoints.
        _ => {
            db.configure_writer()?;
            db.batch("PRAGMA wal_autocheckpoint = 0;")?
        }
    }
    Ok(db)
}

/// How the session's readers open its journal, in `mode`.
pub(super) fn opener(mode: OpenMode) -> crate::db::Opener {
    std::sync::Arc::new(move |path| open(path, mode))
}

/// Fill or check the dictionaries; a new journal also gets its append-only triggers.
fn initialize(db: &Db, seeding: Seeding) -> DbResult<()> {
    codes::seed(&Dictionaries(db, seeding))?;
    if let Seeding::Check = seeding {
        return Ok(());
    }
    let tables = db.query(
        "SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name",
        Vec::new(),
        |row| Ok(row.get::<String>(0)?),
    )?;
    for table in tables {
        if MUTABLE_TABLES.contains(&table.as_str()) {
            continue;
        }
        db.batch(&format!(
            "CREATE TRIGGER {table}_append_only_update BEFORE UPDATE ON {table} \
             BEGIN SELECT RAISE(ABORT, '{table}: append-only'); END; \
             CREATE TRIGGER {table}_append_only_delete BEFORE DELETE ON {table} \
             BEGIN SELECT RAISE(ABORT, '{table}: append-only'); END;"
        ))?;
    }
    Ok(())
}

/// Freeze the shared connection before relinquishing ownership. SQLite also
/// rejects writes made through queries with RETURNING; retained output handles
/// keep reading but cannot mutate this or a later owner's output generation.
pub(super) fn close_writes(db: &Db) -> DbResult<()> {
    db.batch("PRAGMA query_only = ON")
}

/// The session's one read-write connection, shared by the ledger writer and job output
/// streams, and the log that makes what it commits durable, which a session held in
/// memory has none of. Every write holds it from BEGIN through COMMIT, so none observes
/// another's open transaction.
#[derive(Clone)]
pub(crate) struct SharedDb {
    db: std::sync::Arc<std::sync::Mutex<Db>>,
    log: Option<std::sync::Arc<Log>>,
}

/// Why a write did not commit, or may not have.
pub(super) enum WriteError<E = DbError> {
    /// Its work failed, and its transaction rolled back.
    Refused(E),
    /// Its COMMIT failed, which leaves whether it committed unknown.
    Unconfirmed(DbError),
}

impl From<WriteError> for DbError {
    fn from(error: WriteError) -> Self {
        match error {
            WriteError::Refused(error) | WriteError::Unconfirmed(error) => error,
        }
    }
}

impl SharedDb {
    pub(super) fn new(db: Db, log: Option<std::sync::Arc<Log>>) -> Self {
        Self {
            db: std::sync::Arc::new(std::sync::Mutex::new(db)),
            log,
        }
    }

    /// A holder that panicked may have left a transaction open; roll it back so
    /// later statements cannot join it.
    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, Db> {
        #[cfg(debug_assertions)]
        super::core::assert_connection_first();
        self.db.lock().unwrap_or_else(|poisoned| {
            self.db.clear_poison();
            let db = poisoned.into_inner();
            let _ = db.rollback();
            db
        })
    }

    /// Run `work` in one immediate transaction and commit it. What it commits becomes
    /// durable with the next sync, and the log it grew is checkpointed as it grows.
    pub(super) fn write<T, E: From<DbError>>(
        &self,
        work: impl FnOnce(&Db) -> Result<T, E>,
    ) -> Result<T, WriteError<E>> {
        let failed = |_: &Db, failed| match failed {
            Ok((_, cause)) => WriteError::Unconfirmed(cause),
            Err(error) => WriteError::Refused(error),
        };
        self.write_undoing(work, failed)
    }

    /// [`write`](Self::write) `work`; once a failed transaction rolled back, `undo` runs
    /// on the connection still held, given `work`'s error, or its value with COMMIT's,
    /// and returns the error.
    pub(super) fn write_undoing<T, E: From<DbError>, F>(
        &self,
        work: impl FnOnce(&Db) -> Result<T, E>,
        undo: impl FnOnce(&Db, Result<(T, DbError), E>) -> F,
    ) -> Result<T, F> {
        let db = self.lock();
        let failed = match db.transaction(|| work(&db)) {
            Ok(value) => match db.commit() {
                Ok(()) => {
                    if let Some(log) = &self.log {
                        log.committed(&db);
                    }
                    return Ok(value);
                }
                Err(cause) => {
                    let _ = db.rollback();
                    Ok((value, cause))
                }
            },
            Err(error) => Err(error),
        };
        Err(undo(&db, failed))
    }

    /// Make what every transaction committed through it so far durable. It does
    /// not take the connection.
    pub(super) fn sync(&self) -> std::io::Result<()> {
        self.log.as_ref().map_or(Ok(()), |log| log.sync())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    // Schema-level rejection of inconsistent rows. The fixture also drives the
    // encode -> decode round trips in encode.rs.
    use serde_json::json;

    use super::{
        Db, DbError, Encoder, OpenMode, decode_records, open, params, state::SessionSummary,
    };
    use crate::{
        agent::TurnFailure,
        execution::ExecutionLocation,
        identity::{AgentId, EventId, JobId, SessionId},
        job::JobTransition,
        media::{AttachmentRef, BlobRef, ImageFormat, ImageRef},
        provider::protocol::{AssistantItem, ToolCall, ToolResult},
        session::{
            EntryKind, EventRecord, Message, ModeSelection, ModelContext, ModelPurpose, RecordSeq,
            SessionEvent, UserPart,
            tests::{attempt, child_started, profile, requested, start_events},
        },
        tool::policy::Mode,
    };

    pub(super) struct Fixture {
        pub(super) db: Db,
        encoder: Encoder,
        pub(super) records: Vec<Arc<EventRecord>>,
        session: SessionId,
        /// The folds whose rows each commit writes, as the core's do.
        requests: crate::session::ResidentLedger,
        agents: crate::session::agent_state::AgentStates,
    }

    impl Fixture {
        pub(super) fn new() -> Self {
            Self {
                db: open(std::path::Path::new(""), OpenMode::Memory).unwrap(),
                encoder: Encoder::default(),
                records: Vec::new(),
                session: SessionId::from_bytes([3; 16]),
                requests: Default::default(),
                agents: Default::default(),
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
                .map(|(offset, (agent, event))| {
                    Arc::new(EventRecord {
                        id: EventId::generate().unwrap(),
                        sequence: (first + offset as u64).into(),
                        timestamp_millis: 1_700_000_000_000 + offset as i64,
                        agent,
                        event,
                    })
                })
                .collect();
            let (db, encoder) = (&self.db, &mut self.encoder);
            let (mut requests, mut agents) = (self.requests.clone(), self.agents.clone());
            let result = db.transaction(|| {
                encoder.start(db, &records)?;
                for record in &records {
                    encoder.record(db, record)?;
                    agents.apply(record, &mut requests);
                }
                encoder.write_rows(db, &agents.take_unwritten(), &requests)
            });
            match result {
                Ok(()) => {
                    db.commit().unwrap();
                    (self.requests, self.agents) = (requests, agents);
                }
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

        /// Hand-write the root's entry `seq` of `kind` and the `rows` filed under it, or
        /// none of them. Each row binds `?1` to `seq`, then `params`.
        pub(super) fn file(
            &self,
            seq: u64,
            kind: EntryKind,
            rows: &[&'static str],
            params: Vec<libsql::Value>,
        ) -> Result<(), DbError> {
            self.db.atomic(|| {
                self.db.execute(
                    "INSERT INTO entry (seq, public_id, agent, agent_ordinal, created_millis, kind) \
                     SELECT ?1, randomblob(16), a.id, \
                       (SELECT count(*) FROM entry WHERE agent = a.id), 0, ?2 \
                     FROM (SELECT min(id) AS id FROM agent) a",
                    params![seq, kind],
                )?;
                for row in rows {
                    let params = params![seq].into_iter().chain(params.iter().cloned());
                    self.db.execute(*row, params.collect())?;
                }
                Ok(())
            })
        }

        pub(super) fn decode(&self) -> Result<Vec<Arc<EventRecord>>, DbError> {
            decode_records(&self.db, self.session, super::Entries::All)
        }

        pub(super) fn assert_round_trip(&self) {
            let decoded = self.decode().unwrap();
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
            message: Message::Tool(ToolResult {
                call_id: id.into(),
                name: name.into(),
                result: json!({"ok": true, "nested": [1, null]}),
                images,
                is_error: false,
            }),
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
        // Results need an open committed call.
        fixture.reject(root.clone(), result("missing", "read", Vec::new()));
        one!(SessionEvent::MessageCommitted {
            message: Message::Assistant(vec![call("a", "read")]),
        });
        fixture.reject(root.clone(), result("a", "write", Vec::new()));
        one!(result("a", "read", Vec::new()));
        fixture.reject(root.clone(), result("a", "read", Vec::new()));
        // Unknown jobs.
        let job = JobId::new(1).unwrap();
        fixture.reject(
            root.clone(),
            SessionEvent::JobStateChanged {
                job,
                state: JobTransition::Running,
            },
        );
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
            (root.clone(), result("missing", "read", Vec::new())),
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
        // An extra row, a missing row, a changed flag and an unparsable capability.
        for (table, tamper) in [
            (
                "todo_status",
                "INSERT INTO todo_status (name) VALUES ('abandoned')",
            ),
            (
                "capability",
                "INSERT INTO capability (name) VALUES ('not a name')",
            ),
            (
                "todo_status",
                "DROP TRIGGER todo_status_append_only_delete; \
                 DELETE FROM todo_status WHERE name = 'pending'",
            ),
            (
                "entry_kind",
                "DROP TRIGGER entry_kind_append_only_update; \
                 UPDATE entry_kind SET activity = 1 - activity WHERE name = 'agent_interrupted'",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("session.db");
            open(&path, OpenMode::Create)
                .unwrap()
                .batch(tamper)
                .unwrap();
            for mode in [OpenMode::Open, OpenMode::ReadOnly] {
                let opened = open(&path, mode);
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

    /// A row filed under an entry of another kind never decodes, and the session list
    /// never shows it: a title on a completion, a model failure that ended no attempt,
    /// a todo list on an entry that holds none, capabilities or a mode edition on an
    /// entry that applied no mode, status text beside a mode change, a user part in an
    /// assistant message, a user message, model, start or mode under another kind's
    /// entry, and an attempt or its outcome under another kind's entry. An agent failure
    /// that retries never decodes either, though the session list reads it as filed.
    #[test]
    fn rows_under_another_kinds_entry_are_corrupt() {
        const STRAY_PROFILE: &str = "INSERT INTO model_profile (id, name, provider, model, \
             max_context, max_output, supports_images, state_mode, digest) \
             SELECT 99, 'stray', 'stray', 'm', 10, 1, 0, min(name), randomblob(32) FROM state_mode";
        const RESPONSE: &str = "INSERT INTO model_response (entry, message, outcome) \
             VALUES (?1, (SELECT max(message) FROM message_commit), 'answer')";
        let stray_status = "INSERT INTO entry_text (entry, text) VALUES (?1, 'stray')";
        let user_part = "INSERT INTO user_part (message, position, kind, text) \
                         VALUES (?1 + 1000, 0, 'text', 'stray')";
        let commit = "INSERT INTO message_commit (entry, message) VALUES (?1, ?1 + 1000)";
        let cases: [(EntryKind, &[&str]); 14] = [
            (
                EntryKind::AgentCompleted,
                &["INSERT INTO title (entry, source, text) VALUES (?1, 'user', 'stray')"],
            ),
            (
                EntryKind::ModelFailed,
                &["INSERT INTO failure (entry, failure) VALUES (?1, 'output_limit')"],
            ),
            (
                EntryKind::AgentFailed,
                &[
                    "INSERT INTO failure (entry, failure, detail, provider, retry_millis) \
                   VALUES (?1, 'provider', 'x', (SELECT min(name) FROM provider_error_kind), 5)",
                ],
            ),
            (
                EntryKind::AgentCompleted,
                &["INSERT INTO todo_item (entry, position, text, status) \
                   VALUES (?1, 0, 'stray', 'pending')"],
            ),
            (
                EntryKind::AgentCompleted,
                &["INSERT INTO agent_capability (entry, capability) VALUES (?1, 'read')"],
            ),
            (
                EntryKind::AgentCompleted,
                &["INSERT INTO mode (name, version, entry) VALUES ('stray', 1, ?1)"],
            ),
            (
                EntryKind::ModeChanged,
                &[
                    stray_status,
                    "INSERT INTO agent_mode (entry, mode_name, mode_version) \
                     VALUES (?1, 'look', 1)",
                ],
            ),
            (
                EntryKind::MessageCommitted,
                &[
                    "INSERT INTO message (id, role) VALUES (?1 + 1000, 'assistant')",
                    user_part,
                    commit,
                ],
            ),
            (
                EntryKind::Status,
                &[
                    stray_status,
                    "INSERT INTO message (id, role) VALUES (?1 + 1000, 'user')",
                    user_part,
                    commit,
                ],
            ),
            (
                EntryKind::AgentCompleted,
                &[
                    STRAY_PROFILE,
                    "INSERT INTO model_selection (entry, profile) VALUES (?1, 99)",
                ],
            ),
            (
                EntryKind::ModeChanged,
                &[
                    STRAY_PROFILE,
                    "INSERT INTO agent_start (entry, profile, location_target, location_workspace) \
                     VALUES (?1, 99, (SELECT min(id) FROM target), x'2f')",
                    "INSERT INTO agent_mode (entry, mode_name, mode_version) \
                     VALUES (?1, 'look', 1)",
                ],
            ),
            (
                EntryKind::JobClaimed,
                &[
                    "INSERT INTO mode (name, version, entry) VALUES ('stray', 1, ?1)",
                    "INSERT INTO agent_mode (entry, mode_name, mode_version) \
                     VALUES (?1, 'stray', 1)",
                ],
            ),
            (
                EntryKind::Status,
                &[
                    stray_status,
                    "INSERT INTO attempt_outcome (entry, attempt) \
                     VALUES (?1, (SELECT max(entry) FROM model_attempt))",
                    RESPONSE,
                ],
            ),
            (
                EntryKind::ResponseCompleted,
                &[
                    "INSERT INTO model_attempt (entry, request, attempt) \
                     VALUES (?1, (SELECT max(entry) FROM model_request), 2)",
                    "INSERT INTO attempt_outcome (entry, attempt) VALUES (?1, ?1)",
                    RESPONSE,
                ],
            ),
        ];
        for (kind, rows) in cases {
            // An interrupted turn with a mode and a model, and no title.
            let mut fixture = Fixture::new();
            let root = fixture.start("/w");
            let look = ModeSelection {
                name: "look".parse().unwrap(),
                definition: Some(Mode {
                    capabilities: crate::tool::policy::CapabilitySet::empty()
                        .try_into()
                        .unwrap(),
                    instructions: None,
                    hint: None,
                }),
            };
            fixture.one(
                root.clone(),
                SessionEvent::ModeChanged {
                    mode: look,
                    capabilities: crate::tool::policy::CapabilitySet::empty(),
                },
            );
            let context = ModelContext::test(ModelPurpose::Agent, profile());
            let context = fixture.one(root.clone(), SessionEvent::ModelContext { context });
            let request = fixture.one(root.clone(), requested(context)).request();
            fixture.one(root.clone(), attempt(request, 1));
            let answer = Message::Assistant(vec![AssistantItem::text("answer", 0, "text")]);
            fixture.one(root, SessionEvent::MessageCommitted { message: answer });
            let summary = || super::state::summary(&fixture.db).unwrap();
            let before = summary();
            assert_eq!(before.stopped, Some(TurnFailure::Interrupted));
            fixture.file(100, kind, rows, Vec::new()).unwrap();
            assert!(
                matches!(fixture.decode(), Err(DbError::Corrupt(_))),
                "{rows:?}"
            );
            let after = summary();
            let shown = |summary: &SessionSummary| (summary.title.clone(), summary.mode.clone());
            assert_eq!(shown(&after), shown(&before), "{rows:?}");
            if kind != EntryKind::AgentFailed {
                assert_eq!(after.stopped, before.stopped, "{rows:?}");
            }
        }
    }

    /// Closing a session, which interrupts its agents, leaves its last activity
    /// where it was; a message moves it on.
    #[test]
    fn last_activity_ignores_closing() {
        let mut fixture = Fixture::new();
        let root = fixture.start("/w");
        let text = "prompt".into();
        let message = SessionEvent::MessageCommitted {
            message: Message::User(vec![UserPart::Text { text }]),
        };
        let interrupted = SessionEvent::AgentInterrupted;
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
        let before = last_after(vec![message.clone(), interrupted.clone()], 0);
        let after = last_after(vec![interrupted, message], 1);
        assert!(after > before);
    }
}
