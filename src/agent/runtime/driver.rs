//! Agent command loop and completion coordination.

use super::*;

type Completion = RequestCompletion;

// This is only the retained command loop's state. Shared job liveness, pending
// deliveries and owner forwarding remain authoritative in their own managers.
enum DriverPhase {
    // The root has no waiter to settle, only whether its last turn ended in error.
    Root { parked: bool },
    Child(ChildPhase),
}

enum ChildPhase {
    Idle,
    Running {
        completion: Option<Completion>,
    },
    Answered {
        answer: String,
        completion: Option<Completion>,
    },
    // Failed or interrupted turns retain context, but notifications cannot resume them.
    Parked,
}

/// Replace a child's phase, yielding the waiter the old phase held.
fn take_completion(phase: &mut ChildPhase, next: ChildPhase) -> Option<Completion> {
    match std::mem::replace(phase, next) {
        ChildPhase::Running { completion } | ChildPhase::Answered { completion, .. } => completion,
        ChildPhase::Idle | ChildPhase::Parked => None,
    }
}

impl DriverPhase {
    fn parked(&self) -> bool {
        matches!(
            self,
            Self::Root { parked: true } | Self::Child(ChildPhase::Parked)
        )
    }

    fn begin(&mut self, completion: Option<Completion>) -> Option<Completion> {
        let phase = match self {
            Self::Root { parked } => {
                *parked = false;
                return completion;
            }
            Self::Child(phase) => phase,
        };
        // A new explicit waiter supersedes the old one; callback-free queued
        // input and descendant turns keep the current invocation's waiter.
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

    // Parking is unconditional; the returned flag is only whether the failure
    // reached this invocation's waiter, which is what finishes the owning job.
    fn fail(&mut self, error: RequestFailure) -> bool {
        let phase = match self {
            Self::Root { parked } => {
                *parked = true;
                return false;
            }
            Self::Child(phase) => phase,
        };
        let Some(completion) = take_completion(phase, ChildPhase::Parked) else {
            return false;
        };
        completion.send(Err(error)).is_ok()
    }

    // Consume the answer even without a waiter. Empty mailbox wakeups after
    // completion must not publish AgentCompleted again for this invocation.
    // `None` means there was no answer to consume; `Some(true)` handed it to the
    // waiter, so the owning job finishes and its own completion wakes the owner;
    // `Some(false)` consumed it callback-free, leaving the owner to be woken here.
    fn complete(&mut self) -> Option<bool> {
        let Self::Child(phase @ ChildPhase::Answered { .. }) = self else {
            return None;
        };
        let ChildPhase::Answered { answer, completion } =
            std::mem::replace(phase, ChildPhase::Idle)
        else {
            // The guard above matched Answered.
            return None;
        };
        Some(match completion {
            Some(completion) => completion.send(Ok(answer)).is_ok(),
            None => false,
        })
    }
}

impl SessionRuntime {
    /// Wake a child's owner for a reply published without a wake. Only for paths
    /// that do not finish the owning job: a finishing job's own completion presents
    /// reply and completion in one delivery batch, which an earlier wake would split.
    async fn wake_owner(&self, owner_job: Option<JobId>) {
        if let Some(job) = owner_job {
            self.jobs.notify_owner(job).await;
        }
    }

    /// Whether queued input or the agent's own pending deliveries keep its
    /// invocation running. Drains the mailbox into `deferred` to find out.
    async fn invocation_continues(
        &self,
        id: &AgentId,
        rx: &mut mpsc::Receiver<AgentCommand>,
        deferred: &mut VecDeque<AgentCommand>,
    ) -> bool {
        while let Ok(command) = rx.try_recv() {
            deferred.push_back(command);
        }
        let mut queued = deferred.iter();
        queued.any(|command| matches!(command, AgentCommand::QueuedInputs(_)))
            || self.jobs.has_pending(id).await
    }

    /// Resolve an answered invocation once. Without a waiter nothing finishes the
    /// owning job, so the owner is woken here instead.
    async fn complete_invocation(
        &self,
        id: &AgentId,
        owner_job: Option<JobId>,
        phase: &mut DriverPhase,
    ) {
        if let Some(handed) = phase.complete() {
            let _ = self
                .store
                .append(id.clone(), SessionEvent::AgentCompleted)
                .await;
            if !handed {
                self.wake_owner(owner_job).await;
            }
        }
    }

    pub(super) async fn run_agent(self: Arc<Self>, agent_loop: AgentLoop) {
        let AgentLoop {
            control:
                AgentControl {
                    completion_gate,
                    retryable_interrupt,
                },
            id,
            owner_job,
            mut context,
            location,
            mut settings,
            mut rx,
        } = agent_loop;
        let mut phase = if owner_job.is_some() {
            DriverPhase::Child(ChildPhase::Idle)
        } else {
            DriverPhase::Root { parked: false }
        };
        let child = matches!(phase, DriverPhase::Child(_));
        let owner_cancellation = match owner_job {
            Some(job) => match self.jobs.cancellation_token(job).await {
                Ok(token) => token,
                Err(_) => {
                    self.agents_mut().remove(&id);
                    return;
                }
            },
            None => CancellationToken::new(),
        };
        let mut deferred = VecDeque::new();
        loop {
            let command = if let Some(command) = deferred.pop_front() {
                command
            } else {
                tokio::select! {
                    biased;
                    () = owner_cancellation.cancelled() => {
                        // The cancelled owning job finishes and wakes the owner for
                        // whatever is still pending; with no waiter nothing finishes
                        // it, so a retained reply needs the explicit wake.
                        let failure = RequestFailure::Failed("child agent cancelled".to_owned());
                        if !phase.fail(failure) {
                            self.wake_owner(owner_job).await;
                        }
                        let _ = self.store.append(id.clone(), SessionEvent::AgentInterrupted).await;
                        break;
                    }
                    // The mailbox closes only at shutdown, after every resolution
                    // path above already finished the job or woke the owner.
                    command = rx.recv() => match command { Some(command) => command, None => break },
                }
            };
            if phase.parked() && matches!(command, AgentCommand::JobsReady) {
                // Leave durable notifications pending without rearming a failed
                // turn or clearing its interruption marker before explicit resume;
                // the next input's request carries them.
                continue;
            }
            // Register before claiming/persisting a queued message: interrupt must
            // not be lost while a commit is in flight.
            let Some(cancellation) = self.begin_turn(&id) else {
                // Stopping outranks whatever was queued: `Shutdown` can sit behind a
                // `JobsReady`, so the flag, not the command, decides. A pending reply
                // stays journaled for resume.
                let _ = self
                    .store
                    .append(id.clone(), SessionEvent::AgentInterrupted)
                    .await;
                break;
            };
            if let Some(sender) = self.agent_sender(&id) {
                // Every input/notification path shares the same delivery gate.
                let _ = sender.flush_events(&cancellation).await;
            }
            let mut pending_events = None;
            let mut turn = TurnContext {
                agent: &id,
                owner_job,
                cancellation: &cancellation,
                location: &location,
                capabilities: settings.capabilities.clone(),
            };
            let (content, done, options) = match command {
                AgentCommand::QueuedInputs(inputs) => {
                    let (consumed, failed) = self
                        .consume_queued_batch(&mut turn, &mut context, &mut settings, inputs)
                        .await;
                    if failed {
                        queue::reject_pending(&mut rx, &mut deferred);
                    }
                    if !consumed {
                        continue;
                    }
                    (Vec::new(), None, PromptOptions::default())
                }
                AgentCommand::Shutdown => {
                    let _ = self
                        .store
                        .append(id.clone(), SessionEvent::AgentInterrupted)
                        .await;
                    break;
                }
                AgentCommand::Input {
                    content,
                    done,
                    options,
                } => (content, done, options),
                AgentCommand::JobsReady => {
                    let content = match self
                        .pending_event_content(&id, turn.diagnostic_viewer())
                        .await
                    {
                        Ok((content, messages)) if !content.is_empty() => {
                            pending_events = Some(messages);
                            content
                        }
                        Ok(_)
                            if matches!(phase, DriverPhase::Child(ChildPhase::Answered { .. }))
                                && !self.jobs.has_running(&id).await =>
                        {
                            let mut completing = completion_gate.lock().await;
                            if self.invocation_continues(&id, &mut rx, &mut deferred).await {
                                self.wake_owner(owner_job).await;
                                continue;
                            }
                            *completing = false;
                            self.complete_invocation(&id, owner_job, &mut phase).await;
                            continue;
                        }
                        _ => continue,
                    };
                    (content, None, PromptOptions::default())
                }
            };
            let selected = self.select(&mut turn, &mut context, &mut settings, options, false);
            if let Err(error) = selected.await {
                if let Some(done) = done {
                    let _ = done.send(Err((&error).into()));
                }
                // The phase is untouched: a parked child stays parked until an
                // input with an admissible selection resumes its retained task.
                continue;
            }
            let done = phase.begin(done);
            self.activity(&id, AgentActivity::Working);
            if !content.is_empty() {
                let message = Message::User(content);
                let committed = match pending_events {
                    Some(messages) => messages.commit().await,
                    None => self
                        .commit(&id, message.clone())
                        .await
                        .map_err(HarnessError::from),
                };
                if let Err(error) = &committed {
                    if let Some(done) = done {
                        let _ = done.send(Err(error.into()));
                    }
                    // No reply was published in this iteration and every earlier
                    // resolution already finished the job or woke the owner, so the
                    // parked/failed handoff needs no wake of its own.
                    phase.fail(error.into());
                    if child {
                        self.interrupt_tree(&id).await;
                        break;
                    }
                    continue;
                }
                context
                    .projected
                    .push((committed.expect("commit succeeded"), message));
            }
            let result = tokio::select! {
                biased;
                () = owner_cancellation.cancelled() => Err(HarnessError::Interrupted),
                result = self.run_turn(
                    turn,
                    &mut context,
                    &mut settings,
                    &mut rx,
                    &mut deferred,
                ) => result,
            };
            // Serialize the final mailbox check with owner forwarding. An accepted
            // update either joins this request cycle or starts a new retained turn.
            let mut completing = completion_gate.lock().await;
            if result.is_err() {
                // Reject under the forwarding gate too: a late queued update must
                // not start an orphan turn after a retained child has failed.
                queue::reject_pending(&mut rx, &mut deferred);
            }
            if let Ok(answer) = &result {
                // Preserve the latest successful turn BEFORE any pending-work
                // gate can continue the loop (including rejected queued input).
                phase.answered(answer.clone());
            }
            if child
                && result.is_ok()
                && self.invocation_continues(&id, &mut rx, &mut deferred).await
            {
                // No job completion will carry the reply published during the turn.
                self.wake_owner(owner_job).await;
                continue;
            }
            // A terminal turn failure is journaled, not only broadcast: the retry
            // affordance is gated on agent activity, which is otherwise lost when
            // the session is reopened.
            if let Err(error) = &result
                && !matches!(error, HarnessError::Interrupted)
            {
                let _ = self
                    .store
                    .append(
                        id.clone(),
                        SessionEvent::AgentFailed {
                            error: error.to_string(),
                        },
                    )
                    .await;
            }
            self.activity(
                &id,
                match &result {
                    Err(HarnessError::Interrupted) => AgentActivity::Interrupted,
                    Err(error) => AgentActivity::Failed(error.to_string()),
                    Ok(_) if child && self.jobs.has_running(&id).await => {
                        AgentActivity::WaitingChildren
                    }
                    Ok(_) => AgentActivity::Idle,
                },
            );
            if let Some(done) = done {
                let _ = done.send(
                    result
                        .as_ref()
                        .map(Clone::clone)
                        .map_err(RequestFailure::from),
                );
            }
            if !child && let Err(error) = &result {
                phase.fail(error.into());
            }
            if child && result.is_err() {
                if matches!(&result, Err(HarnessError::Interrupted))
                    && (owner_cancellation.is_cancelled()
                        || !retryable_interrupt.load(Ordering::Acquire))
                {
                    // Explicit job/tree cancellation is deliberately final. A
                    // session-turn interrupt only cancels `cancellation` and is
                    // retained below as a retryable Interrupted child job.
                    // An interrupted turn can still have published visible text
                    // (a shielded commit outlives the cancelled turn), so the owner
                    // is woken unless the waiter finishes the job for us.
                    if !phase.fail(RequestFailure::Interrupted) {
                        self.wake_owner(owner_job).await;
                    }
                    let _ = self
                        .store
                        .append(id.clone(), SessionEvent::AgentInterrupted)
                        .await;
                    break;
                }
                // A provider/turn failure is terminal for this invocation, not for
                // the retained child. Keep its command loop, provider session, and
                // projected history alive so the owning job can restart it.
                *completing = false;
                if let Err(error) = &result {
                    // An aborted response commits its partial visible text before
                    // failing the turn; the failing job wakes the owner for it, and
                    // a parked child without a waiter needs the explicit wake.
                    if !phase.fail(error.into()) {
                        self.wake_owner(owner_job).await;
                    }
                }
                continue;
            }
            if matches!(phase, DriverPhase::Child(ChildPhase::Answered { .. }))
                && !self.jobs.has_running(&id).await
            {
                // A descendant may have published a reply and then finished
                // during the awaits above. Check after observing no live jobs.
                if self.jobs.has_pending(&id).await {
                    // The loop continues instead of completing, so the terminal reply
                    // published during this turn needs its wake here.
                    self.wake_owner(owner_job).await;
                    continue;
                }
                *completing = false;
                self.complete_invocation(&id, owner_job, &mut phase).await;
                // Retain the provider session and full projected history while idle.
                // A fresh owner request resumes this same child, never a new agent.
                continue;
            }
            // Loop tail: a child that answered while holding live jobs of its own.
            // The invocation stays open across another mailbox wait, so its terminal
            // reply must not wait for that work to reach the owner. The root has no
            // owner job and never wakes anyone here.
            self.wake_owner(owner_job).await;
        }
        if let Some(job) = owner_job {
            self.jobs.clear_resume_handler(job).await;
        }
        self.agents_mut().remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;
    use crate::job::{JobOutcome, JobSpec, JobState};
    use crate::tool::{ToolOptions, ToolOutput, ToolRegistryBuilder};

    #[tokio::test(start_paused = true)]
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
        let mut events = session.runtime.events.observe().updates;
        let prompt = session.prompt("delegate");
        tokio::pin!(prompt);
        bounded(async {
            loop {
                tokio::select! {
                    result = &mut prompt => panic!("parent returned before child work completed: {result:?}"),
                    event = events.recv() => if matches!(event.unwrap().event, RuntimeEvent::TurnCompleted { text, .. } if text == "premature child answer") { break; },
                }
            }
        }).await;
        let jobs = &session.runtime.jobs;
        let find = async |agent: &AgentId, tool: &str| {
            let mut listed = jobs.list(agent).await.into_iter();
            listed.find(|job| job.tool == tool).unwrap()
        };
        let agent_job = find(&session.root, "agent").await;
        assert!(!agent_job.state.is_terminal());
        let script = find(&session.root.child(1), "script").await;
        jobs.send(script.id, json!("released")).await.unwrap();
        assert_eq!(bounded(prompt).await.unwrap(), "root done");
        // Saved output retains the complete final string.
        let output = jobs.snapshot(agent_job.id).await.unwrap().output;
        assert_eq!(output, Some(json!(final_answer)));
        let requests = requests.lock().unwrap();
        let mut lines = request_runtime_state(&requests[2]).lines().skip(1);
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
            Message::Tool(results) => results.as_slice(),
            _ => &[],
        });
        let result = results.find(|result| result.name == "agent").unwrap();
        assert_eq!(result.result["state"], "completed");
        // A foreground call returns its answer in the result and nothing else is
        // delivered: no progress replies, and the answer only once.
        assert!(result.result["meta"]["last_message"].is_null());
        let serialized = rendered(last);
        assert_eq!(serialized.matches("premature child answer").count(), 0);
        let in_result = serde_json::to_string(&result.result).unwrap();
        let answered = in_result.matches("child work completed").count();
        assert!(answered > 0, "{in_result}");
        assert_eq!(serialized.matches("child work completed").count(), answered);
        for request in requests.iter() {
            let system = &request.system[0].text;
            assert!(!system.contains("compaction"));
            assert!(system.contains("use `job_output` for more output"));
            assert!(!system.contains("Continue with the returned"));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn session_resume_restarts_all_interrupted_children_without_restarting_waiting_parent() {
        let child = |index: usize| {
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
        let parent_session = session.clone();
        let parent = tokio::spawn(async move { parent_session.prompt("delegate").await });
        provider.request(2).await;
        // Observed activity trails the journal; interrupt once the parent is seen waiting.
        bounded(async {
            while !matches!(
                session.observe().await.snapshot.activity.get(&session.root),
                Some(
                    crate::agent::AgentActivity::Tools
                        | crate::agent::AgentActivity::WaitingChildren
                )
            ) {
                poll().await;
            }
        })
        .await;
        let original_jobs = session.runtime.jobs.list(&session.root).await;
        assert_eq!(original_jobs.len(), 2);
        // The children are interrupted; the parent holding them shows interrupted
        // but keeps its wait.
        assert_eq!(session.interrupt().await, 3);
        assert!(!parent.is_finished());
        assert!(!provider.requested_from(3));
        // Resume immediately, even before the child worker journals Interrupted.
        assert!(session.runtime.events.retryable(&session.root));
        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "");
        provider.request(4).await;
        // Resume must leave the original parent wait pending, and shows it waiting
        // again rather than interrupted.
        assert!(!parent.is_finished());
        assert!(!session.runtime.events.retryable(&session.root));
        assert_eq!(
            session.observe().await.snapshot.activity.get(&session.root),
            Some(&crate::agent::AgentActivity::WaitingChildren)
        );
        let records = session.runtime.store.records().await;
        let interrupted = count!(&records, SessionEvent::JobFinished { state, .. } if *state == JobState::Interrupted);
        assert_eq!(interrupted, 2);
        // Only an interrupted stream, never an interrupted startup, journals usage.
        let children = records.iter().filter(|record| record.agent != session.root);
        assert_eq!(count!(children, SessionEvent::Usage { .. }), 2);
        let root_records = records.iter().filter(|record| record.agent == session.root);
        assert_eq!(count!(root_records, SessionEvent::ModelRequested { .. }), 1);
        // Continuation must not create replacement agents.
        let owned =
            count!(&records, SessionEvent::AgentStarted { owner_job, .. } if owner_job.is_some());
        assert_eq!(owned, 2);
        let resumed_jobs = session.runtime.jobs.list(&session.root).await;
        let resumed = resumed_jobs.iter().map(|job| job.id);
        assert!(resumed.eq(original_jobs.iter().map(|job| job.id)));
        assert!(resumed_jobs.iter().all(|j| j.state == JobState::Running));
        let parent_input = |message: &Message| {
            matches!(message, Message::User(content)
            if content.iter().any(|part| matches!(part, UserContent::ParentInput { .. })))
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
    #[tokio::test(start_paused = true)]
    async fn prompt_redirects_a_parent_holding_interrupted_children() {
        let child = json!({"prompt":"child task", "depth":0});
        let steps = [
            Step::new(response(vec![tool_call(0, "agent-0", "agent", child)])),
            Step::new(Vec::new()).midstream(),
            Step::new(answer("redirected")),
            Step::new(answer("child recovered")).gated(),
            Step::new(answer("parent done")),
        ];
        let requests = Requests::default();
        let provider = Script::new(steps, &requests);
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let parent_session = session.clone();
        let parent = tokio::spawn(async move { parent_session.prompt("delegate").await });
        provider.request(1).await;
        bounded(async {
            while !matches!(
                session.observe().await.snapshot.activity.get(&session.root),
                Some(
                    crate::agent::AgentActivity::Tools
                        | crate::agent::AgentActivity::WaitingChildren
                )
            ) {
                poll().await;
            }
        })
        .await;
        assert_eq!(session.interrupt().await, 2);
        assert!(!parent.is_finished());
        assert_eq!(
            bounded(session.prompt("new direction")).await.unwrap(),
            "redirected"
        );
        assert!(bounded(parent).await.unwrap().is_err());
        let jobs = session.runtime.jobs.list(&session.root).await;
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
                &session.runtime.store.records().await,
                SessionEvent::AgentStarted { .. }
            ),
            2
        );
        session.shutdown().await.unwrap();
    }

    /// Queued input joins the held turn instead of ending it: the parent's next
    /// request carries the child's interrupted result and the instruction together.
    #[tokio::test(start_paused = true)]
    async fn queued_input_redirects_a_parent_holding_an_interrupted_child() {
        let child = json!({"prompt":"child task", "depth":0});
        let steps = [
            Step::new(response(vec![tool_call(0, "agent-0", "agent", child)])),
            Step::new(Vec::new()).midstream(),
            Step::new(answer("redirected")),
        ];
        let requests = Requests::default();
        let provider = Script::new(steps, &requests);
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let parent_session = session.clone();
        let parent = tokio::spawn(async move { parent_session.prompt("delegate").await });
        provider.request(1).await;
        bounded(async {
            while !matches!(
                session.observe().await.snapshot.activity.get(&session.root),
                Some(
                    crate::agent::AgentActivity::Tools
                        | crate::agent::AgentActivity::WaitingChildren
                )
            ) {
                poll().await;
            }
        })
        .await;
        assert_eq!(session.interrupt().await, 2);
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
            session.runtime.jobs.list(&session.root).await[0].state,
            JobState::Interrupted
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
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
                let mut chunks = answer("partial child answer");
                let stop_reason = StopReason::Aborted;
                *chunks.last_mut().unwrap() = ResponseChunk::ResponseEnded { stop_reason };
                Step::new(chunks)
            } else {
                Step::fail(ProviderError {
                    retry_after: None,
                    kind: crate::provider::ProviderErrorKind::Authentication,
                    message: "fixture permanent failure".into(),
                })
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
            let parent_session = session.clone();
            let parent = tokio::spawn(async move { parent_session.prompt("wait for input").await });
            let wait_job = bounded(async {
                loop {
                    let mut listed = jobs.list(&session.root).await.into_iter();
                    let running = |job: &crate::job::JobEnvelope| {
                        job.tool == "wait_fixture" && job.state == JobState::Running
                    };
                    if let Some(job) = listed.find(running) {
                        break job.id;
                    }
                    poll().await;
                }
            })
            .await;
            let arguments = json!({"prompt":"retain my task", "bg":true});
            let (executor, agent) = (&session.runtime.executor, session.root.clone());
            let job = executor
                .execute(agent, "agent", arguments, None)
                .await
                .unwrap()
                .job;
            let wait = async || {
                jobs.wait(job, Some(Duration::from_secs(5)), true)
                    .await
                    .unwrap()
            };
            let failed = wait().await;
            assert_eq!(failed.state, JobState::Failed);
            let child = session.root.child(1);
            assert_eq!(requests.lock().unwrap().len(), 2, "no automatic replay");
            if aborted {
                assert!(matches!(&failed.diagnostic.as_ref().unwrap().cause,
                    crate::tool::diagnostic::Cause::Message(message) if message == "provider aborted response"));
                // A rejected model selection neither replays nor unparks the child.
                let sender = session.runtime.agents.read().unwrap()[&child]
                    .sender
                    .clone();
                mailbox_barrier(&sender).await;
                assert_eq!(requests.lock().unwrap().len(), 2);
            }
            let retry = format!("return tool.job({job}).send({{value:'try again'}});");
            let resumed = session.run_script(retry).await.unwrap();
            assert_eq!(resumed.value["value"]["result"]["accepted"], true);
            let recovered = wait().await;
            assert_eq!(recovered.state, JobState::Completed);
            assert_eq!(recovered.output, Some(json!("child recovered")));
            // A child retry must not complete the parent's wait.
            assert!(!parent.is_finished());
            let state = jobs.snapshot(wait_job).await.unwrap().state;
            assert_eq!(state, JobState::Running);
            let records = session.runtime.store.records().await;
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

    #[tokio::test(start_paused = true)]
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
            let output = format!("return tool.job({job}).output();");
            let sent = session.run_script(output).await.unwrap();
            assert_eq!(sent.value["value"]["id"], job.get());
            assert_eq!(sent.value["value"]["state"], "completed");
            assert_eq!(sent.value["value"]["result"], answer);
            assert_eq!(jobs.metadata(job).await.unwrap().state, JobState::Completed);
        }
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            let child_id = Some(child.to_string());
            assert!(
                requests
                    .iter()
                    .all(|request| request.correlation == child_id)
            );
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
                matches!(history.last(), Some(Message::User(content)) if matches!(&content[0], UserContent::ParentInput {text} if text.contains("follow-up two")))
            );
        }
        let records = session.runtime.store.records().await;
        let started =
            count!(&records, SessionEvent::AgentStarted { owner_job: Some(id), .. } if *id == job);
        assert_eq!(started, 1);
        let restored = JobManager::restore(session.runtime.store.clone(), &records);
        let output = restored.await.unwrap().snapshot(job).await.unwrap().output;
        assert_eq!(output, Some(json!("third answer")));
        // Shutdown waits for agent receivers to close: restore the real root loop.
        drop(root_inbox);
        session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn child_finishes_when_another_waiter_claims_its_last_background_result() {
        let (_root, _requests, session) = scripted_session([answer("initial")]).await;
        let (child, sender, job) = retained_child(&session).await;
        let (done, received) = oneshot::channel();
        let mut events = session.runtime.events.observe().updates;
        child_input(&sender, Some(done)).await;
        wait_for_child_answer(&mut events, &child).await;
        claim_completed(&session, job).await;
        assert_eq!(bounded(received).await.unwrap().unwrap(), "initial");
        session.shutdown().await.unwrap();
    }

    /// A retained child with an owner job, plus one background job it owns.
    async fn retained_child(session: &SessionHandle) -> (AgentId, AgentSender, JobId) {
        let child = session.root.child(1);
        let launch = child_launch(session, child.clone(), Some(owner(session).await));
        let sender = session.runtime.spawn_agent(launch).await.unwrap();
        let spec = JobSpec {
            background: true,
            ..JobSpec::test(child.clone(), "manual")
        };
        let job = session
            .runtime
            .jobs
            .create(spec)
            .await
            .unwrap()
            .into_test_id();
        (child, sender, job)
    }

    async fn claim_completed(session: &SessionHandle, job: JobId) {
        let outcome = JobOutcome::Completed(ToolOutput::default());
        session.runtime.jobs.finish(job, outcome).await.unwrap();
        session.runtime.jobs.claim(job).await.unwrap();
    }

    fn input(model: Option<&str>, done: Option<Completion>) -> AgentCommand {
        let model = model.map(str::to_owned);
        let text = if model.is_some() { "" } else { "task" };
        let content = vec![UserContent::Text { text: text.into() }];
        AgentCommand::Input {
            options: PromptOptions { model, mode: None },
            content,
            done,
        }
    }

    async fn child_input(sender: &AgentSender, done: Option<Completion>) {
        sender.send(input(None, done)).await.unwrap();
    }

    // A rejected selection acknowledges all earlier mailbox commands without
    // invoking a provider, replacing a child waiter or starting another turn.
    async fn mailbox_barrier(sender: &AgentSender) {
        let (done, received) = oneshot::channel();
        let rejected = input(Some("missing-driver-test-profile"), Some(done));
        sender.send(rejected).await.unwrap();
        assert!(bounded(received).await.unwrap().is_err());
    }

    async fn wait_for_child_answer(
        events: &mut broadcast::Receiver<crate::agent::ObservedEvent>,
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

    async fn completion_count(session: &SessionHandle, child: &AgentId) -> usize {
        let records = session.runtime.store.records().await;
        let child_records = records.iter().filter(|record| &record.agent == child);
        count!(child_records, SessionEvent::AgentCompleted)
    }

    fn completion_gate(session: &SessionHandle, child: &AgentId) -> Arc<Mutex<bool>> {
        let agents = session.runtime.agents.read().unwrap();
        agents[child].control.completion_gate.clone()
    }

    #[tokio::test(start_paused = true)]
    async fn pending_rejected_input_preserves_latest_child_answer() {
        let (_root, requests, session) =
            scripted_session([answer("old answer"), answer("latest answer")]).await;
        let root_inbox = quiet_root(&session);
        let (child, sender, background) = retained_child(&session).await;
        let (done, received) = oneshot::channel();
        let mut events = session.runtime.events.observe().updates;
        child_input(&sender, Some(done)).await;
        wait_for_child_answer(&mut events, &child).await;
        mailbox_barrier(&sender).await;
        assert_eq!(completion_count(&session, &child).await, 0);

        let gate = completion_gate(&session, &child);
        let completing = gate.lock().await;
        child_input(&sender, None).await;
        wait_for_child_answer(&mut events, &child).await;
        // The newer answer's final mailbox check waits for this forwarding window.
        let cancellation = QueuedPromptCancellation::default();
        assert!(cancellation.cancel());
        let (committed, rejected) = oneshot::channel();
        let text = "cancelled update".to_owned();
        let queued = queue::QueuedInput {
            content: vec![UserContent::Text { text }],
            options: Default::default(),
            cancellation,
            committed,
        };
        let queued = AgentCommand::QueuedInputs(vec![queued]);
        sender.send(queued).await.unwrap();
        claim_completed(&session, background).await;
        sender.send(AgentCommand::JobsReady).await.unwrap();
        drop(completing);
        assert!(bounded(rejected).await.unwrap().is_err());
        assert_eq!(bounded(received).await.unwrap().unwrap(), "latest answer");
        mailbox_barrier(&sender).await;
        assert_eq!(completion_count(&session, &child).await, 1);
        // Rejected input must not run another model turn.
        assert_eq!(requests.lock().unwrap().len(), 2);
        drop(root_inbox);
        session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn callback_free_child_completion_is_consumed_once_on_empty_wakeups() {
        let (_root, requests, session) =
            scripted_session([answer("first"), answer("resumed")]).await;
        let root_inbox = quiet_root(&session);
        let (child, sender, background) = retained_child(&session).await;
        let mut events = session.runtime.events.observe().updates;
        child_input(&sender, None).await;
        wait_for_child_answer(&mut events, &child).await;
        mailbox_barrier(&sender).await;
        let gate = completion_gate(&session, &child);
        let completing = gate.lock().await;
        claim_completed(&session, background).await;
        for _ in 0..3 {
            sender.send(AgentCommand::JobsReady).await.unwrap();
        }
        drop(completing);
        mailbox_barrier(&sender).await;
        assert_eq!(completion_count(&session, &child).await, 1);
        assert_eq!(requests.lock().unwrap().len(), 1);

        // Idle stays resumable, with one fresh completion rather than the consumed one.
        child_input(&sender, None).await;
        wait_for_child_answer(&mut events, &child).await;
        mailbox_barrier(&sender).await;
        sender.send(AgentCommand::JobsReady).await.unwrap();
        mailbox_barrier(&sender).await;
        assert_eq!(completion_count(&session, &child).await, 2);
        assert_eq!(requests.lock().unwrap().len(), 2);
        drop(root_inbox);
        session.shutdown().await.unwrap();
    }
}
