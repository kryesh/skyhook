//! What node reads return: each kind's view of a node and its neighbourhood.

use schemars::JsonSchema;
use serde::Serialize;

use super::super::*;
use crate::cvss::Severity;

/// A result recorded outside the work owning it that differs from the standing one, which that
/// work decides: the latest, with the task and agent that recorded it.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub(crate) struct Proposal<T> {
    pub(crate) value: T,
    pub(crate) event: EventId,
    /// The work it was recorded within; none outside every task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) task: Option<TaskId>,
    pub(crate) agent: AgentId,
}

/// A proposed assessment.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub(crate) struct Scored {
    pub(crate) vector: String,
    pub(crate) severity: Severity,
    pub(crate) score: f64,
}

/// A proposal of the result a phase records on its subject.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub(crate) enum Proposed {
    Premise(PremiseStatus),
    Flaw(FlawStatus),
    Assessment(Scored),
    Verdict(Verdict),
}

/// What every node view leads with.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct Head {
    pub(crate) id: NodeId,
    pub(crate) title: String,
    pub(crate) body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) merged_into: Option<NodeId>,
    pub(crate) evidence: Vec<EvidenceView>,
    /// The latest tasks on it or on a node merged into it, newest first: `task` reads each.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tasks: Vec<TaskId>,
}

/// A neighbouring node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Ref {
    pub(crate) id: NodeId,
    pub(crate) title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct PremiseRef {
    pub(crate) id: PremiseId,
    pub(crate) title: String,
    pub(crate) scope: PremiseScope,
    pub(crate) status: PremiseStatus,
}

/// A decided premise of a flaw leaving a state wrong, which the state's check may contradict:
/// the decision its standing rests on, the task that decision was recorded within, and how much
/// evidence it carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct DecidedPremise {
    #[serde(flatten)]
    pub(crate) premise: PremiseRef,
    pub(crate) decided: EventId,
    /// None when decided outside every task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) task: Option<TaskId>,
    pub(crate) evidence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct FlawRef {
    pub(crate) id: FlawId,
    pub(crate) title: String,
    pub(crate) status: FlawStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct CandidateRef {
    pub(crate) id: CandidateId,
    pub(crate) title: String,
    pub(crate) status: Status,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct EvidenceView {
    pub(crate) id: EvidenceId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) location: Option<Location>,
    /// Where it was observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) environment: Option<EnvironmentId>,
    pub(crate) stance: Stance,
    pub(crate) note: String,
    pub(crate) event: EventId,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct Step {
    pub(crate) position: u32,
    pub(crate) component: Ref,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) location: Option<Location>,
    pub(crate) note: String,
}

/// An input state's trust.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(inline)]
pub(crate) struct Trusted {
    pub(crate) trust: Trust,
}

/// A state a path reads or writes, at the location it does.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct AccessRef {
    pub(crate) id: StateId,
    pub(crate) title: String,
    #[serde(flatten)]
    pub(crate) scope: StateScoped<Trusted>,
    pub(crate) status: InventoryStatus,
    pub(crate) access: Access,
    pub(crate) location: Location,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct ComponentView {
    #[serde(flatten)]
    pub(crate) head: Head,
    pub(crate) status: InventoryStatus,
    pub(crate) files: Vec<String>,
    pub(crate) interfaces: Vec<InterfaceName>,
    /// The live paths stepping through it.
    pub(crate) paths: Vec<Ref>,
    pub(crate) flaws: Vec<FlawRef>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct PathView {
    #[serde(flatten)]
    pub(crate) head: Head,
    pub(crate) steps: Vec<Step>,
    /// The states its flow reads or writes.
    pub(crate) states: Vec<AccessRef>,
    pub(crate) premises: Vec<PremiseRef>,
    pub(crate) candidates: Vec<CandidateRef>,
    /// While unswept and it reads a recorded input or handed state, it owes a sweep.
    pub(crate) status: PathStatus,
}

/// A path's read or write of a state, at the location it does.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct AccessView {
    pub(crate) path: Ref,
    pub(crate) access: Access,
    pub(crate) location: Location,
}

/// An input state's trust and the read defining it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct InputView {
    /// The location of its defining read
    pub(crate) location: Location,
    pub(crate) trust: Trust,
}

/// A live input state's trust and defining read; a merged one's location is free for another.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[schemars(inline)]
pub(crate) struct LiveInput {
    #[serde(flatten)]
    pub(crate) read: Option<InputView>,
}

/// The event that last recorded a state's check across calls, and whether it stands: no access,
/// edit or merge came since, and no premise its checks rest on failed or was contradicted.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct CheckView {
    pub(crate) event: EventId,
    pub(crate) note: Option<String>,
    pub(crate) current: bool,
    /// The premises its checks cleared it on.
    pub(crate) rests_on: Vec<PremiseRef>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct StateView {
    #[serde(flatten)]
    pub(crate) head: Head,
    #[serde(flatten)]
    pub(crate) scope: StateScoped<LiveInput>,
    pub(crate) status: InventoryStatus,
    /// Each path's reads and writes of it, by path.
    pub(crate) accesses: Vec<AccessView>,
    /// The flaws leaving it wrong.
    pub(crate) flaws: Vec<FlawRef>,
    /// The decided premises of those flaws.
    pub(crate) premises: Vec<DecidedPremise>,
    /// Its latest check across calls; none until one is recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) check: Option<CheckView>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct PremiseView {
    #[serde(flatten)]
    pub(crate) head: Head,
    pub(crate) scope: PremiseScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) location: Option<Location>,
    pub(crate) status: PremiseStatus,
    /// A status its settle, or for a deployment premise the user, has yet to decide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) proposed: Option<Proposal<PremiseStatus>>,
    /// A proposal or contradicting evidence came after its status.
    pub(crate) disputed: bool,
    pub(crate) flaws: Vec<Ref>,
    pub(crate) paths: Vec<Ref>,
    /// The states whose checks rest on it.
    pub(crate) states: Vec<Ref>,
    /// The premises it holds only if they do.
    pub(crate) rests_on: Vec<PremiseRef>,
    /// The premises holding only if it does.
    pub(crate) rested_on_by: Vec<PremiseRef>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct Chained {
    pub(crate) id: CandidateId,
    pub(crate) title: String,
    pub(crate) position: u32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct FlawView {
    #[serde(flatten)]
    pub(crate) head: Head,
    pub(crate) component: Ref,
    pub(crate) location: Location,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cwe: Option<u32>,
    pub(crate) status: FlawStatus,
    /// A status its chain has yet to decide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) proposed: Option<Proposal<FlawStatus>>,
    pub(crate) premises: Vec<PremiseRef>,
    /// The states it leaves wrong.
    pub(crate) states: Vec<Ref>,
    pub(crate) candidates: Vec<Chained>,
    /// Its report, as its group's root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) finding: Option<FindingId>,
}

/// A flaw of a candidate's chain, with the premises it depends on.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct ChainLink {
    #[serde(flatten)]
    pub(crate) flaw: FlawRef,
    pub(crate) premises: Vec<PremiseRef>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct AssessmentView {
    pub(crate) vector: String,
    pub(crate) severity: Severity,
    pub(crate) score: f64,
    pub(crate) note: Option<String>,
    /// Nothing it judged changed after what it stands on.
    pub(crate) current: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct VerdictView {
    pub(crate) verdict: Verdict,
    pub(crate) note: Option<String>,
    /// Nothing it judged changed after what it stands on.
    pub(crate) current: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct ArtifactView {
    pub(crate) path: String,
    pub(crate) role: ArtifactRole,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct TestView {
    pub(crate) id: TestId,
    pub(crate) result: TestResult,
    pub(crate) note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) environment: Option<EnvironmentId>,
    pub(crate) artifacts: Vec<ArtifactView>,
}

/// A path a candidate's scenario performs.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct ActionView {
    pub(crate) path: Ref,
    pub(crate) steps: Vec<Step>,
    pub(crate) states: Vec<AccessRef>,
    /// The premises its path relies on.
    pub(crate) premises: Vec<PremiseRef>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct CandidateView {
    #[serde(flatten)]
    pub(crate) head: Head,
    /// The paths its scenario performs, in order: the first is where the attacker starts.
    pub(crate) actions: Vec<ActionView>,
    /// Its flaws from the root.
    pub(crate) chain: Vec<ChainLink>,
    pub(crate) status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) confidence: Option<ConfidenceTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) assessment: Option<AssessmentView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) verdict: Option<VerdictView>,
    /// An assessment its review has yet to decide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) proposed_assessment: Option<Proposal<Scored>>,
    /// A verdict its review has yet to decide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) proposed_verdict: Option<Proposal<Verdict>>,
    pub(crate) tests: Vec<TestView>,
    /// The test its testing stands on: its latest that is not pending, recorded at its standing
    /// verdict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) settling_test: Option<TestId>,
    pub(crate) environments: Vec<EnvironmentView>,
}

/// A scenario a finding reports.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct Scenario {
    pub(crate) candidate: CandidateRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) confidence: Option<ConfidenceTier>,
    pub(crate) severity: Severity,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct FindingView {
    #[serde(flatten)]
    pub(crate) head: Head,
    pub(crate) root: Ref,
    pub(crate) scenarios: Vec<Scenario>,
    /// It reports some scenarios, as they stand: `generate` publishes it.
    pub(crate) current: bool,
}

/// A node with its neighbourhood.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum NodeView {
    Component(ComponentView),
    Path(PathView),
    State(StateView),
    Premise(PremiseView),
    Flaw(FlawView),
    Candidate(CandidateView),
    Finding(FindingView),
}

impl NodeView {
    pub(crate) fn head(&self) -> &Head {
        match self {
            Self::Component(view) => &view.head,
            Self::Path(view) => &view.head,
            Self::State(view) => &view.head,
            Self::Premise(view) => &view.head,
            Self::Flaw(view) => &view.head,
            Self::Candidate(view) => &view.head,
            Self::Finding(view) => &view.head,
        }
    }
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct EnvironmentView {
    pub(crate) id: EnvironmentId,
    pub(crate) instructions: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) candidate: Option<CandidateId>,
    pub(crate) serves: Vec<Served>,
    pub(crate) event: EventId,
}

impl Keyed for EnvironmentView {
    fn key(&self) -> i64 {
        self.id.0
    }
}
