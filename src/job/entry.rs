//! One job's facts: its phase, role state and delivery.
use super::*;

/// Where a job is in its lifecycle. A waiting job holds the question it asked;
/// a finished one holds its published outcome.
pub(super) enum Phase {
    Queued,
    AwaitingApproval,
    Running,
    WaitingInput(QuestionOutput),
    Finished(Box<Finished>),
}

/// A published outcome. Saved results and captures stay in the database.
#[derive(Clone)]
pub(super) struct Finished {
    pub(super) end: JobEnd,
    pub(super) images: Vec<ImageRef>,
    pub(super) diagnostic: Option<Diagnostic>,
}

/// A phase change: the journaled transitions, plus the question a live job asks
/// and the answer that resumes it.
pub(super) enum JobChange {
    Advance(JobTransition),
    Ask(QuestionOutput),
    Answer,
    Finish(Box<Finished>),
}

/// The shape of a change, checked before its event is appended.
#[derive(Clone, Copy)]
pub(super) enum JobStep {
    Advance(JobTransition),
    Answer,
    Finish(JobEnd),
}

/// The job `event`, journaled by `agent`, changes once created, which a commit holds
/// first: its own job events, a child's start in it, and a launched child's turn or
/// job, which count toward the progress of the job `launched` names for its author.
pub(super) fn addressed(
    agent: &AgentId,
    event: &SessionEvent,
    launched: impl FnOnce(&AgentId) -> Option<JobId>,
) -> Option<JobId> {
    use crate::session::Message;
    match event {
        SessionEvent::JobStateChanged { job, .. }
        | SessionEvent::JobFinished { job, .. }
        | SessionEvent::JobClaimed { job }
        | SessionEvent::JobCancelRequested { job }
        | SessionEvent::JobInjected { job }
        | SessionEvent::JobReleased { job }
        | SessionEvent::JobMessageReleased { job }
        | SessionEvent::JobMessageDelivered { job, .. }
        | SessionEvent::AgentStarted {
            owner_job: Some(job),
            ..
        } => Some(*job),
        SessionEvent::MessageCommitted {
            message: Message::Assistant(_),
        }
        | SessionEvent::JobCreated { .. } => launched(agent),
        SessionEvent::SessionStarted { .. }
        | SessionEvent::TitleSet { .. }
        | SessionEvent::TitleCleared
        | SessionEvent::TargetsUpserted { .. }
        | SessionEvent::AgentStarted {
            owner_job: None, ..
        }
        | SessionEvent::TodosReplaced { .. }
        | SessionEvent::ModelChanged { .. }
        | SessionEvent::ModeChanged { .. }
        | SessionEvent::MessageCommitted { .. }
        | SessionEvent::Status { .. }
        | SessionEvent::ModelContext { .. }
        | SessionEvent::ModelRequested { .. }
        | SessionEvent::Compaction { .. }
        | SessionEvent::ModelAttemptStarted(_)
        | SessionEvent::ModelFailed { .. }
        | SessionEvent::ModelAttemptInterrupted(_)
        | SessionEvent::ResponseCompleted { .. }
        | SessionEvent::CompactionFailed { .. }
        | SessionEvent::Usage { .. }
        | SessionEvent::AgentCompleted
        | SessionEvent::AgentInterrupted
        | SessionEvent::AgentFailed { .. } => None,
    }
}

impl JobChange {
    /// The job and change a journaled event records, if it records one.
    pub(super) fn of(event: &SessionEvent) -> Option<(JobId, Self)> {
        match event {
            SessionEvent::JobStateChanged { job, state } => Some((*job, Self::Advance(*state))),
            SessionEvent::JobFinished {
                job,
                state,
                diagnostic,
                images,
            } => Some((
                *job,
                Self::Finish(Box::new(Finished {
                    end: *state,
                    images: images.clone(),
                    diagnostic: diagnostic.clone(),
                })),
            )),
            _ => None,
        }
    }

    pub(super) fn step(&self) -> JobStep {
        match self {
            Self::Advance(transition) => JobStep::Advance(*transition),
            Self::Ask(_) => JobStep::Advance(JobTransition::WaitingInput),
            Self::Answer => JobStep::Answer,
            Self::Finish(finished) => JobStep::Finish(finished.end),
        }
    }

    /// What the journal records for this change.
    pub(super) fn event(&self, job: JobId) -> SessionEvent {
        let state = match self {
            Self::Finish(finished) => {
                return SessionEvent::JobFinished {
                    job,
                    state: finished.end,
                    diagnostic: finished.diagnostic.clone(),
                    images: finished.images.clone(),
                };
            }
            Self::Advance(state) => *state,
            Self::Ask(_) => JobTransition::WaitingInput,
            Self::Answer => JobTransition::Running,
        };
        SessionEvent::JobStateChanged { job, state }
    }
}

impl JobState {
    /// Whether a job in this state admits `step`.
    pub(super) fn admits(self, step: JobStep) -> bool {
        use JobTransition::{AwaitingApproval, Running, WaitingInput};
        match (self, step) {
            (Self::Queued, JobStep::Advance(AwaitingApproval))
            | (Self::AwaitingApproval, JobStep::Advance(Running))
            // A call asked again before it starts, as a redefined target replans it.
            | (Self::Running, JobStep::Advance(AwaitingApproval))
            | (Self::Running | Self::WaitingInput, JobStep::Advance(WaitingInput))
            | (Self::WaitingInput, JobStep::Advance(Running) | JobStep::Answer) => true,
            // Input can restart a finished invocation, unless it was cancelled.
            (Self::Completed | Self::Failed | Self::Interrupted, JobStep::Advance(Running)) => true,
            // Cancelling an interruption ends its resumability; nothing else
            // supersedes a published outcome.
            (Self::Interrupted, JobStep::Finish(end)) => end == JobEnd::Cancelled,
            (Self::Completed | Self::Failed | Self::Cancelled, JobStep::Finish(_)) => false,
            (_, JobStep::Finish(_)) => true,
            (_, JobStep::Advance(_) | JobStep::Answer) => false,
        }
    }
}

/// A change the current phase does not admit, and the state that refused it.
pub(super) struct Rejected(JobState);

/// What a role keeps beyond the shared lifecycle.
pub(super) enum RoleState {
    Tool,
    Wait {
        parked: bool,
    },
    Question,
    Agent(Child),
    Script {
        /// What a `wait` hosted by this script last reported, so it is not
        /// told twice.
        wait_floor: Option<WaitFloor>,
    },
}

impl RoleState {
    pub(super) fn new(role: JobRole) -> Self {
        match role {
            JobRole::Tool => Self::Tool,
            JobRole::Wait => Self::Wait { parked: false },
            JobRole::Question => Self::Question,
            JobRole::Agent => Self::Agent(Child::default()),
            JobRole::Script => Self::Script { wait_floor: None },
        }
    }

    pub(super) fn role(&self) -> JobRole {
        match self {
            Self::Tool => JobRole::Tool,
            Self::Wait { .. } => JobRole::Wait,
            Self::Question => JobRole::Question,
            Self::Agent(_) => JobRole::Agent,
            Self::Script { .. } => JobRole::Script,
        }
    }
}

/// The agent an agent job launched and the replies it has published.
#[derive(Default)]
pub(super) struct Child {
    /// Installed by the launch; a launch still in progress has none.
    pub(super) agent: Option<AgentId>,
    /// Replies queued for delivery to the owner, oldest first, as they are delivered.
    pub(super) messages: Vec<Queued>,
    /// The latest text-only reply, which completing the job makes its result.
    pub(super) answer: Option<Answer>,
    /// Its turns and the jobs it created, across every invocation.
    pub(super) progress: AgentProgress,
    /// Live only: how far the current invocation got toward its child's turn.
    pub(super) start: Start,
}

/// Whether an invocation's child has taken the work that begins it. Until then
/// no turn of its runs for an interrupt to stop, so the interrupt stops the
/// invocation itself.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Start {
    #[default]
    Pending,
    /// Interrupted first: the child's first take parks its turn interrupted.
    Preempted,
    Begun,
}

/// A reply queued for delivery, stamped when it was queued.
pub(super) struct Queued {
    pub(super) stamp: u64,
    pub(super) reply: AgentMessage,
}

named_enum! {
    /// Whether a child's answer is held or released.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) parsed enum AnswerKind {
        Held = "held",
        Released = "released",
    }
}

/// A background child's candidate answer.
pub(super) enum Answer {
    /// Held until its invocation resolves: any end but completion, a later turn or
    /// `notify_owner` releases it.
    Held(AgentMessage),
    /// Released to the owner early, and still queued: completion withdraws it, as
    /// the result delivers it.
    Released(MessageSeq),
}

impl Child {
    /// Queue a reply for delivery to the owner.
    pub(super) fn queue(&mut self, reply: AgentMessage, stamps: &Stamps) {
        let stamp = stamps.now();
        self.messages.push(Queued { stamp, reply });
    }

    /// When the newest queued reply was queued; replies are delivered oldest
    /// first, so it stays queued while any older one does.
    pub(super) fn queued(&self) -> Option<u64> {
        self.messages.last().map(|queued| queued.stamp)
    }

    /// Queue the held answer, if any. An answer already released stays marked,
    /// so completion can still withdraw it.
    pub(super) fn release(&mut self, stamps: &Stamps) {
        let held = self
            .answer
            .take_if(|answer| matches!(answer, Answer::Held(_)));
        if let Some(Answer::Held(message)) = held {
            self.answer = Some(Answer::Released(message.message));
            self.queue(message, stamps);
        }
    }
}

/// When what becomes pending for delivery did so: the sequence of the record that
/// made it pending, so a wait floor covers exactly what its wait could see, and a
/// job folded from its own records alone is stamped as the whole journal stamps it.
#[derive(Default)]
pub(crate) struct Stamps(u64);

impl Stamps {
    /// Stamp what `record`, being applied, makes pending.
    pub(super) fn applying(&mut self, record: RecordSeq) {
        self.0 = record.get();
    }

    pub(super) fn now(&self) -> u64 {
        self.0
    }
}

/// The pending stamp and input revision a script's `wait` last reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WaitFloor {
    pub(crate) stamp: u64,
    pub(crate) input: u64,
}

impl Rejected {
    /// Input refused before its append: a published outcome, or a live state
    /// that takes no input.
    pub(super) fn input(self, job: JobId) -> JobError {
        if self.0.is_terminal() {
            JobError::AlreadyTerminal(job)
        } else {
            JobError::InputUnavailable {
                job,
                reason: InputUnavailableReason::State(self.0),
            }
        }
    }
}

pub(super) struct JobFacts {
    pub(super) origin: Option<crate::session::ModelCallOrigin>,
    pub(super) output_schema: Option<Arc<Value>>,
    pub(super) agent: AgentId,
    pub(super) parent: Option<JobId>,
    pub(super) tool: String,
    pub(super) role: RoleState,
    pub(super) name: Option<JobName>,
    /// Shared with every view of the job.
    pub(super) arguments: Arc<Value>,
    pub(super) created_at_millis: i64,
    pub(super) phase: Phase,
    pub(super) accepts_input: bool,
    pub(super) delivery: DeliveryState,
    pub(super) background: bool,
    pub(super) location: ExecutionLocation,
    /// Live only: what it runs with while it can still run. A job created
    /// here has them until it ends with nothing to restart it; one replayed
    /// from the journal gets them only once something restarts it.
    pub(super) handles: Option<JobHandles>,
    /// Live only: a finish holds the claim until its outcome applies or fails.
    pub(super) finishing: bool,
    /// Live only: the pending deliveries its runs read, which its finish
    /// acknowledges, however it ends.
    pub(super) reads: Vec<Delivery>,
    pub(super) invocation: Invocation,
    pub(super) timing: RunTiming,
    /// Live only.
    pub(super) cancel: CancelState,
}

/// Which invocation of a job is current; each restart of a finished job starts the next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Invocation(u32);

impl Invocation {
    pub(crate) fn nth(n: u32) -> Self {
        Self(n)
    }

    pub(crate) fn number(self) -> u32 {
        self.0
    }
}

/// When a job's current invocation ran, in epoch milliseconds: from its first
/// run, through waits for input, to its end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunTiming {
    /// It has not run.
    #[default]
    Unstarted,
    Since(i64),
    Took {
        since: i64,
        until: i64,
    },
}

impl RunTiming {
    /// The timing once the job entered `state` at `at`.
    pub(crate) fn after(self, state: JobState, at: i64) -> Self {
        match (state, self) {
            (JobState::Running | JobState::WaitingInput, Self::Since(_)) => self,
            (JobState::Running | JobState::WaitingInput, _) => Self::Since(at),
            (state, Self::Since(since)) if state.is_terminal() => Self::Took { since, until: at },
            (_, timing) => timing,
        }
    }
}

/// Invocation `1` of job `0`: what a worker runs and a finish ends, and the key
/// of the output rows it saves.
#[derive(Clone, Copy)]
pub(crate) struct Run(pub(crate) JobId, pub(crate) Invocation);

impl Run {
    /// Its output rows' key columns: the job and the run's generation.
    pub(crate) fn key(self) -> (u64, i64) {
        (self.0.get(), (self.1).0.into())
    }
}

impl JobFacts {
    pub(super) fn role(&self) -> JobRole {
        self.role.role()
    }

    /// The launched agent of an agent job.
    pub(super) fn child(&self) -> Option<&Child> {
        match &self.role {
            RoleState::Agent(child) => Some(child),
            _ => None,
        }
    }

    pub(super) fn child_mut(&mut self) -> Option<&mut Child> {
        match &mut self.role {
            RoleState::Agent(child) => Some(child),
            _ => None,
        }
    }

    /// The journaled projection of the phase.
    pub(super) fn state(&self) -> JobState {
        match &self.phase {
            Phase::Queued => JobState::Queued,
            Phase::AwaitingApproval => JobState::AwaitingApproval,
            Phase::Running => JobState::Running,
            Phase::WaitingInput(_) => JobState::WaitingInput,
            Phase::Finished(finished) => finished.end.into(),
        }
    }

    pub(super) fn finished(&self) -> Option<&Finished> {
        match &self.phase {
            Phase::Finished(finished) => Some(finished.as_ref()),
            _ => None,
        }
    }

    pub(super) fn end(&self) -> Option<JobEnd> {
        self.finished().map(|finished| finished.end)
    }

    pub(super) fn question(&self) -> Option<&QuestionOutput> {
        match &self.phase {
            Phase::WaitingInput(question) => Some(question),
            _ => None,
        }
    }

    /// Whether the phase admits `step`.
    pub(super) fn admits(&self, step: JobStep) -> Result<(), Rejected> {
        let state = self.state();
        state.admits(step).then_some(()).ok_or(Rejected(state))
    }

    /// Whether the phase admits `step` within the invocation running now: only a
    /// resume starts the next, so a finished one takes no further step.
    pub(super) fn steps(&self, step: JobStep) -> Result<(), Rejected> {
        let state = self.state();
        (self.end().is_none() && state.admits(step))
            .then_some(())
            .ok_or(Rejected(state))
    }

    /// Whether invocation `run` is current and its phase admits ending with `end`.
    /// Only a requested cancellation ends an interruption, never its own worker.
    pub(super) fn ends(&self, Run(_, invocation): Run, end: JobEnd) -> bool {
        self.invocation == invocation
            && self.admits(JobStep::Finish(end)).is_ok()
            && (self.end().is_none() || self.cancel_requested())
    }

    /// Enter the phase a change moves to, with every effect on delivery it
    /// implies; the caller checked that the phase admits it.
    pub(super) fn enter(&mut self, change: JobChange, stamps: &Stamps) {
        match change {
            JobChange::Advance(JobTransition::AwaitingApproval) => {
                self.phase = Phase::AwaitingApproval;
            }
            JobChange::Advance(JobTransition::Running) | JobChange::Answer => {
                match &self.phase {
                    // A new invocation: the new outcome is a new delivery.
                    Phase::Finished(previous) => {
                        self.invocation.0 += 1;
                        // An interrupted foreground invocation still has its original
                        // waiter. Any other restart reports through delivery.
                        if previous.end != JobEnd::Interrupted {
                            self.background = true;
                        }
                        self.pend_delivery(stamps);
                        if let Some(child) = self.child_mut() {
                            child.start = Start::Pending;
                        }
                    }
                    Phase::WaitingInput(_) => self.pend_delivery(stamps),
                    Phase::Queued | Phase::AwaitingApproval | Phase::Running => {}
                }
                self.phase = Phase::Running;
            }
            // A question is delivered like an outcome; its answer arrives as a wake.
            JobChange::Advance(JobTransition::WaitingInput) => {
                self.enter(JobChange::Ask(QuestionOutput::default()), stamps);
            }
            JobChange::Ask(question) => {
                self.phase = Phase::WaitingInput(question);
                self.pend_delivery(stamps);
                self.background = true;
            }
            JobChange::Finish(finished) => {
                let end = finished.end;
                self.phase = Phase::Finished(finished);
                self.finishing = false;
                // A new delivery even after an earlier question was acknowledged.
                self.pend_delivery(stamps);
                // What still works for the invocation it ends stops with it.
                let ended = self.invocation;
                let attached = (self.handles.as_mut())
                    .and_then(|handles| handles.task_abort.take_if(|(run, _)| *run == ended));
                if let Some((_, worker)) = attached {
                    worker.abort();
                }
                if let Some(child) = self.child_mut() {
                    if end != JobEnd::Completed {
                        child.release(stamps);
                    } else if let Some(Answer::Released(answer)) = child.answer {
                        child
                            .messages
                            .retain(|queued| queued.reply.message != answer);
                    }
                    child.answer = None;
                }
            }
        }
    }

    /// Waiting on a question for its owner to see. An ask job's question is the
    /// owner's own, answered by the host or the owner's parent, so only its answer
    /// is delivered.
    pub(super) fn asks_owner(&self) -> bool {
        self.question().is_some() && !matches!(self.role, RoleState::Question)
    }

    /// What runs it again once it ends, unless a cancellation was requested.
    pub(super) fn restart(&self) -> Option<Restart> {
        self.restart_shape().filter(|_| !self.cancel_requested())
    }

    /// What its journaled facts let run it again: the child its call installed,
    /// until cancelled, or the call itself, asked for again, while it is stopped
    /// before installing one. A cancellation counts here once recorded: one an
    /// interrupt or a close requested of a finished job in a tree it cancels is
    /// live only, so it cannot decide what reopening would read.
    fn restart_shape(&self) -> Option<Restart> {
        let child = (self.child())
            .filter(|_| self.accepts_input && self.cancel != CancelState::Recorded)?;
        match (self.end(), &child.agent) {
            (Some(JobEnd::Cancelled), _) | (Some(JobEnd::Completed | JobEnd::Failed), None) => None,
            (_, Some(_)) => Some(Restart::Child),
            (None | Some(JobEnd::Interrupted), None) => Some(Restart::Call),
        }
    }

    /// Interrupted, and something can restart it, or could until a cancellation
    /// that has yet to end it.
    pub(super) fn suspended(&self) -> bool {
        self.end() == Some(JobEnd::Interrupted) && self.restart_shape().is_some()
    }

    /// Not finished, or finished but retained for resumption. `active_states`
    /// deliberately differs: it presents a suspended job by its retained state.
    pub(super) fn live(&self) -> bool {
        self.end().is_none() || self.suspended()
    }

    /// Whether a lifecycle rule can still change it without being addressed by id:
    /// it is unfinished or interrupted, has a delivery or a reply pending, or is a
    /// child agent input can resume. Only journaled facts decide it, so a reopened
    /// session decides it alike.
    pub(super) fn resident(&self) -> bool {
        let replies = self.child().is_some_and(|child| !child.messages.is_empty());
        self.cancellable() || self.unacknowledged() || replies || self.restart_shape().is_some()
    }

    /// Still running, or interrupted: cancellation changes its outcome.
    pub(super) fn cancellable(&self) -> bool {
        self.end().is_none_or(|end| end == JobEnd::Interrupted)
    }

    pub(super) fn cancel_requested(&self) -> bool {
        self.cancel != CancelState::Open
    }

    /// Finished, and restartable until a cancellation is recorded: cancelling it
    /// changes no outcome, so the journal records the request.
    pub(super) fn records_cancel(&self) -> bool {
        !self.cancellable() && self.restart_shape().is_some()
    }

    /// Its current invocation's watchdog is armed and still changes its outcome.
    pub(super) fn cancelling(&self) -> bool {
        matches!(self.cancel, CancelState::Requested { watched: Some(watched) } if watched == self.invocation)
            && self.cancellable()
    }

    /// Failed or interrupted and restartable: a session-wide retry restarts it.
    /// Unlike an explicit send, it never restarts Completed.
    pub(super) fn retryable(&self) -> bool {
        matches!(self.end(), Some(JobEnd::Failed | JobEnd::Interrupted)) && self.restart().is_some()
    }

    /// Finished in a way input can restart, if anything restarts it.
    pub(super) fn resumable_end(&self) -> bool {
        matches!(
            self.end(),
            Some(JobEnd::Completed | JobEnd::Failed | JobEnd::Interrupted)
        )
    }

    pub(super) fn pend_delivery(&mut self, stamps: &Stamps) {
        self.delivery = DeliveryState::Pending {
            stamp: stamps.now(),
        };
    }

    /// The owner has seen the pending delivery, through a claim or a notification.
    pub(super) fn acknowledge(&mut self) {
        self.delivery = DeliveryState::Acknowledged;
    }

    pub(super) fn unacknowledged(&self) -> bool {
        matches!(self.delivery, DeliveryState::Pending { .. })
    }

    pub(super) fn deliverable(&self) -> bool {
        // Keep parent waits pending across a retryable interruption. Snapshots
        // still expose the interrupted state to the user.
        !self.suspended() && (self.end().is_some() || self.asks_owner())
    }

    /// The pending delivery a read of this entry presents.
    pub(super) fn delivery(&self, job: JobId) -> Option<Delivery> {
        match self.delivery {
            DeliveryState::Pending { stamp } if self.deliverable() => Some(Delivery { job, stamp }),
            _ => None,
        }
    }

    /// When the lifecycle delivery a background owner has yet to see became pending.
    pub(super) fn lifecycle_pending(&self) -> Option<u64> {
        match self.delivery {
            DeliveryState::Pending { stamp } if self.background && self.deliverable() => {
                Some(stamp)
            }
            _ => None,
        }
    }

    pub(super) fn metadata(&self, id: JobId) -> JobEnvelope {
        let finished = self.finished();
        let diagnostic = finished.and_then(|finished| finished.diagnostic.clone());
        JobEnvelope {
            id,
            agent: self.agent.clone(),
            arguments: Arc::clone(&self.arguments),
            invocation: self.invocation,
            timing: self.timing,
            parent: self.parent,
            tool: self.tool.clone(),
            role: self.role(),
            name: self.name.clone(),
            state: self.state(),
            images: finished.map_or_else(Vec::new, |finished| finished.images.clone()),
            origin: self.origin.clone(),
            question: None,
            diagnostic,
            location: self.location.clone(),
            resumable: self.restart().is_some(),
        }
    }

    pub(super) fn envelope(&self, id: JobId) -> JobEnvelope {
        JobEnvelope {
            question: self.question().cloned(),
            ..self.metadata(id)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeliveryState {
    /// `stamp` orders when it became pending.
    Pending {
        stamp: u64,
    },
    Acknowledged,
}

/// What a live job runs with, beside the facts the core holds.
pub(super) struct JobHandles {
    pub(super) input: mpsc::Sender<Value>,
    pub(super) cancellation: CancellationToken,
    /// The worker attached to the invocation running it, which a forced abort
    /// or that invocation's finish stops.
    pub(super) task_abort: Option<(Invocation, AbortHandle)>,
}

impl JobHandles {
    pub(super) fn new(cancellation: CancellationToken) -> (Self, mpsc::Receiver<Value>) {
        let (input, receiver) = mpsc::channel(JOB_INPUT_CAPACITY);
        let handles = Self {
            input,
            cancellation,
            task_abort: None,
        };
        (handles, receiver)
    }
}

/// Whether cancellation was requested, and for which invocation it armed the
/// forced-abort watchdog that settles whatever outcome it still changes; or,
/// requested of a finished job that could still restart, as the journal records
/// it.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum CancelState {
    #[default]
    Open,
    Requested {
        watched: Option<Invocation>,
    },
    Recorded,
}
