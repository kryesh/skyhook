use std::{fmt, num::NonZeroU16, str::FromStr};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::{DeserializeFromStr, SerializeDisplay};
use skyhook::{
    Bounded, Prose,
    identity::{decode_hex, write_hex},
    named_enum, string_newtype,
};

use super::{Error, Record};
use crate::cvss::Severity;

/// The id of a record a lookup can miss, with the table that holds it.
pub(crate) trait RecordId: Copy + Into<i64> {
    const RECORD: Record;
    /// Selects the row with id `?1`.
    const KNOWN: &'static str;
}

/// A positive SQLite row id; one naming a [`Record`] carries it and its table.
macro_rules! ids {
    ($($name:ident $(: $record:ident in $table:literal)?),* $(,)?) => {$(
        $(impl RecordId for $name {
            const RECORD: Record = Record::$record;
            const KNOWN: &'static str = concat!("SELECT 1 FROM ", $table, " WHERE id=?1");
        })?
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "i64", into = "i64")]
        pub struct $name(pub(super) i64);
        impl JsonSchema for $name {
            fn inline_schema() -> bool { true }
            fn schema_name() -> std::borrow::Cow<'static, str> { stringify!($name).into() }
            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({"type": "integer", "minimum": 1})
            }
        }
        impl $name { pub const fn get(self) -> i64 { self.0 } }
        impl TryFrom<i64> for $name {
            type Error = Error;
            fn try_from(value: i64) -> Result<Self, Error> {
                if value > 0 { Ok(Self(value)) } else { Err(Error::NonPositiveId) }
            }
        }
        impl From<$name> for i64 { fn from(value: $name) -> Self { value.0 } }
        impl fmt::Display for $name { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) } }
    )*};
}

ids!(
    ScopeId: Scope in "scope",
    DeploymentId: Deployment in "deployment",
    PrincipalId: Principal in "principal",
    SourceId: Source in "source",
    LocationId,
    ComponentId: Component in "component",
    PathId: Path in "path",
    UnitId: Unit in "plan_unit",
    OutcomeId,
    GuaranteeId: Guarantee in "guarantee",
    RetrySiteId,
    DeferredId: Deferred in "deferred",
    FlawId: Flaw in "flaw",
    RefutationId: Refutation in "flaw_refutation",
    RelationId,
    UnderminingId,
    PremiseId: Premise in "premise",
    ResolutionId,
    EvidenceId,
    ExposureId: Exposure in "exposure",
    CandidateId: Candidate in "candidate",
    AssessmentId: Assessment in "assessment",
    ValidationId: Validation in "validation",
    PocId: Poc in "poc",
    PocStartId,
    EnvironmentId,
    ContractId: Contract in "contract",
    FindingId: Finding in "finding",
    EventId
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    #[error("text must not be blank")]
    Blank,
    #[error("text exceeds 1 MB")]
    TooLong,
}

string_newtype! {
    /// Agent-authored text: nonblank and at most 1 MB.
    #[derive(PartialOrd, Ord)]
    pub struct Text(TextError) = |value| {
        if value.trim().is_empty() {
            Err(TextError::Blank)
        } else if value.len() > 1_000_000 {
            Err(TextError::TooLong)
        } else {
            Ok(())
        }
    }, schema = schemars::json_schema!({"type": "string", "pattern": "\\S", "maxLength": 1_000_000});
}

impl From<Prose> for Text {
    fn from(prose: Prose) -> Self {
        Self(String::from(prose))
    }
}

named_enum! {
    /// Work a pool reads: open plan units of one kind, candidates owing an assessment, a
    /// review or a test, or root flaws owing a report. `poc_pending` lists candidates whose
    /// test waits on the user or the environment.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Phase {
        Sweep = "sweep", PathReview = "path_review", Question = "question", FollowUp = "follow_up",
        Check = "check", Assess = "assess", Review = "review", Poc = "poc",
        PocPending = "poc_pending", Report = "report",
    }
}
impl Phase {
    /// The plan units a unit phase pools; none for candidate work.
    pub const fn unit(self) -> Option<UnitKind> {
        match self {
            Self::Sweep => Some(UnitKind::Sweep),
            Self::PathReview => Some(UnitKind::PathReview),
            Self::Question => Some(UnitKind::Question),
            Self::FollowUp => Some(UnitKind::FollowUp),
            Self::Check
            | Self::Assess
            | Self::Review
            | Self::Poc
            | Self::PocPending
            | Self::Report => None,
        }
    }

    /// Whether its units go out in bundles: one agent takes every ready one sharing a record.
    pub const fn bundled(self) -> bool {
        matches!(self, Self::Question | Self::FollowUp)
    }

    /// Its place in the pipeline: each stage's work feeds the next, so work at an earlier
    /// stage goes first and holds back the later work it touches.
    pub const fn stage(self) -> u8 {
        match self {
            Self::Sweep => 0,
            Self::PathReview => 1,
            Self::Question | Self::FollowUp | Self::Check => 2,
            Self::Assess => 3,
            Self::Review => 4,
            Self::Poc | Self::PocPending => 5,
            Self::Report => 6,
        }
    }
}
named_enum! {
    /// What a plan unit covers: one component swept once, one path reviewed against its
    /// components' contracts, a question, or a follow-up any agent added.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum UnitKind {
        Sweep = "sweep", PathReview = "path_review", Question = "question", FollowUp = "follow_up",
    }
}
named_enum! {
    /// How a plan unit ended; the note carries its limitation.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Outcome { Completed = "completed", Blocked = "blocked", Inconclusive = "inconclusive", Deferred = "deferred" }
}
named_enum! {
    /// Derived from a candidate's records at its current revision; only a merge is stored.
    /// A noted candidate holds a positive verdict that owes no test of its own: in each root
    /// group it belongs to, a demonstrated scenario stands at its severity or above. What its
    /// tests showed is its [`ConfidenceTier`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Status {
        Identified = "identified", BelowThreshold = "below_threshold", Shortlisted = "shortlisted",
        Validated = "validated", Noted = "noted", Rejected = "rejected", Unproven = "unproven",
        Reported = "reported", Merged = "merged",
    }
}
named_enum! {
    /// How well the evidence supports an assessment's premises, kept apart from severity:
    /// the tiers an assessor picks.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Confidence { Conjectured = "conjectured", Plausible = "plausible", Established = "established" }
}
named_enum! {
    /// A candidate's confidence, lowest first: its assessment's tier, then `confirmed` while
    /// a positive verdict stands and `demonstrated` once a test of it is confirmed.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum ConfidenceTier {
        Conjectured = "conjectured", Plausible = "plausible", Established = "established",
        Confirmed = "confirmed", Demonstrated = "demonstrated",
    }
}
named_enum! {
    /// What a location contributes: the triggering input, the operation that misbehaves,
    /// a guard on the way, state the scenario depends on, or context.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum EvidenceRole { Trigger = "trigger", Operation = "operation", Guard = "guard", State = "state", Context = "context" }
}
named_enum! {
    /// Whether evidence supports or contradicts the record it is about. Contradicting
    /// evidence is a material change: it reopens every decision about the record.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Stance { Supports = "supports", Contradicts = "contradicts", Context = "context" }
}
named_enum! {
    /// How two flaws relate: one root cause, one's effect supplying the other's
    /// precondition, or mutually exclusive outcomes of one trigger.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum RelationKind { SameRootCause = "same_root_cause", Enables = "enables", AlternativeOutcome = "alternative_outcome" }
}
impl RelationKind {
    /// Whether `a` and `b` name the same relation in either order.
    pub(super) const fn symmetric(self) -> bool {
        !matches!(self, Self::Enables)
    }
}
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
    pub enum VerdictKind { Validated = "validated", Rejected = "rejected", Unproven = "unproven" }
}
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
    pub enum RejectionReason { Disproved = "disproved", PolicyExcluded = "policy_excluded" }
}
/// A review's conclusion. A rejection says why: the mechanism was disproved, or the scope's
/// policy keeps a real finding out of reports, citing the current scope's deployment fact
/// excluding it, if any. A duplicate is merged instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Verdict {
    Validated,
    Unproven,
    Disproved,
    PolicyExcluded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deployment: Option<DeploymentId>,
    },
}
impl Verdict {
    /// The stored verdict and the reason a rejection carries.
    pub(super) const fn stored(self) -> (VerdictKind, Option<RejectionReason>) {
        match self {
            Self::Validated => (VerdictKind::Validated, None),
            Self::Unproven => (VerdictKind::Unproven, None),
            Self::Disproved => (VerdictKind::Rejected, Some(RejectionReason::Disproved)),
            Self::PolicyExcluded { .. } => {
                (VerdictKind::Rejected, Some(RejectionReason::PolicyExcluded))
            }
        }
    }
    pub(super) const fn deployment(self) -> Option<DeploymentId> {
        match self {
            Self::PolicyExcluded { deployment } => deployment,
            Self::Validated | Self::Unproven | Self::Disproved => None,
        }
    }
}
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    /// A test's outcome. Confirmed and failed (a run that did not demonstrate the scenario,
    /// inconclusive ones included) are demonstrations; pending waits on the user or the
    /// environment and settles nothing; unavailable (the environment cannot run a real
    /// demonstration) and declined (the user turned this one down) settle the test as
    /// untested.
    pub enum PocResult {
        Confirmed = "confirmed", Failed = "failed", Pending = "pending",
        Unavailable = "unavailable", Declined = "declined",
    }
}
named_enum! {
    /// What a PoC artifact shows: the attack, the control run on the same build that
    /// the guard holds or the benign input is handled, an observation, the harness, or notes.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum ArtifactRole { Attack = "attack", Control = "control", Observation = "observation", Harness = "harness", Notes = "notes" }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PocArtifact {
    /// Path relative to the run's artifacts directory; must be a regular file there.
    pub reference: ArtifactReference,
    pub role: ArtifactRole,
}
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum EventKind {
        Recorded = "recorded", Updated = "updated", Merged = "merged", Refuted = "refuted",
        Resolved = "resolved", Assessed = "assessed", Validated = "validated",
        PocStarted = "poc_started", PocRecorded = "poc_recorded",
        EnvironmentRecorded = "environment_recorded",
        Reported = "reported",
    }
}
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum SourceKind { Network = "network", Stdio = "stdio", File = "file", Env = "env", Ipc = "ipc", Argv = "argv", Other = "other" }
}
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Trust { Unauthenticated = "unauthenticated", Authenticated = "authenticated", Local = "local", Privileged = "privileged" }
}
impl Trust {
    /// Whether input from this trust level is attacker-influenced.
    pub const fn untrusted(self) -> bool {
        !matches!(self, Self::Privileged)
    }
}
named_enum! {
    /// What a guarantee or premise is about: the bound an operation stays within, that
    /// repeated attempts progress or end, what a returned status means, how long an object
    /// lives, who may act, or which state holds.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum InvariantKind {
        Bounds = "bounds", Progress = "progress", StatusContract = "status_contract",
        Lifetime = "lifetime", Authorization = "authorization", State = "state", Other = "other",
    }
}
impl InvariantKind {
    /// Whether a mistaken guarantee or premise of this kind on attacker input corrupts
    /// memory, hangs, retries unsafely or leaves state a later step trusts.
    pub const fn load_bearing(self) -> bool {
        !matches!(self, Self::Authorization | Self::Other)
    }
}
named_enum! {
    /// Where a contract stands by its latest check (`contract_standing`).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum ContractState { Unsettled = "unsettled", Open = "open", Supported = "supported", Broken = "broken" }
}
named_enum! {
    /// The instructions around a reproduction's steps.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
    pub enum InstructionPart { Prerequisite = "prerequisite", Cleanup = "cleanup" }
}
named_enum! {
    /// What a status asking its caller to retry or supply more input can mean at one site:
    /// the input ran out, the input is invalid, or some other failure.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum RetryClass { InputExhausted = "input_exhausted", Invalid = "invalid", Other = "other" }
}
named_enum! {
    /// What a flaw makes its code report for the wrong reason: a returned status, an error,
    /// or state it leaves for others. Every consumer of that value inherits the flaw.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Misreport { Status = "status", Error = "error", State = "state" }
}
named_enum! {
    /// What the operation a flaw lies in does.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum OperationKind {
        MemoryWrite = "memory_write", MemoryRead = "memory_read", LifetimeRelease = "lifetime_release",
        MemoryUse = "memory_use", Interpreter = "interpreter", FilesystemPath = "filesystem_path",
        PrivilegedOperation = "privileged_operation", StateMutation = "state_mutation",
        DataExposure = "data_exposure", ExternalRequest = "external_request", Redirect = "redirect",
        Deserialize = "deserialize", CredentialUse = "credential_use",
        ResourceConsumption = "resource_consumption", ControlFlow = "control_flow",
        LogWrite = "log_write",
    }
}
named_enum! {
    /// What a security guard establishes before the operation it protects.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum GuardKind {
        Authn = "authn", Authz = "authz", Ownership = "ownership", StatePrecondition = "state_precondition",
        InputValidation = "input_validation", BoundsCheck = "bounds_check", LifetimeCheck = "lifetime_check",
        Canonicalisation = "canonicalisation", Integrity = "integrity", RateLimit = "rate_limit",
        Csrf = "csrf", Origin = "origin", ErrorHandling = "error_handling",
    }
}
named_enum! {
    /// How a required guard fails: it does not exist, a path avoids it, it checks the
    /// wrong subject, it is too weak, or its subject can change before the operation.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum DeficiencyKind {
        Missing = "missing", Bypassable = "bypassable", WrongSubject = "wrong_subject",
        Insufficient = "insufficient", Stale = "stale",
    }
}
named_enum! {
    /// The security property a possible effect harms.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Property { Confidentiality = "confidentiality", Integrity = "integrity", Availability = "availability" }
}
named_enum! {
    /// How far review reached into a component: inspected once a completed sweep covered
    /// every file, otherwise reached by some path, otherwise only mapped.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum ComponentCoverage { Mapped = "mapped", Reached = "reached", Inspected = "inspected" }
}

named_enum! {
    /// Whether a path's review is checked: it reaches no recorded component, some flaw of
    /// what it reaches still lacks its standing decision, or none does.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum PathCoverage { Unreached = "unreached", Unchecked = "unchecked", Checked = "checked" }
}

named_enum! {
    /// What a candidate's condition gates: the scenario, or one CVSS base metric.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
    pub enum GateKind { Scenario = "scenario", Metric = "metric" }
}
named_enum! {
    /// What a deployment fact of the scope is about.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum DeploymentKind {
        BuildFlag = "build_flag", Platform = "platform", HostBehaviour = "host_behaviour",
        InputBound = "input_bound", Other = "other",
    }
}
named_enum! {
    /// Whether the deployment in scope admits a fact or excludes it, or has not answered it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum DeploymentStance { Admitted = "admitted", Excluded = "excluded", Unanswered = "unanswered" }
}
named_enum! {
    /// A row of the `verdict_refusal` view, as the refusal it reports.
    #[derive(Clone, Copy)]
    pub(super) parsed enum RefusalKind {
        NoPath = "no_path", Refuted = "refuted", NoContribution = "no_contribution",
        MetricAboveFloor = "metric_above_floor", OpenPremise = "open_premise",
        FailedPremise = "failed_premise",
    }
}
named_enum! {
    /// How a premise is resolved ([`Resolution`]); one without a resolution is open.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
    pub enum ResolutionKind { Holds = "holds", Fails = "fails", Asked = "asked" }
}
named_enum! {
    /// One path's decision about one flaw: it cannot drive it, it drives it without a
    /// security consequence, or it contributes to a candidate.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum ExposureOutcome { Unreachable = "unreachable", NoConsequence = "no_consequence", Contributes = "contributes" }
}
named_enum! {
    /// How a route step is known: read in the code, inferred from it, or assumed.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum RouteBasis { Inspected = "inspected", Inferred = "inferred", Assumed = "assumed" }
}
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum CredentialReferenceKind { EnvironmentVariable = "environment_variable", SecretManager = "secret_manager" }
}

/// Maximum number of run-owned artifact references accepted by one PoC attempt.
pub const MAX_ARTIFACTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("artifact references are portable relative paths below the run's artifacts directory")]
pub struct InvalidArtifactReference;

string_newtype! {
    /// A file relative to `<run-directory>/artifacts/`, never a workspace/absolute path.
    /// Slash-separated components may contain ASCII letters, digits, `.`, `_`, and `-`;
    /// empty, `.` and `..` components are rejected. At most 1024 bytes total and 240 per
    /// component, leaving room for the `-2` a report adds when two artifacts share a name.
    #[derive(PartialOrd, Ord)]
    pub struct ArtifactReference(InvalidArtifactReference) = |reference| {
        let valid_component = |part: &str| {
            !part.is_empty()
                && !matches!(part, "." | "..")
                && part.len() <= 240
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        };
        if reference.len() > 1024 || !reference.split('/').all(valid_component) {
            Err(InvalidArtifactReference)
        } else {
            Ok(())
        }
    }, schema = schemars::json_schema!({
        "type": "string",
        "pattern": "^[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*$",
        "maxLength": 1024
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("endpoint requires HTTP(S) without credentials, query parameters or fragments")]
pub struct InvalidEndpoint;

string_newtype! {
    /// An absolute HTTP(S) address without userinfo, query or fragment.
    pub struct Endpoint(InvalidEndpoint) = |value| {
        let uri: http::Uri = value.parse().map_err(|_| InvalidEndpoint)?;
        if !matches!(uri.scheme_str(), Some("http" | "https"))
            || uri.authority().is_none_or(|a| a.as_str().contains('@'))
            || uri.query().is_some()
            || value.contains('#')
        {
            return Err(InvalidEndpoint);
        }
        Ok(())
    }, schema = schemars::json_schema!({
        "type": "string",
        "description": "An absolute HTTP(S) address without userinfo, query or fragment."
    });
}

/// A full git object id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, SerializeDisplay, DeserializeFromStr)]
pub enum GitOid {
    Sha1([u8; 20]),
    Sha256([u8; 32]),
}
impl fmt::Display for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sha1(bytes) => write_hex(bytes, f),
            Self::Sha256(bytes) => write_hex(bytes, f),
        }
    }
}
impl FromStr for GitOid {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self, Error> {
        decode_hex(value)
            .map(Self::Sha1)
            .or_else(|| decode_hex(value).map(Self::Sha256))
            .ok_or(Error::GitOid)
    }
}

named_enum! {
    /// Whether the checkout had changes beyond its commit; unknown when Git could not say.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
    pub enum Tree { Clean = "clean", Dirty = "dirty", Unknown = "unknown" }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub commit: Option<GitOid>,
    pub tree: Tree,
}

/// The commit, or `unavailable` outside a repository, and the tree's state.
impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.commit {
            Some(commit) => write!(f, "{commit}")?,
            None => f.write_str("unavailable")?,
        }
        write!(f, " tree={}", self.tree)
    }
}

/// A keyset page: records after `after`, at most `limit` of them.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Page {
    pub after: i64,
    pub limit: Bounded<1, 1000>,
}
impl Page {
    pub const fn new(after: i64, limit: Bounded<1, 1000>) -> Self {
        Self { after, limit }
    }
    pub(super) fn limit(self) -> i64 {
        u64::from(self.limit) as i64
    }
}
impl Default for Page {
    fn default() -> Self {
        Self::new(0, Bounded::new::<100>())
    }
}

/// One keyset page: the records read and the cursor to continue from, `None` at the end.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Listing<T> {
    pub items: Vec<T>,
    pub next_after: Option<i64>,
}
impl<T> Listing<T> {
    /// Every record `read` pages through from the start, in pages of the largest size.
    pub fn drain<E>(
        mut read: impl FnMut(Page) -> std::result::Result<Self, E>,
    ) -> std::result::Result<Vec<T>, E> {
        let mut page = Page::new(0, Bounded::new::<1000>());
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
}
impl<T> std::ops::Deref for Listing<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.items
    }
}
impl<T> IntoIterator for Listing<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter()
    }
}
impl<T: Keyed> Listing<T> {
    /// A full page continues after its last key; a shorter one is the end.
    pub(super) fn keyset(items: Vec<T>, page: Page) -> Self {
        let next_after = (items.len() as i64 == page.limit())
            .then(|| items.last().map(Keyed::key))
            .flatten();
        Self { items, next_after }
    }
}

/// The record id a keyset page orders by.
pub trait Keyed {
    fn key(&self) -> i64;
}
macro_rules! keyed {
    ($($type:ty => $field:ident),* $(,)?) => {$(
        impl Keyed for $type {
            fn key(&self) -> i64 {
                self.$field.get()
            }
        }
    )*};
}
keyed!(
    super::events::Event => id,
    super::scope::Scope => id,
    super::scope::Principal => id,
    super::scope::Source => id,
    super::components::Component => id,
    super::components::Guarantee => id,
    super::paths::Path => id,
    super::plan::PlanUnit => id,
    super::flaws::FlawSummary => id,
    super::flaws::Relation => id,
    super::flaws::Exposure => id,
    super::candidates::CandidateSummary => id,
    super::candidates::Assessment => id,
    super::workflow::ValidationAttempt => id,
    super::workflow::PocAttempt => id,
    super::report::Finding => id,
    super::environment::Environment => id,
);

named_enum! {
    /// Whether a launch demonstrates validated candidates against an authorized environment.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
    pub enum PocPolicy { Ask = "ask", Yes = "yes", No = "no" }
}

/// Per-run limits, persisted with the run; a later launch may change its workers. A report
/// states only the threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RunOptions {
    /// The lowest conditional severity sent to review.
    pub threshold: Severity,
    /// How many agents the orchestrator's scheduler runs at once; not enforced.
    #[serde(skip)]
    pub workers: NonZeroU16,
}
impl Default for RunOptions {
    fn default() -> Self {
        Self {
            threshold: Severity::High,
            workers: const { NonZeroU16::new(6).unwrap() },
        }
    }
}

/// A file region: inclusive, one-based lines, in the function containing it when it lies in
/// one. Paths are stored verbatim.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "LocationInput<Option<Text>>")]
pub struct Location {
    pub file: Text,
    pub start_line: u32,
    pub end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<Text>,
}
/// A location's input; its function is `Text` where the location must name one.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LocationInput<F> {
    file: Text,
    #[schemars(range(min = 1))]
    start_line: u32,
    #[schemars(range(min = 1))]
    end_line: u32,
    function: F,
}
impl Location {
    pub fn new(
        file: Text,
        start_line: u32,
        end_line: u32,
        function: Option<Text>,
    ) -> Result<Self, Error> {
        if start_line == 0 || start_line > end_line {
            return Err(Error::Span);
        }
        Ok(Self {
            file,
            start_line,
            end_line,
            function,
        })
    }
    pub fn in_function(self, function: Text) -> FunctionLocation {
        FunctionLocation(Self {
            function: Some(function),
            ..self
        })
    }
}
impl<F: Into<Option<Text>>> TryFrom<LocationInput<F>> for Location {
    type Error = Error;
    fn try_from(value: LocationInput<F>) -> Result<Self, Error> {
        Self::new(
            value.file,
            value.start_line,
            value.end_line,
            value.function.into(),
        )
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}-{}", self.file, self.start_line, self.end_line)?;
        match &self.function {
            Some(function) => write!(f, " in {function}"),
            None => Ok(()),
        }
    }
}

/// A file region in the function containing it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "LocationInput<Text>")]
pub struct FunctionLocation(Location);
impl TryFrom<LocationInput<Text>> for FunctionLocation {
    type Error = Error;
    fn try_from(value: LocationInput<Text>) -> Result<Self, Error> {
        Location::try_from(value).map(Self)
    }
}
impl std::ops::Deref for FunctionLocation {
    type Target = Location;
    fn deref(&self) -> &Location {
        &self.0
    }
}
