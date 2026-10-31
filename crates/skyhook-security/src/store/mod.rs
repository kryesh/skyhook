//! A security session's research as one knowledge graph its agents edit, beside its journal:
//! nodes of seven kinds, the edges between them, and one audit log of every write with the
//! nodes it bore on. What each phase owes is derived from it, never stored.
//!
//! Calls hold one read snapshot or an immediate write transaction on the session's
//! connections. Hosts call from a blocking worker: local libsql completes its futures
//! inline, including lock waits.

use std::fmt;

use chrono::{DateTime, Utc};
use libsql::Row;
use skyhook::{
    db::{Db, DbError, corrupt, params},
    identity::AgentId,
    session::ExtensionDb,
};

mod artifacts;
mod bearing;
mod changes;
mod events;
mod evidence;
mod findings;
mod links;
mod nodes;
mod queues;
mod reads;
mod review;
mod runs;
mod schedule;
mod synthesis;
mod tasks;
mod testing;
mod types;

pub use artifacts::{ArtifactFault, Digesting, OpenArtifact, Stopped, admit};
pub use changes::*;
pub use events::*;
pub use evidence::*;
pub use findings::*;
pub use links::*;
pub use nodes::*;
pub use queues::{QueueEntry, SpentWork};
pub use reads::*;
pub use review::*;
pub use runs::*;
pub use schedule::*;
pub use synthesis::*;
pub use tasks::*;
pub use testing::*;
pub use types::*;

pub type Result<T> = std::result::Result<T, Error>;

/// The most records one call names.
const MAX_ATTACHMENTS: usize = 1000;

/// A record a lookup can miss: a node, of a kind when the id was typed with one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    Node(Option<NodeKind>),
    Task,
    Test,
    Environment,
    Run,
}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Node(None) => "node",
            Self::Node(Some(kind)) => kind.as_str(),
            Self::Task => "task",
            Self::Test => "test",
            Self::Environment => "environment",
            Self::Run => "run",
        })
    }
}

/// A bound a request exceeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    Attachments,
}

impl fmt::Display for Bound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Attachments => write!(f, "{MAX_ATTACHMENTS} records per call"),
        }
    }
}

/// The names of fields, as a request spells them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fields(pub Vec<&'static str>);

impl fmt::Display for Fields {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join(", "))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("research database: {0}")]
    Db(#[from] DbError),
    #[error("research database: {0}")]
    Unconfirmed(#[from] skyhook::session::Unconfirmed),
    #[error("research files: {0}")]
    Io(#[from] std::io::Error),
    #[error("ids must be positive")]
    NonPositiveId,
    #[error("span requires 1 <= start_line <= end_line")]
    Span,
    #[error("{0}")]
    ArtifactReference(#[from] InvalidArtifactReference),
    #[error("{0}")]
    Text(#[from] TextError),
    #[error("{0}")]
    Inadmissible(#[from] Inadmissible),
    #[error("exceeds {0}")]
    Limit(Bound),
    #[error("no {record}{}", id.map_or(String::new(), |id| format!(" with id {id}")))]
    NotFound { record: Record, id: Option<i64> },
    #[error("node {id} is a {found}, not a {expected}")]
    WrongKind {
        id: NodeId,
        expected: NodeKind,
        found: NodeKind,
    },
    #[error("a new {kind} needs {fields}")]
    Missing { kind: NodeKind, fields: Fields },
    #[error("a candidate chains at least one flaw")]
    EmptyChain,
    #[error("a node is not merged into itself")]
    SelfMerge,
    #[error("work is begun by the agent job that does it")]
    TaskNeedsJob,
    #[error("a task begins with at least one subject")]
    NoSubjects,
}

impl From<libsql::Error> for Error {
    fn from(error: libsql::Error) -> Self {
        Self::Db(error.into())
    }
}

/// The research of one session, on its connections.
#[derive(Clone)]
pub struct Store(ExtensionDb);

fn now() -> i64 {
    Utc::now().timestamp_millis()
}

fn time(millis: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(millis)
        .ok_or_else(|| corrupt(format!("timestamp {millis} out of range")).into())
}

/// `values` as one JSON array parameter, which SQL reads with `json_each`.
fn json_list<T: serde::Serialize>(values: &[T]) -> Result<String> {
    serde_json::to_string(values).map_err(|error| corrupt(error.to_string()).into())
}

/// The row a query always yields: an aggregate, or a key the transaction just wrote.
pub(super) fn stored<T>(
    db: &Db,
    sql: impl Into<skyhook::db::SqlText>,
    params: Vec<libsql::Value>,
    map: impl FnOnce(&Row) -> Result<T>,
) -> Result<T> {
    optional(db, sql, params, map)?.ok_or_else(|| corrupt("a stored row is missing").into())
}

fn optional<T>(
    db: &Db,
    sql: impl Into<skyhook::db::SqlText>,
    params: Vec<libsql::Value>,
    map: impl FnOnce(&Row) -> Result<T>,
) -> Result<Option<T>> {
    db.query_row_as(sql, params, map)
}

fn rows<T>(
    db: &Db,
    sql: impl Into<skyhook::db::SqlText>,
    params: Vec<libsql::Value>,
    map: impl FnMut(&Row) -> Result<T>,
) -> Result<Vec<T>> {
    db.query_as(sql, params, map)
}

fn exists(
    db: &Db,
    sql: impl Into<skyhook::db::SqlText>,
    params: Vec<libsql::Value>,
) -> Result<bool> {
    Ok(optional(db, sql, params, |_| Ok(()))?.is_some())
}

fn count(db: &Db, sql: impl Into<skyhook::db::SqlText>, params: Vec<libsql::Value>) -> Result<u64> {
    stored(db, sql, params, |row| Ok(row.get(0)?))
}

/// The journal's row for `actor`, which records an agent before it acts.
fn agent_row(db: &Db, actor: &AgentId) -> Result<i64> {
    skyhook::session::agent_row(db, actor)?
        .ok_or_else(|| corrupt(format!("agent {actor} is not in the journal")).into())
}

/// The live node `id` names, with its kind: a node merged away resolves to the one it was
/// merged into.
fn live_node(db: &Db, id: NodeId) -> Result<(NodeId, NodeKind)> {
    optional(
        db,
        "SELECT coalesce(merged_into,id),kind FROM node WHERE id=?1",
        params![id],
        |row| Ok((NodeId(row.get(0)?), skyhook::db::enum_column(row, 1)?)),
    )?
    .ok_or(Error::NotFound {
        record: Record::Node(None),
        id: Some(id.0),
    })
}

/// The live node of `I`'s kind `id` names, as [`live_node`] resolves it.
fn live<I: KindId>(db: &Db, id: I) -> Result<I> {
    let (node, found) = live_node(db, id.into()).map_err(|_| Error::NotFound {
        record: Record::Node(Some(I::KIND)),
        id: Some(id.into()),
    })?;
    if found != I::KIND {
        return Err(Error::WrongKind {
            id: id.into(),
            expected: I::KIND,
            found,
        });
    }
    Ok(I::of(node))
}

impl Store {
    /// Run `work` on one of the session's reader threads: every store read it makes
    /// runs on that reader's read-only connection, in a snapshot that sees every write
    /// committed before it began. Waiting for a reader holds no thread.
    pub async fn reading<T, E>(
        &self,
        work: impl FnOnce() -> std::result::Result<T, E> + Send + 'static,
    ) -> std::result::Result<T, E>
    where
        T: Send + 'static,
        E: From<tokio::task::JoinError> + Send + 'static,
    {
        self.0.reading(work).await
    }

    /// Run `work`, which may write, once it is this caller's turn among the research
    /// writers, so that waiting holds no thread.
    pub async fn writing<T, E>(
        &self,
        work: impl FnOnce() -> std::result::Result<T, E> + Send + 'static,
    ) -> std::result::Result<T, E>
    where
        T: Send + 'static,
        E: From<tokio::task::JoinError> + Send + 'static,
    {
        self.0.writing(work).await
    }

    /// One read snapshot for every query `f` runs: on the reader inside
    /// [`Store::reading`], otherwise on the writer's connection.
    pub(super) fn read<T>(&self, f: impl FnOnce(&Db) -> Result<T>) -> Result<T> {
        self.0.read(f)
    }

    /// Writes hold the session's write lock from their first statement to commit.
    fn write<T>(&self, f: impl FnOnce(&Db) -> Result<T>) -> Result<T> {
        self.0.write(f)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use skyhook::{
        agent::{HarnessBuilder, SessionHandle},
        identity::AgentId,
        job::JobState,
        provider::{
            Provider, ProviderContext, ProviderError, ResponseStream,
            protocol::{ContextId, ModelRequest},
        },
    };

    use super::*;

    /// A provider for sessions whose scripts must never reach a model.
    pub(crate) struct NoRequests;
    impl Provider for NoRequests {
        fn open_context(
            &self,
            _: ContextId,
        ) -> std::result::Result<Box<dyn ProviderContext>, ProviderError> {
            Ok(Box::new(Self))
        }
    }
    impl ProviderContext for NoRequests {
        fn invoke(&mut self, _: ModelRequest) -> ResponseStream {
            panic!("native-tool scripts must not invoke a provider")
        }
    }

    /// A harness of this product that keeps the research tables, on [`NoRequests`].
    pub(crate) fn harness(root: &std::path::Path) -> HarnessBuilder {
        harness_on(root, Arc::new(NoRequests))
    }

    /// A harness of this product that keeps the research tables, on `provider`.
    pub(crate) fn harness_on(
        root: &std::path::Path,
        provider: Arc<dyn Provider>,
    ) -> HarnessBuilder {
        let profile: skyhook::provider::profile::ModelProfile =
            serde_json::from_value(serde_json::json!({
                "model":"test", "reasoning":null, "max_context":32000, "max_output":1000,
            }))
            .unwrap();
        let identity = skyhook_cli::Product::identity(&crate::product::Security);
        HarnessBuilder::new(root, identity.host)
            .provider(
                "test".parse().unwrap(),
                provider,
                [("test".parse().unwrap(), profile)],
            )
            .default_model("test/test".parse().unwrap())
            .host_capabilities(identity.capabilities)
            .session_extension(identity.tables)
    }

    pub(crate) const HIGH: &str = "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N";

    pub(crate) fn text(value: &str) -> Text {
        value.parse().unwrap()
    }

    pub(crate) fn location(file: &str, line: u32) -> Location {
        Location {
            file: text(file),
            start_line: line,
            end_line: line,
            function: None,
        }
    }

    /// A security session held open for store tests, its run started under PoC policy `yes`.
    pub(crate) struct Session {
        handle: SessionHandle,
        runtime: tokio::runtime::Runtime,
        _directory: tempfile::TempDir,
    }

    impl Drop for Session {
        fn drop(&mut self) {
            let _ = self.runtime.block_on(self.handle.shutdown());
        }
    }

    pub(crate) fn fixture() -> (Session, Store, AgentId) {
        let directory = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.block_on(async {
            let harness = harness(directory.path()).build().await.unwrap();
            harness.new_session().await.unwrap()
        });
        let store = Store::of(handle.store().extension().unwrap());
        store
            .start_run(RunOptions::default(), PocPolicy::Yes)
            .unwrap();
        let actor = handle.root_agent().clone();
        let session = Session {
            handle,
            runtime,
            _directory: directory,
        };
        (session, store, actor)
    }

    fn created<I: KindId>(id: NodeId) -> I {
        I::of(id)
    }

    fn new<D>(title: &str, detail: D) -> New<D> {
        New {
            title: text(title),
            body: text("fixture"),
            detail,
        }
    }

    pub(crate) fn component(store: &Store, actor: &AgentId, name: &str) -> ComponentId {
        let files = vec![text(&format!("{name}/lib.c"))];
        let new = new(name, NewComponent { files });
        created(
            store
                .upsert(actor, &Upsert::Component(Change::Create(new)))
                .unwrap(),
        )
    }

    pub(crate) fn source(store: &Store, actor: &AgentId, name: &str) -> SourceId {
        let detail = NewSource {
            location: location(&format!("{name}/net.c"), 1),
            trust: Trust::Untrusted,
        };
        created(
            store
                .upsert(actor, &Upsert::Source(Change::Create(new(name, detail))))
                .unwrap(),
        )
    }

    /// A path entered by a source of its own, stepping through `through` in order.
    pub(crate) fn path(
        store: &Store,
        actor: &AgentId,
        name: &str,
        through: &[ComponentId],
    ) -> PathId {
        let path = created(
            store
                .upsert(actor, &Upsert::Path(Change::Create(new(name, ()))))
                .unwrap(),
        );
        let source = Link::PathSource {
            path,
            source: source(store, actor, name),
        };
        let steps = (through.iter()).map(|component| Link::PathStep {
            path,
            component: *component,
            location: None,
            note: text("calls"),
            position: None,
        });
        let links: Vec<Link> = std::iter::once(source).chain(steps).collect();
        store.link(actor, &links).unwrap();
        path
    }

    pub(crate) fn premise(store: &Store, actor: &AgentId, scope: PremiseScope) -> PremiseId {
        let detail = NewPremise {
            scope,
            location: None,
        };
        let change = Change::Create(new("the length is checked", detail));
        created(store.upsert(actor, &Upsert::Premise(change)).unwrap())
    }

    pub(crate) fn flaw(store: &Store, actor: &AgentId, component: ComponentId) -> FlawId {
        let detail = NewFlaw {
            component,
            location: location("lib.c", 7),
            cwe: None,
        };
        let change = Change::Create(new("unbounded copy", detail));
        created(store.upsert(actor, &Upsert::Flaw(change)).unwrap())
    }

    pub(crate) fn candidate(
        store: &Store,
        actor: &AgentId,
        path: PathId,
        flaws: &[FlawId],
    ) -> CandidateId {
        let detail = NewCandidate {
            path,
            chain: flaws.to_vec().try_into().unwrap(),
        };
        let change = Change::Create(new("overflow", detail));
        created(store.upsert(actor, &Upsert::Candidate(change)).unwrap())
    }

    /// A candidate of one flaw along a path through the flaw's component.
    #[derive(Clone, Copy)]
    pub(crate) struct Scenario {
        pub(crate) candidate: CandidateId,
        pub(crate) flaw: FlawId,
        pub(crate) path: PathId,
        pub(crate) component: ComponentId,
    }

    /// A candidate of one new flaw along a new path through its new component.
    pub(crate) fn scenario(store: &Store, actor: &AgentId, name: &str) -> Scenario {
        let component = component(store, actor, name);
        let path = path(store, actor, name, &[component]);
        let flaw = flaw(store, actor, component);
        Scenario {
            candidate: candidate(store, actor, path, &[flaw]),
            flaw,
            path,
            component,
        }
    }

    pub(crate) fn assess(store: &Store, actor: &AgentId, id: CandidateId, vector: &str) {
        let vector = vector.parse().unwrap();
        store.assess(actor, id, vector, &text("scored")).unwrap();
    }

    pub(crate) fn verdict(store: &Store, actor: &AgentId, id: CandidateId, verdict: Verdict) {
        store
            .verdict(actor, id, verdict, &text("reviewed"))
            .unwrap();
    }

    /// A new scenario assessed high and validated.
    pub(crate) fn validated(store: &Store, actor: &AgentId, name: &str) -> Scenario {
        let scenario = scenario(store, actor, name);
        assess(store, actor, scenario.candidate, HIGH);
        verdict(store, actor, scenario.candidate, Verdict::Validated);
        scenario
    }

    /// A real file under the run's artifacts directory, tagged with its role.
    pub(crate) fn artifact(store: &Store, name: &str, role: ArtifactRole) -> TestArtifact {
        let path = store.artifact_root().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, name).unwrap();
        TestArtifact {
            reference: name.parse().unwrap(),
            role,
        }
    }

    /// `result` for a test of `id`, with the artifacts a confirmed one needs.
    pub(crate) fn test(
        store: &Store,
        actor: &AgentId,
        id: CandidateId,
        result: TestResult,
    ) -> TestId {
        let artifacts = match result {
            TestResult::Confirmed => vec![
                artifact(store, "attack.txt", ArtifactRole::Attack),
                artifact(store, "control.txt", ArtifactRole::Control),
            ],
            _ => Vec::new(),
        };
        let test = NewTest {
            candidate: id,
            result,
            note: text("tested"),
            environment: None,
            artifacts,
        };
        store.record_test(actor, &test).unwrap()
    }

    /// A child of the root whose running job began `work`.
    pub(crate) fn begun(store: &Store, work: &Begin) -> (AgentId, i64, TaskId) {
        let (agent, job) = child_agent(store, JobState::Running);
        let task = store.begin(&agent, work).unwrap();
        (agent, job, task)
    }

    /// A child of the root with an agent job in `state`, standing in for the journal's
    /// record of a launched child; returns its id and its job's.
    pub(crate) fn child_agent(store: &Store, state: JobState) -> (AgentId, i64) {
        child_of(store, &AgentId::root(store.id()), state)
    }

    /// A child of `parent` with an agent job in `state`, as [`child_agent`] is of the root.
    pub(crate) fn child_of(store: &Store, parent: &AgentId, state: JobState) -> (AgentId, i64) {
        let (index, job) = store
            .write(|db| {
                let parent = agent_row(db, parent)?;
                db.execute(
                    "INSERT INTO entry(public_id,agent,agent_ordinal,created_millis,kind) VALUES \
                     (randomblob(16),?1,(SELECT coalesce(max(agent_ordinal),-1)+1 FROM entry \
                     WHERE agent=?1),0,(SELECT name FROM entry_kind WHERE name!='agent_started' \
                     ORDER BY name LIMIT 1))",
                    params![parent],
                )?;
                db.execute(
                    "INSERT INTO target(name) VALUES ('local') ON CONFLICT DO NOTHING",
                    Vec::new(),
                )?;
                let job = db.insert(
                    "INSERT INTO job(id,created,tool,role,arguments,accepts_input,background,\
                     location_target,location_workspace) VALUES ((SELECT coalesce(max(id),0)+1 FROM \
                     job),(SELECT max(seq) FROM entry),'agent','agent','{\"prompt\":\"brief\"}',0,0,\
                     (SELECT id FROM target WHERE name='local'),x'')",
                    Vec::new(),
                )?;
                let index = stored(
                    db,
                    "SELECT coalesce(max(child_index),-1)+1 FROM agent WHERE parent=?1",
                    params![parent],
                    |row| Ok(row.get::<i64>(0)?),
                )?;
                db.execute(
                    "INSERT INTO agent(parent,child_index,owner_job,available_depth) VALUES \
                     (?1,?2,?3,0)",
                    params![parent, index, job],
                )?;
                db.execute(
                    "INSERT INTO job_status(job,state,invocation,background,cancel_recorded,resident) \
                     VALUES (?1,?2,0,0,0,0)",
                    params![job, state.as_str()],
                )?;
                Ok((index, job))
            })
            .unwrap();
        (parent.child(index as u32), job)
    }

    /// Move `job` to `state` as the journal would.
    pub(crate) fn job_state(store: &Store, job: i64, state: JobState) {
        let ended = state.is_terminal().then_some(0_i64);
        store
            .write(|db| {
                db.execute(
                    "UPDATE job_status SET state=?2,first_run_millis=0,ended_millis=?3 WHERE \
                     job=?1",
                    params![job, state.as_str(), ended],
                )?;
                Ok(())
            })
            .unwrap();
    }

    pub(crate) fn end_job(store: &Store, job: i64) {
        job_state(store, job, JobState::Completed);
    }

    /// Mark `path` swept.
    pub(crate) fn sweep(store: &Store, path: PathId) {
        let swept = StatusChange::Path(path, PathStatus::Swept);
        let actor = AgentId::root(store.id());
        store.set_status(&actor, swept, &text("walked it")).unwrap();
    }

    /// The effect each node bears from event `event`.
    pub(crate) fn marks(store: &Store, event: EventId) -> Vec<(NodeId, EventEffect)> {
        store
            .read(|db| {
                rows(
                    db,
                    "SELECT node,effect FROM event_node WHERE event=?1 ORDER BY node",
                    params![event],
                    |row| Ok((NodeId(row.get(0)?), skyhook::db::enum_column(row, 1)?)),
                )
            })
            .unwrap()
    }

    /// The latest event.
    pub(crate) fn last_event(store: &Store) -> EventId {
        store
            .read(|db| {
                stored(db, "SELECT max(id) FROM event", Vec::new(), |row| {
                    Ok(EventId(row.get(0)?))
                })
            })
            .unwrap()
    }
}
