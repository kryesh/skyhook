//! Session event schema: the in-memory form of normalized session database rows.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, EventId, JobId},
    job::{JobEnd, JobRole, JobTransition},
    media::ImageRef,
    named_enum::named_enum,
    provider::{
        profile::{ModelProfile, ModelRef},
        protocol::{
            CutReason, HistoryLifetime, ModelRequest, Outcome, ResponseSchema, SystemSegment,
            ToolDefinition, Usage,
        },
    },
    session::Message,
    target::TargetDefinition,
    tool::{
        policy::{ApprovalGrant, Capability},
        registry::JobName,
    },
};

named_enum! {
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Deserialize)]
    pub enum ModelPurpose {
        Agent = "agent",
        Compaction = "compaction",
    }
}

named_enum! {
    /// Why a completed response ended early. Refusals and aborts fail the turn
    /// instead, so they are never journaled as a completed response.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Deserialize)]
    pub enum Truncation {
        MaxTokens = "max_tokens",
        Incomplete = "incomplete",
    }
}

/// How a response that completed the turn ended.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompletedOutcome {
    Answer,
    ToolUse,
    Cut(Truncation),
}

/// A refusal or abort is the cut that fails the turn instead.
impl TryFrom<Outcome> for CompletedOutcome {
    type Error = CutReason;

    fn try_from(outcome: Outcome) -> Result<Self, CutReason> {
        Ok(match outcome {
            Outcome::Answer => Self::Answer,
            Outcome::ToolUse => Self::ToolUse,
            Outcome::Cut(CutReason::MaxTokens) => Self::Cut(Truncation::MaxTokens),
            Outcome::Cut(CutReason::Incomplete) => Self::Cut(Truncation::Incomplete),
            Outcome::Cut(reason @ (CutReason::Refusal | CutReason::Aborted)) => return Err(reason),
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

        /// Spelled as an integer wherever a schema names it.
        impl schemars::JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                u64::schema_name()
            }
            fn schema_id() -> std::borrow::Cow<'static, str> {
                u64::schema_id()
            }
            fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
                u64::json_schema(generator)
            }
            fn inline_schema() -> bool {
                true
            }
        }

        impl $name {
            /// The number, for storage and wire formats that carry it as one.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
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
    pub(in crate::session) const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// This record's sequence as the request it is; the caller matched the event.
    #[must_use]
    pub const fn request(self) -> RequestSeq {
        RequestSeq(self.0)
    }

    /// This record's sequence as the commit it is; the caller matched the event.
    #[must_use]
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

/// How far a failed compaction round got before it failed.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionFailure {
    BeforeRequest,
    Requested(RequestSeq),
    Attempted(AttemptRef),
}

impl CompactionFailure {
    #[must_use]
    pub fn request(self) -> Option<RequestSeq> {
        match self {
            Self::BeforeRequest => None,
            Self::Requested(request) => Some(request),
            Self::Attempted(attempt) => Some(attempt.request),
        }
    }

    #[must_use]
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
    pub before_tokens: u64,
    pub after_tokens: u64,
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
    #[must_use]
    pub fn template(&self) -> ModelRequest {
        let profile = &self.profile.profile;
        ModelRequest {
            model: profile.model.clone(),
            system: self.system.clone(),
            history: Vec::new(),
            tail: Vec::new(),
            history_lifetime: HistoryLifetime::default(),
            tools: self.tools.clone(),
            response_schema: self.response_schema.clone(),
            reasoning: profile.reasoning.clone(),
            max_output_tokens: Some(profile.max_output),
            blobs: Default::default(),
        }
    }
}

/// The mode an entry applies. The session pins a mode's definition on its first use
/// and keeps it whatever the configuration later says.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModeSelection {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<crate::tool::policy::Mode>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    /// The session's pinned harness settings; every agent's capabilities are a subset.
    SessionStarted {
        targets: Vec<TargetDefinition>,
        capabilities: Vec<Capability>,
    },
    TitleSet {
        title: String,
    },
    TargetsUpserted {
        targets: Vec<TargetDefinition>,
    },
    AgentStarted {
        #[serde(skip_serializing_if = "Option::is_none")]
        owner_job: Option<JobId>,
        /// None for a tool-only agent without a model, such as a remote worker.
        profile: Option<ProfileSnapshot>,
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
        /// Committed messages sent after the checkpoint: its retained messages, then
        /// every later committed message up to the last one named.
        history: Vec<MessageSeq>,
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
    ModelFailed {
        attempt: AttemptRef,
        error: String,
        kind: ModelFailureKind,
    },
    /// An attempt ended without an outcome: cancelled, or open when the session stopped.
    ModelAttemptInterrupted(AttemptRef),
    /// Refusals and aborts fail the turn before this point and are recorded by
    /// `ModelFailed` instead.
    ResponseCompleted {
        attempt: AttemptRef,
        /// The committed assistant message this response produced.
        message: MessageSeq,
        outcome: CompletedOutcome,
    },
    /// The `ModelFailed` at `failure` will be retried after a recovery delay.
    /// This is host-facing status, not model-visible conversation history.
    ModelRecoveryScheduled {
        failure: RecordSeq,
        /// Backoff scheduled before the next invocation.
        delay_millis: u64,
    },
    CompactionSkipped {
        attempt: AttemptRef,
        reason: String,
    },
    CompactionFailed {
        failure: CompactionFailure,
        error: String,
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
        output_schema: Option<Value>,
        accepts_input: bool,
        background: bool,
        location: ExecutionLocation,
    },
    /// A policy approved this grant for the rest of the session.
    ApprovalGranted {
        grant: ApprovalGrant,
    },
    /// The grant journaled at sequence `grant` no longer applies.
    ApprovalRevoked {
        grant: RecordSeq,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        output_diagnostic: Option<crate::tool::diagnostic::Diagnostic>,
        images: Vec<ImageRef>,
    },
    JobClaimed {
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
        error: String,
    },
}

named_enum! {
    /// Classification of a failed model request.
    #[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq, Deserialize)]
    pub enum ModelFailureKind {
        /// Transport, protocol, or validation failure.
        #[default]
        Error = "error",
        /// The model declined to answer. Deterministic for a given request, so it is
        /// never retried automatically; only a parent agent or a human may retry it.
        Refusal = "refusal",
    }
}

named_enum! {
    /// The journal's name for each event, also the entry's subtype table selector.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Deserialize)]
    pub(crate) enum EntryKind {
        SessionStarted = "session_started",
        TitleSet = "title_set",
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
        ModelRecoveryScheduled = "model_recovery_scheduled",
        ModelAttemptInterrupted = "model_attempt_interrupted",
        ResponseCompleted = "response_completed",
        Usage = "usage",
        Compaction = "compaction",
        CompactionSkipped = "compaction_skipped",
        CompactionFailed = "compaction_failed",
        JobCreated = "job_created",
        JobStateChanged = "job_state_changed",
        JobFinished = "job_finished",
        JobClaimed = "job_claimed",
        JobInjected = "job_injected",
        JobMessageDelivered = "job_message_delivered",
        ApprovalGranted = "approval_granted",
        ApprovalRevoked = "approval_revoked",
    }
}

impl SessionEvent {
    pub(crate) fn kind(&self) -> EntryKind {
        match self {
            Self::SessionStarted { .. } => EntryKind::SessionStarted,
            Self::TitleSet { .. } => EntryKind::TitleSet,
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
            Self::ModelRecoveryScheduled { .. } => EntryKind::ModelRecoveryScheduled,
            Self::CompactionSkipped { .. } => EntryKind::CompactionSkipped,
            Self::CompactionFailed { .. } => EntryKind::CompactionFailed,
            Self::Usage { .. } => EntryKind::Usage,
            Self::JobCreated { .. } => EntryKind::JobCreated,
            Self::ApprovalGranted { .. } => EntryKind::ApprovalGranted,
            Self::ApprovalRevoked { .. } => EntryKind::ApprovalRevoked,
            Self::JobStateChanged { .. } => EntryKind::JobStateChanged,
            Self::JobFinished { .. } => EntryKind::JobFinished,
            Self::JobClaimed { .. } => EntryKind::JobClaimed,
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

impl EventRecord {
    pub(crate) fn append_identity(&self) -> super::AppendIdentity {
        super::AppendIdentity {
            event: self.id,
            session: self.agent.session(),
            sequence: self.sequence,
        }
    }
}
