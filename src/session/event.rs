//! Session event schema: the in-memory form of normalized session database rows.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use crate::{
    agent::{CompactionFault, Failure},
    execution::ExecutionLocation,
    identity::{AgentId, EventId, JobId},
    job::{JobEnd, JobRole, JobTransition},
    media::ImageRef,
    named_enum::named_enum,
    provider::{
        ProviderErrorKind,
        profile::{ModelProfile, ModelRef},
        protocol::{
            CutReason, HistoryLifetime, ModelRequest, Outcome, ResponseSchema, SystemSegment,
            ToolDefinition, Usage,
        },
    },
    session::Message,
    target::TargetDefinition,
    tool::{policy::Capability, registry::JobName},
};

named_enum! {
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Hash)]
    pub enum ModelPurpose {
        Agent = "agent",
        Compaction = "compaction",
    }
}

named_enum! {
    /// Where a session title came from. A title the user set is never replaced by
    /// an automatic one.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum TitleSource {
        /// The session's first prompt.
        Prompt = "prompt",
        User = "user",
        /// A summary of the conversation.
        Generated = "generated",
    }
}

named_enum! {
    /// Why a completed response ended early. A refusal fails the turn instead, so it
    /// is never journaled as a completed response.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum Truncation {
        MaxTokens = "max_tokens",
        Incomplete = "incomplete",
    }
}

crate::named_enum::detailed_enum! {
    /// How a response that completed the turn ended.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum CompletedOutcome / ResponseOutcome {
        Answer = "answer",
        ToolUse = "tool_use",
        Cut(Truncation) = "cut",
    }
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
    pub todos: Vec<crate::agent::TodoItem>,
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
    pub tools: Vec<ToolDefinition>,
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
            reasoning: profile.reasoning.clone(),
            max_output_tokens: Some(profile.max_output),
            blobs: Default::default(),
        }
    }
}

/// The mode an entry applies. An entry whose definition differs from the name's
/// latest edition records a new edition; one without names the latest. Sessions run
/// the configured definitions, so earlier editions only show what entries ran under.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModeSelection {
    pub name: crate::tool::policy::ModeName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<crate::tool::policy::Mode>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// The session's pinned target definitions.
    SessionStarted {
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
        capabilities: Vec<Capability>,
        location: ExecutionLocation,
    },
    TodosReplaced {
        items: Vec<crate::agent::TodoItem>,
    },
    /// Model applied to a submitted user turn; never a pending UI selection.
    ModelChanged {
        profile: ProfileSnapshot,
    },
    /// Mode applied to a submitted user turn, with the capabilities it granted.
    ModeChanged {
        mode: ModeSelection,
        capabilities: Vec<Capability>,
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
    Compaction {
        checkpoint: CompactionCheckpoint,
    },
    /// A new attempt starts for an existing frozen logical request. This resets
    /// its live output in host projections without changing model history.
    ModelAttemptStarted(AttemptRef),
    /// A refusal is never retried automatically: it is deterministic for a given
    /// request, so only a parent agent or a human may retry it.
    ModelFailed {
        attempt: AttemptRef,
        failure: AttemptFailure,
    },
    /// An attempt ended without an outcome: cancelled, or open when the session stopped.
    ModelAttemptInterrupted(AttemptRef),
    /// A refusal fails the turn before this point and is recorded by `ModelFailed`
    /// instead.
    ResponseCompleted {
        attempt: AttemptRef,
        /// The committed assistant message this response produced.
        message: MessageSeq,
        outcome: CompletedOutcome,
    },
    CompactionFailed {
        failure: CompactionFailure,
        error: CompactionFault,
    },
    Usage {
        request: RequestSeq,
        usage: Usage,
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
        diagnostic: Option<crate::tool::diagnostic::Diagnostic>,
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
    /// The child reply committed at `source` reached the owner in `notification`.
    JobMessageDelivered {
        job: JobId,
        source: MessageSeq,
        notification: MessageSeq,
    },
    AgentCompleted,
    AgentInterrupted,
    /// A turn ended in a terminal failure that retains the agent for an external
    /// retry. Journaled so a resumed session re-arms its retry affordance instead
    /// of appearing idle; the agent itself never retries on this signal.
    AgentFailed {
        failure: Failure,
    },
}

named_enum! {
    /// The journal's name for each event, also the entry's subtype table selector.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub(crate) enum EntryKind {
        SessionStarted = "session_started",
        TitleSet = "title_set",
        TitleCleared = "title_cleared",
        TargetsUpserted = "targets_upserted",
        AgentStarted = "agent_started",
        AgentCompleted = "agent_completed",
        AgentInterrupted = "agent_interrupted",
        AgentFailed = "agent_failed",
        ModelChanged = "model_changed",
        ModeChanged = "mode_changed",
        TodosReplaced = "todos_replaced",
        MessageCommitted = "message_committed",
        Status = "status",
        ModelContext = "model_context",
        ModelRequested = "model_requested",
        ModelAttemptStarted = "model_attempt_started",
        ModelFailed = "model_failed",
        ModelAttemptInterrupted = "model_attempt_interrupted",
        ResponseCompleted = "response_completed",
        Usage = "usage",
        Compaction = "compaction",
        CompactionFailed = "compaction_failed",
        JobCreated = "job_created",
        JobStateChanged = "job_state_changed",
        JobFinished = "job_finished",
        JobClaimed = "job_claimed",
        JobCancelRequested = "job_cancel_requested",
        JobReleased = "job_released",
        JobMessageReleased = "job_message_released",
        JobInjected = "job_injected",
        JobMessageDelivered = "job_message_delivered",
    }
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

    pub(crate) fn kind(&self) -> EntryKind {
        match self {
            Self::SessionStarted { .. } => EntryKind::SessionStarted,
            Self::TitleSet { .. } => EntryKind::TitleSet,
            Self::TitleCleared => EntryKind::TitleCleared,
            Self::TargetsUpserted { .. } => EntryKind::TargetsUpserted,
            Self::AgentStarted { .. } => EntryKind::AgentStarted,
            Self::TodosReplaced { .. } => EntryKind::TodosReplaced,
            Self::ModelChanged { .. } => EntryKind::ModelChanged,
            Self::ModeChanged { .. } => EntryKind::ModeChanged,
            Self::MessageCommitted { .. } => EntryKind::MessageCommitted,
            Self::Status { .. } => EntryKind::Status,
            Self::ModelContext { .. } => EntryKind::ModelContext,
            Self::ModelRequested { .. } => EntryKind::ModelRequested,
            Self::Compaction { .. } => EntryKind::Compaction,
            Self::ModelAttemptStarted { .. } => EntryKind::ModelAttemptStarted,
            Self::ModelFailed { .. } => EntryKind::ModelFailed,
            Self::ModelAttemptInterrupted { .. } => EntryKind::ModelAttemptInterrupted,
            Self::ResponseCompleted { .. } => EntryKind::ResponseCompleted,
            Self::CompactionFailed { .. } => EntryKind::CompactionFailed,
            Self::Usage { .. } => EntryKind::Usage,
            Self::JobCreated { .. } => EntryKind::JobCreated,
            Self::JobStateChanged { .. } => EntryKind::JobStateChanged,
            Self::JobFinished { .. } => EntryKind::JobFinished,
            Self::JobClaimed { .. } => EntryKind::JobClaimed,
            Self::JobCancelRequested { .. } => EntryKind::JobCancelRequested,
            Self::JobReleased { .. } => EntryKind::JobReleased,
            Self::JobMessageReleased { .. } => EntryKind::JobMessageReleased,
            Self::JobInjected { .. } => EntryKind::JobInjected,
            Self::JobMessageDelivered { .. } => EntryKind::JobMessageDelivered,
            Self::AgentCompleted => EntryKind::AgentCompleted,
            Self::AgentInterrupted => EntryKind::AgentInterrupted,
            Self::AgentFailed { .. } => EntryKind::AgentFailed,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct EventRecord {
    pub id: EventId,
    pub sequence: RecordSeq,
    pub timestamp_millis: i64,
    pub agent: AgentId,
    pub event: SessionEvent,
}
