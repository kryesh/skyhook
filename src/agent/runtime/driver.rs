//! Agent command loop and completion coordination.

use super::*;

// This is only the retained command loop's state; the core owns job liveness,
// pending deliveries and owner forwarding.
enum DriverPhase {
    // The root has no waiter to settle; whether it is parked is its published turn.
    Root,
    Child(ChildPhase),
}

enum ChildPhase {
    Idle,
    Running {
        completion: Option<ChildCompletion>,
    },
    Answered {
        answer: String,
        completion: Option<ChildCompletion>,
    },
}

/// Who took a child's answer when its invocation completed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Handoff {
    /// The waiter has it, so the owning job finishes and wakes the owner itself.
    Waiter,
    /// Consumed callback-free; the owner still needs a wake.
    Consumed,
}

/// Replace a child's phase, yielding the waiter the old phase held.
fn take_completion(phase: &mut ChildPhase, next: ChildPhase) -> Option<ChildCompletion> {
    match std::mem::replace(phase, next) {
        ChildPhase::Running { completion } | ChildPhase::Answered { completion, .. } => completion,
        ChildPhase::Idle => None,
    }
}

impl DriverPhase {
    fn is_child(&self) -> bool {
        matches!(self, Self::Child(_))
    }

    /// A child answered, and its invocation has yet to resolve.
    fn answered_open(&self) -> bool {
        matches!(self, Self::Child(ChildPhase::Answered { .. }))
    }

    /// The waiter the turn's caller settles: the root's, never a child's own.
    fn begin(&mut self, completion: Option<RequestCompletion>) -> Option<RequestCompletion> {
        let phase = match self {
            Self::Root => return completion,
            Self::Child(phase) => phase,
        };
        // A new explicit waiter supersedes the old one; callback-free queued
        // input and descendant turns keep the current invocation's waiter.
        let completion = match completion {
            Some(RequestCompletion::Child(completion)) => Some(completion),
            None => None,
            root => return root,
        };
        let previous = take_completion(phase, ChildPhase::Idle);
        *phase = ChildPhase::Running {
            completion: completion.or(previous),
        };
        None
    }

    fn answered(&mut self, answer: String) {
        if let Self::Child(phase) = self {
            let completion = take_completion(phase, ChildPhase::Idle);
            *phase = ChildPhase::Answered { answer, completion };
        }
    }

    // Whether the failure reached this invocation's waiter, which is what
    // finishes the owning job. Parking is the published turn's, not the phase's.
    fn fail(&mut self, error: TurnFailure) -> bool {
        let phase = match self {
            Self::Root => return false,
            Self::Child(phase) => phase,
        };
        let Some(completion) = take_completion(phase, ChildPhase::Idle) else {
            return false;
        };
        completion.send(Err(error)).is_ok()
    }

    // Consume the answer even without a waiter, so a later wakeup does not
    // publish AgentCompleted again for this invocation.
    // `None` means there was no answer to consume.
    fn complete(&mut self) -> Option<Handoff> {
        let Self::Child(phase @ ChildPhase::Answered { .. }) = self else {
            return None;
        };
        let ChildPhase::Answered { answer, completion } =
            std::mem::replace(phase, ChildPhase::Idle)
        else {
            // The guard above matched Answered.
            return None;
        };
        let handed = completion.is_some_and(|completion| completion.send(Ok(answer)).is_ok());
        Some(if handed {
            Handoff::Waiter
        } else {
            Handoff::Consumed
        })
    }
}

/// What an agent's loop takes next.
enum Work {
    Command(AgentCommand),
    /// Deliveries its owner has yet to see.
    Jobs,
}

/// What an agent waiting for work is given.
enum Taken {
    /// Work, with the token of the turn it begins, and the turn the agent returns
    /// to if the work runs none. The token is already cancelled when an
    /// interrupt stopped the invocation the work begins.
    Work(Work, CancellationToken, Turn),
    /// Its answered invocation has nothing left to keep it open.
    Settle,
    /// The session refuses new turns, and the commands the agent held with them.
    Refused(crate::session::SessionPhase, Vec<AgentCommand>),
    Gone,
}

/// Whether `id`, between turns, takes the deliveries pending for it: only while
/// the session takes them, and not those an invocation yet to begin owns, as
/// its first turn carries them.
pub(super) fn takes_deliveries(state: &CoreState, id: &AgentId) -> bool {
    let unbegun = state.jobs.unbegun(id).is_some();
    state.agents.flow == Flow::Open && state.jobs.has_pending(id) && !unbegun
}

/// Whether live jobs, queued input or pending deliveries keep `id`'s answered
/// invocation open.
fn kept_open(state: &CoreState, id: &AgentId) -> bool {
    let queued = state.agents.get(id).is_some_and(agents::AgentFacts::queued);
    queued || state.jobs.has_running(id) || state.jobs.has_pending(id)
}

impl SessionRuntime {
    /// Release a child's held answer to its owner as a reply. Only for paths that
    /// do not finish the owning job: a completing job's answer is its result.
    async fn wake_owner(&self, owner_job: Option<JobId>) {
        if let Some(job) = owner_job {
            // Refused, the answer stays held; ending the job releases it.
            let _ = self.jobs.notify_owner(job).await;
        }
    }

    /// `id`'s next command, or the deliveries an unparked agent has pending, with
    /// a fresh token for the turn it begins, busy from here on so an interrupt
    /// stops it; or the session's refusal of new turns, with everything the agent
    /// held. A paused session gives it no deliveries: they stay pending until
    /// `continue` or new input opens it. An `answered` invocation is settled once
    /// nothing keeps it open, however its last work went. Only the owner's
    /// cancellation abandons what this takes, and that ends the loop, dropping
    /// the inbox with it.
    async fn take_work(&self, id: &AgentId, answered: bool) -> Taken {
        let (id, supervisor) = (id.clone(), self.jobs.supervisor().clone());
        let take = move |state: &mut CoreState| {
            let deliveries = takes_deliveries(state, &id);
            let (phase, open) = (state.phase.clone(), kept_open(state, &id));
            let Some(facts) = state.agents.get_mut(&id) else {
                return Some(Taken::Gone);
            };
            if phase.admits(Command::BeginTurn).is_err() {
                return Some(Taken::Refused(phase, facts.inbox.drain(..).collect()));
            }
            let work = match facts.inbox.pop_front() {
                Some(command) => Work::Command(command),
                // A parked turn leaves deliveries pending, as a paused session does:
                // the next input's request carries them.
                None if deliveries && facts.turn == Turn::Idle => Work::Jobs,
                None if answered && !open => return Some(Taken::Settle),
                None => return None,
            };
            // Input that begins an invocation an interrupt stopped first runs as a
            // turn interrupted on taking it: its input commits, and nothing requests.
            // The token is cancelled before anyone holds it.
            let begins = matches!(work, Work::Command(AgentCommand::Input { .. }));
            let token = supervisor.begin_turn(&id);
            let turn = if begins && state.jobs.begin(&id) {
                token.cancel();
                Turn::Parked(TurnFailure::Interrupted)
            } else {
                Turn::Busy
            };
            let resting = facts.set_turn(turn);
            Some(Taken::Work(work, token, resting))
        };
        self.store.until(take).await.unwrap_or(Taken::Gone)
    }

    /// Whether `id`'s answered turn goes on for the work waiting at its answer.
    pub(super) async fn rejoin(&self, id: &AgentId) -> bool {
        let id = id.clone();
        let rejoin = move |state: &mut CoreState| {
            let deliveries = takes_deliveries(state, &id);
            state.agents.rejoin(&id, deliveries)
        };
        self.store.live(rejoin).await
    }

    /// Settle `id`'s turn at `turn`, unless an interrupt stopped it first.
    async fn rest(&self, id: &AgentId, turn: Turn) {
        let id = id.clone();
        let rest = move |state: &mut CoreState| {
            let busy = state
                .agents
                .get_mut(&id)
                .filter(|facts| facts.turn == Turn::Busy);
            if let Some(facts) = busy {
                facts.set_turn(turn);
            }
        };
        self.store.live(rest).await;
    }

    /// End `id`'s turn as `failure` says. A turn an interrupt or closing already
    /// stopped or held ends interrupted, however it ended: its interruption
    /// stands. An interrupted end is journaled when it leaves a request pending,
    /// such as one retrying, which it closes, or ends a child's invocation for
    /// good. Otherwise a failure is journaled and parks the turn, rejecting the
    /// queued input that would start an orphan turn, as the interrupt did for what
    /// it found. A child's invocation it ends resolves, unless its interruption is
    /// final. Reports how the turn ended, and whether its interruption is final.
    async fn end_turn(
        &self,
        id: &AgentId,
        failure: Option<TurnFailure>,
        owner_cancelled: bool,
    ) -> (Option<TurnFailure>, bool) {
        let (id, unrecorded) = (id.clone(), failure.clone());
        let commit = move |state: &CoreState| {
            let stopped = state.agents.get(&id).is_some_and(|facts| {
                matches!(
                    facts.turn,
                    Turn::Held | Turn::Parked(TurnFailure::Interrupted)
                )
            });
            let failure = stopped.then_some(TurnFailure::Interrupted).or(failure);
            let interrupted = failure == Some(TurnFailure::Interrupted);
            let last = interrupted && (owner_cancelled || !stopped);
            let ends = (last && id.parent().is_some()) || state.ledger.open(&id).is_some();
            let journaled = match &failure {
                Some(TurnFailure::Failed(failure)) => {
                    let failure = failure.clone();
                    vec![(id.clone(), SessionEvent::AgentFailed { failure })]
                }
                Some(_) if ends => vec![(id.clone(), SessionEvent::AgentInterrupted)],
                _ => Vec::new(),
            };
            let ended = move |state: &mut CoreState, _: &[EventRecord]| {
                if failure.is_some() && !stopped {
                    let rejected = state.agents.take_queued(&id).into_iter().flatten();
                    rejected.for_each(|input| input.reject(HarnessError::Interrupted));
                }
                let facts = state.agents.get_mut(&id)?;
                facts.set_turn(failure.clone().map_or(Turn::Idle, Turn::Parked));
                // Explicit job/tree cancellation is deliberately final. A session-turn
                // interrupt is retained as a retryable Interrupted child job.
                if failure.is_some() && !last {
                    facts.invocation = Invocation::Resolved;
                }
                Some((failure, last))
            };
            Ok::<_, SessionError>(Plan::new(journaled, ended))
        };
        let ended = self.store.commit(Command::Journal, commit).await;
        ended.ok().flatten().unwrap_or((unrecorded, true))
    }

    /// Resolve `id`'s invocation unless something keeps it open, in one step with
    /// owner forwarding.
    async fn resolve_invocation(&self, id: &AgentId) -> bool {
        let id = id.clone();
        let resolve = move |state: &mut CoreState| {
            let resolved = !kept_open(state, &id);
            let facts = state.agents.get_mut(&id)?;
            if resolved {
                facts.invocation = Invocation::Resolved;
            }
            Some(resolved)
        };
        self.store.live(resolve).await.unwrap_or(false)
    }

    /// Complete an answered child's invocation once nothing keeps it open: work
    /// that kept it open may have been rejected, withdrawn or consumed elsewhere
    /// without running a turn. Without a waiter nothing finishes the owning job,
    /// so the owner is woken here instead. One that stays open waits idle for that
    /// work, and its reply must not wait for the work to reach the owner.
    async fn settle_answered(
        &self,
        id: &AgentId,
        owner_job: Option<JobId>,
        phase: &mut DriverPhase,
    ) {
        if !phase.answered_open() {
            return;
        }
        if !self.resolve_invocation(id).await {
            self.wake_owner(owner_job).await;
            return;
        }
        if let Some(handoff) = phase.complete() {
            let _ = (self.store)
                .append(id.clone(), SessionEvent::AgentCompleted)
                .await;
            if handoff == Handoff::Consumed {
                self.wake_owner(owner_job).await;
            }
        }
    }

    pub(super) async fn run_agent(self: Arc<Self>, agent_loop: AgentLoop) {
        let AgentLoop {
            id,
            owner_job,
            mut context,
            location,
            mut settings,
        } = agent_loop;
        let _turns = self.jobs.supervisor().turns_of(&id);
        let mut phase =
            owner_job.map_or(DriverPhase::Root, |_| DriverPhase::Child(ChildPhase::Idle));
        let child = phase.is_child();
        let owner_cancellation = match owner_job.map(|job| self.jobs.cancellation_token(job)) {
            Some(Ok(token)) => token,
            Some(Err(_)) => return self.exited(&id, owner_job).await,
            None => self.jobs.supervisor().token(),
        };
        loop {
            let taken = tokio::select! {
                biased;
                () = owner_cancellation.cancelled() => {
                    // The cancelled owning job finishes and wakes the owner for
                    // whatever is still pending; with no waiter nothing finishes
                    // it, so a retained reply needs the explicit wake.
                    if !phase.fail(TurnFailure::Cancelled) {
                        self.wake_owner(owner_job).await;
                    }
                    let _ = self.store.append(id.clone(), SessionEvent::AgentInterrupted).await;
                    break;
                }
                taken = self.take_work(&id, phase.answered_open()) => taken,
            };
            let (work, cancellation, resting) = match taken {
                Taken::Work(work, cancellation, resting) => (work, cancellation, resting),
                Taken::Settle => {
                    self.settle_answered(&id, owner_job, &mut phase).await;
                    continue;
                }
                // The phase, not the command, decides: whatever the agent holds is
                // refused with it, and a pending reply stays journaled.
                Taken::Refused(refused, held) => {
                    for command in held {
                        command.refuse(|| refused.refusal().into());
                    }
                    let _ = (self.store)
                        .append(id.clone(), SessionEvent::AgentInterrupted)
                        .await;
                    break;
                }
                Taken::Gone => break,
            };
            // Every input/notification path shares the same delivery window.
            let _ = self.flush_events(&id, &cancellation).await;
            let mut delivered = Ok(());
            let mut turn = TurnContext {
                agent: &id,
                owner_job,
                cancellation: &cancellation,
                location: &location,
                capabilities: settings.capabilities.clone(),
            };
            let empty = || (Vec::new(), None, Selection::default());
            let work = match work {
                Work::Command(AgentCommand::QueuedInputs(inputs)) => {
                    let batch = self
                        .consume_queued_batch(&mut turn, &mut context, &mut settings, inputs)
                        .await;
                    if batch.failed {
                        self.reject_queued(&id).await;
                    }
                    batch.consumed.then(empty)
                }
                Work::Command(AgentCommand::Input {
                    content,
                    done,
                    options,
                }) => Some((content, done, options)),
                Work::Jobs => match self.deliver_events(&id, turn.diagnostic_viewer()).await {
                    Ok(None) => None,
                    committed => {
                        delivered = committed.map(drop);
                        Some(empty())
                    }
                },
            };
            // Work that runs no turn may still have been what kept an answered
            // invocation open.
            let Some((content, done, options)) = work else {
                self.rest(&id, resting).await;
                self.settle_answered(&id, owner_job, &mut phase).await;
                continue;
            };
            let images = content.iter().any(UserPart::is_image);
            let selected = self.select(&mut turn, &mut context, &mut settings, options, images);
            if let Err(error) = selected.await {
                // The phase is untouched: a parked child stays parked until an
                // input with an admissible selection resumes its retained task.
                self.rest(&id, resting).await;
                if let Some(done) = done {
                    done.settle(Err(error));
                }
                continue;
            }
            let done = phase.begin(done);
            if !content.is_empty() && delivered.is_ok() {
                let message = Message::User(content);
                delivered = self
                    .commit(&id, message)
                    .await
                    .map(drop)
                    .map_err(Into::into);
            }
            if let Err(error) = delivered {
                let failure = TurnFailure::from(&error);
                if let Some(done) = done {
                    done.settle(Err(error));
                }
                // No reply was published in this iteration and every earlier
                // resolution already finished the job or woke the owner, so
                // the parked/failed handoff needs no wake of its own.
                phase.fail(failure.clone());
                self.rest(&id, Turn::Parked(failure)).await;
                if child {
                    self.interrupt_tree(&id).await;
                    break;
                }
                continue;
            }
            let result = tokio::select! {
                biased;
                () = owner_cancellation.cancelled() => Err(HarnessError::Interrupted),
                result = self.run_turn(turn, &mut context, &mut settings) => result,
            };
            let failure = result.as_ref().err().map(TurnFailure::from);
            // A terminal turn failure is journaled, not only broadcast: the retry
            // affordance is gated on agent activity, which is otherwise lost when
            // the session is reopened.
            let cancelled = owner_cancellation.is_cancelled();
            let (failure, last) = self.end_turn(&id, failure, cancelled).await;
            let result = match failure {
                Some(TurnFailure::Interrupted) => Err(HarnessError::Interrupted),
                _ => result,
            };
            if let Ok(answer) = &result {
                phase.answered(answer.clone());
            }
            // Only the root holds its own waiter; a child's lives in its phase.
            if let Some(done) = done {
                done.settle(result);
                continue;
            }
            if child && let Some(failure) = failure {
                if last {
                    // An interrupted turn can still have published visible text (a
                    // shielded commit outlives the cancelled turn), so the owner is
                    // woken unless the waiter finishes the job for us.
                    if !phase.fail(TurnFailure::Interrupted) {
                        self.wake_owner(owner_job).await;
                    }
                    break;
                }
                // A provider/turn failure is terminal for this invocation, not for
                // the retained child. Keep its loop, provider session, and projected
                // history alive so the owning job can restart it. An aborted response
                // commits its partial visible text before failing the turn; the
                // failing job wakes the owner for it, and a parked child without a
                // waiter needs the explicit wake.
                if !phase.fail(failure) {
                    self.wake_owner(owner_job).await;
                }
                continue;
            }
            // Retain the provider session and full projected history while idle. A
            // fresh owner request resumes this same child, never a new agent.
            self.settle_answered(&id, owner_job, &mut phase).await;
        }
        self.exited(&id, owner_job).await;
    }

    /// The loop is gone: its owning job can no longer resume it, and input to it
    /// is refused as stopped.
    async fn exited(&self, id: &AgentId, owner_job: Option<JobId>) {
        if let Some(job) = owner_job {
            self.jobs.clear_resume_handler(job).await;
        }
        let id = id.clone();
        // What its inbox held is dropped: those waiting on it see it stopped.
        self.store
            .live(move |state| drop(state.agents.remove(&id)))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;
    use crate::job::{JobEnd, JobOutcome, JobSpec, JobState};
    use crate::tool::{ToolOptions, ToolOutput, ToolRegistryBuilder};

    #[tokio::test]
    async fn child_completion_waits_for_background_work_and_returns_its_updated_answer() {
        let final_answer = "child work completed\n".repeat(500);
        let work = json!({"prompt":"work"});
        let script = json!({"source":"return await receive();", "bg":true});
        let (_root, requests, session) = scripted_session([
            response(vec![tool_call(0, "agent", "agent", work)]),
            response(vec![tool_call(0, "script", "script", script)]),
            answer("premature child answer"),
            answer(&final_answer),
            answer("root done"),
        ])
        .await;
        let mut events = session.runtime.store.observe().updates;
        let prompt = session.prompt("delegate");
        tokio::pin!(prompt);
        bounded(async {
            loop {
                tokio::select! {
                    result = &mut prompt => panic!("parent returned before child work completed: {result:?}"),
                    event = events.recv() => if matches!(event.unwrap().event, RuntimeEvent::TurnCompleted { agent } if agent == session.root.child(1)) { break; },
                }
            }
        }).await;
        let jobs = &session.runtime.jobs;
        let find = async |agent: &AgentId, tool: &str| {
            let mut listed = jobs.list(agent).into_iter();
            listed.find(|job| job.tool == tool).unwrap()
        };
        let agent_job = find(&session.root, "agent").await;
        assert!(!agent_job.state.is_terminal());
        let script = find(&session.root.child(1), "script").await;
        jobs.send(script.id, json!("released")).await.unwrap();
        assert_eq!(bounded(prompt).await.unwrap(), "root done");
        // Saved output retains the complete final string.
        let output = jobs.test_snapshot(agent_job.id).await.unwrap().output;
        assert_eq!(output, Some(json!(final_answer)));
        let requests = requests.lock().unwrap();
        let state = request_runtime_state(&requests[2]);
        let mut lines = state.lines().skip(1);
        let header = "jobs: job parent tool name state age_s turns tool_calls";
        assert_eq!(lines.next(), Some(header));
        let fields = lines.next().unwrap().split(' ').collect::<Vec<_>>();
        assert_eq!(fields.len(), 8, "same-location jobs need no overrides");
        assert_eq!(fields[0], script.id.to_string());
        assert_eq!(&fields[1..4], &["-", "script", "-"]);
        assert!(matches!(fields[4], "queued" | "running"));
        fields[5].parse::<u64>().unwrap();
        assert_eq!(&fields[6..], &["-", "-"]);
        assert!(lines.next().is_none(), "empty todos are omitted");
        let last = requests.last().unwrap();
        let mut results = last.history.iter().flat_map(|message| match message {
            Sent::Tool(results) => results.as_slice(),
            _ => &[],
        });
        let result = results.find(|result| result.name == "agent").unwrap();
        assert!(!result.is_error);
        // A foreground call returns its answer in the result and nothing else is
        // delivered: no progress replies, and the answer only once.
        let serialized = rendered(last);
        assert_eq!(serialized.matches("premature child answer").count(), 0);
        let in_result = serde_json::to_string(&result.result).unwrap();
        let answered = in_result.matches("child work completed").count();
        assert!(answered > 0, "{in_result}");
        assert_eq!(serialized.matches("child work completed").count(), answered);
        for request in requests.iter() {
            let system = &request.system[0].text;
            assert!(!system.contains("compaction"));
            assert!(system.contains("use `jobs` for more output"));
            assert!(!system.contains("Continue with the returned"));
        }
    }

    /// A turn is busy from the moment its loop takes the input: an interrupt that
    /// lands before the request stops it, and no request is made.
    #[tokio::test]
    async fn an_interrupt_stops_a_turn_that_took_its_input() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [answer("unrequested")]);
        let other = crate::tests::profile("other", false);
        let harness = serving(root.path(), provider, [("other", other)]);
        let session = ephemeral_session(&harness.build().await.unwrap()).await;
        // Its model change journals while the loop holds the input it took.
        let selection = session.selection(Some(&model_ref("other")), None);
        let store = &session.runtime.store;
        let (reached, resume) = store.pause_committed(|_| true).await;
        let prompt = tokio::spawn({
            let session = session.clone();
            async move {
                session
                    .prompt_with_options("task", &[], selection.unwrap())
                    .await
            }
        });
        bounded(reached).await.unwrap();
        // Queued behind the paused append, ahead of the loop's next step.
        let interrupt = session.interrupt();
        tokio::pin!(interrupt);
        assert!(futures_util::poll!(interrupt.as_mut()).is_pending());
        resume.send(()).unwrap();
        assert_eq!(bounded(interrupt).await, 1);
        let prompted = bounded(prompt).await.unwrap();
        assert!(
            matches!(prompted, Err(HarnessError::Interrupted)),
            "{prompted:?}"
        );
        assert!(requests.lock().unwrap().is_empty());
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn session_resume_restarts_all_interrupted_children_without_restarting_waiting_parent() {
        let child = |index: u32| {
            let arguments = json!({"prompt":format!("child task {index}"), "depth":0});
            tool_call(index, &format!("agent-{index}"), "agent", arguments)
        };
        // Both children hang mid-stream in their first request, then answer once resumed.
        let hang = || Step::new(Vec::new()).midstream();
        let recovered = || Step::new(answer("child recovered")).gated();
        let steps = [
            Step::new(response((0..2).map(child).collect())),
            hang(),
            hang(),
            recovered(),
            recovered(),
            Step::new(answer("parent done")),
        ];
        let requests = Requests::default();
        let provider = Script::new(steps, &requests);
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let parent = spawn_prompt(&session, "delegate");
        provider.request(2).await;
        root_waiting(&session).await;
        let original_jobs = session.runtime.jobs.list(&session.root);
        assert_eq!(original_jobs.len(), 2);
        // The children are interrupted; the parent holding them shows interrupted
        // but keeps its wait.
        assert_eq!(session.interrupt().await, 3);
        assert!(!parent.is_finished());
        assert!(!provider.requested_from(3));
        // Resume immediately, even before the child worker journals Interrupted.
        assert_eq!(turn(&session, &session.root), Turn::Held);
        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "");
        provider.request(4).await;
        // Resume must leave the original parent wait pending, and shows it waiting
        // again rather than interrupted.
        assert!(!parent.is_finished());
        assert_eq!(turn(&session, &session.root), Turn::Busy);
        assert_eq!(
            session.observe().snapshot.activity[&session.root].state,
            crate::agent::AgentActivity::WaitingChildren
        );
        let records = session.runtime.store.records();
        let interrupted = count!(&records, SessionEvent::JobFinished { state, .. } if *state == JobEnd::Interrupted);
        assert_eq!(interrupted, 2);
        // Only an interrupted stream, never an interrupted startup, closes an attempt.
        let children = records.iter().filter(|record| record.agent != session.root);
        assert_eq!(
            count!(children, SessionEvent::ModelAttemptInterrupted(_)),
            2
        );
        let root_records = records.iter().filter(|record| record.agent == session.root);
        assert_eq!(count!(root_records, SessionEvent::ModelRequested { .. }), 1);
        // Continuation must not create replacement agents.
        let owned =
            count!(&records, SessionEvent::AgentStarted { owner_job, .. } if owner_job.is_some());
        assert_eq!(owned, 2);
        let resumed_jobs = session.runtime.jobs.list(&session.root);
        let resumed = resumed_jobs.iter().map(|job| job.id);
        assert!(resumed.eq(original_jobs.iter().map(|job| job.id)));
        assert!(resumed_jobs.iter().all(|j| j.state == JobState::Running));
        let parent_input = |message: &Sent| {
            matches!(message, Sent::User(content)
            if content.iter().any(|part| matches!(part, SentPart::Text { text } if text.starts_with("Owner input:"))))
        };
        for request in &requests.lock().unwrap()[3..5] {
            let history = &request.history;
            let text = serde_json::to_string(history).unwrap();
            assert_eq!(text.matches("child task").count(), 1);
            assert!(!history.iter().any(parent_input));
        }
        provider.release(3);
        provider.release(4);
        assert_eq!(bounded(parent).await.unwrap().unwrap(), "parent done");
        session.shutdown().await.unwrap();
    }

    /// A prompt to a parent holding interrupted children breaks the link: its held
    /// turn ends, the prompt starts the next one, and the children stay retained.
    #[tokio::test]
    async fn prompt_redirects_a_parent_holding_interrupted_children() {
        let after = [
            Step::new(answer("redirected")),
            Step::new(answer("child recovered")).gated(),
            Step::new(answer("parent done")),
        ];
        let root = tempfile::tempdir().unwrap();
        let (_harness, session, provider, requests, parent, _) =
            held_on_interrupted_child(root.path(), after).await;
        assert!(!parent.is_finished());
        assert_eq!(
            bounded(session.prompt("new direction")).await.unwrap(),
            "redirected"
        );
        assert!(bounded(parent).await.unwrap().is_err());
        let jobs = session.runtime.jobs.list(&session.root);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, JobState::Interrupted);
        // The redirected request carries the interrupted result and the prompt.
        let history = serde_json::to_string(&requests.lock().unwrap()[2].history).unwrap();
        assert!(
            history.contains("interrupted") && history.contains("new direction"),
            "{history}"
        );
        // The retained child still continues, and its answer reaches the root as
        // an event rather than through the ended wait.
        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "");
        provider.request(3).await;
        provider.release(3);
        let woken = provider.request(4).await;
        assert!(rendered(&woken).contains("child recovered"));
        assert_eq!(
            count!(
                &session.runtime.store.records(),
                SessionEvent::AgentStarted { .. }
            ),
            2
        );
        session.shutdown().await.unwrap();
    }

    /// Queued input joins the held turn instead of ending it: the parent's next
    /// request carries the child's interrupted result and the instruction together.
    #[tokio::test]
    async fn queued_input_redirects_a_parent_holding_an_interrupted_child() {
        let root = tempfile::tempdir().unwrap();
        let (_harness, session, _, requests, parent, _) =
            held_on_interrupted_child(root.path(), [Step::new(answer("redirected"))]).await;
        let prompt = QueuedPrompt {
            text: "use the right user".into(),
            ..Default::default()
        };
        let receipts = bounded(enqueue_prompts(&session, vec![prompt])).await;
        assert!(receipts[0].is_ok());
        assert_eq!(bounded(parent).await.unwrap().unwrap(), "redirected");
        let history = serde_json::to_string(&requests.lock().unwrap()[2].history).unwrap();
        assert!(
            history.contains("interrupted") && history.contains("use the right user"),
            "{history}"
        );
        assert_eq!(
            session.runtime.jobs.list(&session.root)[0].state,
            JobState::Interrupted
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_child_send_preserves_the_parents_pending_wait() {
        for aborted in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let requests = Requests::default();
            let release = Arc::new(tokio::sync::Semaphore::new(0));
            let gate = release.clone();
            let wait_fixture = move |_, _| {
                let gate = gate.clone();
                async move {
                    gate.acquire().await.unwrap().forget();
                    Ok(ToolOutput::new(json!("released")))
                }
            };
            let mut tools = ToolRegistryBuilder::default();
            let (schema, options) = (json!({"type":"object"}), ToolOptions::default());
            let description = "Wait for the test gate";
            let registered =
                tools.register_dynamic("wait_fixture", description, schema, options, wait_fixture);
            registered.unwrap();
            let wait = response(vec![tool_call(0, "wait", "wait_fixture", json!({}))]);
            // The child's first invocation fails outright, or aborts after partial text.
            let failed = if aborted {
                let partial = AssistantItem::text("answer", 0, "partial child answer");
                Step::new(cut(vec![partial], CutReason::Aborted))
            } else {
                let kind = crate::provider::ProviderErrorKind::Authentication;
                Step::fail(kind.error("fixture permanent failure"))
            };
            let answers = ["child recovered", "parent done", "parent done"];
            let answers = answers.map(|text| Step::new(answer(text)));
            let steps = [Step::new(wait), failed].into_iter().chain(answers);
            let provider = Script::new(steps, &requests);
            let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
                .tools(tools.build())
                .build()
                .await
                .unwrap();
            let session = harness.new_session().await.unwrap();
            let jobs = &session.runtime.jobs;
            let parent = spawn_prompt(&session, "wait for input");
            let running = |job: &crate::job::JobEnvelope| {
                job.tool == "wait_fixture" && job.state == JobState::Running
            };
            let wait_job = job_found(&session, &session.root, running).await;
            let arguments = json!({"prompt":"retain my task", "bg":true});
            let (executor, agent) = (&session.runtime.executor, session.root.clone());
            let job = executor
                .execute(agent, "agent", arguments, None)
                .await
                .unwrap()
                .job;
            let wait = async || bounded(jobs.wait(job, None, true)).await.unwrap();
            let failed = wait().await;
            assert_eq!(failed.state, JobState::Failed);
            let child = session.root.child(1);
            assert_eq!(requests.lock().unwrap().len(), 2, "no automatic replay");
            if aborted {
                assert!(matches!(&failed.diagnostic.as_ref().unwrap().cause,
                    crate::tool::diagnostic::Cause::Message(message) if message == "provider aborted response"));
                // A rejected selection neither replays nor unparks the child.
                mailbox_barrier(&session, &child).await;
                assert_eq!(requests.lock().unwrap().len(), 2);
            }
            let retry = format!("return tool.job({job}).send({{value:'try again'}});");
            let resumed = session.run_script(retry).await.unwrap();
            assert_eq!(resumed.value["value"], json!({}));
            let recovered = wait().await;
            assert_eq!(recovered.state, JobState::Completed);
            assert_eq!(recovered.output, Some(json!("child recovered")));
            // A child retry must not complete the parent's wait.
            assert!(!parent.is_finished());
            let state = jobs.snapshot(wait_job).unwrap().state;
            assert_eq!(state, JobState::Running);
            let records = session.runtime.store.records();
            let root_records = records.iter().filter(|record| record.agent == session.root);
            // No new parent request before its pending wait is released.
            assert_eq!(count!(root_records, SessionEvent::ModelRequested { .. }), 1);
            let child_records = records.iter().filter(|record| record.agent == child);
            assert_eq!(count!(child_records, SessionEvent::AgentStarted { .. }), 1);
            let history = requests.lock().unwrap()[2].history.clone();
            let history = serde_json::to_string(&history).unwrap();
            assert_eq!(history.matches("retain my task").count(), 1);
            assert_eq!(history.matches("try again").count(), 1);
            let partial = history.matches("partial child answer").count();
            assert_eq!(partial, usize::from(aborted));
            release.add_permits(1);
            assert_eq!(bounded(parent).await.unwrap().unwrap(), "parent done");
            session.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn completed_child_resumes_same_history_and_job_repeatedly() {
        let answers = ["first answer", "second answer", "third answer"].map(answer);
        let (_root, requests, session) = scripted_session(answers).await;
        let root_inbox = quiet_root(&session);
        let jobs = &session.runtime.jobs;
        let arguments = json!({"prompt":"remember the initial task", "depth":0});
        let (executor, agent) = (&session.runtime.executor, session.root.clone());
        let first = executor
            .execute(agent, "agent", arguments, None)
            .await
            .unwrap();
        assert_eq!(first.output.value, "first answer");
        let job = first.job;
        let child = session.root.child(1);
        let follow_ups = [
            ("follow-up one", "second answer"),
            ("follow-up two", "third answer"),
        ];
        for (instruction, answer) in follow_ups {
            let value = json!(instruction);
            let send = format!("return tool.job({job}).send({{value:{value}}});");
            session.run_script(send).await.unwrap();
            jobs.wait(job, None, true).await.unwrap();
            let output = format!("return tool.jobs({{job:{job}}});");
            let sent = session.run_script(output).await.unwrap();
            assert_eq!(sent.value["value"]["id"], job.get());
            assert_eq!(sent.value["value"]["state"], "completed");
            assert_eq!(sent.value["value"]["result"], answer);
            assert_eq!(jobs.metadata(job).unwrap().state, JobState::Completed);
        }
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            let child_id = ContextId::from(&child);
            assert!(requests.iter().all(|request| request.context == child_id));
            let history = &requests[2].history;
            let text = serde_json::to_string(history).unwrap();
            let expected = ["remember the initial task", "first answer", "follow-up one"];
            for expected in expected
                .into_iter()
                .chain(["second answer", "follow-up two"])
            {
                assert!(text.contains(expected), "missing {expected}: {text}");
            }
            assert!(
                matches!(history.last(), Some(Sent::User(content)) if matches!(&content[0], SentPart::Text {text} if text.contains("follow-up two")))
            );
        }
        let records = session.runtime.store.records();
        let started =
            count!(&records, SessionEvent::AgentStarted { owner_job: Some(id), .. } if *id == job);
        assert_eq!(started, 1);
        let restored = session.runtime.jobs.test_replay();
        let output = restored.test_snapshot(job).await.unwrap().output;
        assert_eq!(output, Some(json!("third answer")));
        // Shutdown waits for agent receivers to close: restore the real root loop.
        drop(root_inbox);
        session.shutdown().await.unwrap();
    }

    /// A background child's answer, released early while it still held work, is
    /// its result once: completion withdraws the undelivered reply, as replay does.
    #[tokio::test]
    async fn child_finishes_once_its_last_work_ends() {
        for background in [false, true] {
            let (_root, _requests, session) = scripted_session([answer("initial")]).await;
            let root_inbox = quiet_root(&session);
            let (owner, child, job) = retained_child(&session, background).await;
            let (done, received) = oneshot::channel();
            let mut events = session.runtime.store.observe().updates;
            child_input(&session, &child, Some(done)).await;
            wait_for_child_answer(&mut events, &child).await;
            let jobs = &session.runtime.jobs;
            if background {
                // The loop tail releases the answer: the child still holds work.
                until_pending(&session, &session.root).await;
            }
            claim_completed(&session, job).await;
            let answer = bounded(received).await.unwrap().unwrap();
            assert_eq!(answer, "initial");
            let result = JobOutcome::Completed(ToolOutput::new(answer.into()));
            jobs.finish(owner, result).await.unwrap();
            let delivery = jobs.pending_delivery(&session.root).await.unwrap();
            assert!(delivery.messages().is_empty());
            drop((delivery, root_inbox));
            session.shutdown().await.unwrap();
        }
    }

    /// A retained child with an owner job, plus one job of its own that keeps an
    /// invocation it answered open until it ends.
    async fn retained_child(session: &SessionHandle, background: bool) -> (JobId, AgentId, JobId) {
        let child = session.root.child(1);
        let owner = owner(session, background).await;
        let launch = child_launch(session, None);
        let spawned = (session.runtime).spawn_agent(child.clone(), Some(owner), launch);
        spawned.await.unwrap();
        let spec = JobSpec::test(child.clone(), "manual");
        let job = session.runtime.jobs.test_create(spec).await;
        (owner, child, job)
    }

    async fn claim_completed(session: &SessionHandle, job: JobId) {
        let outcome = JobOutcome::Completed(ToolOutput::default());
        session.runtime.jobs.finish(job, outcome).await.unwrap();
        session.runtime.jobs.test_claim(job).await.unwrap();
    }

    fn input(text: &str, options: Selection, done: Option<ChildCompletion>) -> AgentCommand {
        let content = vec![UserPart::Text { text: text.into() }];
        AgentCommand::Input {
            options,
            content,
            done: done.map(RequestCompletion::Child),
        }
    }

    async fn child_input(session: &SessionHandle, child: &AgentId, done: Option<ChildCompletion>) {
        send(session, child, input("task", Selection::default(), done)).await;
    }

    // A rejected selection (a mode, which only the root can enter) acknowledges all
    // earlier inbox commands without invoking a provider, replacing a child
    // waiter or starting another turn.
    async fn mailbox_barrier(session: &SessionHandle, child: &AgentId) {
        let (done, received) = oneshot::channel();
        let mode = SessionMode {
            runtime: session.runtime.instance,
            name: "driver-test-mode".parse().unwrap(),
        };
        let selection = Selection {
            mode: Some(mode),
            ..Default::default()
        };
        send(session, child, input("", selection, Some(done))).await;
        assert!(bounded(received).await.unwrap().is_err());
    }

    async fn wait_for_child_answer(
        events: &mut tokio::sync::broadcast::Receiver<crate::agent::ObservedEvent>,
        child: &AgentId,
    ) {
        bounded(async {
            loop {
                if matches!(events.recv().await.unwrap().event, RuntimeEvent::TurnCompleted { agent, .. } if &agent == child) {
                    break;
                }
            }
        }).await;
    }

    fn completion_count(session: &SessionHandle, child: &AgentId) -> usize {
        let records = session.runtime.store.records();
        let child_records = records.iter().filter(|record| &record.agent == child);
        count!(child_records, SessionEvent::AgentCompleted)
    }

    /// Rejected input does not keep an answered invocation open: once the job that
    /// did ends, the invocation completes with its latest answer, without another
    /// turn. The completed child is idle, so a later interrupt leaves it alone.
    #[tokio::test]
    async fn rejected_input_leaves_the_latest_child_answer_to_complete() {
        let (_root, requests, session) =
            scripted_session([answer("old answer"), answer("latest answer")]).await;
        let root_inbox = quiet_root(&session);
        let (_, child, work) = retained_child(&session, false).await;
        let (done, received) = oneshot::channel();
        let mut events = session.runtime.store.observe().updates;
        child_input(&session, &child, Some(done)).await;
        wait_for_child_answer(&mut events, &child).await;
        mailbox_barrier(&session, &child).await;
        assert_eq!(completion_count(&session, &child), 0);
        child_input(&session, &child, None).await;
        wait_for_child_answer(&mut events, &child).await;
        // Input its sender already withdrew is rejected.
        let cancellation = QueuedPromptCancellation::default();
        assert!(cancellation.cancel());
        let (committed, rejected) = oneshot::channel();
        let text = "cancelled update".to_owned();
        let queued = queue::QueuedInput {
            content: vec![UserPart::Text { text }],
            options: Default::default(),
            cancellation,
            committed,
        };
        send(&session, &child, AgentCommand::QueuedInputs(vec![queued])).await;
        assert!(bounded(rejected).await.unwrap().is_err());
        mailbox_barrier(&session, &child).await;
        assert_eq!(completion_count(&session, &child), 0);
        claim_completed(&session, work).await;
        assert_eq!(bounded(received).await.unwrap().unwrap(), "latest answer");
        mailbox_barrier(&session, &child).await;
        assert_eq!(completion_count(&session, &child), 1);
        assert_eq!(turn(&session, &child), Turn::Idle);
        assert_eq!(session.interrupt().await, 0);
        assert_eq!(turn(&session, &child), Turn::Idle);
        // Rejected input must not run another model turn.
        assert_eq!(requests.lock().unwrap().len(), 2);
        drop(root_inbox);
        session.shutdown().await.unwrap();
    }

    /// Without a waiter, an answered invocation completes once, however its last
    /// work ended, and the idle child resumes with a fresh completion.
    #[tokio::test]
    async fn callback_free_child_completion_is_consumed_once() {
        let (_root, requests, session) =
            scripted_session([answer("first"), answer("resumed")]).await;
        let root_inbox = quiet_root(&session);
        let (_, child, work) = retained_child(&session, false).await;
        let mut events = session.runtime.store.observe().updates;
        child_input(&session, &child, None).await;
        wait_for_child_answer(&mut events, &child).await;
        mailbox_barrier(&session, &child).await;
        assert_eq!(completion_count(&session, &child), 0);
        claim_completed(&session, work).await;
        mailbox_barrier(&session, &child).await;
        assert_eq!(completion_count(&session, &child), 1);
        assert_eq!(requests.lock().unwrap().len(), 1);

        // Idle stays resumable, with one fresh completion rather than the consumed one.
        child_input(&session, &child, None).await;
        wait_for_child_answer(&mut events, &child).await;
        mailbox_barrier(&session, &child).await;
        assert_eq!(completion_count(&session, &child), 2);
        assert_eq!(requests.lock().unwrap().len(), 2);
        drop(root_inbox);
        session.shutdown().await.unwrap();
    }

    /// A host call answers no model call, so a redirect lets go of its retained
    /// child through the release record alone: the child goes on in the
    /// background, live and on replay, and its later completion wakes the root.
    #[tokio::test]
    async fn redirect_releases_a_host_called_child_to_deliver_its_completion() {
        let steps = [
            Step::new(Vec::new()).midstream(),
            Step::new(answer("redirected")),
            Step::new(answer("child recovered")).gated(),
        ];
        let requests = Requests::default();
        let provider = Script::new(steps, &requests);
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let (executor, owner) = (session.runtime.executor.clone(), session.root.clone());
        let arguments = json!({"prompt":"child task", "depth":0});
        let host =
            tokio::spawn(async move { executor.execute(owner, "agent", arguments, None).await });
        provider.held(0).await;
        let jobs = &session.runtime.jobs;
        let child = jobs.list(&session.root)[0].id;
        assert!(jobs.metadata(child).unwrap().origin.is_none());
        assert_eq!(session.interrupt().await, 1);
        assert_eq!(
            bounded(session.prompt("new direction")).await.unwrap(),
            "redirected"
        );
        assert!(bounded(host).await.unwrap().is_err());
        assert_eq!(jobs.snapshot(child).unwrap().state, JobState::Interrupted);
        let replayed = jobs.test_replay();
        assert!(replayed.is_background(child).unwrap());

        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "");
        provider.request(2).await;
        let root_inbox = quiet_root(&session);
        provider.release(2);
        bounded(jobs.wait_settled(child)).await.unwrap();
        let completed = jobs.test_snapshot(child).await.unwrap();
        assert_eq!(
            (completed.state, completed.output),
            (JobState::Completed, Some(json!("child recovered")))
        );
        assert!(pending(&session, &session.root));
        drop(root_inbox);
        session.shutdown().await.unwrap();
    }

    /// A turn that reaches its request once closing stopped it ends interrupted,
    /// as closing left it, not failed by the refusal.
    #[tokio::test]
    async fn a_turn_closing_stopped_before_its_request_ends_interrupted() {
        let (_root, _requests, session) = scripted_session([answer("never")]).await;
        let store = &session.runtime.store;
        let context = |event: &SessionEvent| matches!(event, SessionEvent::ModelContext { .. });
        let pause = store.pause_committed(context);
        let (reached, resume) = pause.await;
        let prompt = spawn_prompt(&session, "hello");
        bounded(reached).await.unwrap();
        // Closing commits ahead of the request the turn makes once its context publishes.
        let mut shutdown = Box::pin(session.shutdown());
        assert!(futures_util::FutureExt::now_or_never(&mut shutdown).is_none());
        resume.send(()).unwrap();
        let (prompted, shut) = bounded(async { tokio::join!(prompt, shutdown) }).await;
        assert!(matches!(prompted.unwrap(), Err(HarnessError::Interrupted)));
        shut.unwrap();
        assert_eq!(
            count!(&store.records(), SessionEvent::AgentFailed { .. }),
            0
        );
        let stopped = store.read(|state| super::agents::stopped_turn(state, &session.root));
        assert_eq!(stopped, Some(TurnFailure::Interrupted));
        session.close().await.unwrap();
    }
}
