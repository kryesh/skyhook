//! A security session's research as one knowledge graph its agents edit, beside its journal:
//! nodes of seven kinds, the edges between them, and one audit log of every write with the
//! nodes it bore on. What each phase owes is derived from it.
//!
//! Calls hold one read snapshot or an immediate write transaction on the session's
//! connections. Hosts call from a blocking worker: local libsql completes its futures
//! inline, including lock waits.

use std::fmt;

use chrono::{DateTime, Utc};
use libsql::Row;
use skyhook::identity::AgentId;
use skyhook::{
    db::{Db, DbError, corrupt, params},
    session::ExtensionDb,
};

mod graph;
mod ledger;
mod reads;
mod runs;
mod types;
mod work;

pub(crate) use graph::*;
pub(crate) use ledger::*;
pub(crate) use reads::*;
pub(crate) use runs::*;
pub use runs::{LivePolicy, Run};
pub(crate) use types::*;
pub(crate) use work::*;

pub(crate) type Result<T> = std::result::Result<T, Error>;

/// The most records one call names.
const MAX_ATTACHMENTS: usize = 1000;

/// A record a lookup can miss: a node, of a kind when the id was typed with one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Record {
    Node(Option<NodeKind>),
    Task,
    Environment,
    Run,
}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Node(None) => "node",
            Self::Node(Some(kind)) => kind.as_str(),
            Self::Task => "task",
            Self::Environment => "environment",
            Self::Run => "run",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("research database: {0}")]
    Db(#[from] DbError),
    #[error("research database: {0}")]
    Unconfirmed(#[from] skyhook::session::Unconfirmed),
    #[error("research files: {0}")]
    Io(#[from] std::io::Error),
    #[error("ids must be positive")]
    NonPositiveId,
    #[error("a span needs 1 <= start <= end")]
    Span,
    #[error("{0}")]
    InterfaceName(#[from] InvalidInterfaceName),
    #[error("{0}")]
    ServiceAddress(#[from] InvalidServiceAddress),
    #[error("interface {0} is served twice")]
    ServedTwice(InterfaceName),
    #[error("no interface named {0}: record it on its component first")]
    UnknownInterface(InterfaceName),
    #[error("{0}")]
    ArtifactReference(#[from] InvalidArtifactReference),
    #[error("{0}")]
    Text(#[from] TextError),
    #[error("{0}")]
    Inadmissible(#[from] Inadmissible),
    #[error("exceeds {MAX_ATTACHMENTS} records per call")]
    Limit,
    #[error("no {record}{}", id.map_or(String::new(), |id| format!(" with id {id}")))]
    NotFound { record: Record, id: Option<i64> },
    #[error("node {id} is a {found}, not a {expected}")]
    WrongKind {
        id: NodeId,
        expected: NodeKind,
        found: NodeKind,
    },
    #[error("the list names at least one {0}")]
    Empty(NodeKind),
    #[error("a node is not merged into itself")]
    SelfMerge,
    #[error(
        "node {node} is a {node_kind}; node {into} is a {into_kind}: merge joins nodes of one kind"
    )]
    MergeKinds {
        node: NodeId,
        node_kind: NodeKind,
        into: NodeId,
        into_kind: NodeKind,
    },
    #[error("a finding is not merged: it follows its root flaw")]
    FindingMerge,
    #[error("a premise does not rest on itself")]
    SelfBasis,
    #[error(
        "a `checked` state names in `rests_on` at least one premise its clearing relies on: a \
         recorded one (a deployment premise for a documented contract included) or a new code \
         premise for each general claim the recorded accesses do not show"
    )]
    Unrested,
    #[error("input state {0} is read at that location: merge into it")]
    StateAt(StateId),
    #[error("only an input state has a trust and a location")]
    NotInput,
    #[error("work is begun by the agent job that does it")]
    TaskNeedsJob,
    #[error("this session keeps no security research")]
    NoResearch,
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

fn first_rows<T>(
    db: &Db,
    sql: impl Into<skyhook::db::SqlText>,
    params: Vec<libsql::Value>,
    limit: usize,
    map: impl FnMut(&Row) -> Result<T>,
) -> Result<Vec<T>> {
    db.query_up_to(sql, params, limit, map)
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
    pub(crate) async fn reading<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        self.0.reading(work).await
    }

    /// Run `work`, which may write, once it is this caller's turn among the research
    /// writers, so that waiting holds no thread.
    pub(crate) async fn writing<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        self.0.writing(work).await
    }

    /// Run `work`, which reads artifacts or writes reports, once it is this caller's turn
    /// among those who do, unless `stop` comes first.
    pub(crate) async fn filing<T: Send + 'static, S>(
        &self,
        stop: impl Future<Output = S>,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> std::result::Result<T, S> {
        self.0.filing(stop, work).await
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

    use skyhook::identity::AgentId;
    use skyhook::{
        agent::{HarnessBuilder, SessionHandle},
        job::JobState,
        provider::{
            Provider, ProviderContext, ProviderError, ResponseStream,
            protocol::{ContextId, ModelRequest},
        },
    };

    use super::*;

    /// Every record `read` pages through from the start, in pages of the largest size.
    pub(crate) fn drain<T>(mut read: impl FnMut(Page) -> Result<Listing<T>>) -> Result<Vec<T>> {
        let mut page = Page::new(0, skyhook_base::bounded::Bounded::new::<1000>());
        let mut items = Vec::new();
        loop {
            let listing = read(page)?;
            items.extend(listing.items);
            match listing.next_after {
                Some(after) => page.after = after,
                None => return Ok(items),
            }
        }
    }

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
        let profile: skyhook::provider::model::ModelProfile = skyhook::yaml::parse(
            "{model: test, reasoning: null, max_context: 32000, max_output: 1000}",
        )
        .unwrap();
        let identity = skyhook::product::Product::identity(&crate::product::Security);
        HarnessBuilder::new(root, identity.host)
            .provider(
                "test".parse().unwrap(),
                provider,
                [("test".parse().unwrap(), profile)],
            )
            .default_model("test/test".parse().unwrap())
            .host_capabilities(identity.capabilities)
            .session_schema(identity.schema)
    }

    pub(crate) const HIGH: &str = "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N";
    pub(crate) const NONE: &str = "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:N/VI:N/VA:N/SC:N/SI:N/SA:N";

    pub(crate) fn text(value: &str) -> Text {
        value.parse().unwrap()
    }

    pub(crate) fn location(file: &str, line: u32) -> Location {
        Location::Code(CodeSite {
            file: text(file),
            start_line: line,
            end_line: line,
            function: None,
        })
    }

    /// A security session held open for store tests, its run started under live policy `yes`.
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
        fixture_under(LivePolicy::Yes)
    }

    /// A new run under `live`.
    pub(crate) fn fixture_under(live: LivePolicy) -> (Session, Store, AgentId) {
        let directory = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.block_on(async {
            let harness = harness(directory.path()).build().await.unwrap();
            harness.new_session().await.unwrap()
        });
        let store = Store::of(handle.store().extension().unwrap());
        let options = RunOptions {
            live,
            ..RunOptions::default()
        };
        store.start_run(options).unwrap();
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

    /// Title `title` with the fixture's body.
    fn titled(title: &str) -> NewText {
        NewText {
            title: text(title),
            body: text("fixture"),
        }
    }

    /// A component owning `<name>/lib.c` and the interface `<name>-api`.
    pub(crate) fn component(store: &Store, actor: &AgentId, name: &str) -> ComponentId {
        let NewText { title, body } = titled(name);
        let new = NewComponent {
            title,
            body,
            files: Listed::of([text(&format!("{name}/lib.c"))]),
            interfaces: Listed::of([format!("{name}-api").parse().unwrap()]),
        };
        created(
            store
                .upsert(actor, &Upsert::Create(NewNode::Component(new)))
                .unwrap(),
        )
    }

    /// A path reading an input state of its own in code, stepping through `through` in order.
    pub(crate) fn path(
        store: &Store,
        actor: &AgentId,
        name: &str,
        through: &[ComponentId],
    ) -> PathId {
        path_from(
            store,
            actor,
            name,
            through,
            location(&format!("{name}/net.c"), 1),
        )
    }

    /// A path reading an input state of its own at `at`, stepping through `through` in order.
    pub(crate) fn path_from(
        store: &Store,
        actor: &AgentId,
        name: &str,
        through: &[ComponentId],
        at: Location,
    ) -> PathId {
        let NewText { title, body } = titled(name);
        let new = NewPath {
            title,
            body,
            steps: Listed::of(steps(through.iter().map(|component| (*component, "calls")))),
        };
        let path = created(
            store
                .upsert(actor, &Upsert::Create(NewNode::Path(new)))
                .unwrap(),
        );
        let NewText { title, body } = titled(name);
        let detail = NewState::Input(NewInput {
            title,
            body,
            location: at.clone(),
            trust: Trust::Untrusted,
        });
        let input = created(
            store
                .upsert(actor, &Upsert::Create(NewNode::State(detail)))
                .unwrap(),
        );
        store
            .link(actor, &[access(path, input, Access::Read, at)])
            .unwrap();
        path
    }

    /// Steps through each component, with its note.
    fn steps<'a>(steps: impl IntoIterator<Item = (ComponentId, &'a str)>) -> Vec<PathStep> {
        (steps.into_iter())
            .map(|(component, note)| PathStep {
                component,
                location: None,
                note: text(note),
            })
            .collect()
    }

    /// Rewrite path `path`'s steps whole: through each component, with its note.
    pub(crate) fn restep(
        store: &Store,
        actor: &AgentId,
        path: PathId,
        through: &[(ComponentId, &str)],
    ) {
        let edit = PathEdit {
            id: path,
            title: None,
            body: None,
            steps: Some(Listed::of(steps(through.iter().copied()))),
        };
        store
            .upsert(actor, &Upsert::Edit(NodeEdit::Path(edit)))
            .unwrap();
    }

    /// A state the target keeps, of `scope`.
    pub(crate) fn state(store: &Store, actor: &AgentId, scope: fn(NewText) -> NewState) -> StateId {
        let change = Upsert::Create(NewNode::State(scope(titled("decoder state"))));
        created(store.upsert(actor, &change).unwrap())
    }

    /// Path `path`'s `access` of `state` at `at`.
    pub(crate) fn access(path: PathId, state: StateId, access: Access, at: Location) -> Edge {
        Edge::PathState {
            path,
            state,
            access,
            location: at,
        }
    }

    /// A binary image's code at `start`, entering a path.
    pub(crate) fn daemon(start: u64) -> Location {
        Location::Binary(BinarySite {
            image: text("bin/daemon"),
            start: Address(start),
            end: Address(start + 0x58),
            symbol: None,
        })
    }

    /// An operation of `interface`.
    pub(crate) fn operation(interface: &str) -> Location {
        Location::Interface(InterfaceSite {
            interface: interface.parse().unwrap(),
            operation: text("POST /files"),
            field: None,
        })
    }

    /// An environment for any candidate serving `interfaces`, recorded by `actor`.
    pub(crate) fn serve(
        store: &Store,
        actor: &AgentId,
        interfaces: &[&str],
    ) -> Result<EnvironmentId> {
        let environment = NewEnvironment {
            instructions: text("the staging deployment"),
            candidate: None,
            serves: (interfaces.iter())
                .map(|interface| Served {
                    interface: interface.parse().unwrap(),
                    address: "https://staging.example.test".parse().unwrap(),
                })
                .collect::<Vec<_>>()
                .try_into()
                .and_then(Listed::try_into)
                .unwrap(),
        };
        store.environment(actor, &environment)
    }

    pub(crate) fn premise(store: &Store, actor: &AgentId, scope: PremiseScope) -> PremiseId {
        let NewText { title, body } = titled("the length is checked");
        let new = NewPremise {
            title,
            body,
            scope,
            location: None,
        };
        let change = Upsert::Create(NewNode::Premise(new));
        created(store.upsert(actor, &change).unwrap())
    }

    pub(crate) fn flaw(store: &Store, actor: &AgentId, component: ComponentId) -> FlawId {
        let NewText { title, body } = titled("unbounded copy");
        let new = NewFlaw {
            title,
            body,
            component,
            location: location("lib.c", 7),
            cwe: None,
        };
        let change = Upsert::Create(NewNode::Flaw(new));
        created(store.upsert(actor, &change).unwrap())
    }

    pub(crate) fn candidate(
        store: &Store,
        actor: &AgentId,
        path: PathId,
        flaws: &[FlawId],
    ) -> CandidateId {
        let NewText { title, body } = titled("overflow");
        let new = NewCandidate {
            title,
            body,
            actions: vec![path].try_into().unwrap(),
            chain: flaws.to_vec().try_into().unwrap(),
        };
        let change = Upsert::Create(NewNode::Candidate(new));
        created(store.upsert(actor, &change).unwrap())
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

    /// Run `record` as a child of the root whose job began `work`, then end its job: what it
    /// records is that phase's own work.
    pub(crate) fn within<T>(store: &Store, work: &Begin, record: impl FnOnce(&AgentId) -> T) -> T {
        let (agent, job, _) = begun(store, work);
        let recorded = record(&agent);
        end_job(store, job);
        recorded
    }

    /// Assess `id` at `vector` in a review of it.
    pub(crate) fn assess(store: &Store, id: CandidateId, vector: &str) {
        within(store, &Begin::Review(Listed::of([id])), |agent| {
            let vector = vector.parse().unwrap();
            store.assess(agent, id, vector, &text("scored")).unwrap();
        });
    }

    /// Rewrite candidate `id`'s body as `body`.
    pub(crate) fn reword(store: &Store, actor: &AgentId, id: CandidateId, body: &str) {
        let edit = CandidateEdit {
            id,
            title: None,
            body: Some(text(body)),
            actions: None,
            chain: None,
        };
        (store.upsert(actor, &Upsert::Edit(NodeEdit::Candidate(edit)))).unwrap();
    }

    /// Judge `id` in a review of it, which scores it again at its standing vector first.
    pub(crate) fn verdict(store: &Store, id: CandidateId, verdict: Verdict) {
        let vector: String = (store.read(|db| {
            let sql = "SELECT vector FROM candidate_result WHERE candidate=?1";
            stored(db, sql, params![id], |row| Ok(row.get(0)?))
        }))
        .unwrap();
        within(store, &Begin::Review(Listed::of([id])), |agent| {
            let vector = vector.parse().unwrap();
            store.assess(agent, id, vector, &text("scored")).unwrap();
            store
                .verdict(agent, id, verdict, &text("reviewed"))
                .unwrap();
        });
    }

    /// Validate `id` anew in reviews of it: disproved, then validated, so the validation is a
    /// new verdict rather than a restatement.
    pub(crate) fn revalidate(store: &Store, id: CandidateId) {
        verdict(store, id, Verdict::Disproved);
        verdict(store, id, Verdict::Validated);
    }

    /// Decide `premise` in a settle of it.
    pub(crate) fn decide(store: &Store, premise: PremiseId, status: PremiseStatus) {
        within(store, &Begin::Settle(Listed::of([premise])), |agent| {
            let change = StatusChange::Premise {
                id: premise,
                status,
            };
            store.set_status(agent, change, &text("settled")).unwrap();
        });
    }

    /// Judge `flaw` in a chain of it.
    pub(crate) fn judge(store: &Store, flaw: FlawId, status: FlawStatus) {
        within(store, &Begin::Chain(Listed::of([flaw])), |agent| {
            let change = StatusChange::Flaw { id: flaw, status };
            store.set_status(agent, change, &text("chained")).unwrap();
        });
    }

    /// Check `state` across calls in a check of it, resting on `rests_on`.
    pub(crate) fn check(store: &Store, state: StateId, rests_on: &[PremiseId]) {
        within(store, &Begin::Check(Listed::of([state])), |agent| {
            let rests_on = Some(rests_on.to_vec()).try_into().unwrap();
            let checked = StatusChange::State(StateMark::Checked {
                id: state,
                rests_on,
            });
            store.set_status(agent, checked, &text("checked")).unwrap();
        });
    }

    /// A new scenario assessed high and validated.
    pub(crate) fn validated(store: &Store, actor: &AgentId, name: &str) -> Scenario {
        let scenario = scenario(store, actor, name);
        assess(store, scenario.candidate, HIGH);
        verdict(store, scenario.candidate, Verdict::Validated);
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
        let task = store.begin(&agent, work).unwrap().task;
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
                    "INSERT INTO entry(agent,agent_ordinal,created_millis,kind) VALUES \
                     (?1,(SELECT coalesce(max(agent_ordinal),-1)+1 FROM entry \
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
                     (SELECT id FROM target WHERE name='local'),'')",
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
                    "INSERT INTO job_status(job,state,terminal,invocation,background,\
                     cancel_recorded,resident) VALUES (?1,?2,?3,0,0,0,0)",
                    params![job, state.as_str(), state.is_terminal()],
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
                    "UPDATE job_status SET state=?2,terminal=?3,first_run_millis=0,ended_millis=?4 \
                     WHERE job=?1",
                    params![job, state.as_str(), state.is_terminal(), ended],
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
        let swept = StatusChange::Path {
            id: path,
            status: PathStatus::Swept,
        };
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
