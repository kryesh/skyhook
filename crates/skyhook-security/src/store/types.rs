use std::{fmt, num::NonZeroU16};

use libsql::Value;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::{Bounded, db::Sql, named_enum, string_newtype};

use super::Error;
pub use super::artifacts::Inadmissible;
pub use crate::cvss::Severity;

/// A positive SQLite row id.
macro_rules! ids {
    ($($name:ident),* $(,)?) => {$(
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
        impl Sql for $name { fn sql(self) -> Value { Value::Integer(self.0) } }
        impl fmt::Display for $name { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) } }
    )*};
}

ids!(
    NodeId,
    ComponentId,
    PathId,
    SourceId,
    PremiseId,
    FlawId,
    CandidateId,
    FindingId,
    EventId,
    TaskId,
    TestId,
    EnvironmentId,
    EvidenceId,
    LocationId,
);

/// The id of a node of one kind.
pub trait KindId: Copy + Into<NodeId> + Into<i64> {
    const KIND: NodeKind;
    fn of(node: NodeId) -> Self;
}

macro_rules! kind_ids {
    ($($name:ident: $kind:ident),* $(,)?) => {$(
        impl KindId for $name {
            const KIND: NodeKind = NodeKind::$kind;
            fn of(node: NodeId) -> Self { Self(node.0) }
        }
        impl From<$name> for NodeId { fn from(value: $name) -> Self { Self(value.0) } }
    )*};
}

kind_ids!(
    ComponentId: Component,
    PathId: Path,
    SourceId: Source,
    PremiseId: Premise,
    FlawId: Flaw,
    CandidateId: Candidate,
    FindingId: Finding,
);

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum NodeKind {
        Component = "component", Path = "path", Source = "source", Premise = "premise",
        Flaw = "flaw", Candidate = "candidate", Finding = "finding",
    }
}

named_enum! {
    /// Work the scheduler derives: paths owing a sweep, premises a settling look, flaws a
    /// chain, candidates an assessment, a review or a test, and root flaws a report.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Phase {
        Sweep = "sweep", Settle = "settle", Chain = "chain", Assess = "assess",
        Review = "review", Test = "test", Report = "report",
    }
}
impl Phase {
    /// The kind of node its work is about.
    pub const fn subject(self) -> NodeKind {
        match self {
            Self::Sweep => NodeKind::Path,
            Self::Settle => NodeKind::Premise,
            Self::Chain | Self::Report => NodeKind::Flaw,
            Self::Assess | Self::Review | Self::Test => NodeKind::Candidate,
        }
    }

    /// Its place in the pipeline: each stage's work feeds the next, so work at an earlier
    /// stage goes first and holds back the later work it touches.
    pub const fn stage(self) -> u8 {
        match self {
            Self::Sweep => 0,
            Self::Settle | Self::Chain => 1,
            Self::Assess => 2,
            Self::Review => 3,
            Self::Test => 4,
            Self::Report => 5,
        }
    }

    /// Whether its entries go out in bundles: one agent takes every ready one sharing a node.
    pub const fn bundled(self) -> bool {
        matches!(self, Self::Settle | Self::Chain)
    }
}

named_enum! {
    /// Who can drive a source's input.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Trust { Untrusted = "untrusted", Authenticated = "authenticated", Local = "local", Privileged = "privileged" }
}
named_enum! {
    /// Whether a premise is about the code, or a deployment condition only the user answers.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum PremiseScope { Code = "code", Deployment = "deployment" }
}
// The statuses a request sets are inlined, so that its `status` takes any of them.
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    #[schemars(inline)]
    pub enum PremiseStatus { Holds = "holds", Fails = "fails", Undecided = "undecided" }
}
// A flaw is suspected until it is refuted, or contained: real, with no observable consequence
// found.
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    #[schemars(inline)]
    pub enum FlawStatus { Suspected = "suspected", Contained = "contained", Refuted = "refuted" }
}
// Whether a path's sweep is done. Any agent marks it.
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    #[schemars(inline)]
    pub enum PathStatus { Swept = "swept", Unswept = "unswept" }
}
named_enum! {
    /// A review's conclusion; its rationale is the verdict event's note.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Verdict { Validated = "validated", Disproved = "disproved", Excluded = "excluded", Unproven = "unproven" }
}
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum EventKind {
        Created = "created", Edited = "edited", Linked = "linked", Unlinked = "unlinked",
        Evidence = "evidence", Merged = "merged", Status = "status", Assessed = "assessed",
        Verdict = "verdict", Tested = "tested", Environment = "environment", Began = "began",
        Reported = "reported",
    }
}
named_enum! {
    /// A candidate's standing, derived from its results and what they rest on. A noted
    /// candidate is validated, and another of its root group stands validated and demonstrated
    /// at its severity or above.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Status {
        Identified = "identified", BelowThreshold = "below_threshold", Shortlisted = "shortlisted",
        Validated = "validated", Noted = "noted", Disproved = "disproved", Excluded = "excluded",
        Unproven = "unproven", Merged = "merged",
    }
}
named_enum! {
    /// How far an assessed candidate is established, lowest first: conjectured while a premise
    /// of its chain is undecided or contradicts it, confirmed by a standing positive verdict and
    /// demonstrated by a confirmed test of an uncontradicted chain.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum ConfidenceTier {
        Conjectured = "conjectured", Established = "established", Confirmed = "confirmed",
        Demonstrated = "demonstrated",
    }
}

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

named_enum! {
    /// Whether evidence supports or contradicts the node it is about. Contradicting evidence
    /// changes the node, reopening the work that read it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Stance { Supports = "supports", Contradicts = "contradicts", Context = "context" }
}

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    /// A test's outcome. Confirmed and failed (a run that did not demonstrate the scenario,
    /// inconclusive ones included) are demonstrations; pending waits on the user or the
    /// environment and settles nothing; unavailable (the environment cannot run a real
    /// demonstration) and declined (the user turned this one down) settle the test as
    /// untested.
    pub enum TestResult {
        Confirmed = "confirmed", Failed = "failed", Pending = "pending",
        Unavailable = "unavailable", Declined = "declined",
    }
}

named_enum! {
    /// What a test artifact shows: the attack, the control run on the same build that
    /// the guard holds or the benign input is handled, an observation, the harness, or notes.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum ArtifactRole { Attack = "attack", Control = "control", Observation = "observation", Harness = "harness", Notes = "notes" }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TestArtifact {
    /// Path relative to the run's artifacts directory; must be a regular file there.
    pub reference: ArtifactReference,
    pub role: ArtifactRole,
}

named_enum! {
    /// How an event bears on a node it names, weakest first: it only concerns it, it moves
    /// the node's standing, or it changes the node itself.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum EventEffect { Concerns = "concerns", Standing = "standing", Changed = "changed" }
}

/// Maximum number of run-owned artifact references accepted by one test.
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
    pub(crate) fn limit(self) -> i64 {
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
    pub(crate) fn keyset(items: Vec<T>, page: Page) -> Self {
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

impl Sql for &Text {
    fn sql(self) -> Value {
        Value::Text(self.as_str().to_owned())
    }
}
