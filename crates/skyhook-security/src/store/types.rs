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
    StateId,
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
    StateId: State,
    PremiseId: Premise,
    FlawId: Flaw,
    CandidateId: Candidate,
    FindingId: Finding,
);

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum NodeKind {
        Component = "component", Path = "path", State = "state", Premise = "premise",
        Flaw = "flaw", Candidate = "candidate", Finding = "finding",
    }
}

named_enum! {
    /// Work the scheduler derives: paths owing a sweep or a probe, states a check across calls,
    /// premises a settling look, flaws a chain, candidates a review or a test, and root flaws a
    /// report.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Phase {
        Sweep = "sweep", Probe = "probe", Check = "check", Settle = "settle", Chain = "chain",
        Review = "review", Test = "test", Report = "report",
    }
}
impl Phase {
    /// The kind of node its work is about.
    pub const fn subject(self) -> NodeKind {
        match self {
            Self::Sweep | Self::Probe => NodeKind::Path,
            Self::Check => NodeKind::State,
            Self::Settle => NodeKind::Premise,
            Self::Chain | Self::Report => NodeKind::Flaw,
            Self::Review | Self::Test => NodeKind::Candidate,
        }
    }

    /// Its place in the pipeline: each stage's work feeds the next, so work at an earlier
    /// stage goes first and holds back the later work it touches.
    pub const fn stage(self) -> u8 {
        match self {
            Self::Sweep | Self::Probe | Self::Check => 0,
            Self::Settle | Self::Chain => 1,
            Self::Review => 2,
            Self::Test => 3,
            Self::Report => 4,
        }
    }

    /// Whether its entries go out in bundles: one agent takes every ready one of an entry's
    /// bundle, paths and states by state, premises and flaws by path, candidates by root group.
    pub const fn bundled(self) -> bool {
        matches!(
            self,
            Self::Sweep | Self::Check | Self::Settle | Self::Chain | Self::Review
        )
    }
}

// Scopes are inlined like statuses, so that a request's `scope` takes either kind's. A state
// lives as input an actor writes and the target takes in, target-written state the caller hands
// back into later calls, one instance's fields across its calls, or storage that outlives the
// caller's interaction, read by other flows.
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    #[schemars(inline)]
    pub enum StateScope { Input = "input", Handed = "handed", Instance = "instance", Stored = "stored" }
}
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Access { Read = "read", Write = "write" }
}
named_enum! {
    /// Who can drive an input state.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Trust { Untrusted = "untrusted", Authenticated = "authenticated", Local = "local", Privileged = "privileged" }
}
// Whether a premise is about the code, or a deployment condition only the user answers.
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    #[schemars(inline)]
    pub enum PremiseScope { Code = "code", Deployment = "deployment" }
}
// The statuses a request sets are inlined, so that its `status` takes any of them.
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    #[schemars(inline)]
    pub enum PremiseStatus { Holds = "holds", Fails = "fails", Undecided = "undecided" }
}
// A flaw is suspected until it is refuted, or confirmed: a verified defect with no chain to an
// observable consequence found so far.
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    #[schemars(inline)]
    pub enum FlawStatus { Suspected = "suspected", Confirmed = "confirmed", Refuted = "refuted" }
}
// Whether a path's sweep is done, or the path was recorded in error. Any agent marks it.
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    #[schemars(inline)]
    pub enum PathStatus { Swept = "swept", Unswept = "unswept", Retracted = "retracted" }
}
// Whether a state or component stands as recorded, or was recorded in error.
named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    #[schemars(inline)]
    pub enum InventoryStatus { Recorded = "recorded", Retracted = "retracted" }
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
    /// Nonblank, at most 1 MB.
    #[derive(PartialOrd, Ord)]
    pub struct Text(TextError) = |value| {
        if value.trim().is_empty() {
            Err(TextError::Blank)
        } else if value.len() > 1_000_000 {
            Err(TextError::TooLong)
        } else {
            Ok(())
        }
    }, schema = schemars::json_schema!({"type": "string", "pattern": "\\S"});
}

named_enum! {
    /// Whether evidence supports or contradicts the node it is about. Contradicting evidence
    /// changes the node, reopening the work that read it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum Stance { Supports = "supports", Contradicts = "contradicts", Context = "context" }
}

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    /// A test's outcome. Confirmed and failed (evidence that the scenario does not work as
    /// formulated) are demonstrations; pending waits on the user or the
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
    /// How an event bears on a node it names, weakest first: it only concerns it, it proposes
    /// a result differing from the one standing on it, which only that result's work reads, it
    /// moves the node's standing, or it changes the node itself.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum EventEffect {
        Concerns = "concerns", Disputes = "disputes", Standing = "standing", Changed = "changed",
    }
}
named_enum! {
    /// How an event of a phase's bearing on a node moves that work: it moves what the work rests
    /// on, as a material edit of the node or otherwise, or it disputes the result standing on the
    /// node, which gives the work fresh tries but leaves that result current.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
    pub enum BearingRole { Rests = "rests", Edits = "edits", Disputes = "disputes" }
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
    pub(crate) fn limit(self) -> usize {
        u64::from(self.limit) as usize
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
        let next_after = (items.len() == page.limit())
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
    /// Whether a launch works against a running target: probing it in an environment serving its
    /// interfaces and demonstrating validated candidates. Static analysis of binaries is reading,
    /// allowed under any.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
    pub enum LivePolicy { Ask = "ask", Yes = "yes", No = "no" }
}

/// Per-run limits, persisted with the run; a later launch may change its workers. A report
/// states only the threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RunOptions {
    /// The lowest conditional severity a review goes on to judge.
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

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
    pub enum LocationKind { Code = "code", Binary = "binary", Interface = "interface" }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("an interface name is 1 to 128 bytes without whitespace")]
pub struct InvalidInterfaceName;

string_newtype! {
    /// A component's interface by its short name, 1 to 128 bytes without whitespace; a location
    /// or an environment names one already recorded in a component's `interfaces`.
    #[derive(PartialOrd, Ord)]
    pub struct InterfaceName(InvalidInterfaceName) = |name| {
        if name.is_empty() || name.len() > 128 || name.chars().any(char::is_whitespace) {
            Err(InvalidInterfaceName)
        } else {
            Ok(())
        }
    }, schema = schemars::json_schema!({"type": "string", "pattern": "^\\S+$"});
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{}", SERVICE_ADDRESS)]
pub struct InvalidServiceAddress;

/// What a service address is, as its schema and its refusal state it.
const SERVICE_ADDRESS: &str = "an absolute URL with a host, such as tcp://10.0.0.5:7000, or a file \
                               URL with an absolute path, such as file:///srv/harness/decode";

string_newtype! {
    /// Where an environment serves an interface: an absolute URL with a host, or a file URL
    /// naming a local socket, binary or harness by its absolute path.
    pub struct ServiceAddress(InvalidServiceAddress) = |address| {
        let valid = match url::Url::parse(address) {
            Ok(url) if url.scheme() == "file" => names_local_path(address, &url),
            _ => address.parse::<http::Uri>().is_ok_and(|uri| {
                uri.scheme().is_some() && uri.host().is_some_and(|host| !host.is_empty())
            }),
        };
        if valid { Ok(()) } else { Err(InvalidServiceAddress) }
    }, schema = schemars::json_schema!({"type": "string", "format": "uri", "description": SERVICE_ADDRESS});
}

/// Whether file URL `url`, parsed from `address`, names an absolute local path as written:
/// parsing drops or escapes whitespace and control characters and keeps malformed escapes, so
/// `address` holds none, and each escape decodes to a byte a path may hold.
fn names_local_path(address: &str, url: &url::Url) -> bool {
    let escapes = address.split('%').skip(1).all(|escaped| {
        (escaped.get(..2))
            .is_some_and(|hex| hex != "00" && hex.bytes().all(|b| b.is_ascii_hexdigit()))
    });
    escapes
        && !address.chars().any(|c| c.is_control() || c.is_whitespace())
        && url.path() != "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.to_file_path().is_ok()
}

impl Sql for &Text {
    fn sql(self) -> Value {
        Value::Text(self.as_str().to_owned())
    }
}
