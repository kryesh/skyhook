//! Agent command loop and completion coordination.

use super::*;

impl SessionRuntime {
    pub(super) async fn run_agent(self: Arc<Self>, agent_loop: AgentLoop) {
        let AgentLoop {
            id,
            owner_job,
            mut context,
            mut model_profile,
            location,
            capabilities,
            mut rx,
        } = agent_loop;
        let is_child = owner_job.is_some();
        let owner_cancellation = match owner_job {
            Some(job) => match self.jobs.cancellation_token(job).await {
                Ok(token) => token,
                Err(_) => {
                    self.agents
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&id);
                    return;
                }
            },
            None => CancellationToken::new(),
        };
        let completion_gate = self
            .agents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .expect("registered live agent")
            .completion_gate
            .clone();
        let mut child_done: Option<oneshot::Sender<Result<String, String>>> = None;
        let mut child_answer = None;
        let mut deferred = VecDeque::new();
        loop {
            let command = if let Some(command) = deferred.pop_front() {
                command
            } else {
                tokio::select! {
                    biased;
                    () = owner_cancellation.cancelled() => {
                        if let Some(done) = child_done.take() { let _ = done.send(Err("child agent cancelled".to_owned())); }
                        let _ = self.store.append(id.clone(), SessionEvent::AgentInterrupted).await;
                        break;
                    }
                    command = rx.recv() => match command { Some(command) => command, None => break },
                }
            };
            // Register before claiming/persisting a queued message: interrupt must
            // not be lost while a commit is in flight.
            let cancellation = self.begin_turn(&id);
            if let Some(sender) = self.agent_sender(&id) {
                // Every input/notification path shares the same delivery gate.
                let _ = sender.flush_events(&cancellation).await;
            }
            let mut pending_events = None;
            let (content, done, selected_model) = match command {
                AgentCommand::QueuedInputs(inputs) => {
                    if !self
                        .consume_queued_batch(
                            &id,
                            &mut context,
                            &mut model_profile,
                            &capabilities,
                            &cancellation,
                            inputs,
                        )
                        .await
                    {
                        continue;
                    }
                    (Vec::new(), None, None)
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
                    model,
                } => (content, done, model),
                AgentCommand::JobsReady => {
                    let content = match self
                        .pending_event_content(&id, &capabilities, &location)
                        .await
                    {
                        Ok((content, messages)) if !content.is_empty() => {
                            pending_events = Some(messages);
                            content
                        }
                        Ok(_)
                            if is_child
                                && child_answer.is_some()
                                && !self.jobs.has_running(&id).await =>
                        {
                            let mut completing = completion_gate.lock().await;
                            while let Ok(command) = rx.try_recv() {
                                deferred.push_back(command);
                            }
                            if deferred
                                .iter()
                                .any(|command| matches!(command, AgentCommand::QueuedInputs(_)))
                                || self.jobs.has_pending(&id).await
                            {
                                continue;
                            }
                            *completing = false;
                            if let Some(done) = child_done.take() {
                                let _ =
                                    done.send(Ok(child_answer.take().expect("child has answered")));
                            }
                            let _ = self
                                .store
                                .append(id.clone(), SessionEvent::AgentCompleted)
                                .await;
                            continue;
                        }
                        _ => continue,
                    };
                    (content, None, None)
                }
            };
            if let Some(model) = selected_model.filter(|model| model != &model_profile)
                && let Err(error) = self
                    .select_model(&id, &mut context, &mut model_profile, &capabilities, model)
                    .await
            {
                if let Some(done) = done {
                    let _ = done.send(Err(error.to_string()));
                }
                continue;
            }
            let done = if is_child {
                if done.is_some() {
                    child_done = done;
                }
                None
            } else {
                done
            };
            self.activity(&id, AgentActivity::Working);
            if !content.is_empty() {
                let message = Message::User(content);
                let committed = match pending_events {
                    Some(messages) => messages.commit(&self, &id, message.clone()).await,
                    None => self
                        .commit(&id, message.clone())
                        .await
                        .map_err(HarnessError::from),
                };
                if let Err(error) = &committed {
                    if let Some(done) = done.or_else(|| child_done.take()) {
                        let _ = done.send(Err(error.to_string()));
                    }
                    if is_child {
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
                    TurnContext {
                        agent: &id,
                        owner_job,
                        cancellation: &cancellation,
                        location: &location,
                        capabilities: &capabilities,
                    },
                    &mut context,
                    &mut model_profile,
                    &mut rx,
                    &mut deferred,
                ) => result,
            };
            if result.is_err() {
                // Unclaimed queue entries remain caller-owned after an interrupt;
                // do not silently start a new turn for them.
                queue::reject_pending(&mut rx, &mut deferred);
            }
            // Serialize the final mailbox check with owner forwarding. An accepted
            // update either joins this request cycle or starts a new retained turn.
            let mut completing = completion_gate.lock().await;
            if is_child && result.is_ok() {
                while let Ok(command) = rx.try_recv() {
                    deferred.push_back(command);
                }
                if deferred
                    .iter()
                    .any(|command| matches!(command, AgentCommand::QueuedInputs(_)))
                    || self.jobs.has_pending(&id).await
                {
                    continue;
                }
            }
            self.activity(
                &id,
                match &result {
                    Err(HarnessError::Interrupted) => AgentActivity::Interrupted,
                    Err(error) => AgentActivity::Failed(error.to_string()),
                    Ok(_) if is_child && self.jobs.has_running(&id).await => {
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
                        .map_err(ToString::to_string),
                );
            }
            if is_child && result.is_err() {
                self.interrupt_tree(&id).await;
                // A cancelled child must not keep its command loop alive.
                if let Some(done) = child_done.take() {
                    let _ = done.send(
                        result
                            .as_ref()
                            .map(Clone::clone)
                            .map_err(ToString::to_string),
                    );
                }
                let _ = self
                    .store
                    .append(id.clone(), SessionEvent::AgentInterrupted)
                    .await;
                break;
            }
            if is_child {
                child_answer = result.as_ref().ok().cloned();
            }
            if is_child && !self.jobs.has_running(&id).await {
                // A descendant may have published a reply and then finished
                // during the awaits above. Check after observing no live jobs.
                if self.jobs.has_pending(&id).await {
                    continue;
                }
                *completing = false;
                if let Some(done) = child_done.take() {
                    let _ = done.send(
                        result
                            .as_ref()
                            .map(Clone::clone)
                            .map_err(ToString::to_string),
                    );
                }
                let _ = self
                    .store
                    .append(id.clone(), SessionEvent::AgentCompleted)
                    .await;
                // Retain the provider session and full projected history while idle.
                // A fresh owner request resumes this same child, never a new agent.
                child_answer = None;
                continue;
            }
        }
        if let Some(job) = owner_job {
            self.jobs.clear_resume_handler(job).await;
        }
        self.agents
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;

    #[tokio::test]
    async fn child_completion_waits_for_background_work_and_returns_its_updated_answer() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let call = |name: &str, arguments| {
            response(vec![AssistantContent::tool_call(
                "tool-0",
                0,
                ToolCall {
                    id: name.to_owned(),
                    name: name.to_owned(),
                    arguments,
                },
            )])
        };
        let text =
            |text: &str| response(vec![AssistantContent::text("answer", 0, text.to_owned())]);
        let answer = "child work completed\n".repeat(500);
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    call("agent", json!({"prompt":"work"})),
                    call(
                        "script",
                        json!({"source":"return await receive();", "bg":true}),
                    ),
                    text("premature child answer"),
                    text(&answer),
                    text("root done"),
                    // The large final message occupies its own bounded delivery
                    // batch after the premature response, independently of the
                    // foreground tool result. A no-tool parent boundary must
                    // continue to consume that pending batch before returning.
                    text("root done"),
                ],
            ),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let mut events = session.runtime.events.subscribe();
        let prompt = session.prompt("delegate");
        tokio::pin!(prompt);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut prompt => panic!("parent returned before child work completed: {result:?}"),
                    event = events.recv() => if matches!(event.unwrap(), RuntimeEvent::TurnCompleted { text, .. } if text == "premature child answer") { break; },
                }
            }
        }).await.unwrap();
        let agent_job = session
            .runtime
            .jobs
            .list(&session.root)
            .await
            .into_iter()
            .find(|job| job.tool == "agent")
            .unwrap();
        assert!(!agent_job.state.is_terminal());
        let child = session.root.child(1);
        let script = session
            .runtime
            .jobs
            .list(&child)
            .await
            .into_iter()
            .find(|job| job.tool == "script")
            .unwrap();
        session
            .runtime
            .jobs
            .send(script.id, json!("released"))
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), prompt)
                .await
                .unwrap()
                .unwrap(),
            "root done"
        );
        assert_eq!(
            session
                .runtime
                .jobs
                .snapshot(agent_job.id)
                .await
                .unwrap()
                .output,
            Some(json!(answer)),
            "saved output retains the complete final string"
        );
        let requests = requests.lock().unwrap();
        let active_state = request_runtime_state(&requests[2]);
        let mut lines = active_state.lines().skip(1);
        assert_eq!(
            lines.next(),
            Some("jobs: job parent tool name state age_s turns tool_calls")
        );
        let fields = lines.next().unwrap().split(' ').collect::<Vec<_>>();
        assert_eq!(fields.len(), 8, "same-location jobs need no overrides");
        assert_eq!(fields[0], script.id.to_string());
        assert_eq!(&fields[1..4], &["-", "script", "-"]);
        assert!(matches!(fields[4], "queued" | "running"));
        fields[5].parse::<u64>().unwrap();
        assert_eq!(&fields[6..], &["-", "-"]);
        assert!(lines.next().is_none(), "empty todos are omitted");
        let last = requests.last().unwrap();
        let result = request_history(last)
            .iter()
            .filter_map(|message| match message {
                Message::Tool(results) => Some(results),
                _ => None,
            })
            .flatten()
            .find(|result| result.name == "agent")
            .expect("child tool result expected");
        assert_eq!(result.result["state"], "completed");
        assert!(
            result.result.get("result").is_none(),
            "automatic tool result must not repeat final text"
        );
        assert!(result.result["last_message"].is_u64());
        let serialized = serde_json::to_string(&last.messages).unwrap();
        assert_eq!(serialized.matches("premature child answer").count(), 1);
        assert_eq!(
            serialized.matches("child work completed").count(),
            500,
            "large final message is delivered once independently of the tool result"
        );
        assert!(result.result.get("truncated").is_none());
        for request in requests.iter() {
            let system = &request.system[0].text;
            assert!(!system.contains("compaction"));
            assert!(system.contains("Use job_output to retrieve truncated results."));
            assert!(!system.contains("Continue with the returned"));
        }
    }

    #[tokio::test]
    async fn completed_child_resumes_same_history_and_job_repeatedly() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    response(vec![AssistantContent::text("answer", 0, "first answer")]),
                    response(vec![AssistantContent::text("answer", 0, "second answer")]),
                    response(vec![AssistantContent::text("answer", 0, "third answer")]),
                ],
            ),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        // This test drives tools directly; suppress autonomous parent wakeups.
        let (quiet_sender, _quiet_receiver) = mpsc::channel(AGENT_CHANNEL_CAPACITY);
        session
            .runtime
            .agents
            .write()
            .unwrap()
            .get_mut(&session.root)
            .unwrap()
            .sender = AgentSender::new(quiet_sender);
        let first = session
            .runtime
            .executor
            .execute(
                session.root.clone(),
                "agent",
                json!({"prompt":"remember the initial task", "depth":0}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(first.output.value, "first answer");
        let job = first.job;
        let child = session.root.child(1);
        assert_eq!(session.runtime.jobs.prune_claimed().await.unwrap(), 0);
        for (instruction, answer) in [
            ("follow-up one", "second answer"),
            ("follow-up two", "third answer"),
        ] {
            session
                .run_script(format!(
                    "return tool.job({job}).send({{value:{}}});",
                    json!(instruction)
                ))
                .await
                .unwrap();
            session.runtime.jobs.wait(job, None, true).await.unwrap();
            let sent = session
                .run_script(format!("return tool.job({job}).output();"))
                .await
                .unwrap();
            assert_eq!(sent.value["value"]["id"], job.get());
            assert_eq!(sent.value["value"]["state"], "completed");
            assert_eq!(sent.value["value"]["result"], answer);
            assert_eq!(
                session.runtime.jobs.metadata(job).await.unwrap().state,
                crate::job::JobState::Completed
            );
        }
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert!(
                requests
                    .iter()
                    .all(|request| request.correlation.as_deref()
                        == Some(child.to_string().as_str()))
            );
            let history = request_history(&requests[2]);
            let text = serde_json::to_string(history).unwrap();
            for expected in [
                "remember the initial task",
                "first answer",
                "follow-up one",
                "second answer",
                "follow-up two",
            ] {
                assert!(text.contains(expected), "missing {expected}: {text}");
            }
            assert!(
                matches!(history.last(), Some(Message::User(content)) if matches!(&content[0], UserContent::ParentInput {text} if text.contains("follow-up two")))
            );
        }
        assert_eq!(session.runtime.store.records().await.iter().filter(|record| matches!(&record.event, SessionEvent::AgentStarted {owner_job:Some(id), ..} if *id == job)).count(), 1);
        let restored = JobManager::restore(
            session.runtime.store.clone(),
            &session.runtime.store.records().await,
        )
        .await
        .unwrap();
        assert_eq!(
            restored.snapshot(job).await.unwrap().output,
            Some(json!("third answer"))
        );
        // Shutdown now waits for agent receivers to close. Restore the real
        // root loop instead of waiting on the intentionally unpolled test inbox.
        session
            .runtime
            .agents
            .write()
            .unwrap()
            .get_mut(&session.root)
            .unwrap()
            .sender = session.root_tx.clone();
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn child_finishes_when_another_waiter_claims_its_last_background_result() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(BlockingFirstProvider {
                calls: Arc::new(AtomicUsize::new(0)),
                requests: Arc::new(StdMutex::new(Vec::new())),
                release: release.clone(),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let child = session.root.child(1);
        let owner_job = session
            .runtime
            .jobs
            .create(crate::job::JobSpec::test(session.root.clone(), "agent"))
            .await
            .unwrap()
            .id;
        let job = session
            .runtime
            .jobs
            .create(crate::job::JobSpec {
                background: true,
                ..crate::job::JobSpec::test(child.clone(), "manual")
            })
            .await
            .unwrap()
            .id;
        let sender = session
            .runtime
            .spawn_agent(AgentLaunch {
                id: child.clone(),
                owner_job: Some(owner_job),
                model_profile: "test".to_owned(),
                todos: None,
                available_depth: 0,
                location: crate::execution::ExecutionLocation::root(workspace.path().to_path_buf()),
            })
            .await
            .unwrap();
        let (done, received) = oneshot::channel();
        let mut events = session.runtime.events.subscribe();
        sender
            .send(AgentCommand::Input {
                model: None,
                content: vec![UserContent::Text {
                    text: "task".to_owned(),
                }],
                done: Some(done),
            })
            .await
            .unwrap();
        release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(events.recv().await.unwrap(), RuntimeEvent::TurnCompleted {agent, ..} if agent == child) { break; }
            }
        }).await.unwrap();
        session
            .runtime
            .jobs
            .finish(
                job,
                crate::job::JobOutcome::Completed(crate::tool::ToolOutput::default()),
            )
            .await
            .unwrap();
        session.runtime.jobs.claim(job).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), received)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            "initial"
        );
        session.shutdown().await.unwrap();
    }
}
