//! What node reads return: each kind's view of a node and its neighbourhood.

use schemars::JsonSchema;
use serde::Serialize;

use super::super::*;

/// A result recorded outside the work owning it that differs from the standing one, which that
/// work decides: the latest, with the task and agent that recorded it.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Proposal<T> {
    pub value: T,
    pub event: EventId,
    /// The work it was recorded within; none outside every task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
    pub agent: AgentId,
}

/// A proposed assessment.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Scored {
    pub vector: String,
    pub severity: Severity,
    pub score: f64,
}

/// A proposal of the result a phase records on its subject.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum Proposed {
    Premise(PremiseStatus),
    Flaw(FlawStatus),
    Assessment(Scored),
    Verdict(Verdict),
}

/// What every node view leads with.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Head {
    pub id: NodeId,
    pub title: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_into: Option<NodeId>,
    pub evidence: Vec<EvidenceView>,
    /// The latest tasks on it or on a node merged into it, newest first: `task` reads each.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<TaskId>,
}

/// A neighbouring node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Ref {
    pub id: NodeId,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct PremiseRef {
    pub id: PremiseId,
    pub title: String,
    pub scope: PremiseScope,
    pub status: PremiseStatus,
}

/// A decided premise of a flaw leaving a state wrong, which the state's check may contradict:
/// the decision its standing rests on, the task that decision was recorded within, and how much
/// evidence it carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct DecidedPremise {
    #[serde(flatten)]
    pub premise: PremiseRef,
    pub decided: EventId,
    /// None when decided outside every task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskId>,
    pub evidence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct FlawRef {
    pub id: FlawId,
    pub title: String,
    pub status: FlawStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CandidateRef {
    pub id: CandidateId,
    pub title: String,
    pub status: Status,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct EvidenceView {
    pub id: EvidenceId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    /// Where it was observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentId>,
    pub stance: Stance,
    pub note: String,
    pub event: EventId,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Step {
    pub position: u32,
    pub component: Ref,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    pub note: String,
}

/// A state's scope, with an input state's trust.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum Scoped {
    Input { trust: Trust },
    Handed,
    Instance,
    Stored,
}

/// A state a path reads or writes, at the location it does.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct AccessRef {
    pub id: StateId,
    pub title: String,
    #[serde(flatten)]
    pub scope: Scoped,
    pub status: InventoryStatus,
    pub access: Access,
    pub location: Location,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ComponentView {
    #[serde(flatten)]
    pub head: Head,
    pub status: InventoryStatus,
    pub files: Vec<String>,
    pub interfaces: Vec<InterfaceName>,
    /// The live paths stepping through it.
    pub paths: Vec<Ref>,
    pub flaws: Vec<FlawRef>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct PathView {
    #[serde(flatten)]
    pub head: Head,
    pub steps: Vec<Step>,
    /// The states its flow reads or writes.
    pub states: Vec<AccessRef>,
    pub premises: Vec<PremiseRef>,
    pub candidates: Vec<CandidateRef>,
    /// While unswept and it reads a recorded input or handed state, it owes a sweep.
    pub status: PathStatus,
}

/// A path's read or write of a state, at the location it does.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct AccessView {
    pub path: Ref,
    pub access: Access,
    pub location: Location,
}

/// An input state's trust and the read defining it.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct InputView {
    pub location: Location,
    pub trust: Trust,
}

/// The event that last recorded a state's check across calls, and whether it stands: no access,
/// edit or merge came since, and no premise its checks rest on failed or was contradicted.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CheckView {
    pub event: EventId,
    pub note: Option<String>,
    pub current: bool,
    /// The premises its checks cleared it on.
    pub rests_on: Vec<PremiseRef>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct StateView {
    #[serde(flatten)]
    pub head: Head,
    pub scope: StateScope,
    pub status: InventoryStatus,
    /// A live input state's trust and defining read; a merged one's location is free for another.
    #[serde(flatten)]
    pub input: Option<InputView>,
    /// Each path's reads and writes of it, by path.
    pub accesses: Vec<AccessView>,
    /// The flaws leaving it wrong.
    pub flaws: Vec<FlawRef>,
    /// The decided premises of those flaws.
    pub premises: Vec<DecidedPremise>,
    /// Its latest check across calls; none until one is recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckView>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct PremiseView {
    #[serde(flatten)]
    pub head: Head,
    pub scope: PremiseScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    pub status: PremiseStatus,
    /// A status its settle, or for a deployment premise the user, has yet to decide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed: Option<Proposal<PremiseStatus>>,
    /// A proposal or contradicting evidence came after its status.
    pub disputed: bool,
    pub flaws: Vec<Ref>,
    pub paths: Vec<Ref>,
    /// The states whose checks rest on it.
    pub states: Vec<Ref>,
    /// The premises it holds only if they do.
    pub rests_on: Vec<PremiseRef>,
    /// The premises holding only if it does.
    pub rested_on_by: Vec<PremiseRef>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Chained {
    pub id: CandidateId,
    pub title: String,
    pub position: u32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct FlawView {
    #[serde(flatten)]
    pub head: Head,
    pub component: Ref,
    pub location: Location,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwe: Option<u32>,
    pub status: FlawStatus,
    /// A status its chain has yet to decide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed: Option<Proposal<FlawStatus>>,
    pub premises: Vec<PremiseRef>,
    /// The states it leaves wrong.
    pub states: Vec<Ref>,
    pub candidates: Vec<Chained>,
    /// Its report, as its group's root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding: Option<FindingId>,
}

/// A flaw of a candidate's chain, with the premises it depends on.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ChainLink {
    #[serde(flatten)]
    pub flaw: FlawRef,
    pub premises: Vec<PremiseRef>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct AssessmentView {
    pub vector: String,
    pub severity: Severity,
    pub score: f64,
    pub note: Option<String>,
    /// Nothing it judged changed after what it stands on.
    pub current: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct VerdictView {
    pub verdict: Verdict,
    pub note: Option<String>,
    /// Nothing it judged changed after what it stands on.
    pub current: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ArtifactView {
    pub path: String,
    pub role: ArtifactRole,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct TestView {
    pub id: TestId,
    pub result: TestResult,
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentId>,
    pub artifacts: Vec<ArtifactView>,
}

/// A path a candidate's scenario performs.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ActionView {
    pub path: Ref,
    pub steps: Vec<Step>,
    pub states: Vec<AccessRef>,
    /// The premises its path relies on.
    pub premises: Vec<PremiseRef>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CandidateView {
    #[serde(flatten)]
    pub head: Head,
    /// The paths its scenario performs, in order: the first is where the attacker starts.
    pub actions: Vec<ActionView>,
    /// Its flaws from the root.
    pub chain: Vec<ChainLink>,
    pub status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<ConfidenceTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assessment: Option<AssessmentView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<VerdictView>,
    /// An assessment its review has yet to decide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed_assessment: Option<Proposal<Scored>>,
    /// A verdict its review has yet to decide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposed_verdict: Option<Proposal<Verdict>>,
    pub tests: Vec<TestView>,
    /// The test its testing stands on: its latest that is not pending, recorded at its standing
    /// verdict.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settling_test: Option<TestId>,
    pub environments: Vec<EnvironmentView>,
}

/// A scenario a finding reports.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Scenario {
    pub candidate: CandidateRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<ConfidenceTier>,
    pub severity: Severity,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct FindingView {
    #[serde(flatten)]
    pub head: Head,
    pub root: Ref,
    pub scenarios: Vec<Scenario>,
    /// It reports some scenarios, as they stand: `generate` publishes it.
    pub current: bool,
}

/// A node with its neighbourhood.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeView {
    Component(ComponentView),
    Path(PathView),
    State(StateView),
    Premise(PremiseView),
    Flaw(FlawView),
    Candidate(CandidateView),
    Finding(FindingView),
}

impl NodeView {
    pub fn head(&self) -> &Head {
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
pub struct EnvironmentView {
    pub id: EnvironmentId,
    pub instructions: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<CandidateId>,
    pub serves: Vec<Served>,
    pub event: EventId,
}

impl Keyed for EnvironmentView {
    fn key(&self) -> i64 {
        self.id.0
    }
}
