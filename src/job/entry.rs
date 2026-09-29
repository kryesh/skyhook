//! One job's live entry: its phase, role state and delivery.
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
    pub(super) output_diagnostic: Option<Diagnostic>,
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

impl JobChange {
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
                    output_diagnostic: finished.output_diagnostic.clone(),
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
    /// Installed by the live child: input can restart a finished invocation.
    pub(super) resume: Option<ResumeHandler>,
    /// Replies queued for delivery to the owner.
    pub(super) messages: Vec<AgentMessage>,
    /// When the newest queued reply was published; replies are delivered
    /// oldest first, so it stays queued while any older one does.
    pub(super) message_stamp: u64,
    /// The latest text-only reply, which completing the job makes its result.
    pub(super) answer: Option<Answer>,
}

/// A background child's candidate answer.
pub(super) enum Answer {
    /// Held until its invocation resolves: any end but completion, a later turn or
    /// `notify_owner` releases it.
    Held(AgentMessage),
    /// Released to the owner early. Completion withdraws it while still queued, as
    /// replay, which never saw the release, never queued it.
    Released(MessageSeq),
}

impl Child {
    /// Queue a reply for delivery to the owner.
    pub(super) fn queue(&mut self, message: AgentMessage) {
        self.message_stamp = next_pending_stamp();
        self.messages.push(message);
    }

    /// Queue the held answer, if any. An answer already released stays marked,
    /// so completion can still withdraw it.
    pub(super) fn release(&mut self) {
        let held = self
            .answer
            .take_if(|answer| matches!(answer, Answer::Held(_)));
        if let Some(Answer::Held(message)) = held {
            self.answer = Some(Answer::Released(message.message));
            self.queue(message);
        }
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

    /// A step whose order its caller guarantees, refused before its append: a
    /// published outcome, or else the phase moved underneath it.
    pub(super) fn lifecycle(self, job: JobId) -> JobError {
        if self.0.is_terminal() {
            JobError::AlreadyTerminal(job)
        } else {
            JobError::PhaseMoved(job)
        }
    }

    /// The change was checked before its append, so the phase moved underneath it.
    pub(super) fn journaled(self, id: JobId) -> JobError {
        JobError::PhaseMoved(id)
    }
}

pub(super) struct JobEntry {
    pub(super) origin: Option<crate::session::ModelCallOrigin>,
    pub(super) output_schema: Option<Value>,
    pub(super) agent: AgentId,
    pub(super) parent: Option<JobId>,
    pub(super) tool: String,
    pub(super) role: RoleState,
    pub(super) name: Option<JobName>,
    pub(super) created_at_millis: i64,
    pub(super) phase: Phase,
    pub(super) accepts_input: bool,
    pub(super) input: mpsc::Sender<Value>,
    pub(super) cancellation: CancellationToken,
    pub(super) notify: Arc<Notify>,
    pub(super) operation: Arc<Mutex<()>>,
    pub(super) task_abort: Option<AbortHandle>,
    pub(super) cancellation_watchdog_started: bool,
    pub(super) delivery: DeliveryState,
    pub(super) background: bool,
    pub(super) location: ExecutionLocation,
}

impl JobEntry {
    pub(super) fn new(spec: JobSpec, created_at_millis: i64) -> (Self, mpsc::Receiver<Value>) {
        let (input, receiver) = mpsc::channel(JOB_INPUT_CAPACITY);
        (
            Self {
                origin: spec.origin,
                output_schema: spec.output_schema,
                agent: spec.agent,
                parent: spec.parent,
                tool: spec.tool,
                role: RoleState::new(spec.role),
                name: spec.name,
                created_at_millis,
                phase: Phase::Queued,
                accepts_input: spec.accepts_input,
                input,
                cancellation: CancellationToken::new(),
                notify: Arc::new(Notify::new()),
                operation: Arc::new(Mutex::new(())),
                task_abort: None,
                cancellation_watchdog_started: false,
                delivery: DeliveryState::Pending {
                    stamp: next_pending_stamp(),
                },
                background: spec.background,
                location: spec.location,
            },
            receiver,
        )
    }

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

    /// Whether the phase admits `step`; `apply` changes nothing otherwise.
    pub(super) fn admits(&self, step: JobStep) -> Result<(), Rejected> {
        use JobTransition::{AwaitingApproval, Running, WaitingInput};
        let admitted = match (&self.phase, step) {
            (Phase::Queued, JobStep::Advance(AwaitingApproval))
            | (Phase::AwaitingApproval, JobStep::Advance(Running))
            | (Phase::Running | Phase::WaitingInput(_), JobStep::Advance(WaitingInput))
            | (Phase::WaitingInput(_), JobStep::Advance(Running) | JobStep::Answer) => true,
            (Phase::Finished(_), JobStep::Advance(Running)) => self.resumable_end(),
            // Cancelling an interruption ends its resumability; nothing else
            // supersedes a published outcome.
            (Phase::Finished(previous), JobStep::Finish(end)) => {
                previous.end == JobEnd::Interrupted && end == JobEnd::Cancelled
            }
            (_, JobStep::Finish(_)) => true,
            (_, JobStep::Advance(_) | JobStep::Answer) => false,
        };
        admitted.then_some(()).ok_or(Rejected(self.state()))
    }

    /// Move to the next phase with every live effect the change implies.
    pub(super) fn apply(&mut self, change: JobChange) -> Result<(), Rejected> {
        self.admits(change.step())?;
        match change {
            JobChange::Advance(JobTransition::AwaitingApproval) => {
                self.phase = Phase::AwaitingApproval;
            }
            JobChange::Advance(JobTransition::Running) | JobChange::Answer => {
                match &self.phase {
                    // A new invocation: the previous worker is gone and the new
                    // outcome is a new delivery.
                    Phase::Finished(previous) => {
                        // An interrupted foreground invocation still has its original
                        // waiter. Any other restart reports through delivery.
                        if previous.end != JobEnd::Interrupted {
                            self.background = true;
                        }
                        self.task_abort = None;
                        self.pend_delivery();
                    }
                    Phase::WaitingInput(_) => self.pend_delivery(),
                    Phase::Queued | Phase::AwaitingApproval | Phase::Running => {}
                }
                self.phase = Phase::Running;
            }
            JobChange::Advance(JobTransition::WaitingInput) => self.ask(QuestionOutput::default()),
            JobChange::Ask(question) => self.ask(question),
            JobChange::Finish(finished) => {
                let end = finished.end;
                self.task_abort = None;
                self.phase = Phase::Finished(finished);
                // A new delivery even after an earlier question was acknowledged.
                self.pend_delivery();
                if let Some(child) = self.child_mut() {
                    if end == JobEnd::Cancelled {
                        child.resume = None;
                    }
                    if end != JobEnd::Completed {
                        child.release();
                    } else if let Some(Answer::Released(answer)) = child.answer {
                        child.messages.retain(|message| message.message != answer);
                    }
                    child.answer = None;
                }
            }
        }
        Ok(())
    }

    /// A question is delivered like an outcome; its answer arrives as a wake.
    pub(super) fn ask(&mut self, question: QuestionOutput) {
        self.phase = Phase::WaitingInput(question);
        self.pend_delivery();
        self.background = true;
    }

    pub(super) fn waiting(&self) -> bool {
        self.question().is_some()
    }

    pub(super) fn resume(&self) -> Option<&ResumeHandler> {
        self.child()?.resume.as_ref()
    }

    /// Interrupted with a handler that can restart it.
    pub(super) fn suspended(&self) -> bool {
        self.end() == Some(JobEnd::Interrupted) && self.resume().is_some()
    }

    /// Not finished, or finished but retained for resumption. `active_states`
    /// deliberately differs: it presents a suspended job by its retained state.
    pub(super) fn live(&self) -> bool {
        self.end().is_none() || self.suspended()
    }

    /// Ended, other than by a retained interruption.
    pub(super) fn settled(&self) -> bool {
        self.end().is_some() && !self.suspended()
    }

    /// Still running, or interrupted: cancellation changes its outcome.
    pub(super) fn cancellable(&self) -> bool {
        self.end().is_none_or(|end| end == JobEnd::Interrupted)
    }

    /// Cancelled with an outcome its cancellation has yet to settle.
    pub(super) fn cancelling(&self) -> bool {
        self.cancellation_watchdog_started && self.cancellable()
    }

    /// Finished in a way input can restart, given a handler.
    pub(super) fn resumable_end(&self) -> bool {
        matches!(
            self.end(),
            Some(JobEnd::Completed | JobEnd::Failed | JobEnd::Interrupted)
        )
    }

    /// Failed or interrupted, uncancelled, with a live handler: a session-wide
    /// retry restarts it. Unlike an explicit send, it never restarts Completed.
    pub(super) fn session_retryable(&self) -> bool {
        matches!(self.end(), Some(JobEnd::Failed | JobEnd::Interrupted))
            && !self.cancellation.is_cancelled()
            && self.resume().is_some()
    }

    pub(super) fn pend_delivery(&mut self) {
        self.delivery = DeliveryState::Pending {
            stamp: next_pending_stamp(),
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
        !self.suspended() && (self.end().is_some() || self.waiting())
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
            parent: self.parent,
            tool: self.tool.clone(),
            role: self.role(),
            name: self.name.clone(),
            state: self.state(),
            output: None,
            origin: self.origin.clone(),
            question: None,
            diagnostic,
            output_diagnostic: finished.and_then(|finished| finished.output_diagnostic.clone()),
            location: self.location.clone(),
            resumable: self.accepts_input && self.resume().is_some(),
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
