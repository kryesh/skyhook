//! Session event schema: the in-memory form of normalized session database rows.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, EventId, JobId, QueueAttemptId},
    job::{JobRole, JobState},
    media::ImageRef,
    provider::{
        profile::ModelProfile,
        protocol::{
            HistoryLifetime, Message, ModelRequest, ResponseSchema, StopReason, SystemSegment,
            ToolDefinition, Usage, UserContent,
        },
    },
    target::TargetDefinition,
    tool::policy::{ApprovalGrant, Capability},
};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPurpose {
    Agent,
    Compaction,
}

/// A named model profile exactly as it was applied, independent of later configuration.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ProfileSnapshot {
    pub name: String,
    pub profile: ModelProfile,
}

/// Durable replacement of one agent's model-visible history.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CompactionCheckpoint {
    /// Version of the structured continuation schema used for this checkpoint.
    pub schema_version: u16,
    pub previous: Option<u64>,
    pub frontier: u64,
    pub message: Message,
    /// Reconciled owner todos, activated atomically with this history replacement.
    pub todos: Vec<crate::agent::TodoItem>,
    pub retained: Vec<u64>,
    pub request: u64,
    /// The summary attempt of `request` that produced this checkpoint.
    pub attempt: u64,
    pub max_context: u64,
    pub before_tokens: u64,
    pub after_tokens: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ModelCallOrigin {
    pub message: u64,
    pub call_id: String,
}

/// Immutable accepted submission. Session and destination agent are bound by its record.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct QueueIntent {
    pub attempt: QueueAttemptId,
    pub content: Vec<UserContent>,
    pub model: Option<String>,
}

/// Final resolution of an intent; NotCommitted explicitly abandons that attempt.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueueSettlement {
    Committed { event: EventId },
    NotCommitted,
}

/// Shared settings of every request in one model context. History and tail are
/// journaled per request; provider, model, reasoning and output budget come from
/// the profile and correlation from the agent.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ModelContext {
    pub purpose: ModelPurpose,
    pub profile: ProfileSnapshot,
    pub system: Vec<SystemSegment>,
    pub tools: Vec<ToolDefinition>,
    pub response_schema: Option<ResponseSchema>,
}

impl ModelContext {
    /// The history-free request these settings describe for `agent`.
    #[must_use]
    pub fn template(&self, agent: &AgentId) -> ModelRequest {
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
            correlation: Some(agent.to_string()),
            blobs: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    QueueIntent {
        intent: QueueIntent,
    },
    QueueSettlement {
        attempt: QueueAttemptId,
        settlement: QueueSettlement,
    },
    QueueAcknowledged {
        attempt: QueueAttemptId,
    },
    /// The session's pinned harness settings; every agent's capabilities are a subset.
    SessionStarted {
        targets: Vec<TargetDefinition>,
        capabilities: Vec<Capability>,
        max_child_depth: u32,
    },
    /// A process reopened the session and settled work that was in flight.
    SessionResumed,
    TitleSet {
        title: String,
    },
    TargetsUpserted {
        targets: Vec<TargetDefinition>,
    },
    AgentStarted {
        parent: Option<AgentId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        owner_job: Option<JobId>,
        /// None for a tool-only agent without a model, such as a remote worker.
        profile: Option<ProfileSnapshot>,
        available_depth: u32,
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
        context: u64,
        /// Journal sequences of the committed messages or compaction checkpoints sent as history.
        history: Vec<u64>,
        /// Request-specific messages sent after history.
        tail: Vec<Message>,
        history_lifetime: HistoryLifetime,
        purpose: ModelPurpose,
    },
    Compaction {
        checkpoint: CompactionCheckpoint,
    },
    /// A new attempt starts for an existing frozen logical request. This resets
    /// its live output in host projections without changing model history.
    ModelAttemptStarted {
        request: u64,
        attempt: u64,
    },
    ModelFailed {
        request: u64,
        attempt: u64,
        error: String,
        kind: ModelFailureKind,
    },
    /// An attempt ended without an outcome: cancelled, or open when the session stopped.
    ModelAttemptInterrupted {
        request: u64,
        attempt: u64,
    },
    /// Terminal stop reason of a response that completed the turn. Refusals and
    /// aborts fail the turn before this point and are recorded by `ModelFailed`
    /// instead, so this distinguishes an ordinary end of turn from truncation or
    /// a stop sequence.
    ResponseCompleted {
        request: u64,
        attempt: u64,
        /// The committed assistant message this response produced.
        message: Option<u64>,
        stop_reason: StopReason,
    },
    /// A failed model request will be retried after a recovery delay.
    /// This is host-facing status, not model-visible conversation history.
    ModelRecoveryScheduled {
        /// Sequence of the failed `ModelRequested` event.
        request: u64,
        /// The next logical invocation; the failed attempt is `attempt - 1`.
        attempt: u64,
        /// Historical bounded limits remain readable; None means retry indefinitely.
        #[serde(skip_serializing_if = "Option::is_none")]
        max_attempts: Option<u64>,
        /// Backoff scheduled before the next invocation.
        delay_millis: u64,
        /// The failure that triggered this recovery.
        error: String,
    },
    CompactionSkipped {
        request: u64,
        attempt: u64,
        reason: String,
    },
    CompactionFailed {
        request: Option<u64>,
        attempt: Option<u64>,
        error: String,
    },
    Usage {
        request: Option<u64>,
        usage: Usage,
    },
    JobCreated {
        job: JobId,
        parent: Option<JobId>,
        origin: Option<ModelCallOrigin>,
        tool: String,
        role: JobRole,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        arguments: Value,
        output_schema: Option<Value>,
        accepts_input: bool,
        background: bool,
        /// Host authorization scope the job's requests are grouped under.
        #[serde(skip_serializing_if = "Option::is_none")]
        authorization_scope: Option<u64>,
        location: ExecutionLocation,
    },
    /// A policy approved this grant for the rest of the session while authorizing `job`.
    ApprovalGranted {
        job: JobId,
        grant: ApprovalGrant,
    },
    /// The grant journaled at sequence `grant` no longer applies.
    ApprovalRevoked {
        grant: u64,
    },
    JobStateChanged {
        job: JobId,
        state: JobState,
    },
    JobFinished {
        job: JobId,
        state: JobState,
        error: Option<String>,
        images: Vec<ImageRef>,
        #[serde(skip_serializing_if = "Option::is_none")]
        denial: Option<crate::tool::Denial>,
    },
    JobClaimed {
        job: JobId,
    },
    /// The job's outcome was delivered to its owner, by the `notification`
    /// message committed in the same transaction when there is one.
    JobInjected {
        job: JobId,
        #[serde(skip_serializing_if = "Option::is_none")]
        notification: Option<u64>,
    },
    /// The child reply committed at `source` reached the owner in `notification`.
    JobMessageDelivered {
        job: JobId,
        source: u64,
        notification: u64,
    },
    QuestionOpened {
        job: JobId,
        question_id: String,
        questions: Value,
    },
    QuestionResolved {
        job: JobId,
        question_id: String,
        answers: Value,
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

/// Classification of a failed model request.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFailureKind {
    /// Transport, protocol, or validation failure.
    #[default]
    Error,
    /// The model declined to answer. Deterministic for a given request, so it is
    /// never retried automatically; only a parent agent or a human may retry it.
    Refusal,
}

impl SessionEvent {
    /// The attempt a queue event inherently names.
    pub(crate) fn queue_attempt(&self) -> Option<QueueAttemptId> {
        match self {
            Self::QueueIntent { intent } => Some(intent.attempt),
            Self::QueueSettlement { attempt, .. } | Self::QueueAcknowledged { attempt } => {
                Some(*attempt)
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct EventRecord {
    pub id: EventId,
    pub queue_attempt: Option<QueueAttemptId>,
    pub sequence: u64,
    pub timestamp_millis: i64,
    pub agent: AgentId,
    pub event: SessionEvent,
}

impl EventRecord {
    pub(crate) fn append_identity(&self) -> super::AppendIdentity {
        super::AppendIdentity {
            event: self.id,
            queue_attempt: self.queue_attempt,
            session: self.agent.session(),
            sequence: self.sequence,
        }
    }
}
