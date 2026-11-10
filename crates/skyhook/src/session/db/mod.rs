//! The session journal's database: its schema, connections and shared writer.

use std::path::Path;

use libsql::{Row, Value};

use crate::{
    db::{Db, DbError, DbResult, Dictionaries, Place, Seeding, Sql, SqlText, params},
    session::{BlobError, MessageSeq, RecordSeq, RequestSeq},
};
use skyhook_media::BlobDigest;

mod agent_state;
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
mod requests;
mod resident;
pub(super) mod stats;
mod summary;
mod wal;

pub(crate) use crate::db::{bytes, corrupt, enum_column, optional_enum_column, rejected};
pub(super) use decode::{Entries, agent_paths, decode_records, u64_of};
pub(super) use encode::Encoder;
pub use encode::agent_row;
pub(crate) use job::JobRow;
pub(in crate::session) use job::agent_job;
pub(super) use job::{creations, job_rows, write_status};
pub(super) use message::Scope;
pub(crate) use output::{
    CaptureBytes, CaptureExtent, CaptureRow, OutputRows, SavedRows, StagedBytes, StagedCapture,
    Stored, TargetFile,
};
pub(super) use output::{keep, remove_staged};
pub(super) use pages::{
    agent_summaries, child_jobs, failed_after, newest_jobs, root_jobs, transcript,
};
pub(crate) use reads::Reads;
pub(super) use requests::requests;
pub(crate) use resident::AgentRecords;
pub(super) use resident::{Opening, agent, opening};
pub use summary::{SessionSummary, SessionTitle};
pub(super) use summary::{counters, host, root_prompt, summary, title};
pub(super) use wal::Log;

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
sql_sequence!(
    RecordSeq,
    RequestSeq,
    MessageSeq,
    skyhook_worker::job::JobId
);

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
    skyhook_tools::target::TargetRef,
    skyhook_base::newtype::Prose,
    crate::config::mode::ModeName,
    crate::job::JobName,
    skyhook_tools::output::FieldPointer,
    crate::provider::model::ProviderName,
    crate::provider::model::ModelName,
    crate::provider::protocol::WireModel,
    crate::provider::protocol::ItemId,
    crate::provider::protocol::BlockId,
    crate::provider::protocol::Scope
);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum OpenMode {
    Open,
    ReadOnly,
    /// Read-only, to a database its store opened: the store's writer checked it.
    Reader,
}

/// The journal's schema, serialized. The identity its header carries, which its DDL
/// sets, is the one every session database must have.
pub(super) const IMAGE: &[u8] = ::schema::image!();

/// A database's identity: its application id and user version.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    application: i64,
    version: i64,
}

impl Identity {
    /// The identity a serialized database's header holds.
    fn of_image(image: &[u8]) -> Self {
        let field = |offset: usize| {
            image
                .get(offset..offset + 4)
                .and_then(|bytes| bytes.try_into().ok())
                .map_or(0, |bytes| i64::from(i32::from_be_bytes(bytes)))
        };
        Self {
            application: field(68),
            version: field(60),
        }
    }

    fn of(db: &Db) -> DbResult<Self> {
        let pragma = |name| {
            db.query_row(format!("PRAGMA {name}"), Vec::new(), |row| {
                Ok(row.get::<i64>(0)?)
            })
            .map(Option::unwrap_or_default)
        };
        Ok(Self {
            application: pragma("application_id")?,
            version: pragma("user_version")?,
        })
    }

    /// Whether this build reads it, the journal image's.
    fn supported(self) -> DbResult<()> {
        match self == Self::of_image(IMAGE) {
            true => Ok(()),
            false => Err(DbError::Unsupported(self.version)),
        }
    }
}

/// A new session database at `place`, copied from `image`, the journal's schema image
/// or one layered on it, once its identity is checked. Its dictionaries are filled in
/// the transaction `tables` fills a product's in.
pub(super) fn create(
    place: Place<'_>,
    image: &'static [u8],
    tables: impl FnOnce(&Db) -> DbResult<()>,
) -> DbResult<Db> {
    Identity::of_image(image).supported()?;
    let db = Db::create(place, image, |db| {
        initialize(db, Seeding::Fill)?;
        tables(db)
    })?;
    db.configure_writer()?;
    Ok(db)
}

/// Open the journal at `path`, checking its identity and, unless `mode` is a
/// [`Reader`](OpenMode::Reader), its dictionaries.
pub(super) fn open(path: &Path, mode: OpenMode) -> DbResult<Db> {
    use crate::db::OpenMode as Shared;
    let shared = match mode {
        OpenMode::Open => Shared::Open,
        OpenMode::ReadOnly | OpenMode::Reader => Shared::ReadOnly,
    };
    let db = Db::connect(path, shared)?;
    Identity::of(&db)?.supported()?;
    if mode != OpenMode::Reader {
        initialize(&db, Seeding::Check)?;
    }
    match mode {
        OpenMode::ReadOnly | OpenMode::Reader => {
            db.batch("PRAGMA foreign_keys = ON; PRAGMA query_only = 1;")?
        }
        OpenMode::Open => db.configure_writer()?,
    }
    Ok(db)
}

/// How the session's readers open its journal, in `mode`.
pub(super) fn opener(mode: OpenMode) -> crate::db::Opener {
    std::sync::Arc::new(move |path| open(path, mode))
}

/// Fill or check the dictionaries.
fn initialize(db: &Db, seeding: Seeding) -> DbResult<()> {
    codes::seed(&Dictionaries(db, seeding))
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
    use std::assert_matches;
    use std::sync::Arc;
    // Schema-level rejection of inconsistent rows. The fixture also drives the
    // encode -> decode round trips in encode.rs.
    use serde_json::json;

    use super::{
        Db, DbError, Encoder, IMAGE, OpenMode, Place, create, decode_records, open, params,
    };
    use crate::identity::{AgentId, SessionId};
    use crate::{
        job::JobTransition,
        provider::protocol::{AssistantItem, ToolCall, ToolResult},
        session::TurnFailure,
        session::{
            EntryKind, EventRecord, Message, ModelContext, ModelPurpose, RecordSeq, SessionEvent,
            UserPart,
            tests::{attempt, child_started, profile, requested, start_events},
        },
    };
    use skyhook_media::{AttachmentRef, BlobRef, ImageFormat, ImageRef};
    use skyhook_tools::target::ExecutionLocation;
    use skyhook_worker::job::JobId;

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
                db: create(Place::Memory, IMAGE, |_| Ok(())).unwrap(),
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
            self.db.atomic(|| self.entry_rows(seq, kind, rows, params))
        }

        /// `file`'s rows, which `check` sees before they are rolled back.
        fn filed<T>(
            &self,
            seq: u64,
            kind: EntryKind,
            rows: &[&'static str],
            check: impl FnOnce() -> T,
        ) -> T {
            let filed = self
                .db
                .transaction(|| self.entry_rows(seq, kind, rows, Vec::new()));
            filed.unwrap();
            let seen = check();
            self.db.rollback().unwrap();
            seen
        }

        fn entry_rows(
            &self,
            seq: u64,
            kind: EntryKind,
            rows: &[&'static str],
            params: Vec<libsql::Value>,
        ) -> Result<(), DbError> {
            self.db.execute(
                "INSERT INTO entry (seq, agent, agent_ordinal, created_millis, kind) \
                 SELECT ?1, a.id, \
                   (SELECT count(*) FROM entry WHERE agent = a.id), 0, ?2 \
                 FROM (SELECT min(id) AS id FROM agent) a",
                params![seq, kind],
            )?;
            for row in rows {
                let params = params![seq].into_iter().chain(params.iter().cloned());
                self.db.execute(*row, params.collect())?;
            }
            Ok(())
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
    }

    /// A session created from the journal's image holds what its DDL creates, and only
    /// an image of the journal's identity creates one.
    #[test]
    fn the_schema_image_holds_what_its_ddl_creates() {
        let root = tempfile::tempdir().unwrap();
        let ddl = Db::connect(&root.path().join("ddl.db"), crate::db::OpenMode::Create).unwrap();
        ddl.atomic(|| {
            ddl.batch(include_str!("../../../../schema/src/session.sql"))?;
            super::initialize(&ddl, crate::db::Seeding::Fill)
        })
        .unwrap();
        let contents = |db: &Db| {
            let rows = |sql: String| {
                db.query(sql, Vec::new(), |row| {
                    (0..row.column_count())
                        .map(|column| Ok(row.get_value(column)?))
                        .collect::<Result<Vec<_>, DbError>>()
                })
                .unwrap()
            };
            let schema = rows(
                "SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema \
                               ORDER BY name"
                    .into(),
            );
            let tables = schema
                .iter()
                .filter(|row| row[0] == libsql::Value::Text("table".into()))
                .map(|row| match &row[1] {
                    libsql::Value::Text(name) => {
                        (name.clone(), rows(format!("SELECT * FROM {name}")))
                    }
                    other => panic!("{other:?} is not a name"),
                })
                .collect::<Vec<_>>();
            let page_size = rows("PRAGMA page_size".into());
            (schema, tables, page_size)
        };
        let path = root.path().join("session.db");
        // Created as SQLite creates one, whatever more the umask would allow.
        let umask = unsafe { libc::umask(0o002) };
        let durable = create(Place::File(&path), IMAGE, |_| Ok(())).unwrap();
        unsafe { libc::umask(umask) };
        let permissions = std::fs::metadata(&path).unwrap().permissions();
        let mode = std::os::unix::fs::PermissionsExt::mode(&permissions) & 0o777;
        assert_eq!(mode, 0o644, "{mode:o}");
        let memory = create(Place::Memory, IMAGE, |_| Ok(())).unwrap();
        assert!(contents(&ddl) == contents(&durable));
        assert!(contents(&ddl) == contents(&memory));
        // An image of another identity creates nothing.
        let foreign = create(Place::Memory, &[0; 100], |_| Ok(()));
        assert!(matches!(foreign, Err(DbError::Unsupported(0))));
    }

    #[test]
    fn an_opened_journal_must_hold_this_builds_dictionaries() {
        // An extra row, a changed flag and an unparsable capability.
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
                "entry_kind",
                "UPDATE entry_kind SET activity = 1 - activity WHERE name = 'agent_interrupted'",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("session.db");
            create(Place::File(&path), IMAGE, |_| Ok(()))
                .unwrap()
                .batch(tamper)
                .unwrap();
            for mode in [OpenMode::Open, OpenMode::ReadOnly] {
                let opened = open(&path, mode);
                assert!(matches!(opened, Err(DbError::Dictionary(name)) if name == table));
            }
        }
    }

    /// A row filed under an entry of another kind never decodes, and the session list
    /// never shows it: a title on a completion, a todo list on an entry that holds none,
    /// capabilities on an entry that applied no mode, a user part in an assistant
    /// message, and a user message under another kind's entry. An agent failure that
    /// retries never decodes either, though the session list reads it as filed.
    #[test]
    fn rows_under_another_kinds_entry_are_corrupt() {
        let user_part = "INSERT INTO user_part (message, position, kind, text) \
                         VALUES (?1 + 1000, 0, 'text', 'stray')";
        let commit = "INSERT INTO message_commit (entry, message) VALUES (?1, ?1 + 1000)";
        let cases: [(EntryKind, &[&str]); 6] = [
            (
                EntryKind::AgentCompleted,
                &["INSERT INTO title (entry, source, text) VALUES (?1, 'user', 'stray')"],
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
                    "INSERT INTO entry_text (entry, text) VALUES (?1, 'stray')",
                    "INSERT INTO message (id, role) VALUES (?1 + 1000, 'user')",
                    user_part,
                    commit,
                ],
            ),
        ];
        // An interrupted turn with no title.
        let mut fixture = Fixture::new();
        let root = fixture.start("/w");
        let context = ModelContext::test(ModelPurpose::Agent, profile());
        let context = fixture.one(root.clone(), SessionEvent::ModelContext { context });
        let request = fixture.one(root.clone(), requested(context)).request();
        fixture.one(root, attempt(request, 1));
        let summary = || super::summary::summary(&fixture.db).unwrap();
        let before = summary();
        assert_eq!(before.stopped, Some(TurnFailure::Interrupted));
        for (kind, rows) in cases {
            let (decoded, after) = fixture.filed(100, kind, rows, || (fixture.decode(), summary()));
            assert_matches!(decoded, Err(DbError::Corrupt(_)), "{rows:?}");
            assert_eq!(after.title, before.title, "{rows:?}");
            if kind != EntryKind::AgentFailed {
                assert_eq!(after.stopped, before.stopped, "{rows:?}");
            }
        }
        fixture.assert_round_trip();
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
            let last = super::summary::summary(&fixture.db).unwrap().last_millis;
            assert_eq!(last, at(activity));
            last
        };
        let before = last_after(vec![message.clone(), interrupted.clone()], 0);
        let after = last_after(vec![interrupted, message], 1);
        assert!(after > before);
    }
}
