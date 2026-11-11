use std::fmt;

use libsql::Value;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::Sql;
use skyhook_base::{
    enums::{Kinds, NamedEnum},
    string_newtype,
};

use super::{Error, MAX_ATTACHMENTS};

/// A positive SQLite row id.
macro_rules! ids {
    ($($name:ident),* $(,)?) => {$(
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "i64", into = "i64")]
        pub(crate) struct $name(pub(super) i64);
        impl JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> { "Id".into() }
            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({"type": "integer", "minimum": 1})
            }
        }
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

/// The most records one call names, at least `MIN` of them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "Vec<T>",
    bound(deserialize = "T: Deserialize<'de>, Listed<T, MIN>: TryFrom<Vec<T>, Error = Error>")
)]
pub(crate) struct Listed<T, const MIN: usize = 0>(Vec<T>);

impl<T> TryFrom<Vec<T>> for Listed<T> {
    type Error = Error;
    fn try_from(items: Vec<T>) -> Result<Self, Error> {
        match items.len() {
            0..=MAX_ATTACHMENTS => Ok(Self(items)),
            _ => Err(Error::Limit),
        }
    }
}

impl<I: KindId> TryFrom<Vec<I>> for Listed<I, 1> {
    type Error = Error;
    fn try_from(ids: Vec<I>) -> Result<Self, Error> {
        match ids.len() {
            0 => Err(Error::Empty(I::KIND)),
            1..=MAX_ATTACHMENTS => Ok(Self(ids)),
            _ => Err(Error::Limit),
        }
    }
}

impl<T> Default for Listed<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T, const MIN: usize> std::ops::Deref for Listed<T, MIN> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T, const MIN: usize> IntoIterator for Listed<T, MIN> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<T: JsonSchema, const MIN: usize> JsonSchema for Listed<T, MIN> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        format!("Listed{MIN}_{}", T::schema_name()).into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = schemars::json_schema!({
            "type": "array",
            "items": generator.subschema_for::<T>(),
        });
        if MIN > 0 {
            schema.insert("minItems".to_owned(), MIN.into());
        }
        schema.insert("maxItems".to_owned(), MAX_ATTACHMENTS.into());
        schema
    }
}

/// The id of a node of one kind.
pub(crate) trait KindId: Copy + Into<NodeId> + Into<i64> {
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

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NodeKind {
    Component,
    Path,
    State,
    Premise,
    Flaw,
    Candidate,
    Finding,
}

/// A state by its scope, an input state's with `I` and another's with `K`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Kinds)]
#[serde(tag = "scope", rename_all = "snake_case")]
// Scopes are inlined like statuses, so that a request's `scope` takes either kind's. A state
// lives as input an actor writes and the target takes in, target-written state the caller hands
// back into later calls, one instance's fields across its calls, or storage that outlives the
// caller's interaction, read by other flows.
#[kinds(name = StateScope, derive(PartialOrd, Ord, Hash, JsonSchema), schemars(inline))]
pub(crate) enum StateScoped<I, K = ()> {
    /// Actor-written data the target takes in
    Input(I),
    /// Target-written, carried by the caller into later calls
    Handed(K),
    /// One object's fields across its calls
    Instance(K),
    /// Outlives the caller's interaction, read by other flows
    Stored(K),
}

impl<I> StateScoped<I> {
    /// The state of `scope`, an input state's read by `input`.
    pub(crate) fn of(
        scope: StateScope,
        input: impl FnOnce() -> super::Result<I>,
    ) -> super::Result<Self> {
        Ok(match scope {
            StateScope::Input => Self::Input(input()?),
            StateScope::Handed => Self::Handed(()),
            StateScope::Instance => Self::Instance(()),
            StateScope::Stored => Self::Stored(()),
        })
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Access {
    Read,
    Write,
}

/// Who can drive an input state
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Trust {
    Untrusted,
    Authenticated,
    Local,
    Privileged,
}

// Whether a premise is about the code, or a deployment condition only the user answers.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[schemars(inline)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PremiseScope {
    Code,
    Deployment,
}

// The statuses a request sets are inlined, so that its `status` takes any of them.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[schemars(inline)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PremiseStatus {
    Holds,
    Fails,
    Undecided,
}

// A flaw is suspected until it is refuted, or confirmed: a verified defect with no chain to an
// observable consequence found so far.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[schemars(inline)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FlawStatus {
    Suspected,
    Confirmed,
    Refuted,
}

// Whether a path's sweep is done, or the path was recorded in error. Any agent marks it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[schemars(inline)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PathStatus {
    Swept,
    Unswept,
    Retracted,
}

// Whether a state or component stands as recorded, or was recorded in error.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[schemars(inline)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InventoryStatus {
    Recorded,
    Retracted,
}

/// A review's conclusion; its rationale is the verdict event's note
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    Validated,
    Disproved,
    Excluded,
    Unproven,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventKind {
    Created,
    Edited,
    Linked,
    Unlinked,
    Evidence,
    Merged,
    Status,
    Assessed,
    Verdict,
    Tested,
    Environment,
    Began,
    Reported,
}

/// A candidate's standing, derived from its results and what they rest on. A noted
/// candidate is validated, and another of its root group stands validated and demonstrated
/// at its severity or above
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Identified,
    BelowThreshold,
    Shortlisted,
    Validated,
    Noted,
    Disproved,
    Excluded,
    Unproven,
    Merged,
}

/// How far an assessed candidate is established, lowest first: conjectured while a premise
/// of its chain is undecided or contradicts it, confirmed by a standing positive verdict and
/// demonstrated by a confirmed test of an uncontradicted chain.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConfidenceTier {
    Conjectured,
    Established,
    Confirmed,
    Demonstrated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TextError {
    #[error("text must not be blank")]
    Blank,
    #[error("text exceeds 1 MB")]
    TooLong,
}

string_newtype! {
    /// At most 1 MB
    #[derive(PartialOrd, Ord)]
    pub(crate) struct Text(TextError) = |value| {
        if value.trim().is_empty() {
            Err(TextError::Blank)
        } else if value.len() > 1_000_000 {
            Err(TextError::TooLong)
        } else {
            Ok(())
        }
    }, schema = schemars::json_schema!({"type": "string", "pattern": "\\S"});
}

/// Whether evidence supports or contradicts the node it is about. Contradicting evidence
/// changes the node, reopening the work that read it
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stance {
    Supports,
    Contradicts,
    Context,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
/// A test's outcome. Confirmed and failed (evidence that the scenario does not work as
/// formulated) are demonstrations; pending waits on the user or the
/// environment and settles nothing; unavailable (the environment cannot run a real
/// demonstration) and declined (the user turned this one down) settle the test as
/// untested
#[serde(rename_all = "snake_case")]
pub(crate) enum TestResult {
    Confirmed,
    Failed,
    Pending,
    Unavailable,
    Declined,
}

/// What a test artifact shows: the attack, the control run on the same build that
/// the guard holds or the benign input is handled, an observation, the harness, or notes
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactRole {
    Attack,
    Control,
    Observation,
    Harness,
    Notes,
}

/// How an event bears on a node it names, weakest first: it only concerns it, it proposes
/// a result differing from the one standing on it, which only that result's work reads, it
/// moves the node's standing, or it changes the node itself.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema, NamedEnum,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventEffect {
    Concerns,
    Disputes,
    Standing,
    Changed,
}

/// How an event of a phase's bearing on a node moves that work: it moves what the work rests
/// on, as a material edit of the node or otherwise, or it disputes the result standing on the
/// node, which gives the work fresh tries but leaves that result current.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BearingRole {
    Rests,
    Edits,
    Disputes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("an interface name is 1 to 128 bytes without whitespace")]
pub(crate) struct InvalidInterfaceName;

string_newtype! {
    /// A component's interface by its short name, at most 128 bytes; a location or an
    /// environment names one already recorded in a component's `interfaces`
    #[derive(PartialOrd, Ord)]
    pub(crate) struct InterfaceName(InvalidInterfaceName) = |name| {
        if name.is_empty() || name.len() > 128 || name.chars().any(char::is_whitespace) {
            Err(InvalidInterfaceName)
        } else {
            Ok(())
        }
    }, schema = schemars::json_schema!({"type": "string", "pattern": "^\\S+$"});
}

impl Sql for &Text {
    fn sql(self) -> Value {
        Value::Text(self.as_str().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl<T, const MIN: usize> Listed<T, MIN>
    where
        Self: TryFrom<Vec<T>, Error = Error>,
    {
        pub(crate) fn of(items: impl Into<Vec<T>>) -> Self {
            Self::try_from(items.into()).unwrap()
        }
    }

    /// A list holds at most a call's records, and one that needs a node holds at least one.
    #[test]
    fn a_listed_call_names_its_bounded_records() {
        let ids = |count: i64| serde_json::json!((1..=count).collect::<Vec<_>>());
        let read = |count| serde_json::from_value::<Listed<PathId, 1>>(ids(count));
        assert_eq!(read(1000).unwrap().len(), 1000);
        let refused = |count| read(count).unwrap_err().to_string();
        assert!(refused(1001).contains(&Error::Limit.to_string()));
        assert!(refused(0).contains(&Error::Empty(NodeKind::Path).to_string()));
        let open = |count| serde_json::from_value::<Listed<PathId>>(ids(count));
        assert!(open(0).unwrap().is_empty());
        assert!(open(1001).is_err());
    }
}
