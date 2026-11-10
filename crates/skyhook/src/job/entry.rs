//! One job's facts: its phase, role state and delivery.
use super::*;

/// Where a job is in its lifecycle. A waiting job holds what it asks; a
/// finished one holds its published outcome.
pub(super) enum Phase {
    Queued,
    AwaitingApproval,
    Running,
    WaitingInput(Asking),
    Finished(Box<Finished>),
}

/// A published outcome. Saved results and captures stay in the database.
#[derive(Clone)]
pub(super) struct Finished {
    pub(super) end: JobEnd,
    pub(super) images: Vec<ImageRef>,
    pub(super) diagnostic: Option<Diagnostic>,
}

/// A phase change: the journaled transitions, plus what a live job asks.
pub(super) enum JobChange {
    Advance(JobTransition),
    Ask(Asking),
    Finish(Box<Finished>),
}

/// The shape of a change, checked before its event is appended.
#[derive(Clone, Copy)]
pub(super) enum JobStep {
    Advance(JobTransition),
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
            | (Self::WaitingInput, JobStep::Advance(Running)) => true,
            // Input can restart a finished invocation, unless it was cancelled.
            (Self::Completed | Self::Failed | Self::Interrupted, JobStep::Advance(Running)) => true,
            // Cancelling an interruption ends its resumability; nothing else
            // supersedes a published outcome.
            (Self::Interrupted, JobStep::Finish(end)) => end == JobEnd::Cancelled,
            (Self::Completed | Self::Failed | Self::Cancelled, JobStep::Finish(_)) => false,
            (_, JobStep::Finish(_)) => true,
            (_, JobStep::Advance(_)) => false,
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
    /// Live only: how far its question went.
    Question(AskPhase),
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
            JobRole::Question => Self::Question(AskPhase::Issued),
            JobRole::Agent => Self::Agent(Child::default()),
            JobRole::Script => Self::Script { wait_floor: None },
        }
    }

    pub(super) fn role(&self) -> JobRole {
        match self {
            Self::Tool => JobRole::Tool,
            Self::Wait { .. } => JobRole::Wait,
            Self::Question(_) => JobRole::Question,
            Self::Agent(_) => JobRole::Agent,
            Self::Script { .. } => JobRole::Script,
        }
    }
}

/// How far an ask job's question went.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AskPhase {
    Issued,
    /// It joined a batch of the questions its issuer asks together.
    Batched,
    /// An answer routed through the job asking it was accepted into its mailbox.
    Answered,
}

/// The agent an agent job launched and the replies it has published.
#[derive(Default)]
pub(super) struct Child {
    /// Installed by the launch; a launch still in progress has none.
    pub(super) agent: Option<AgentId>,
    /// Replies queued for delivery, oldest first, as they are delivered: to its
    /// owner as events while it reports to its owner, otherwise in its foreground
    /// call's result.
    pub(super) messages: Vec<Queued>,
    /// The latest text-only reply, which completing the job makes its result.
    pub(super) answer: Option<Answer>,
    /// Its turns and the jobs it created, across every invocation.
    pub(super) progress: AgentProgress,
    /// Live only: how far the current invocation got toward its child's turn.
    pub(super) start: Start,
    /// Live only, as the holds are: the delivery each outstanding hold of the job
    /// captured, once per hold.
    pub(super) held: Vec<u64>,
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
    /// When its turn committed, in epoch milliseconds.
    pub(super) at: i64,
    /// Live only, as the holds are: the pending delivery that presented it as its
    /// invocation's result before a restart superseded that result. A read that
    /// captured that delivery has it already, so it is held from its owner while
    /// such a hold is outstanding.
    pub(super) superseded: Option<u64>,
}

impl Queued {
    /// The reply as a foreground call carries it.
    pub(super) fn child_reply(&self) -> ChildReply {
        ChildReply {
            text: self.reply.text.clone(),
            at: chrono::DateTime::from_timestamp_millis(self.at).unwrap_or_default(),
        }
    }
}

/// A child's candidate answer.
#[derive(Kinds)]
#[kinds(name = AnswerKind, serde(rename_all = "snake_case"))]
pub(crate) enum Answer {
    /// Held until its invocation resolves: any end but completion, a later turn or
    /// `notify_owner` releases it.
    Held { reply: AgentMessage, at: i64 },
    /// Released to the owner early, and still queued: completion withdraws it, as
    /// the result delivers it.
    Released(MessageSeq),
    /// The result completion made it, which its owner has yet to see: a restart
    /// before then supersedes that result, and releases the answer as a reply.
    Result { reply: AgentMessage, at: i64 },
}

impl Child {
    /// Queue a reply for delivery.
    pub(super) fn queue(&mut self, reply: AgentMessage, at: i64, stamps: &Stamps) {
        let stamp = stamps.now();
        self.messages.push(Queued {
            stamp,
            reply,
            at,
            superseded: None,
        });
    }

    /// The queued replies its owner may be shown, oldest first: all but a
    /// superseded result an outstanding hold captured.
    pub(super) fn deliverable(&self) -> impl Iterator<Item = &Queued> {
        let held = |queued: &&Queued| {
            (queued.superseded).is_some_and(|delivery| self.held.contains(&delivery))
        };
        self.messages.iter().filter(move |queued| !held(queued))
    }

    /// When the newest deliverable reply was queued; replies are delivered oldest
    /// first, so it stays queued while any older one does.
    pub(super) fn queued(&self) -> Option<u64> {
        self.deliverable().last().map(|queued| queued.stamp)
    }

    /// Queue the held answer, if any. An answer already released stays marked,
    /// so completion can still withdraw it.
    pub(super) fn release(&mut self, stamps: &Stamps) {
        let held = self
            .answer
            .take_if(|answer| matches!(answer, Answer::Held { .. }));
        if let Some(Answer::Held { reply, at }) = held {
            self.answer = Some(Answer::Released(reply.message));
            self.queue(reply, at, stamps);
        }
    }

    /// Make the answer the result its completion delivers: withdrawn from the
    /// queued replies if it was released early.
    fn complete(&mut self) {
        self.answer = match self.answer.take() {
            Some(Answer::Held { reply, at }) => Some(Answer::Result { reply, at }),
            Some(Answer::Released(answer)) => (self.messages.iter())
                .position(|queued| queued.reply.message == answer)
                .map(|index| {
                    let Queued { reply, at, .. } = self.messages.remove(index);
                    Answer::Result { reply, at }
                }),
            Some(Answer::Result { .. }) | None => None,
        };
    }

    /// Queue the result a restart supersedes before its owner saw it as a reply,
    /// marked with `presented`, the delivery that presented it.
    fn supersede(&mut self, presented: Option<u64>, stamps: &Stamps) {
        if let Some(Answer::Result { reply, at }) = self.answer.take() {
            self.messages.push(Queued {
                stamp: stamps.now(),
                reply,
                at,
                superseded: presented,
            });
        }
    }

    /// The reply queued for the result that the delivery pending since
    /// `delivery` presented, before a restart superseded it.
    pub(super) fn superseded(&self, delivery: u64) -> Option<MessageSeq> {
        (self.messages.iter())
            .find(|queued| queued.superseded == Some(delivery))
            .map(|queued| queued.reply.message)
    }

    /// A hold that captured `delivery` ended.
    pub(super) fn unhold(&mut self, delivery: u64) {
        if let Some(index) = self.held.iter().position(|held| *held == delivery) {
            self.held.swap_remove(index);
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
    pub(super) invocation: Invocation,
    pub(super) timing: RunTiming,
    pub(super) cancel: CancelState,
    pub(super) live: Live,
}

/// What a job has only while this process holds it: a job read from the journal
/// starts with none of it.
#[derive(Default)]
pub(super) struct Live {
    /// What it runs with while it can still run. A job created here has them
    /// until it ends with nothing to restart it; one replayed from the journal
    /// gets them only once something restarts it.
    pub(super) handles: Option<JobHandles>,
    /// A finish holds the claim until its outcome applies or fails.
    pub(super) finishing: bool,
    /// The pending deliveries its runs read, which its finish acknowledges,
    /// however it ends.
    pub(super) reads: Vec<Delivery>,
    /// The deliveries its runs captured to present, each held for it from the
    /// capture until it ends, whether or not the read succeeds.
    pub(super) holds: Vec<Delivery>,
    /// What moved its call to the background, when its turn did.
    pub(super) moved: Option<super::MoveTrigger>,
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

    pub(super) fn asking(&self) -> Option<&Asking> {
        match &self.phase {
            Phase::WaitingInput(asking) => Some(asking),
            _ => None,
        }
    }

    /// How far an ask job's question went.
    pub(super) fn ask(&self) -> Option<AskPhase> {
        match self.role {
            RoleState::Question(phase) => Some(phase),
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
            JobChange::Advance(JobTransition::Running) => {
                match self.end() {
                    // A new invocation: the new outcome is a new delivery.
                    Some(previous) => {
                        // An interrupted foreground invocation still has its original
                        // waiter. Any other restart supersedes what its call would
                        // read, and reports through delivery.
                        if previous != JobEnd::Interrupted {
                            self.background = true;
                        }
                        let presented = match self.delivery {
                            DeliveryState::Pending { stamp } => Some(stamp),
                            DeliveryState::Acknowledged => None,
                        };
                        self.invocation.0 += 1;
                        self.pend_delivery(stamps);
                        if let Some(child) = self.child_mut() {
                            child.supersede(presented, stamps);
                            child.start = Start::Pending;
                        }
                    }
                    None if self.asking().is_some() => self.pend_delivery(stamps),
                    None => {}
                }
                self.phase = Phase::Running;
            }
            // A question is delivered like an outcome; its answer arrives as a wake.
            JobChange::Advance(JobTransition::WaitingInput) => {
                self.enter(JobChange::Ask(Asking::default()), stamps);
            }
            // Its owner answers a question, so the question reports through
            // delivery after the replies before it, whoever waits on the job.
            JobChange::Ask(asking) => {
                self.phase = Phase::WaitingInput(asking);
                self.pend_delivery(stamps);
            }
            JobChange::Finish(finished) => {
                let end = finished.end;
                self.phase = Phase::Finished(finished);
                self.live.finishing = false;
                // A new delivery even after an earlier question was acknowledged.
                self.pend_delivery(stamps);
                // What still works for the invocation it ends stops with it.
                let ended = self.invocation;
                let attached = (self.live.handles.as_mut())
                    .and_then(|handles| handles.task_abort.take_if(|(run, _)| *run == ended));
                if let Some((_, worker)) = attached {
                    worker.abort();
                }
                let call = !self.background;
                if let Some(child) = self.child_mut() {
                    if end == JobEnd::Completed {
                        child.complete();
                        if call {
                            // Its result saved them, as `saved_replies` read them.
                            child.messages.clear();
                        }
                    } else {
                        child.release(stamps);
                        child.answer = None;
                    }
                }
            }
        }
    }

    /// Waiting on a question for its owner to see. An ask job's question is the
    /// owner's own, answered by the host or the owner's parent, so only its answer
    /// is delivered.
    pub(super) fn asks_owner(&self) -> bool {
        self.asking().is_some() && self.role() != JobRole::Question
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

    /// Not finished, or finished but retained for resumption.
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
        if let Some(child) = self.child_mut() {
            (child.answer).take_if(|answer| matches!(answer, Answer::Result { .. }));
        }
    }

    /// The owner has claimed the pending delivery. Claiming a foreground job's
    /// outcome is its call returning, with the replies its result saved. A
    /// completed result ends the invocation, after which replies are only
    /// recorded, unless replies came after it; anything else ends the call. A
    /// question is its owner's to read and leaves the call waiting.
    pub(super) fn claimed(&mut self) {
        let left = self.child().is_some_and(|child| !child.messages.is_empty());
        if let Some(end) = self.end()
            && (left || end != JobEnd::Completed)
        {
            self.background = true;
        }
        self.acknowledge();
    }

    /// The replies finishing with `end` saves in the result, oldest first: a
    /// foreground call's completion saves every reply queued for it but an
    /// answer released early, which is the result's own reply. `None` when the
    /// finish saves none, which stays so: a released call stays released.
    pub(super) fn saved_replies(&self, end: JobEnd) -> Option<Vec<&Queued>> {
        let child = (self.child()).filter(|_| !self.background && end == JobEnd::Completed)?;
        let released = match child.answer {
            Some(Answer::Released(answer)) => Some(answer),
            _ => None,
        };
        let mut saved: Vec<_> = (child.messages.iter())
            .filter(|queued| Some(queued.reply.message) != released)
            .collect();
        saved.sort_by_key(|queued| queued.reply.message);
        Some(saved)
    }

    /// Whether its owner is shown what it reports, rather than a call waiting on
    /// it: everything once it is background, and its question, after the replies
    /// before it, whoever waits on it.
    fn reports_to_owner(&self) -> bool {
        self.background || self.asks_owner()
    }

    /// When the newest reply queued for the owner's notifications was queued: a
    /// foreground call's replies travel with its result instead.
    pub(super) fn replies_queued(&self) -> Option<u64> {
        (self.child().and_then(Child::queued)).filter(|_| self.reports_to_owner())
    }

    pub(super) fn unacknowledged(&self) -> bool {
        matches!(self.delivery, DeliveryState::Pending { .. })
    }

    pub(super) fn deliverable(&self) -> bool {
        self.concluded() || self.asks_owner()
    }

    /// Whether it has an outcome to deliver. A retryable interruption keeps parent
    /// waits pending; snapshots still expose the interrupted state to the user.
    pub(super) fn concluded(&self) -> bool {
        !self.suspended() && self.end().is_some()
    }

    /// The pending delivery a read of this entry presents.
    pub(super) fn delivery(&self, job: JobId) -> Option<Delivery> {
        match self.delivery {
            DeliveryState::Pending { stamp } if self.deliverable() => Some(Delivery { job, stamp }),
            _ => None,
        }
    }

    /// When the lifecycle delivery its owner has yet to see became pending.
    pub(super) fn lifecycle_pending(&self) -> Option<u64> {
        match self.delivery {
            DeliveryState::Pending { stamp } if self.reports_to_owner() && self.deliverable() => {
                Some(stamp)
            }
            _ => None,
        }
    }

    pub(super) fn envelope(&self, id: JobId) -> JobEnvelope {
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
            diagnostic,
            location: self.location.clone(),
            resumable: self.restart().is_some(),
            moved: self.live.moved,
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
