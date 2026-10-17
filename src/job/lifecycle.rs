//! Job creation, state transitions, and terminal result supervision.

use super::*;

/// Proof of a claim on invocation `0`'s finish. Only an outcome applying ends
/// it, never a drop, so its holder runs to the end as a completion owner does.
/// Once a later invocation runs, nothing it holds applies to that one.
pub(super) struct FinishClaim(Run);

pub(super) enum Unclaimed {
    /// Another finish holds the claim; the phase it leaves may still admit this end.
    InFlight,
    /// No such job, a later invocation runs, or its phase admits no such end.
    Ended,
}

impl JobManager {
    pub async fn create(&self, mut spec: JobSpec) -> Result<JobLease, JobError> {
        if spec.agent.session() != self.inner.store.id() {
            return Err(SessionError::WrongSession.into());
        }
        let manager = self.clone();
        self.inner
            .store
            .commit::<_, JobError>(Command::CreateJob, move |state| {
                if let Some(parent) = spec.parent
                    && !state.jobs.entries.contains_key(&parent)
                {
                    return Err(JobError::Unknown(parent));
                }
                // Only a busy turn makes calls.
                if spec.origin.is_some() && !state.agents.busy(&spec.agent) {
                    return Err(JobError::Interrupted);
                }
                let id = state.jobs.next_id()?;
                let created = SessionEvent::JobCreated {
                    origin: spec.origin,
                    job: id,
                    parent: spec.parent,
                    tool: spec.tool,
                    role: spec.role,
                    name: spec.name,
                    arguments: std::mem::take(&mut spec.arguments),
                    output_schema: spec.output_schema,
                    accepts_input: spec.accepts_input,
                    background: spec.background,
                    location: spec.location,
                };
                let parent = spec.parent;
                // A caller that gives up drops the lease wherever it lands, which
                // cancels the job.
                Ok(Plan::new(vec![(spec.agent, created)], move |state, _| {
                    let cancellation = manager.token_under(parent);
                    let (handles, input) = JobHandles::new(cancellation.clone());
                    manager
                        .inner
                        .handles
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(id, handles);
                    // Created under a cancelled parent, it is cancelled with it.
                    let jobs = &state.jobs.entries;
                    let parent = parent.and_then(|parent| jobs.get(&parent));
                    if parent.is_some_and(JobFacts::cancel_requested) {
                        manager.cancel_trees(state, vec![id], CancelScope::Outcome);
                    }
                    let run = Run(id, Invocation::default());
                    JobLease::new(manager, run, cancellation, input)
                }))
            })
            .await
    }

    /// A new token under `parent`'s, or the session root's.
    pub(super) fn token_under(&self, parent: Option<JobId>) -> CancellationToken {
        match parent {
            Some(parent) => self.handles(parent, |parent| parent.cancellation.child_token()),
            None => self.inner.supervisor.token(),
        }
    }

    /// Journal one change to a job, refused, as `refused` reports it, when the
    /// phase does not admit its step.
    pub(super) async fn journal_change(
        &self,
        id: JobId,
        change: JobChange,
        refused: fn(Rejected, JobId) -> JobError,
    ) -> Result<(), JobError> {
        // A question is delivered, so it wakes a background owner.
        let wakes = matches!(change, JobChange::Ask(_));
        let changed = move |state: &CoreState| {
            let entry = state.jobs.entries.get(&id).ok_or(JobError::Unknown(id))?;
            entry
                .steps(change.step())
                .map_err(|rejected| refused(rejected, id))?;
            let journaled = vec![(entry.agent.clone(), change.event(id))];
            Ok(Plan::new(journaled, move |state, _| {
                let entry = state.jobs.entries.get_mut(&id).expect("jobs stay");
                // The journal records only that it asked.
                if let JobChange::Ask(question) = change {
                    entry.phase = Phase::WaitingInput(question);
                }
                if wakes {
                    woken(state, id);
                }
            }))
        };
        self.inner.store.commit(Command::Journal, changed).await
    }

    /// A lease's startup transition. Its stage guarantees the order, so the only
    /// phase that refuses is a job finished underneath it.
    pub(super) async fn advance(&self, id: JobId, state: JobTransition) -> Result<(), JobError> {
        let change = JobChange::Advance(state);
        (self.journal_change(id, change, |_, job| JobError::AlreadyTerminal(job))).await
    }

    /// Publish the outcome of invocation `run` as its completion owner, once no
    /// other finish is in flight. A phase that then admits no such end, or a later
    /// invocation, keeps its outcome. Nothing races the claim it takes: only a
    /// closing session's abort abandons it.
    pub(super) async fn settle(&self, run: Run, outcome: JobOutcome) {
        let end = outcome.end();
        let claimed = self
            .inner
            .store
            .until(move |state| match claim(state, run, end) {
                Ok(claim) => Some(Some(claim)),
                Err(Unclaimed::Ended) => Some(None),
                Err(Unclaimed::InFlight) => None,
            });
        if let Ok(Some(claim)) = claimed.await {
            self.publish_claimed(claim, outcome).await;
        }
    }

    /// Publish `outcome` as the finish `claim` holds. One the journal refuses
    /// leaves the job as the journal does.
    pub(super) async fn publish_claimed(&self, claim: FinishClaim, outcome: JobOutcome) {
        let FinishClaim(Run(id, _)) = claim;
        if self.publish(&claim, outcome).await.is_err() {
            let released = move |state: &mut CoreState| {
                if let Some(entry) = state.jobs.entries.get_mut(&id) {
                    entry.finishing = false;
                }
            };
            self.inner.store.live(released).await;
        }
    }

    /// Journal the finish `claim` holds with `outcome`'s output, and the
    /// acknowledgement of what the job read, refused once a later invocation
    /// runs. An output that cannot be prepared stops the session.
    async fn publish(&self, claim: &FinishClaim, outcome: JobOutcome) -> Result<(), JobError> {
        let FinishClaim(run @ Run(id, invocation)) = *claim;
        let end = outcome.end();
        let (finish, output) = self.finished(run, outcome).await?;
        let manager = self.clone();
        let finished = move |state: &CoreState| {
            let entry = state.jobs.entries.get(&id).ok_or(JobError::Unknown(id))?;
            (entry.ends(run, end).then_some(())).ok_or(JobError::AlreadyTerminal(id))?;
            let mut journaled = vec![(entry.agent.clone(), finish.event(id))];
            journaled.extend(Delivery::claims(entry.reads.clone(), state));
            let acknowledged = Plan::new(journaled, move |state, _| {
                if let Some(entry) = state.jobs.entries.get_mut(&id) {
                    entry.reads.clear();
                }
                let attached = |handles: &mut JobHandles| {
                    handles
                        .task_abort
                        .take_if(|(attached, _)| *attached == invocation)
                };
                if let Some((_, worker)) = manager.handles(id, attached) {
                    worker.abort();
                }
                woken(state, id)
            });
            Ok(acknowledged.saving(output))
        };
        self.inner.store.commit(Command::Journal, finished).await
    }

    /// The change that ends invocation `run` with `outcome`, with the output it
    /// saves unless an earlier finish of the run saved one; refused unless that
    /// invocation is current and its phase admits the end. The output is prepared
    /// here, off the core.
    pub(super) async fn finished(
        &self,
        run: Run,
        outcome: JobOutcome,
    ) -> Result<(JobChange, Option<Prepared>), JobError> {
        let Run(id, invocation) = run;
        let end = outcome.end();
        let (output, mut diagnostic) = match outcome {
            JobOutcome::Completed(output) => (Some(output), None),
            JobOutcome::NoResult => (None, None),
            JobOutcome::Failed { diagnostic, output } => (output, Some(diagnostic)),
        };
        // A job that completes without a result left nothing unfinished.
        let capture_complete = output.as_ref().map_or(end == JobEnd::Completed, |output| {
            output.streams == crate::tool::StreamEnd::Finished
        });
        let has_result = output.is_some();
        let crate::tool::output::Output {
            value,
            images,
            captures,
            presented,
            ..
        } = output.unwrap_or_default();
        let output = has_result.then_some(value);
        let mut finished = Box::new(Finished {
            end,
            images,
            diagnostic: None,
        });
        let (previous, schema) = self
            .entry(id, |entry| {
                let schema = entry.output_schema.clone().unwrap_or(Value::Bool(true));
                (entry.ends(run, end)).then(|| (entry.finished().cloned(), schema))
            })?
            .ok_or(JobError::AlreadyTerminal(id))?;
        let rows = if let Some(previous) = previous {
            // Cancelling an interruption ends resumability, not its published
            // output, which stays as it was saved.
            if let Some(interrupted) = previous.diagnostic {
                diagnostic = diagnostic.map(|d| d.or(interrupted.context.into()));
            }
            finished.images = previous.images;
            None
        } else {
            let saved = output::Output::of(self.store(), id, invocation);
            // Captures have independent pointer/type metadata. A failed tool need not
            // produce a result, and unfinished JSON captures are not valid result trees.
            let product = (output, capture_complete);
            let rows = tokio::task::spawn_blocking(move || {
                output::prepare_completed(&saved, &schema, product, captures, presented)
            });
            Some(match rows.await {
                Ok(rows) => rows.map_err(SessionError::Io),
                Err(panicked) => Err(SessionError::Task(panicked)),
            })
        };
        finished.diagnostic = diagnostic.map(PartialDiagnostic::resolve);
        Ok((JobChange::Finish(finished), rows))
    }
}

/// Wake job `id`'s owner if what changed it leaves it something to deliver:
/// the one rule for every change that can make an outcome, a question or a
/// reply pending. A wake only opens its owner's coalescing window.
pub(crate) fn woken(state: &mut CoreState, id: JobId) {
    let entry = &state.jobs.entries[&id];
    if entry.has_pending() {
        state.agents.wake(&entry.agent);
    }
}

/// Claim the finish of `run` with `end` before any output is saved, unless
/// another finish holds it.
pub(super) fn claim(
    state: &mut CoreState,
    run: Run,
    end: JobEnd,
) -> Result<FinishClaim, Unclaimed> {
    let entry = state.jobs.entries.get_mut(&run.0).ok_or(Unclaimed::Ended)?;
    entry.ends(run, end).then_some(()).ok_or(Unclaimed::Ended)?;
    let free = !std::mem::replace(&mut entry.finishing, true);
    free.then_some(FinishClaim(run)).ok_or(Unclaimed::InFlight)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestRuntime;
    use crate::tool::{ToolOptions, ToolRegistryBuilder};

    impl JobManager {
        /// Claim the finish of `run` with `end` before any output is saved.
        pub(in crate::job) async fn claim(
            &self,
            run: Run,
            end: JobEnd,
        ) -> Result<FinishClaim, Unclaimed> {
            self.inner
                .store
                .live(move |state| claim(state, run, end))
                .await
        }

        /// Finish a job's current invocation, unless another finish holds its claim.
        pub(crate) async fn finish(&self, id: JobId, outcome: JobOutcome) -> Result<(), JobError> {
            let claim = self.claim(self.run(id), outcome.end()).await;
            let claim = claim.map_err(|_| JobError::AlreadyTerminal(id))?;
            self.publish(&claim, outcome).await
        }

        /// Job `id`'s current invocation.
        pub(in crate::job) fn run(&self, id: JobId) -> Run {
            Run(id, self.entry(id, |entry| entry.invocation).unwrap())
        }
    }

    #[derive(Deserialize, JsonSchema)]
    struct NoArgs {}

    #[tokio::test]
    async fn invalid_create_preflight_does_not_append_or_allocate() {
        let (_root, jobs, agent) = super::super::tests::runtime().await;
        let before = jobs.store().records().len();
        let unknown = JobId::new(99).unwrap();
        let spec = JobSpec {
            parent: Some(unknown),
            ..JobSpec::test(agent.clone(), "invalid-parent")
        };
        assert!(matches!(jobs.create(spec).await, Err(JobError::Unknown(id)) if id == unknown));
        let other = AgentId::root(crate::identity::SessionId::generate().unwrap());
        assert!(matches!(
            jobs.create(JobSpec::test(other, "wrong-session")).await,
            Err(JobError::Session(SessionError::WrongSession))
        ));
        assert_eq!(jobs.store().records().len(), before);
        assert!(jobs.facts(|jobs| jobs.entries.is_empty()));
        assert_eq!(
            jobs.test_create(JobSpec::test(agent, "valid")).await.get(),
            1
        );
    }

    /// A capture that cannot be admitted (invalid UTF-8 or unreadable) is
    /// published as incomplete; it never turns a completed job into a volatile,
    /// unjournaled failure.
    #[tokio::test]
    async fn invalid_or_unreadable_text_captures_still_journal_completion() {
        use crate::job::output::{CaptureKind, PendingCapture};
        use std::io::Write as _;
        for unreadable in [false, true] {
            let (_root, jobs, agent) = super::super::tests::runtime().await;
            let id = jobs.test_create(JobSpec::test(agent, "capture")).await;
            let output = jobs.output(id);
            let field = "/result/stdout".parse().unwrap();
            let mut writer = PendingCapture::create(&output, &field, CaptureKind::Text)
                .unwrap()
                .open();
            writer.write_all(b"partial \xff output").unwrap();
            let capture = writer.finish().unwrap();
            if unreadable {
                output.test_delete_capture(&capture);
            }
            let product =
                ToolOutput::new(serde_json::json!({"exit_code": 0})).with_captures(vec![capture]);
            jobs.test_complete(id, product).await;
            assert_eq!(jobs.snapshot(id).unwrap().state, JobState::Completed);
            let journaled = jobs.store().records().into_iter().any(|record| {
                matches!(
                    record.event,
                    SessionEvent::JobFinished { job, state: JobEnd::Completed, .. } if job == id
                )
            });
            assert!(journaled, "unreadable: {unreadable}");
            let replay = jobs.test_replay();
            let replayed = replay.test_snapshot(id).await.unwrap();
            assert_eq!(replayed.state, JobState::Completed);
            assert_eq!(replayed.output, Some(serde_json::json!({"exit_code": 0})));
        }
    }

    /// A handler panic wakes waiters with a failed job rather than leaving it
    /// running.
    #[tokio::test]
    async fn handler_panics_are_supervised_as_failed_jobs() {
        let runtime = TestRuntime::new().await;
        let mut builder = ToolRegistryBuilder::default();
        let options = ToolOptions::new(Vec::new()).background();
        builder
            .register::<NoArgs, String, _, _>("panic", "panic", options, |_, _| async move {
                panic!("handler panic");
            })
            .unwrap();
        let (jobs, executor) = (runtime.jobs.clone(), runtime.executor(builder));
        let background = serde_json::json!({crate::tool::registry::BACKGROUND: true});
        let running = executor
            .run_host(&runtime.agent, "panic", background)
            .await
            .unwrap();
        let failed = crate::tests::bounded(jobs.wait(running.job, true))
            .await
            .unwrap();
        assert_eq!(failed.state, JobState::Failed);
        let diagnostic = failed.envelope.diagnostic.unwrap();
        assert_eq!(
            diagnostic.context.operation,
            crate::tool::diagnostic::Operation::Execute
        );
    }

    /// Racing finishes publish one outcome with its own output: the finish
    /// claim refuses the others before they save anything.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_finishes_keep_the_output_of_the_outcome_that_commits() {
        let (_root, jobs, agent) = super::super::tests::runtime().await;
        for _ in 0..40 {
            let lease = jobs
                .test_running(JobSpec::test(agent.clone(), "raced"))
                .await;
            let (id, run) = (lease.id(), jobs.run(lease.id()));
            let (_input, _worker) = lease.split();
            let completed = JobOutcome::Completed(ToolOutput::new(serde_json::json!("A")));
            let outcomes = [
                completed,
                ToolError::cancelled().into(),
                ToolError::interrupted().into(),
            ];
            let finishes = outcomes.map(|outcome| {
                super::super::supervisor::spawn_completion(jobs.clone(), run, async { outcome })
            });
            for finish in finishes {
                bounded(finish).await.unwrap();
            }
            // Unrequested, the cancellation never supersedes an interruption.
            let published = jobs.test_snapshot(id).await.unwrap();
            let output = match published.state {
                JobState::Completed => Some(serde_json::json!("A")),
                JobState::Cancelled | JobState::Interrupted => None,
                state => panic!("{state:?}"),
            };
            assert_eq!(published.output, output);
        }
    }

    /// A watchdog that fires while an interruption is still saving its output
    /// settles its cancellation once that finish applies, so the drain ends.
    #[tokio::test]
    async fn a_cancellation_behind_an_in_flight_interruption_settles_after_it() {
        let (_root, jobs, agent) = super::super::tests::runtime().await;
        let lease = jobs.test_running(JobSpec::test(agent, "interrupted")).await;
        let id = lease.id();
        let (_input, _worker) = lease.split();
        let claim = jobs.claim(jobs.run(id), JobEnd::Interrupted).await;
        let claim = claim.unwrap_or_else(|_| panic!("a running job's finish is unclaimed"));
        // The watchdog aborts this past the grace, then settles the cancellation.
        let worker = tokio::spawn(std::future::pending::<()>());
        jobs.handles(id, |handles| {
            handles.task_abort = Some((jobs.run(id).1, worker.abort_handle()))
        });
        jobs.cancel(id).await.unwrap();
        assert!(bounded(worker).await.unwrap_err().is_cancelled());
        let interrupted = ToolError::interrupted().into();
        jobs.publish(&claim, interrupted).await.unwrap();
        bounded(jobs.cancel_and_drain()).await.unwrap();
        assert_eq!(jobs.snapshot(id).unwrap().state, JobState::Cancelled);
    }

    /// A finish bound to an earlier invocation never ends a later one: neither a
    /// settle that waited behind the interruption's claim nor the old worker's
    /// late completion ends the invocation owner input resumed, and no
    /// cancellation but a requested one ends the interruption before it.
    #[tokio::test]
    async fn finishes_of_an_earlier_invocation_leave_the_resumed_one_running() {
        let (_root, jobs, agent) = super::super::tests::runtime().await;
        let spec = JobSpec {
            accepts_input: true,
            role: JobRole::Agent,
            ..JobSpec::test(agent, "resumed")
        };
        let lease = jobs.test_running(spec).await;
        let (id, run) = (lease.id(), jobs.run(lease.id()));
        let (_input, _worker) = lease.split();
        let handler: ResumeHandler = Arc::new(|_, mut input| {
            Box::pin(async move { Ok(ToolOutput::new(input.recv().await.unwrap())) })
        });
        jobs.set_resume_handler(id, handler).await.unwrap();
        let stale = || JobOutcome::Completed(ToolOutput::new(serde_json::json!("stale")));
        let (release, released) = tokio::sync::oneshot::channel();
        let late = super::super::supervisor::spawn_completion(jobs.clone(), run, async move {
            released.await.unwrap();
            stale()
        });
        let claim = jobs.claim(run, JobEnd::Interrupted).await;
        let claim = claim.unwrap_or_else(|_| panic!("a running job's finish is unclaimed"));
        // Its first claim is queued before the interruption publishes: InFlight.
        let waiting = jobs.settle(run, stale());
        tokio::pin!(waiting);
        assert!(futures_util::poll!(&mut waiting).is_pending());
        let interrupted = ToolError::interrupted().into();
        jobs.publish(&claim, interrupted).await.unwrap();
        let unrequested = jobs.claim(run, JobEnd::Cancelled).await;
        assert!(
            unrequested.is_err(),
            "a worker's cancellation ends its interruption"
        );
        jobs.send(id, serde_json::json!("resume")).await.unwrap();
        bounded(waiting).await;
        release.send(()).unwrap();
        bounded(late).await.unwrap();
        assert_eq!(jobs.metadata(id).unwrap().state, JobState::Running);
        jobs.send(id, serde_json::json!("own")).await.unwrap();
        let own = bounded(jobs.wait(id, true)).await.unwrap();
        assert_eq!(own.state, JobState::Completed);
        assert_eq!(own.output, Some(serde_json::json!("own")));
        let cancelled = |event: &SessionEvent| {
            matches!(
                event,
                SessionEvent::JobFinished {
                    state: JobEnd::Cancelled,
                    ..
                }
            )
        };
        assert_eq!(count(&jobs, cancelled), 0);
    }

    // Creation ownership and admission tests.

    use crate::tests::bounded;

    fn count(jobs: &JobManager, matches: impl Fn(&SessionEvent) -> bool) -> usize {
        let records = jobs.store().records();
        records
            .iter()
            .filter(|record| matches(&record.event))
            .count()
    }

    fn creation_count(jobs: &JobManager) -> usize {
        count(jobs, |event| {
            matches!(event, SessionEvent::JobCreated { .. })
        })
    }

    #[tokio::test]
    async fn creation_inherits_concurrent_parent_cancellation() {
        let (_root, jobs, agent) = crate::job::tests::runtime().await;
        let parent = jobs
            .test_create(JobSpec::test(agent.clone(), "parent"))
            .await;
        jobs.test_finish(parent, serde_json::json!("done")).await;
        // Cancellation is ordered behind the creation's commit, and reaches it.
        let cancelling = jobs.clone();
        let cancelled = crate::session::tests::on_commit(
            jobs.store(),
            |_| true,
            move || async move { cancelling.cancel(parent).await },
        );
        let cancelled = cancelled.await;
        let spec = JobSpec {
            parent: Some(parent),
            ..JobSpec::test(agent, "child")
        };
        let child = jobs.create(spec).await.unwrap();
        bounded(cancelled).await.unwrap();
        assert!(child.cancellation_token().is_cancelled());
        let settled = bounded(jobs.wait(child.id(), false)).await;
        assert_eq!(settled.unwrap().state, JobState::Cancelled);
        assert_eq!(creation_count(&jobs), 2);
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Op {
        Create,
        Transition,
        Claim,
        Send,
        RequestInput,
        ResumeInput,
    }

    /// Creates job 1 (except for `Create`) in the state `op` requires, holding
    /// its lease while it is live.
    async fn owned_job(op: Op) -> (tempfile::TempDir, JobManager, AgentId, JobId, Box<dyn Send>) {
        let (root, jobs, agent) = crate::job::tests::runtime().await;
        let id = JobId::new(1).unwrap();
        let spec = JobSpec {
            accepts_input: true,
            role: JobRole::Agent,
            ..JobSpec::test(agent.clone(), "owned")
        };
        let lease: Box<dyn Send> = match op {
            Op::Create => Box::new(()),
            Op::Transition => Box::new(jobs.test_approving(spec).await),
            Op::Claim | Op::Send => {
                assert_eq!(jobs.test_create(spec).await, id);
                let handler: ResumeHandler = Arc::new(|value, _| {
                    Box::pin(async move { Ok(ToolOutput::new(value.unwrap())) })
                });
                jobs.set_resume_handler(id, handler).await.unwrap();
                jobs.test_finish(id, serde_json::json!("previous")).await;
                Box::new(())
            }
            Op::RequestInput | Op::ResumeInput => {
                let lease = jobs.test_running(spec).await;
                if op == Op::ResumeInput {
                    let question = crate::job::tests::question("before");
                    jobs.request_input(id, question).await.unwrap();
                }
                Box::new(lease)
            }
        };
        (root, jobs, agent, id, lease)
    }

    async fn run(op: Op, jobs: JobManager, agent: AgentId, id: JobId) -> Result<(), JobError> {
        match op {
            Op::Create => jobs
                .create(JobSpec::test(agent, "abandoned"))
                .await
                .map(drop),
            Op::Transition => jobs.advance(id, JobTransition::Running).await,
            Op::Claim => jobs.test_claim(id).await,
            Op::Send => jobs.send(id, serde_json::json!("resumed")).await,
            Op::RequestInput => {
                let question = crate::job::tests::question("after");
                jobs.request_input(id, question).await
            }
            Op::ResumeInput => jobs.resume_input(id).await,
        }
    }

    /// A caller gone once its change is queued leaves that change and the work
    /// it starts whole, live and on replay, and nothing is published before the
    /// change is durable. A creation whose caller is gone settles cancelled.
    #[tokio::test]
    async fn abandoned_callers_keep_their_queued_change_and_what_follows() {
        use Op::*;
        for op in [Create, Transition, Claim, Send, RequestInput, ResumeInput] {
            let case = format!("{op:?}");
            let (_root, jobs, agent, id, _lease) = owned_job(op).await;
            let state = |jobs: &JobManager| jobs.metadata(id).ok().map(|m| m.state);
            let acknowledged =
                |jobs: &JobManager| jobs.entry(id, |entry| !entry.unacknowledged()).unwrap();
            let before = state(&jobs);
            let mut updates = jobs.store().observe().updates;
            // The held connection parks the change its first poll queues, so the
            // caller is gone before it could hear back.
            let held = crate::session::tests::hold_connection(jobs.store());
            let mut caller = Box::pin(run(op, jobs.clone(), agent, id));
            assert!(futures_util::poll!(&mut caller).is_pending(), "{case}");
            drop(caller);
            assert_eq!(state(&jobs), before, "{case}");
            if op == Claim {
                assert!(!acknowledged(&jobs));
            }
            drop(held);
            let expected = match op {
                Create => JobState::Cancelled,
                Transition | ResumeInput => JobState::Running,
                Claim | Send => JobState::Completed,
                RequestInput => JobState::WaitingInput,
            };
            // The record the abandoned operation journals last, and how many of
            // its kind the job then has.
            let last = |event: &SessionEvent| match event {
                SessionEvent::JobFinished { job, .. } => *job == id && matches!(op, Create | Send),
                SessionEvent::JobClaimed { .. } => op == Claim,
                SessionEvent::JobStateChanged { state, .. } => match state {
                    JobTransition::Running => matches!(op, Transition | ResumeInput),
                    JobTransition::WaitingInput => op == RequestInput,
                    _ => false,
                },
                _ => false,
            };
            let total = if matches!(op, Send | ResumeInput) {
                2
            } else {
                1
            };
            bounded(async {
                while count(&jobs, last) < total {
                    updates.recv().await.unwrap();
                }
            })
            .await;
            assert_eq!(state(&jobs), Some(expected), "{case}");
            let replay = jobs.test_replay();
            assert_eq!(state(&replay), Some(expected), "{case}");
            assert_eq!(count(&jobs, last), total, "{case}");
            if op == Claim {
                assert!(acknowledged(&jobs) && acknowledged(&replay));
            }
            if op == Create {
                assert_eq!(creation_count(&jobs), 1);
            }
        }
    }
}
