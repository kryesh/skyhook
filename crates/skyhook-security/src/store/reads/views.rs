//! What node reads return: each kind's view of a node and its neighbourhood.

use schemars::JsonSchema;
use serde::Serialize;

use super::super::*;

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

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct SourceRef {
    pub id: SourceId,
    pub title: String,
    pub location: Location,
    pub trust: Trust,
    pub status: InventoryStatus,
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
    pub sources: Vec<SourceRef>,
    pub premises: Vec<PremiseRef>,
    pub candidates: Vec<CandidateRef>,
    /// While unswept and a recorded source enters it, it owes a sweep.
    pub status: PathStatus,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct SourceView {
    #[serde(flatten)]
    pub head: Head,
    /// Absent once merged: its location is free for another source.
    #[serde(flatten)]
    pub live: Option<LiveSource>,
}

/// What a live source holds.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct LiveSource {
    pub location: Location,
    pub trust: Trust,
    pub status: InventoryStatus,
    pub paths: Vec<Ref>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct PremiseView {
    #[serde(flatten)]
    pub head: Head,
    pub scope: PremiseScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    pub status: PremiseStatus,
    /// Contradicting evidence came after its latest status.
    pub disputed: bool,
    pub flaws: Vec<Ref>,
    pub paths: Vec<Ref>,
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
    pub premises: Vec<PremiseRef>,
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
    pub sources: Vec<SourceRef>,
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
    pub tests: Vec<TestView>,
    /// The test its testing stands on: its latest that is not pending, recorded at its latest
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
    Source(SourceView),
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
            Self::Source(view) => &view.head,
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
