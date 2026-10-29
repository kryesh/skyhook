//! A security session's research, in tables beside its journal.
//!
//! Calls hold one read snapshot or an immediate write transaction on the session's
//! connections. Hosts call from a blocking worker: local libsql completes its futures
//! inline, including lock waits.

use std::fmt;

use chrono::{DateTime, Utc};
use libsql::Row;
use skyhook::{
    db::{Db, DbError, Sql, bytes, corrupt, enum_column, params},
    identity::{AgentId, SessionId},
    session::ExtensionDb,
};

/// The column of an actor joined as `a`, decoded by [`actor_column`]. Column lists are
/// macros so that queries stay static text, prepared once per connection.
macro_rules! actor_columns {
    () => {
        "a.agent"
    };
}

mod artifacts;
mod authored;
mod candidates;
mod checks;
mod components;
mod consequences;
mod environment;
mod events;
mod flaws;
mod overview;
mod paths;
mod plan;
mod premises;
mod progress;
mod queues;
mod report;
mod retry;
mod runs;
mod schedule;
mod scope;
mod types;
mod workflow;

pub use artifacts::*;
pub use authored::*;
pub use candidates::*;
pub use checks::*;
pub use components::*;
pub use consequences::*;
pub use environment::*;
pub use events::*;
pub use flaws::*;
pub use overview::*;
pub use paths::*;
pub use plan::*;
pub use premises::*;
pub use progress::*;
pub use queues::*;
pub use report::*;
pub use retry::*;
pub use runs::*;
pub use schedule::*;
pub use scope::*;
pub use types::*;
pub use workflow::*;

pub type Result<T> = std::result::Result<T, Error>;

/// The most records one call attaches.
const MAX_ATTACHMENTS: usize = 1000;

/// A record a lookup can miss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    Run,
    Scope,
    Deployment,
    Principal,
    Source,
    Component,
    Path,
    Unit,
    Guarantee,
    Deferred,
    Flaw,
    Refutation,
    Premise,
    Exposure,
    Contract,
    Candidate,
    Assessment,
    Validation,
    Poc,
    Finding,
}

/// A bound a request exceeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    Attachments,
    RouteSteps,
    Artifacts,
}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Run => "run",
            Self::Scope => "scope",
            Self::Deployment => "deployment",
            Self::Principal => "principal",
            Self::Source => "source",
            Self::Component => "component",
            Self::Path => "path",
            Self::Unit => "plan unit",
            Self::Guarantee => "guarantee",
            Self::Deferred => "deferred region",
            Self::Flaw => "flaw",
            Self::Refutation => "refutation",
            Self::Premise => "premise",
            Self::Exposure => "exposure",
            Self::Contract => "contract",
            Self::Candidate => "candidate",
            Self::Assessment => "assessment",
            Self::Validation => "validation",
            Self::Poc => "poc",
            Self::Finding => "finding",
        })
    }
}

impl fmt::Display for Bound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Attachments => f.write_str("1000 attachments per call"),
            Self::RouteSteps => f.write_str("1000 route steps"),
            Self::Artifacts => write!(f, "{MAX_ARTIFACTS} artifacts per PoC"),
        }
    }
}

/// Why a positive verdict is refused: what the candidate's records do not yet establish.
#[derive(Clone, Debug, thiserror::Error)]
pub enum Refusal {
    #[error("candidate {candidate} names no path; add the path its scenario starts from")]
    NoPath { candidate: CandidateId },
    #[error(
        "flaw {flaw} has no current `contributes` exposure along any of candidate {candidate}'s paths"
    )]
    NoContribution {
        candidate: CandidateId,
        flaw: FlawId,
    },
    #[error("flaw {flaw} is refuted at its current revision")]
    Refuted { flaw: FlawId },
    #[error("premise {premise} is unresolved; settle it before a positive verdict")]
    OpenPremise { premise: PremiseId },
    #[error("premise {premise} fails, so the scenario does not hold as stated")]
    FailedPremise { premise: PremiseId },
    #[error(
        "premise {premise} gates {metric} and does not hold; reassess with {metric}:{} before a positive verdict",
        metric.floor()
    )]
    MetricAboveFloor {
        premise: PremiseId,
        metric: BaseMetric,
    },
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
    #[error("git revision must be a full SHA-1 or SHA-256 object id")]
    GitOid,
    #[error("{0}")]
    Text(#[from] TextError),
    #[error("{0}")]
    ArtifactReference(#[from] InvalidArtifactReference),
    #[error("{0}")]
    Endpoint(#[from] InvalidEndpoint),
    #[error("invalid credential reference; supply a name, not a secret value")]
    CredentialReference,
    #[error("no {record}{}", id.map_or(String::new(), |id| format!(" with id {id}")))]
    NotFound { record: Record, id: Option<i64> },
    #[error("{record} {id} was merged into {into}; record against {into}")]
    Merged { record: Record, id: i64, into: i64 },
    #[error("file {file} was swept as part of component {component} and stays there")]
    FileSwept {
        file: String,
        component: ComponentId,
    },
    #[error("file {file} belongs to no component; record it with its component first")]
    Unowned { file: String },
    #[error(
        "retry sites {sites:?} are unclassified; classify each (input_exhausted, invalid or other) before the sweep completes"
    )]
    UnclassifiedRetrySites { sites: Vec<RetrySiteId> },
    #[error(
        "no retry site of status {status} is recorded in this component; record every site of it (retry_site) before a guarantee spanning it"
    )]
    NoRetrySites { status: String },
    #[error(
        "a guarantee spanning retry status {status} cites every site of it its component records: add evidence on it at sites {sites:?}, or the flaw a site shows"
    )]
    UncitedRetrySites {
        status: String,
        sites: Vec<RetrySiteId>,
    },
    #[error(
        "guarantee {guarantee} has no evidence inside deferred region {deferred}; a settlement cites only guarantees argued from the region's own code"
    )]
    OutsideDeferred {
        guarantee: GuaranteeId,
        deferred: DeferredId,
    },
    #[error("only an inconclusive outcome names the premises it leaves open")]
    OpenWithoutInconclusive,
    #[error(
        "an inconclusive question or follow-up names the premises it leaves open in `open`; record the missing fact as a premise first, or end the unit deferred with the region it leaves"
    )]
    InconclusiveNeedsPremise,
    #[error(
        "premise {premise} is a flaw's condition, a fact about the code: record the sites you enumerated as evidence on it and list those you did not trace in `untraced`"
    )]
    UntracedSites { premise: PremiseId },
    #[error(
        "premise {premise} is a flaw's condition, a fact about the code: settle it from the code, holding or failing; it is never asked of the user, and no deployment fact decides it"
    )]
    CodeFact { premise: PremiseId },
    #[error(
        "premise {premise} is a condition of candidate {owner}'s own scenario; a candidate relies only on deployment and flaw conditions, so record a shared one on the deployment or the flaw"
    )]
    ScenarioCondition {
        premise: PremiseId,
        owner: CandidateId,
    },
    #[error(
        "no path with an untrusted source reaches component {component}; its contracts are checked once one does"
    )]
    Unreached { component: ComponentId },
    #[error(
        "a flaw undermining a guarantee its check names broke contract {contract}; the flaw carries the work"
    )]
    Broken { contract: ContractId },
    #[error("a check names its owed `contract`, or else its `component`, `function` and `kind`")]
    ContractKey,
    #[error("a {kind} guarantee names the function it is argued in")]
    GuaranteeNeedsFunction { kind: InvariantKind },
    #[error("a path needs at least one source")]
    EmptyPath,
    #[error("cannot relate or merge a flaw with itself")]
    SelfFlaw,
    #[error("cannot merge a candidate into itself")]
    SelfCandidate,
    #[error("a candidate needs at least one member flaw")]
    NoMembers,

    #[error(
        "assessment {assessment} at the candidate's current revision already has a verdict; reassess after a material change"
    )]
    AssessmentReviewed { assessment: AssessmentId },
    #[error("candidate is {0}; a verdict judges an assessment at its current revision")]
    Unassessed(Status),
    #[error("exceeds {0}")]
    Limit(Bound),
    #[error("candidate has no positive analytical validation at its current revision")]
    NoPositiveValidation,
    #[error("validation {tested} is not a positive verdict on this candidate")]
    NotPositive { tested: ValidationId },
    #[error(
        "the scenario was contradicted since this test was opened (a member refuted, or a condition gating it failing or undermined); end this attempt and leave the candidate to a later test"
    )]
    Contradicted,
    #[error("a test is opened by the agent job that runs it")]
    TestNeedsJob,
    #[error(
        "you recorded the positive verdict on candidate {candidate}; a doubt that needs a follow-up is an unproven verdict with the open question as a premise"
    )]
    QuestionsOwnVerdict { candidate: CandidateId },
    #[error(
        "candidate {candidate} has nothing an unproven verdict can wait on: no follow-up naming it is open, none of its conditions is asked of the user under the current scope, and the follow-ups settling its open conditions ended at their revisions; decide each open condition: settle it by the bounded check it needs, resolve it holding by an admitted deployment fact or failing, reject the candidate policy_excluded when no admitted fact grants it, or resolve it asked when the scope's facts do not address it"
    )]
    NothingPending { candidate: CandidateId },
    #[error(
        "a follow-up that asks about a guarantee names the flaw it asks about; without a flaw it names only conditions of the candidate it names"
    )]
    InvariantWithoutFlaw,
    #[error("candidate {candidate} is not a scenario this report states; notes name its scenarios")]
    UnreportedScenario { candidate: CandidateId },
    #[error("PoC {poc} is not a test this report states")]
    UnstatedTest { poc: PocId },
    #[error(
        "{record} {id} has prose already; a report takes one note per scenario and one narrative per test"
    )]
    RepeatedProse { record: Record, id: i64 },
    #[error(
        "PoC {poc} recorded no artifact {reference}; cite an artifact of a test this report states"
    )]
    UnstatedArtifact {
        poc: PocId,
        reference: ArtifactReference,
    },
    #[error("root flaw {root} leads to no scenario with a positive verdict")]
    NothingToReport { root: FlawId },
    #[error("unit {unit} already ended; a unit has one outcome")]
    UnitEnded { unit: UnitId },
    #[error(
        "you planned unit {unit} or recorded what triggered its check, so another agent answers it; you may end it only blocked, inconclusive or deferred"
    )]
    OwnUnit { unit: UnitId },
    #[error("premise {premise} is not a premise of candidate {candidate}")]
    NotItsPremise {
        premise: PremiseId,
        candidate: CandidateId,
    },
    #[error("confirmed PoC requires at least one `{role}` artifact")]
    ConfirmationNeedsArtifact { role: ArtifactRole },
    #[error("PoC artifact {reference}: {fault}")]
    Artifact {
        reference: ArtifactReference,
        fault: ArtifactFault,
    },
    #[error("report snapshot changed; regenerate artifacts from a fresh snapshot")]
    SnapshotChanged,
    #[error(
        "deployment {deployment} belongs to an earlier scope; cite a deployment fact of the current scope"
    )]
    DeploymentSuperseded { deployment: DeploymentId },
    #[error(
        "deployment {deployment} is {stance} by the scope: a condition holds by what the scope admits, and a policy exclusion cites what it excludes"
    )]
    DeploymentStance {
        deployment: DeploymentId,
        stance: DeploymentStance,
    },
    #[error(transparent)]
    Refused(#[from] Refusal),
}

use crate::cvss::{BaseMetric, Severity};

impl From<libsql::Error> for Error {
    fn from(error: libsql::Error) -> Self {
        Self::Db(error.into())
    }
}

/// The research of one session, on its connections.
#[derive(Clone)]
pub struct Store(ExtensionDb);

pub(super) fn now() -> i64 {
    Utc::now().timestamp_millis()
}

pub(super) fn time(millis: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(millis)
        .ok_or_else(|| corrupt(format!("timestamp {millis} out of range")).into())
}

/// `values` as one JSON array parameter, which SQL reads with `json_each`.
pub(super) fn json_list<T: serde::Serialize>(values: &[T]) -> Result<String> {
    serde_json::to_string(values).map_err(|error| corrupt(error.to_string()).into())
}

pub(super) fn session_column(row: &Row, index: i32) -> Result<SessionId> {
    Ok(SessionId::from_bytes(bytes(row.get(index)?)?))
}

pub(super) fn actor_column(row: &Row, offset: i32) -> Result<AgentId> {
    let agent = row
        .get::<String>(offset)?
        .parse()
        .map_err(|_| corrupt("invalid audit agent"))?;
    Ok(agent)
}

/// The row of `id`, which `sql` selects as `?1`.
pub(super) fn one<T, I: RecordId>(
    db: &Db,
    sql: impl Into<skyhook::db::SqlText>,
    id: I,
    map: impl FnOnce(&Row) -> Result<T>,
) -> Result<T> {
    let id = id.into();
    optional(db, sql, params![id], map)?.ok_or(Error::NotFound {
        record: I::RECORD,
        id: Some(id),
    })
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

impl Record {
    /// The error for a missing record no id names.
    pub(super) fn missing(self) -> Error {
        Error::NotFound {
            record: self,
            id: None,
        }
    }
}

pub(super) fn optional<T>(
    db: &Db,
    sql: impl Into<skyhook::db::SqlText>,
    params: Vec<libsql::Value>,
    map: impl FnOnce(&Row) -> Result<T>,
) -> Result<Option<T>> {
    db.query_row_as(sql, params, map)
}

pub(super) fn rows<T>(
    db: &Db,
    sql: impl Into<skyhook::db::SqlText>,
    params: Vec<libsql::Value>,
    map: impl FnMut(&Row) -> Result<T>,
) -> Result<Vec<T>> {
    db.query_as(sql, params, map)
}

pub(super) fn exists(
    db: &Db,
    sql: impl Into<skyhook::db::SqlText>,
    params: Vec<libsql::Value>,
) -> Result<bool> {
    Ok(optional(db, sql, params, |_| Ok(()))?.is_some())
}

pub(super) fn count(
    db: &Db,
    sql: impl Into<skyhook::db::SqlText>,
    params: Vec<libsql::Value>,
) -> Result<u64> {
    stored(db, sql, params, |row| Ok(row.get::<i64>(0)?.max(0) as u64))
}

/// The actor's row, created on first use.
pub(super) fn actor_row(db: &Db, actor: &AgentId) -> Result<i64> {
    let key = || params![actor.to_string()];
    let existing = optional(db, "SELECT id FROM actor WHERE agent=?1", key(), |row| {
        Ok(row.get::<i64>(0)?)
    })?;
    if let Some(id) = existing {
        return Ok(id);
    }
    // The journal records an agent before it acts.
    let mut params = key();
    params.extend(params![skyhook::session::agent_row(db, actor)?]);
    Ok(db.insert(
        "INSERT INTO actor(agent,session_agent) VALUES (?1,?2)",
        params,
    )?)
}

impl Sql for &Text {
    fn sql(self) -> libsql::Value {
        self.as_str().sql()
    }
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
    pub(super) fn write<T>(&self, f: impl FnOnce(&Db) -> Result<T>) -> Result<T> {
        self.0.write(f)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::Arc;

    use skyhook::{
        agent::{HarnessBuilder, SessionHandle},
        identity::{AgentId, JobId},
        provider::{
            Provider, ProviderContext, ProviderError, ResponseStream,
            profile::ModelProfile,
            protocol::{ContextId, ModelRequest},
        },
    };

    use super::*;
    use crate::cvss::Cvss40;

    pub(super) const HIGH: &str = "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N";

    pub(crate) fn text(value: &str) -> Text {
        value.parse().unwrap()
    }

    pub(super) fn new_premise(kind: InvariantKind, statement: &str) -> NewPremise {
        NewPremise {
            kind,
            statement: text(statement),
        }
    }

    /// Where a condition gating `candidate`'s scenario is recorded.
    pub(super) fn scenario(candidate: CandidateId) -> PremiseOn {
        PremiseOn::Candidate {
            id: candidate,
            gates: Gate::Scenario,
        }
    }

    /// A real file under the run's artifacts directory, tagged with its role.
    pub(crate) fn artifact(store: &Store, name: &str, role: ArtifactRole) -> PocArtifact {
        let path = store.artifact_root().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, name).unwrap();
        PocArtifact {
            reference: name.parse().unwrap(),
            role,
        }
    }

    /// The two artifacts a confirmed PoC needs.
    pub(crate) fn confirmed_artifacts(store: &Store, attack: &str) -> [PocArtifact; 2] {
        [
            artifact(store, attack, ArtifactRole::Attack),
            artifact(store, "control.log", ArtifactRole::Control),
        ]
    }

    /// The least prose a finding of `root` is populated with.
    pub(crate) fn report_input(root: FlawId) -> WriteUp {
        ReportInput {
            root,
            title: text("title"),
            summary: text("summary"),
            analysis: text("analysis"),
            impact: text("impact"),
            remediation: text("remediation"),
            reproduction: Reproduction {
                prerequisites: Vec::new(),
                steps: vec![ReproductionStep {
                    instruction: text("step"),
                    expected_result: text("result"),
                    artifacts: Vec::new(),
                }],
                outcome: ReproductionOutcome::Unobserved {
                    expected_result: text("result"),
                    limitations: text("analysis only"),
                },
                cleanup: Vec::new(),
            },
            notes: Default::default(),
            narratives: Default::default(),
            sections: Vec::new(),
        }
        .canonical()
    }

    pub(super) fn vector(value: &str) -> Cvss40 {
        value.parse().unwrap()
    }

    pub(crate) fn revision() -> Revision {
        Revision {
            commit: None,
            tree: Tree::Clean,
        }
    }

    pub(super) fn location(file: &str, line: u32) -> Location {
        Location::new(text(file), line, line, None).unwrap()
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
        let profile: ModelProfile = serde_json::from_value(serde_json::json!({
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

    /// A security session held open for store tests, its run started under PoC policy `yes`.
    pub(crate) struct Session {
        pub(crate) handle: SessionHandle,
        runtime: tokio::runtime::Runtime,
        directory: tempfile::TempDir,
    }

    impl Session {
        pub(crate) fn path(&self) -> &std::path::Path {
            self.directory.path()
        }
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
            .start_run(RunOptions::default(), &revision(), PocPolicy::Yes)
            .unwrap();
        let actor = handle.root_agent().clone();
        let session = Session {
            handle,
            runtime,
            directory,
        };
        (session, store, actor)
    }

    /// A journaled script or one of its scheduling calls, without starting an executor.
    pub(crate) fn journal_job(
        store: &Store,
        parent: Option<JobId>,
        page: Option<&Startables>,
    ) -> JobId {
        store.write(|db| {
            let entry = db.insert(
                "INSERT INTO entry(public_id,agent,agent_ordinal,created_millis,kind) \
                 SELECT randomblob(16),id,(SELECT coalesce(max(agent_ordinal),-1)+1 FROM entry \
                 WHERE agent=a.id),0,'job_created' FROM agent a WHERE parent IS NULL",
                Vec::new(),
            )?;
            let (tool, role, arguments, state) = match page {
                Some(_) => ("research_query", "tool", r#"{"action":"startable"}"#, "completed"),
                None => ("script", "script", r#"{"source":""}"#, "running"),
            };
            let id = db.insert(
                "INSERT INTO job(id,created,parent,tool,role,arguments,accepts_input,background,\
                 location_target,location_workspace) VALUES ((SELECT coalesce(max(id),0)+1 FROM job),\
                 ?1,?2,?3,?4,?5,0,1,(SELECT id FROM target WHERE name='root'),x'')",
                params![entry, parent.map(|id| id.get() as i64), tool, role, arguments],
            )?;
            db.execute(
                "INSERT INTO job_status(job,state,invocation,background,cancel_recorded,resident) \
                 VALUES (?1,?2,0,1,0,0)",
                params![id,state],
            )?;
            if let Some(page) = page {
                db.execute(
                    "INSERT INTO job_output(job,generation,captures_complete,result) VALUES (?1,0,1,?2)",
                    params![id,serde_json::to_string(page).unwrap()],
                )?;
            }
            Ok(JobId::new(id as u64).unwrap())
        }).unwrap()
    }

    /// An agent other than `actor`, which answers the units `actor` plans.
    pub(crate) fn answerer(actor: &AgentId) -> AgentId {
        AgentId::root(actor.session()).child(999)
    }

    pub(crate) fn source(store: &Store, actor: &AgentId, name: &str, trust: Trust) -> SourceId {
        store
            .add_source(
                actor,
                &NewSource {
                    kind: SourceKind::Network,
                    trust,
                    entry: location(&format!("{name}.c"), 1),
                    description: text(name),
                    principals: Vec::new(),
                    handoff: None,
                },
            )
            .unwrap()
    }

    pub(crate) fn component(store: &Store, actor: &AgentId, name: &str) -> ComponentId {
        store
            .add_component(
                actor,
                &NewComponent {
                    name: text(name),
                    description: text("fixture component"),
                    files: vec![text(&format!("{name}/lib.c"))],
                },
            )
            .unwrap()
    }

    pub(crate) fn new_flaw(component: ComponentId, title: &str) -> NewFlaw {
        NewFlaw {
            component,
            title: text(title),
            statement: text("the copy length is not bounded by the buffer"),
            operation: OperationKind::MemoryWrite,
            deficiency: Some(GuardDeficiency {
                guard: GuardKind::BoundsCheck,
                deficiency: DeficiencyKind::Missing,
            }),
            misreports: None,
            effects: vec![Effect {
                property: Property::Integrity,
                note: text("adjacent memory is overwritten"),
            }],
            evidence: Vec::new(),
        }
    }

    /// A path of one new source, reaching `reaches`.
    pub(crate) fn path(
        store: &Store,
        actor: &AgentId,
        name: &str,
        reaches: &[ComponentId],
    ) -> PathId {
        let source = source(store, actor, name, Trust::Unauthenticated);
        store
            .add_path(
                actor,
                &NewPath {
                    description: text(name),
                    sources: vec![source],
                    reaches: (reaches.iter())
                        .map(|component| Reach {
                            component: *component,
                            note: text("direct call"),
                        })
                        .collect(),
                },
            )
            .unwrap()
    }

    pub(super) fn contributes(store: &Store, actor: &AgentId, flaw: FlawId, path: PathId) {
        store
            .record_exposure(
                actor,
                &NewExposure {
                    flaw,
                    path,
                    outcome: ExposureOutcome::Contributes,
                    conditions: text("the request sets the length"),
                    note: text("traced from the entry"),
                    route: vec![RouteStep {
                        location: location("entry.c", 3),
                        basis: RouteBasis::Inspected,
                        note: text("the length is passed through"),
                    }],
                },
            )
            .unwrap();
    }

    /// End every open sweep, then every path review, as their pools would.
    pub(crate) fn reviewed(store: &Store, actor: &AgentId) {
        for phase in [Phase::Sweep, Phase::PathReview] {
            for subject in queued(store, phase) {
                let WorkSubject::Unit(unit) = subject else {
                    unreachable!("unit phases list units")
                };
                store
                    .record_outcome(
                        &answerer(actor),
                        unit,
                        Outcome::Completed,
                        &text("done"),
                        &[],
                    )
                    .unwrap();
            }
        }
    }

    /// A candidate over one new flaw, raised along a path that reaches its component and
    /// exposed along it, its sweep and path review ended.
    pub(super) fn candidate(
        store: &Store,
        actor: &AgentId,
        name: &str,
    ) -> (CandidateId, FlawId, PathId) {
        let component = component(store, actor, name);
        let path = path(store, actor, name, &[component]);
        let flaw = store.add_flaw(actor, &new_flaw(component, name)).unwrap();
        contributes(store, actor, flaw, path);
        reviewed(store, actor);
        let candidate = NewCandidate {
            title: text(name),
            ..new_candidate(FlawChoice::Existing(flaw), vec![path])
        };
        (store.add_candidate(actor, &candidate).unwrap(), flaw, path)
    }

    /// A candidate of one member flaw along `paths`.
    pub(super) fn new_candidate(flaw: FlawChoice, paths: Vec<PathId>) -> NewCandidate {
        NewCandidate {
            title: text("tail overflow"),
            description: text("unchecked copy"),
            cwe: std::num::NonZeroU32::new(120),
            members: vec![Member { flaw, step: None }],
            paths,
            premises: Vec::new(),
            evidence: Vec::new(),
        }
    }

    pub(crate) fn shortlisted(store: &Store, actor: &AgentId) -> CandidateId {
        let sequence = store
            .run_state()
            .unwrap()
            .progress
            .candidates
            .values()
            .sum::<u64>();
        let (id, _, _) = candidate(store, actor, &format!("overflow-{sequence}"));
        assess(store, actor, id);
        id
    }

    pub(crate) fn validated(store: &Store, actor: &AgentId) -> CandidateId {
        let id = shortlisted(store, actor);
        verdict(store, actor, id, Verdict::Validated);
        id
    }

    /// `actor`'s verdict on `id`'s current assessment.
    pub(crate) fn verdict(
        store: &Store,
        actor: &AgentId,
        id: CandidateId,
        verdict: Verdict,
    ) -> ValidationAttempt {
        store
            .record_validation(actor, id, verdict, &text("reviewed"))
            .unwrap()
    }

    /// `result` for the test of `id`'s latest verdict, with the artifacts a confirmed one
    /// needs.
    pub(crate) fn poc(
        store: &Store,
        actor: &AgentId,
        id: CandidateId,
        result: PocResult,
    ) -> PocAttempt {
        let artifacts = match result {
            PocResult::Confirmed => confirmed_artifacts(store, "payload.txt").to_vec(),
            _ => Vec::new(),
        };
        store
            .record_poc(
                actor,
                id,
                tested(store, id),
                result,
                &text("tested"),
                &artifacts,
            )
            .unwrap()
    }

    /// Records a test environment.
    pub(crate) fn environment(store: &Store, actor: &AgentId) -> EnvironmentId {
        let input = EnvironmentInput {
            name: text("container"),
            instructions: text("start the container"),
            target: None,
            endpoints: Vec::new(),
            credentials: Vec::new(),
        };
        store.record_environment(actor, &input).unwrap()
    }

    /// Resolves `premise` as holding, citing no deployment fact.
    pub(super) fn holds(store: &Store, actor: &AgentId, premise: PremiseId) -> ResolutionId {
        let holds = Resolution::Holds { deployment: None };
        store
            .resolve_premise(actor, premise, holds, &text("traced"))
            .unwrap()
    }

    pub(super) fn follow_up_unit(
        candidate: Option<CandidateId>,
        flaw: Option<FlawId>,
        invariants: Vec<Invariant>,
    ) -> UnitWork {
        UnitWork::FollowUp {
            question: text("does it hold?"),
            stopping: text("a resolution"),
            candidate,
            flaw,
            invariants,
        }
    }

    /// Every unit of `kind` planned, open or ended, in plan order.
    pub(super) fn units(store: &Store, kind: UnitKind) -> Vec<PlanUnit> {
        let filter = PlanFilter {
            kind: Some(kind),
            outcome: None,
        };
        Listing::drain(|page| store.plan(&filter, page)).unwrap()
    }

    /// Supporting evidence at `line` of `file`, in `DecodeStream`.
    pub(super) fn evidence_at(file: &str, line: u32) -> NewEvidence {
        NewEvidence {
            location: Location::new(text(file), line, line, Some(text("DecodeStream"))).unwrap(),
            role: EvidenceRole::Guard,
            stance: Stance::Supports,
            note: text("checked at the site"),
        }
    }

    pub(super) fn new_guarantee(kind: InvariantKind, evidence: Vec<NewEvidence>) -> NewGuarantee {
        NewGuarantee {
            kind,
            statement: text("a retry stages at most eight bytes"),
            preconditions: text("every retry means the input ran out"),
            function: Some(text("DecodeStream")),
            evidence,
            retry_status: None,
        }
    }

    /// Everything `root`'s report would render now.
    pub(crate) fn snapshot(store: &Store, root: FlawId) -> ReportSnapshot {
        store.read(|db| report::report_snapshot(db, root)).unwrap()
    }

    pub(crate) fn status(store: &Store, id: CandidateId) -> Status {
        store.candidate(id).unwrap().candidate.status
    }

    pub(crate) fn confidence(store: &Store, id: CandidateId) -> Option<ConfidenceTier> {
        store.candidate(id).unwrap().candidate.confidence
    }

    /// A child of the root with an agent job in `state`, standing in for the journal's
    /// record of a launched child; returns its id and its job's.
    pub(crate) fn child_agent(store: &Store, state: skyhook::job::JobState) -> (AgentId, i64) {
        let (index, job) = store
            .write(|db| {
                let root = "(SELECT id FROM agent WHERE parent IS NULL)";
                db.execute(
                    format!(
                        "INSERT INTO entry(public_id,agent,agent_ordinal,created_millis,kind) VALUES \
                         (randomblob(16),{root},(SELECT coalesce(max(agent_ordinal),-1)+1 FROM entry \
                         WHERE agent={root}),0,(SELECT name FROM entry_kind WHERE name!='agent_started' \
                         ORDER BY name LIMIT 1))"
                    ),
                    Vec::new(),
                )?;
                db.execute(
                    "INSERT INTO target(name) VALUES ('local') ON CONFLICT DO NOTHING",
                    Vec::new(),
                )?;
                let job = db.insert(
                    "INSERT INTO job(id,created,tool,role,arguments,accepts_input,background,\
                     location_target,location_workspace) VALUES ((SELECT coalesce(max(id),0)+1 FROM \
                     job),(SELECT max(seq) FROM entry),'agent','agent','{}',0,0,(SELECT id FROM target \
                     WHERE name='local'),x'')",
                    Vec::new(),
                )?;
                let index = stored(
                    db,
                    format!("SELECT coalesce(max(child_index),-1)+1 FROM agent WHERE parent={root}"),
                    Vec::new(),
                    |row| Ok(row.get::<i64>(0)?),
                )?;
                db.execute(
                    format!(
                        "INSERT INTO agent(parent,child_index,owner_job,available_depth) VALUES \
                         ({root},?1,?2,0)"
                    ),
                    params![index, job],
                )?;
                db.execute(
                    "INSERT INTO job_status(job,state,invocation,background,cancel_recorded,resident) \
                     VALUES (?1,?2,0,0,0,0)",
                    params![job, state.as_str()],
                )?;
                Ok((index, job))
            })
            .unwrap();
        (AgentId::root(store.id()).child(index as u32), job)
    }

    /// End `job` as the journal would.
    pub(crate) fn end_job(store: &Store, job: i64) {
        store
            .write(|db| {
                db.execute(
                    "UPDATE job_status SET state='completed',first_run_millis=0,ended_millis=0 WHERE \
                     job=?1",
                    params![job],
                )?;
                Ok(())
            })
            .unwrap();
    }

    /// The candidate's first root flaw, whose finding reports it.
    pub(crate) fn root_of(store: &Store, id: CandidateId) -> FlawId {
        store.candidate(id).unwrap().roots[0]
    }

    /// The candidate's latest verdict, which a test names.
    pub(crate) fn tested(store: &Store, id: CandidateId) -> ValidationId {
        Listing::drain(|page| store.validation_attempts(id, page))
            .unwrap()
            .pop()
            .unwrap()
            .id
    }

    /// Everything `phase` owes that can start now.
    pub(super) fn entries(store: &Store, phase: Phase) -> Vec<QueueEntry> {
        Listing::drain(|page| store.read(|db| queues::page(db, phase, page))).unwrap()
    }

    /// The subjects `phase` lists.
    pub(super) fn queued(store: &Store, phase: Phase) -> Vec<WorkSubject> {
        (entries(store, phase).into_iter())
            .map(|entry| entry.subject)
            .collect()
    }

    pub(in crate::store) fn contradiction(note: &str) -> Vec<NewEvidence> {
        vec![NewEvidence {
            stance: Stance::Contradicts,
            note: text(note),
            ..evidence_at("check.c", 9)
        }]
    }

    pub(in crate::store) fn assess(store: &Store, actor: &AgentId, id: CandidateId) {
        assess_at(store, actor, id, HIGH, Assessor::Researcher).unwrap();
    }

    /// `assessor`'s established assessment of `id` at `scored`.
    pub(in crate::store) fn assess_at(
        store: &Store,
        actor: &AgentId,
        id: CandidateId,
        scored: &str,
        assessor: Assessor,
    ) -> Result<AssessmentResult> {
        store.assess(
            actor,
            id,
            vector(scored),
            Confidence::Established,
            &text("remote memory corruption"),
            assessor,
        )
    }

    pub(in crate::store) fn flaw_revision(store: &Store, flaw: FlawId) -> u32 {
        store.flaw(flaw).unwrap().record.flaw.revision
    }

    /// The flaw and path of a candidate's first member.
    pub(in crate::store) fn member(store: &Store, id: CandidateId) -> (FlawId, PathId) {
        let recorded = store.candidate(id).unwrap();
        (recorded.members[0].flaw.id, recorded.paths[0])
    }
}
