//! Session event schema: the in-memory form of normalized session database rows.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use crate::identity::AgentId;
use crate::target::TargetDefinition;
use crate::tool::policy::CapabilitySet;
use crate::{
    job::{JobEnd, JobTransition},
    provider::{
        ProviderErrorKind,
        model::{ModelProfile, ModelRef},
        protocol::{
            CutReason, HistoryLifetime, ModelRequest, Outcome, ResponseSchema, SystemSegment, Usage,
        },
    },
    session::Message,
    session::{CompactionFault, Failure},
};
use crate::{
    job::{JobName, JobRole},
    tool::catalog::ToolDefinition,
};
use skyhook_base::enums::{Kinds, NamedEnum};
use skyhook_media::ImageRef;
use skyhook_tools::target::ExecutionLocation;
use skyhook_worker::job::JobId;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Hash, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum ModelPurpose {
    Agent,
    Compaction,
}

/// Where a session title came from. A title the user set is never replaced by
/// an automatic one.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    /// The session's first prompt.
    Prompt,
    User,
    /// A summary of the conversation.
    Generated,
}

/// Why a completed response ended early. A refusal fails the turn instead, so it
/// is never journaled as a completed response.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum Truncation {
    MaxTokens,
    Incomplete,
}

/// How a response that completed the turn ended.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Kinds)]
#[serde(rename_all = "snake_case")]
#[kinds(name = ResponseOutcome)]
pub enum CompletedOutcome {
    Answer,
    ToolUse,
    Cut(Truncation),
}

/// The model refused: the cut that fails the turn instead of completing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused;

impl TryFrom<Outcome> for CompletedOutcome {
    type Error = Refused;

    fn try_from(outcome: Outcome) -> Result<Self, Refused> {
        Ok(match outcome {
            Outcome::Answer => Self::Answer,
            Outcome::ToolUse => Self::ToolUse,
            Outcome::Cut(CutReason::MaxTokens) => Self::Cut(Truncation::MaxTokens),
            Outcome::Cut(CutReason::Incomplete) => Self::Cut(Truncation::Incomplete),
            Outcome::Cut(CutReason::Refusal) => return Err(Refused),
        })
    }
}

/// A journal sequence only the store's append and the database decoder mint;
/// everything else holds one it received from a record. This crate's own tests
/// mint through `From<u64>`; tests elsewhere append to a store.
macro_rules! sequence {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(
            Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            /// The number, for storage and wire formats that carry it as one.
            pub const fn get(self) -> u64 {
                self.0
            }

            /// The sequence before this one, if any.
            pub const fn preceding(self) -> Option<Self> {
                match self.0.checked_sub(1) {
                    Some(sequence) => Some(Self(sequence)),
                    None => None,
                }
            }
        }

        #[cfg(test)]
        impl From<u64> for $name {
            fn from(sequence: u64) -> Self {
                Self(sequence)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

sequence! {
    /// The sequence of any journal record.
    RecordSeq
}
sequence! {
    /// The sequence of a `ModelRequested` record.
    RequestSeq
}
sequence! {
    /// The sequence of a `MessageCommitted` record.
    MessageSeq
}

impl RecordSeq {
    pub(in crate::session) const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// The sequence the next record takes.
    #[must_use]
    pub(crate) const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// This record's sequence as the request it is; the caller matched the event.
    pub const fn request(self) -> RequestSeq {
        RequestSeq(self.0)
    }

    /// This record's sequence as the commit it is; the caller matched the event.
    pub const fn message(self) -> MessageSeq {
        MessageSeq(self.0)
    }
}

impl From<RequestSeq> for RecordSeq {
    fn from(sequence: RequestSeq) -> Self {
        Self(sequence.0)
    }
}

impl From<MessageSeq> for RecordSeq {
    fn from(sequence: MessageSeq) -> Self {
        Self(sequence.0)
    }
}

/// One provider invocation of a frozen logical request: the `ModelRequested`
/// sequence and the 1-based attempt number.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Hash)]
pub struct AttemptRef {
    pub request: RequestSeq,
    pub attempt: u64,
}

/// The backoff before a failed attempt's retry, kept to the millisecond.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct RetryDelay(u64);

impl RetryDelay {
    pub fn millis(self) -> u64 {
        self.0
    }
}

impl From<std::time::Duration> for RetryDelay {
    fn from(delay: std::time::Duration) -> Self {
        Self(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX))
    }
}

/// How a failed attempt leaves its request: failed, or retrying a transient
/// provider failure, the only kind that retries.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptFailure {
    Final(Failure),
    /// The next attempt is due `delay` after the failure.
    Retrying {
        message: String,
        kind: ProviderErrorKind,
        delay: RetryDelay,
    },
}

impl AttemptFailure {
    /// Why the attempt failed, whether or not its request retries.
    pub fn failure(&self) -> Failure {
        match self {
            Self::Final(failure) => failure.clone(),
            Self::Retrying { message, kind, .. } => Failure::Provider(message.clone(), *kind),
        }
    }
}

/// How far a failed compaction round got before it failed.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionFailure {
    BeforeRequest,
    Requested(RequestSeq),
    Attempted(AttemptRef),
}

impl CompactionFailure {
    pub fn request(self) -> Option<RequestSeq> {
        match self {
            Self::BeforeRequest => None,
            Self::Requested(request) => Some(request),
            Self::Attempted(attempt) => Some(attempt.request),
        }
    }

    pub fn attempt(self) -> Option<AttemptRef> {
        match self {
            Self::Attempted(attempt) => Some(attempt),
            Self::BeforeRequest | Self::Requested(_) => None,
        }
    }
}

/// A named model profile exactly as it was applied, independent of later configuration.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ProfileSnapshot {
    pub name: ModelRef,
    pub profile: ModelProfile,
}

/// Durable replacement of one agent's model-visible history.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CompactionCheckpoint {
    pub frontier: RecordSeq,
    pub message: Message,
    /// Reconciled owner todos, activated atomically with this history replacement.
    pub todos: Vec<crate::session::TodoItem>,
    pub retained: Vec<MessageSeq>,
    /// The summary attempt that produced this checkpoint.
    pub attempt: AttemptRef,
    pub trigger: CompactionTrigger,
}

/// What made an agent compact.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    /// The completed response reported this occupancy, over the threshold.
    Occupancy(u64),
    /// A request overflowed the model's context window.
    Overflow,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ModelCallOrigin {
    pub message: MessageSeq,
    pub call_id: String,
}

/// Shared settings of every request in one model context. History and tail are
/// journaled per request; provider, model, reasoning and output budget come from
/// the profile.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ModelContext {
    pub purpose: ModelPurpose,
    pub profile: ProfileSnapshot,
    pub system: Vec<SystemSegment>,
    pub tools: Vec<Arc<ToolDefinition>>,
    pub response_schema: Option<ResponseSchema>,
}

impl ModelContext {
    /// A context with no system prompt, tools or response schema.
    #[cfg(test)]
    pub(crate) fn test(purpose: ModelPurpose, profile: ProfileSnapshot) -> Self {
        Self {
            purpose,
            profile,
            system: Vec::new(),
            tools: Vec::new(),
            response_schema: None,
        }
    }

    /// The history-free request these settings describe.
    pub fn template(&self) -> ModelRequest {
        let profile = &self.profile.profile;
        ModelRequest {
            model: profile.model.clone(),
            system: self.system.clone(),
            context: Vec::new(),
            history_lifetime: HistoryLifetime::default(),
            tools: self.tools.clone(),
            response_schema: self.response_schema.clone(),
            reasoning: profile.reasoning,
            max_output_tokens: profile.max_output,
            blobs: Default::default(),
        }
    }
}

/// The mode an entry applies. An entry whose definition differs from the name's
/// latest edition records a new edition; one without names the latest. Sessions run
/// the configured definitions, so earlier editions only show what entries ran under.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModeSelection {
    pub name: crate::config::mode::ModeName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<crate::config::mode::Edition>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Kinds)]
#[serde(tag = "type", rename_all = "snake_case")]
#[kinds(
    name = EntryKind,
    doc = "The journal's name for each event, also the entry's subtype table selector."
)]
pub enum SessionEvent {
    /// The creating host and the session's pinned target definitions.
    SessionStarted {
        host: crate::identity::HostName,
        targets: Vec<TargetDefinition>,
    },
    TitleSet {
        title: String,
        source: TitleSource,
    },
    /// The user's titles no longer apply; the automatic title shows again.
    TitleCleared,
    TargetsUpserted {
        targets: Vec<TargetDefinition>,
    },
    AgentStarted {
        #[serde(skip_serializing_if = "Option::is_none")]
        owner_job: Option<JobId>,
        profile: ProfileSnapshot,
        available_depth: u32,
        /// The mode the agent starts in, when it has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<ModeSelection>,
        capabilities: CapabilitySet,
        location: ExecutionLocation,
    },
    AgentCompleted,
    AgentInterrupted,
    /// A turn ended in a terminal failure that retains the agent for an external
    /// retry. Journaled so a resumed session re-arms its retry affordance instead
    /// of appearing idle; the agent itself never retries on this signal.
    AgentFailed {
        failure: Failure,
    },
    /// Model applied to a submitted user turn.
    ModelChanged {
        profile: ProfileSnapshot,
    },
    /// Mode applied to a submitted user turn, with the capabilities it granted.
    ModeChanged {
        mode: ModeSelection,
        capabilities: CapabilitySet,
    },
    TodosReplaced {
        items: Vec<crate::session::TodoItem>,
    },
    MessageCommitted {
        message: Message,
    },
    /// Host-facing conversation status, excluded from model-visible history.
    Status {
        message: String,
    },
    ModelContext {
        context: ModelContext,
    },
    /// A provider call with exact ordered messages, independent of future state/configuration.
    ModelRequested {
        context: RecordSeq,
        /// The compaction whose message opens the history, when the agent has one.
        checkpoint: Option<RecordSeq>,
        /// History after the checkpoint is its retained messages, then the agent's
        /// later commits up to `through`.
        through: Option<MessageSeq>,
        /// The first of those messages the request sent, when older ones were dropped for
        /// it to fit; the checkpoint's own message is always sent.
        from: Option<MessageSeq>,
        /// Request-specific messages sent after history.
        tail: Vec<Message>,
        history_lifetime: HistoryLifetime,
    },
    /// A new attempt starts for an existing frozen logical request. This resets
    /// its live output in host projections without changing model history.
    ModelAttemptStarted(AttemptRef),
    ModelFailed {
        attempt: AttemptRef,
        failure: AttemptFailure,
    },
    /// An attempt ended without an outcome: cancelled, or open when the session stopped.
    ModelAttemptInterrupted(AttemptRef),
    ResponseCompleted {
        attempt: AttemptRef,
        /// The committed assistant message this response produced.
        message: MessageSeq,
        outcome: CompletedOutcome,
    },
    Usage {
        request: RequestSeq,
        usage: Usage,
    },
    Compaction {
        checkpoint: CompactionCheckpoint,
    },
    CompactionFailed {
        failure: CompactionFailure,
        error: CompactionFault,
    },
    JobCreated {
        job: JobId,
        parent: Option<JobId>,
        origin: Option<ModelCallOrigin>,
        tool: String,
        role: JobRole,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<JobName>,
        arguments: Value,
        output_schema: Option<Arc<Value>>,
        accepts_input: bool,
        background: bool,
        location: ExecutionLocation,
    },
    JobStateChanged {
        job: JobId,
        state: JobTransition,
    },
    JobFinished {
        job: JobId,
        state: JobEnd,
        #[serde(skip_serializing_if = "Option::is_none")]
        diagnostic: Option<skyhook_tools::diagnostic::Diagnostic>,
        images: Vec<ImageRef>,
    },
    JobClaimed {
        job: JobId,
    },
    /// Cancellation was requested of a finished job something could still
    /// restart, which it no longer can.
    JobCancelRequested {
        job: JobId,
    },
    /// The job's foreground holder let go of it; it goes on in the background.
    JobReleased {
        job: JobId,
    },
    /// The child's held answer went to its owner as a reply, its invocation going on.
    JobMessageReleased {
        job: JobId,
    },
    /// The job's outcome was delivered to its owner.
    JobInjected {
        job: JobId,
    },
    /// The child reply committed at `source` reached the owner in `notification`,
    /// or, with none, as the result a call read before a restart superseded it.
    JobMessageDelivered {
        job: JobId,
        source: MessageSeq,
        notification: Option<MessageSeq>,
    },
}

impl EntryKind {
    pub(crate) fn is_activity(self) -> bool {
        !matches!(self, Self::AgentInterrupted)
    }

    /// Whether an agent's history fold reads this kind after its checkpoint's frontier.
    pub(crate) const fn history_folded(self) -> bool {
        matches!(
            self,
            Self::ModelRequested
                | Self::ResponseCompleted
                | Self::MessageCommitted
                | Self::ModeChanged
        )
    }
}

impl SessionEvent {
    /// A usage report for `request`; none when the provider reported nothing, so a
    /// missing report never reads as a measured zero.
    #[must_use]
    pub fn usage(request: RequestSeq, usage: Usage) -> Option<Self> {
        (usage != Usage::default()).then_some(Self::Usage { request, usage })
    }

    /// Whether this record is the session's activity, as the session list orders
    /// by. Closing a session is not: it interrupts its agents.
    pub fn is_activity(&self) -> bool {
        self.kind().is_activity()
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct EventRecord {
    pub sequence: RecordSeq,
    pub timestamp_millis: i64,
    pub agent: AgentId,
    pub event: SessionEvent,
}
