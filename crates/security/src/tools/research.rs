use serde_json::Value;
use skyhook::identity::AgentId;
use skyhook::tool::catalog::RegistryError;
use skyhook::tool::policy::{Capability, CapabilitySet};
use skyhook::tool::{ToolContext, ToolError, ToolRegistryBuilder};

use super::{Action, Placement, holds, options, output};
use crate::{
    capabilities::SecurityCapability,
    cvss::Cvss40,
    reporting::{self, Generated},
    store::{
        Assessment, Begun, CandidateId, Edge, EnvironmentId, EvidenceId, FindingId, FlawId, Listed,
        NewEnvironment, NewEvidence, NewTest, NodeId, StatusChange, Store, TestId, Text,
    },
};

actions! {
    enum Research {
        /// When your brief assigns you scheduled work, begin it before anything else. Its result
        /// names what changed for each subject since its last task in this phase.
        /// Afterwards, job events list changes other agents make to what it rests on; re-read
        /// those records before you decide on them. → {task, scope: the run's scope, {components:
        /// [{id, title}], interfaces: [{interface, component}], excluded: [{premise, title}]
        /// (failing deployment premises), assumed: [{premise, title, status}] (the other
        /// deployment premises)}, changes?: per subject worked before
        /// in the phase, what changed since, proposals?: per subject, each result recorded
        /// outside this phase for it to decide, premises?: per state checked, the decided premises of the
        /// flaws leaving it wrong}
        begin => Begin(crate::store::Begin) -> Begun,
        /// Create a node, its fields inside `record`: `{action: "upsert", record: {kind: "path",
        /// title, body}}`; or edit node `id` of its `kind`, the fields given: `{action: "upsert",
        /// record: {kind: "path", id, title}}`. → node id
        upsert => Upsert { record: crate::store::Upsert } -> NodeId,
        /// Add edges
        link => Link { links: Listed<Edge> } -> (),
        /// Remove edges
        unlink => Unlink { links: Listed<Edge> } -> (),
        /// Observations about one node. → evidence ids
        evidence => Evidence { node: NodeId, items: Listed<NewEvidence> } -> Vec<EvidenceId>,
        merge => Merge(crate::store::Merge) -> (),
        /// Decide a premise, judge a flaw, mark a path swept or unswept, mark a state `checked`
        /// across calls, or retract a state, path or component recorded in error: it then stands
        /// in no work or count until marked `recorded` (a path `unswept`) again.
        /// `{action: "set_status", record: {kind: "path", id, status: "swept"}, note}`. A
        /// `checked` state names in `rests_on` the premises its clearing relies on:
        /// `{record: {kind: "state", id, status: "checked", rests_on: [premise ids]}, note}`; each
        /// owes a settle while undecided, and one failing or contradicted owes the check again
        set_status => SetStatus { record: StatusChange, note: Text } -> (),
        /// The store scores `vector`: record the vector, never compute a score. → {severity,
        /// score, shortlisted}
        assess => Assess {
            candidate: CandidateId,
            /// `CVSS:4.0/AV:_/AC:_/AT:_/PR:_/UI:_/VC:_/VI:_/VA:_/SC:_/SI:_/SA:_`, every base metric once, in that order
            vector: Cvss40,
            note: Text,
        } -> Assessment,
        verdict => Verdict { candidate: CandidateId, verdict: crate::store::Verdict, note: Text } -> (),
        /// A `pending` result's note says what it waits for. → test id
        test => Test(NewTest) -> TestId,
        /// A running instance, for one candidate or any: how to work in it, and the interfaces
        /// it serves. → environment id
        environment => Environment(NewEnvironment) -> EnvironmentId,
        /// The root flaw's report: markdown citing node and test ids. → finding id
        report => Report { root: FlawId, title: Text, body: Text } -> FindingId,
        /// Render the run's reports/ from the store: a folder per root with a current
        /// finding, and an index. → {reports: root ids written, unwritten: [{root, error}]}
        generate => Generate {} -> Generated,
    }
    unions: [record]
}

impl Action for Research {
    fn permitted(action: Name, capabilities: &CapabilitySet) -> bool {
        use SecurityCapability::{Coordinate, Research, Scheduled, Test, Verdict};
        match action {
            Name::begin => holds(capabilities, [Scheduled.capability()]),
            // A verdict judges research, so it needs both authorities; a test needs its own,
            // so an agent that tests and records what it observes never decides verdicts.
            Name::verdict => holds(capabilities, [Research, Verdict].map(|c| c.capability())),
            Name::test => holds(capabilities, [Test.capability()]),
            Name::report => holds(capabilities, [Verdict.capability(), Capability::Write]),
            Name::generate => holds(capabilities, [Coordinate.capability()]),
            _ => holds(capabilities, [Research.capability()]),
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
            Self::Upsert(a) => output::<result::Upsert>(store.upsert(actor, &a.record)),
            Self::Link(a) => output::<result::Link>(store.link(actor, &a.links)),
            Self::Unlink(a) => output::<result::Unlink>(store.unlink(actor, &a.links)),
            Self::Evidence(a) => {
                output::<result::Evidence>(store.evidence(actor, a.node, &a.items))
            }
            Self::Merge(merge) => output::<result::Merge>(store.merge(actor, &merge)),
            Self::SetStatus(a) => {
                output::<result::SetStatus>(store.set_status(actor, a.record, &a.note))
            }
            Self::Assess(a) => {
                output::<result::Assess>(store.assess(actor, a.candidate, a.vector, &a.note))
            }
            Self::Verdict(a) => {
                output::<result::Verdict>(store.verdict(actor, a.candidate, a.verdict, &a.note))
            }
            Self::Test(test) => output::<result::Test>(store.record_test(actor, &test)),
            Self::Environment(environment) => {
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
        "Record the run's research graph. Result: JobView<T>, T as each action gives after →, else null",
        options::<Research>(),
        |store, actor, context, research| research.execute(store, actor, context),
    )
}
