use serde_json::Value;
use skyhook::{
    identity::AgentId,
    tool::{
        RegistryError, ToolContext, ToolError, ToolRegistryBuilder,
        policy::{Capability, CapabilitySet},
    },
};

use super::{Action, Placement, holds, options, output};
use crate::{
    capabilities::SecurityCapability,
    cvss::Cvss40,
    reporting::{self, Generated},
    store::{
        Assessment, CandidateId, EnvironmentId, EvidenceId, FindingId, FlawId, NewEnvironment,
        NewEvidence, NewTest, NodeId, Served, Serves, StatusInput, Store, TaskId, TestArtifact,
        TestId, TestResult, Text, Upsert, UpsertInput,
    },
};

actions! {
    enum Research {
        /// When your brief assigns you scheduled work, begin it before anything else: one phase
        /// and its subjects.
        /// Afterwards, job events list changes other agents make to what it rests on; re-read
        /// those records before you decide on them.
        begin => Begin(crate::store::Begin) -> TaskId,
        /// Create a node of `kind` (no `id`: its fields), or edit node `id` (the fields given).
        /// A source at a location already recorded returns the source there.
        upsert => Upsert(UpsertInput) -> NodeId,
        /// Add edges; an ordered one at `position`, or last.
        link => Link { links: Vec<crate::store::Link> } -> (),
        /// Remove edges; a path step by its position.
        unlink => Unlink { links: Vec<crate::store::Unlink> } -> (),
        /// Observations about one node; contradicting evidence disputes it.
        evidence => Evidence { node: NodeId, items: Vec<NewEvidence> } -> Vec<EvidenceId>,
        /// Fold a duplicate node of any kind but a finding into `into`, one of its kind: its
        /// edges move to `into`.
        merge => Merge(crate::store::Merge) -> (),
        /// Decide a premise, judge a flaw, mark a path swept or unswept, or retract a source,
        /// path or component recorded in error: it then stands in no work or count until marked
        /// `recorded` (a path `unswept`) again.
        set_status => SetStatus(StatusInput) -> (),
        /// The store scores `vector` and returns its severity and score: record the vector,
        /// never compute a score.
        assess => Assess {
            candidate: CandidateId,
            /// `CVSS:4.0/AV:_/AC:_/AT:_/PR:_/UI:_/VC:_/VI:_/VA:_/SC:_/SI:_/SA:_`, every base
            /// metric once, in that order.
            vector: Cvss40,
            note: Text,
        } -> Assessment,
        verdict => Verdict { candidate: CandidateId, verdict: crate::store::Verdict, note: Text } -> (),
        /// `failed`: evidence shows the scenario does not work as formulated; `pending`: waits on the user (say
        /// for what); `unavailable`: no real demonstration can run here; `declined`: the user
        /// turned it down.
        test => Test {
            candidate: CandidateId,
            result: TestResult,
            note: Text,
            environment: Option<EnvironmentId>,
            /// Files below the run's artifacts directory, each tagged with its role.
            #[serde(default)]
            artifacts: Vec<TestArtifact>,
        } -> TestId,
        /// A running instance, for one candidate or any: how to work in it, and the interfaces
        /// it serves.
        environment => Environment {
            instructions: Text,
            candidate: Option<CandidateId>,
            #[serde(default)]
            #[schemars(with = "Vec<Served>")]
            serves: Serves,
        } -> EnvironmentId,
        /// The root flaw's report: markdown citing node and test ids.
        report => Report { root: FlawId, title: Text, body: Text } -> FindingId,
        /// Render the run's reports/ from the store: a folder per root with a current
        /// finding, and an index.
        generate => Generate {} -> Generated,
    }
    unions: [links]
}

impl Action for Research {
    fn permitted(action: Name, capabilities: &CapabilitySet) -> bool {
        let [research, verdict, test, scheduled, coordinate] =
            SecurityCapability::ALL.map(SecurityCapability::capability);
        match action {
            Name::begin => holds(capabilities, [scheduled]),
            // A verdict judges research, so it needs both authorities; a test needs its own,
            // so an agent that tests and records what it observes never decides verdicts.
            Name::verdict => holds(capabilities, [research, verdict]),
            Name::test => holds(capabilities, [test]),
            Name::report => holds(capabilities, [verdict, Capability::Write]),
            Name::generate => holds(capabilities, [coordinate]),
            _ => holds(capabilities, [research]),
        }
    }

    /// A test admits its artifact files, and generation writes report files, around their
    /// store work.
    fn placement(&self) -> Placement {
        match self {
            Self::Test(_) | Self::Generate(_) => Placement::Outside,
            _ => Placement::Writer,
        }
    }
}

impl Research {
    fn execute(
        self,
        store: &Store,
        actor: &AgentId,
        context: &ToolContext,
    ) -> Result<Value, ToolError> {
        match self {
            Self::Begin(work) => output::<result::Begin>(store.begin(actor, &work)),
            Self::Upsert(input) => output::<result::Upsert>(
                Upsert::try_from(input).and_then(|upsert| store.upsert(actor, &upsert)),
            ),
            Self::Link(a) => output::<result::Link>(store.link(actor, &a.links)),
            Self::Unlink(a) => output::<result::Unlink>(store.unlink(actor, &a.links)),
            Self::Evidence(a) => {
                output::<result::Evidence>(store.evidence(actor, a.node, &a.items))
            }
            Self::Merge(merge) => output::<result::Merge>(store.merge(actor, &merge)),
            Self::SetStatus(input) => {
                let (change, note) = input.change();
                output::<result::SetStatus>(store.set_status(actor, change, note))
            }
            Self::Assess(a) => {
                output::<result::Assess>(store.assess(actor, a.candidate, a.vector, &a.note))
            }
            Self::Verdict(a) => {
                output::<result::Verdict>(store.verdict(actor, a.candidate, a.verdict, &a.note))
            }
            Self::Test(a) => {
                let test = NewTest {
                    candidate: a.candidate,
                    result: a.result,
                    note: a.note,
                    environment: a.environment,
                    artifacts: a.artifacts,
                };
                output::<result::Test>(store.record_test(actor, &test))
            }
            Self::Environment(a) => {
                let environment = NewEnvironment {
                    instructions: a.instructions,
                    candidate: a.candidate,
                    serves: a.serves,
                };
                output::<result::Environment>(store.environment(actor, &environment))
            }
            Self::Report(a) => {
                output::<result::Report>(store.report(actor, a.root, &a.title, &a.body))
            }
            Self::Generate(_) => {
                output::<result::Generate>(reporting::generate(store, &|| context.is_cancelled()))
            }
        }
    }
}

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    super::register(
        builder,
        "research",
        "Record the run's research graph: begin your work, create and edit nodes, link them, record evidence, merge duplicates, set statuses, assess, judge and test candidates, describe environments, and report root flaws. Result: JobView<T>: begin → task id; upsert → node id; evidence → evidence ids; assess → {severity, score, shortlisted}; test, environment, report → the new id; generate → {reports: root ids written, unwritten: [{root, error}]}; the rest → null.",
        options::<Research>(),
        |store, actor, context, research| research.execute(store, actor, context),
    )
}
