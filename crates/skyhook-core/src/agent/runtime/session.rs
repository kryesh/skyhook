//! Public session submission, inspection, and shutdown API.

use super::*;

impl Harness {
    pub async fn new_session(&self) -> Result<SessionHandle, HarnessError> {
        let store = SessionStore::create(&self.inner.session_root).await?;
        let root = AgentId::root(store.id());
        let started = store
            .append(
                root.clone(),
                SessionEvent::SessionStarted {
                    targets: self.inner.target_definitions.clone(),
                },
            )
            .await?;
        let runtime = SessionRuntime::build(self.inner.clone(), store, vec![started]).await?;
        runtime.start_root(None).await
    }

    pub async fn resume_session(&self, id: SessionId) -> Result<SessionHandle, HarnessError> {
        let (store, records) = SessionStore::open(&self.inner.session_root, id).await?;
        let root = AgentId::root(id);
        let selection = crate::session::agent_selection(&records, &root);
        let runtime = SessionRuntime::build(self.inner.clone(), store, records).await?;
        runtime.start_root(selection).await
    }
}
impl SessionHandle {
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.root.session()
    }

    #[must_use]
    pub fn root_agent(&self) -> &AgentId {
        &self.root
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.runtime.events.subscribe()
    }

    /// Observe without a gap between the initial snapshot and subsequent updates.
    pub async fn observe(&self) -> Observation {
        self.runtime.catch_up_store_events().await;
        self.runtime.events.observe()
    }

    /// Host-only startup diagnostics, never inserted into agent context.
    pub fn startup_warnings(&self) -> &[String] {
        &self.runtime.startup_warnings
    }

    /// Host skill diagnostics that must not write directly to a terminal.
    pub fn warnings(&self) -> &[String] {
        self.runtime.harness.skills.warnings()
    }

    pub fn directory(&self) -> &Path {
        self.runtime.store.directory()
    }

    /// Persist a host-facing status without adding it to the agent's model context.
    pub async fn record_status(&self, agent: AgentId, message: String) -> Result<(), HarnessError> {
        self.runtime
            .store
            .append(agent, SessionEvent::Status { message })
            .await?;
        Ok(())
    }

    pub async fn inspect_jobs(&self, agent: &AgentId) -> Vec<crate::job::JobEnvelope> {
        self.runtime.jobs.list(agent).await
    }

    pub async fn inspect_output(
        &self,
        query: crate::job::JobOutputQuery,
    ) -> Result<serde_json::Value, crate::tool::ToolError> {
        self.runtime
            .jobs
            .inspect_output(query, &self.runtime.harness.capabilities)
            .await
    }

    /// Inspect output and hydrate eligible automatic capture text.
    pub async fn inspect_output_with_captures(
        &self,
        query: crate::job::JobOutputQuery,
    ) -> Result<crate::job::PresentedOutput, crate::tool::ToolError> {
        self.runtime
            .jobs
            .inspect_output_with_captures(query, &self.runtime.harness.capabilities)
            .await
    }

    /// Selectable saved-output pointers, unaffected by presentation-only wrappers.
    pub async fn inspect_output_fields(
        &self,
        job: JobId,
    ) -> Result<Vec<String>, crate::tool::ToolError> {
        self.runtime.jobs.inspect_output_fields(job).await
    }

    pub async fn cancel_job(
        &self,
        job: JobId,
    ) -> Result<crate::job::JobEnvelope, crate::job::JobError> {
        self.runtime.jobs.cancel(job).await
    }

    pub async fn usage(&self) -> Usage {
        *self.runtime.usage.lock().await
    }

    /// Current todo lists for all session agents, including historical children.
    /// Subscribe before reading this snapshot to observe subsequent replacements.
    pub async fn todos(&self) -> Vec<TodoSnapshot> {
        self.runtime.todos.snapshots().await
    }

    #[must_use]
    pub fn tools(&self) -> &ToolRegistry {
        self.runtime.executor.registry()
    }

    pub async fn prompt(&self, text: impl Into<String>) -> Result<String, HarnessError> {
        self.prompt_with_options(text, &[], PromptOptions::default())
            .await
    }

    /// Continue retained history after a failed/interrupted turn, without duplicating its input.
    ///
    /// A soft session interruption first restarts every retained interrupted child.
    /// This deliberately does not enqueue a root request while a parent is still
    /// waiting on those children; ordinary completion delivery wakes it later.
    pub async fn continue_turn(&self) -> Result<String, HarnessError> {
        // Interrupt requests cancel model futures before their owning job has
        // finished journaling. An immediate resume must not miss those children.
        let interrupted = self
            .runtime
            .agents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, agent)| agent.control.retryable_interrupt.load(Ordering::Acquire))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        self.runtime
            .jobs
            .settle_interrupted_agents(&interrupted)
            .await;
        let root_retryable = matches!(
            self.runtime
                .events
                .observe()
                .snapshot
                .activity
                .get(&self.root),
            Some(crate::agent::AgentActivity::Failed(_) | crate::agent::AgentActivity::Interrupted)
        );
        self.runtime.jobs.continue_resumable_children().await?;
        if root_retryable {
            // An independently failed root has no live wait to preserve; continue
            // it after scheduling descendant recovery.
            return self.submit(Vec::new(), None).await;
        }
        // Child-only recovery deliberately leaves a live/waiting root request
        // untouched. Its normal delivery path observes the replacement result.
        Ok(String::new())
    }

    /// Execute a JavaScript workflow through the session's registered `script` tool.
    pub async fn run_script(
        &self,
        source: impl Into<String>,
    ) -> Result<crate::tool::ToolOutput, HarnessError> {
        let result = self
            .runtime
            .executor
            .clone()
            .with_capabilities(
                self.runtime
                    .harness
                    .capabilities
                    .for_agent(self.runtime.harness.max_child_depth),
            )
            .execute(
                self.root.clone(),
                "script",
                json!({"source": source.into()}),
                None,
            )
            .await?;
        Ok(result.output)
    }

    /// Submit a user message and wait for the resulting turn to finish.
    /// Explicit request-boundary enqueues may change the model during that turn.
    pub async fn prompt_with_options(
        &self,
        text: impl Into<String>,
        attachments: &[crate::media::Attachment],
        options: PromptOptions,
    ) -> Result<String, HarnessError> {
        let content = self
            .prepare_prompt(text.into(), attachments, &options)
            .await?;
        self.submit(content, options.model).await
    }

    pub(super) async fn prepare_prompt(
        &self,
        text: String,
        attachments: &[crate::media::Attachment],
        options: &PromptOptions,
    ) -> Result<Vec<UserContent>, HarnessError> {
        use crate::media::Attachment;
        if let Some(model) = &options.model
            && !self.runtime.harness.model_profiles.contains_key(model)
        {
            return Err(HarnessError::UnknownModelProfile(model.clone()));
        }
        // Check every limit before storing any blob.
        let mut images = 0;
        let mut total = 0_u64;
        for attachment in attachments {
            if let Attachment::Image { image, .. } = attachment {
                let bytes = image.bytes().len() as u64;
                images += 1;
                total = total.saturating_add(bytes);
                if bytes > MAX_IMAGE_BYTES
                    || images > MAX_IMAGES_PER_SUBMISSION
                    || total > MAX_IMAGE_BYTES_PER_SUBMISSION
                {
                    return Err(HarnessError::ImageLimit);
                }
            }
        }
        let mut content = vec![UserContent::Text { text }];
        for attachment in attachments {
            let attachment = self.runtime.store.store_attachment(attachment).await?;
            content.push(UserContent::Attachment { attachment });
        }
        Ok(content)
    }

    async fn submit(
        &self,
        content: Vec<UserContent>,
        model: Option<String>,
    ) -> Result<String, HarnessError> {
        let (done_tx, done_rx) = oneshot::channel();
        self.root_tx
            .send(AgentCommand::Input {
                model,
                content,
                done: Some(done_tx),
            })
            .await
            .map_err(|_| HarnessError::AgentStopped)?;
        done_rx
            .await
            .map_err(|_| HarnessError::AgentStopped)?
            .map_err(|failure| HarnessError::Agent(failure.to_string()))
    }

    /// Stop runtime producers and drain their accepted work. The journal stays
    /// open so a host can record its final status after observing shutdown errors.
    /// Await that status append before dropping the session/store owner.
    pub async fn shutdown(&self) -> Result<(), HarnessError> {
        self.runtime
            .shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
        // Wake provider-held mailbox consumers before waiting for intake: a
        // bounded mailbox send may currently hold the admission gate.
        self.runtime.interrupt_tree(&self.root).await;
        // Passing through the gate once is the admission barrier: an operation
        // already holding it finishes its send or write, and every later one
        // observes `shutting_down`. Holding it any longer would stall abandon,
        // acknowledgement and reclaim for the whole drain.
        drop(self.runtime.queue_state.gate.lock().await);
        // Completed children retain idle loops for resumption, and must also stop.
        let senders = self
            .runtime
            .agents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|agent| agent.sender.clone())
            .collect::<Vec<_>>();
        for sender in &senders {
            let _ = sender.send(AgentCommand::Shutdown).await;
        }
        self.runtime.jobs.cancel_and_drain().await?;
        for sender in senders {
            sender.closed().await;
        }
        // Agent loops can finish scheduling cancellation-owned descendants while
        // they unwind. Persist those outcomes before the host drops its runtime.
        self.runtime.jobs.cancel_and_drain().await?;
        self.runtime.mcp.shutdown().await;
        self.runtime.router.shutdown().await;
        self.runtime.jobs.drain_creations().await;
        self.runtime.store.drain().await?;
        Ok(())
    }

    /// Interrupt active turns while retaining child jobs for retry. Explicit
    /// `job_cancel` remains the non-resumable cancellation path.
    pub async fn interrupt(&self) -> usize {
        self.runtime.interrupt_turns(&self.root).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::TodoStatus;
    use crate::agent::runtime::tests::*;

    #[tokio::test]
    async fn shutdown_drains_before_final_host_status_without_closing_journal() {
        let (root, _, session) = scripted_session([]).await;
        let result = session.run_script("console.log('completed'); return 42;");
        assert_eq!(result.await.unwrap().value["value"], 42);
        session.shutdown().await.unwrap();
        // Hosts record final status after shutdown so drain errors are not hidden.
        let status = session.record_status(session.root.clone(), "Completed".into());
        status.await.unwrap();
        let sessions = root.path().join("sessions");
        let durable = SessionStore::read_records(&sessions, session.id()).await;
        let durable = durable.unwrap();
        assert!(matches!(&durable.last().unwrap().event,
            SessionEvent::Status { message } if message == "Completed"));
        assert_eq!(session.runtime.store.records().await, durable);
    }

    #[tokio::test]
    async fn compaction_resume_restores_todos_and_the_first_provider_request() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let reconciled = vec![
            todo("Inspect queue", TodoStatus::Completed),
            todo("Verify queue fix", TodoStatus::Pending),
        ];
        let mut summary = summary_json();
        summary["todo_reconciliation"] = json!(["Inspection finished"]);
        summary["todos"] = json!(reconciled);
        let mut completed = answer("checkpoint installed");
        let usage = Usage {
            input_tokens: 48_000,
            cached_input_tokens: 2_000,
            output_tokens: 1_200,
        };
        completed.insert(completed.len() - 1, ResponseChunk::UsageUpdated { usage });
        let responses = [completed, answer(summary.to_string()), answer("resumed")];
        let provider = scripted_provider(&requests, responses);
        let profile = ModelProfile::new("test", "test", None, 64_000, 4096, false);
        let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
            .model_profile("test", profile)
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let todos = &session.runtime.todos;
        let root_todos = vec![todo("Inspect queue", TodoStatus::InProgress)];
        todos.replace(&session.root, root_todos).await.unwrap();
        let child = session.root.child(1);
        let child_todos = vec![todo("Independent child work", TodoStatus::InProgress)];
        todos.replace(&child, child_todos.clone()).await.unwrap();
        // Exceed the retention tail; only the high-usage response triggers the checkpoint.
        let history = AssistantContent::text("history", 0, "research ".repeat(40_000));
        let history = Message::Assistant(vec![history]);
        session
            .runtime
            .commit(&session.root, history)
            .await
            .unwrap();
        let answer = session.prompt("Compact this investigation.").await.unwrap();
        assert_eq!(answer, "checkpoint installed");
        let records = session.runtime.store.records().await;
        let checkpoints =
            events!(records, SessionEvent::Compaction { checkpoint } => checkpoint.clone());
        let checkpoint = checkpoints.into_iter().next().unwrap();
        assert_eq!(checkpoint.todos, reconciled);
        let prior_request = {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 2);
            assert!(captured[0].response_schema.is_none());
            assert!(captured[1].response_schema.is_some());
            assert!(captured[1].messages().any(|message| matches!(message,
                Message::Assistant(items) if items == &vec![AssistantContent::text("answer", 0, "checkpoint installed")])));
            captured[0].clone()
        };
        let id = session.id();
        shutdown_session(session).await;

        let resumed = harness.resume_session(id).await.unwrap();
        let todos = &resumed.runtime.todos;
        let found = todos.inspect(&resumed.root, None).await.unwrap().items;
        assert_eq!(found, reconciled);
        let found = todos.inspect(&child, None).await.unwrap().items;
        assert_eq!(found, child_todos);
        assert_eq!(resumed.prompt("Continue.").await.unwrap(), "resumed");
        {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 3, "exactly one resumed provider invocation");
            let first = &captured[2];
            assert_eq!(first.history.first(), Some(&checkpoint.message));
            assert_eq!(first.system, prior_request.system);
            assert_eq!(first.tools, prior_request.tools);
            assert_eq!(first.model, prior_request.model);
            // do not leak the summary schema
            assert!(first.response_schema.is_none());
            let runtime = request_runtime_state(first);
            assert!(runtime.contains("Inspect queue") && runtime.contains("completed"));
            assert!(runtime.contains("Verify queue fix") && runtime.contains("pending"));
        }
        shutdown_session(resumed).await;
    }

    #[tokio::test]
    async fn switching_models_preserves_image_history_and_reports_unsupported_images() {
        let root = tempfile::tempdir().unwrap();
        let image = crate::media::Attachment::Image {
            file: Some(root.path().join("sample.png")),
            image: crate::tests::png(b"image fixture"),
        };
        let requests = Requests::default();
        let responses = [answer("Image received"), answer("Image still present")];
        let provider = scripted_provider(&requests, responses);
        let vision = ModelProfile::new("test", "vision-model", None, 128_000, 4096, true);
        let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
            .model_profile("vision", vision)
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let before = session.runtime.store.records().await.len();
        let oversized = crate::media::Attachment::Image {
            file: None,
            image: crate::tests::png(&vec![0; MAX_IMAGE_BYTES as usize]),
        };
        let oversized = [oversized];
        let rejected = session.prompt_with_options("Oversized image", &oversized, model("vision"));
        assert!(matches!(rejected.await, Err(HarnessError::ImageLimit)));
        assert_eq!(session.runtime.store.records().await.len(), before);
        let images = [image];
        let with_image = session.prompt_with_options("Look at this", &images, model("vision"));
        with_image.await.unwrap();
        let error = session.prompt_with_options("Go on", &[], model("test"));
        let error = error.await.unwrap_err().to_string();
        assert!(error.contains("does not support image"), "{error}");
        assert_eq!(requests.lock().unwrap().len(), 1);
        let again = session.prompt_with_options("Use vision again", &[], model("vision"));
        again.await.unwrap();
        assert!(contains_images(&requests.lock().unwrap()[1].history));
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn continuing_interrupted_turn_retains_input_once_and_shutdown_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let provider = Arc::new(BlockingFirstProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            requests: requests.clone(),
            release: Arc::new(tokio::sync::Semaphore::new(0)),
        });
        let harness = test_harness(root.path(), &root.path().join("sessions"), provider).await;
        let session = harness.new_session().await.unwrap();
        let task = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("retained input").await }
        });
        bounded(async {
            while requests.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        session.interrupt().await;
        assert!(task.await.unwrap().is_err());
        let status = session.record_status(session.root.clone(), "Interrupted".into());
        status.await.unwrap();
        assert_eq!(session.continue_turn().await.unwrap(), "jobs handled");
        let records = session.runtime.store.records().await;
        let retained = count!(&records, SessionEvent::MessageCommitted { message: Message::User(blocks) }
            if blocks.iter().any(|block| matches!(block, UserContent::Text { text } if text == "retained input")));
        let interrupted =
            count!(&records, SessionEvent::Status { message } if message == "Interrupted");
        assert_eq!((retained, interrupted), (1, 1));
        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured.len(), 2);
        assert!(captured[0].messages().eq(captured[1].messages()));
        session.shutdown().await.unwrap();
        tokio::task::yield_now().await;
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn script_messages_preserve_order_across_calls_and_cancel_waiting_receivers() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, Arc::new(HangingProvider)).await;
        let session = harness.new_session().await.unwrap();
        let executor = &session.runtime.executor;
        let jobs = &session.runtime.jobs;
        let background = async |source: &str| {
            let arguments = json!({"source": source, "bg":true});
            let running = executor.execute(session.root.clone(), "script", arguments, None);
            running.await.unwrap().job
        };
        let running = background("const messages=[]; for(let i=0;i<4;i++) messages.push(await receive()); return {messages, notifyType:typeof notify};").await;
        let id = running.get();
        let queued = session
            .run_script(format!(
                r#"
    const accepted = [];
    for (const value of [{{text:"hello 🌏", nested:[1,true]}}, null, false]) {{
      accepted.push(await tool.job({id}).send({{value}}));
    }}
    const pending = await tool.job({id}).output();
    return {{accepted, state:pending.state}};
    "#
            ))
            .await
            .unwrap();
        let accepted = json!(vec![json!({"accepted":true}); 3]);
        assert_eq!(queued.value["value"]["accepted"], accepted);
        assert_eq!(queued.value["value"]["state"], "running");
        let last = format!("return tool.job({id}).send({{value:\"last\"}});");
        session.run_script(last).await.unwrap();
        jobs.wait(running, None, true).await.unwrap();
        let output = format!("return tool.job({id}).output();");
        let completed = session.run_script(output).await.unwrap();
        assert_eq!(completed.value["value"]["state"], "completed");
        assert_eq!(
            completed.value["value"]["result"]["value"],
            json!({
                "messages":[{"text":"hello 🌏", "nested":[1,true]}, null, false, "last"],
                "notifyType":"undefined",
            })
        );

        let waiting = background("return await receive();").await;
        let id = waiting.get();
        let cancel = format!("await tool.job({id}).output(); return tool.job({id}).cancel();");
        session.run_script(cancel).await.unwrap();
        jobs.wait(waiting, None, true).await.unwrap();
        let output = format!("return tool.job({id}).output();");
        let cancelled = session.run_script(output).await.unwrap();
        assert_eq!(cancelled.value["value"]["state"], "cancelled");
    }
}
