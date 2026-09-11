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
    pub async fn continue_turn(&self) -> Result<String, HarnessError> {
        self.submit(Vec::new(), None).await
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

    pub async fn prompt_with_images(
        &self,
        text: impl Into<String>,
        paths: &[PathBuf],
    ) -> Result<String, HarnessError> {
        self.prompt_with_options(text, paths, PromptOptions::default())
            .await
    }

    /// Submit a user message and wait for the resulting turn to finish.
    /// Explicit request-boundary enqueues may change the model during that turn.
    pub async fn prompt_with_options(
        &self,
        text: impl Into<String>,
        paths: &[PathBuf],
        options: PromptOptions,
    ) -> Result<String, HarnessError> {
        let content = self.prepare_prompt(text.into(), paths, &options).await?;
        self.submit(content, options.model).await
    }

    /// Queue a message for the next safe model-request boundary, including while a
    /// request or tool is running. Images and the selected model are captured at
    /// submission. Returns after history commit, NOT after the model finishes.
    ///
    /// Retain a clone of `token` to cancel an unclaimed submission. A successful
    /// cancellation guarantees that its message will not enter history. Cancelled
    /// submissions return [`HarnessError::Interrupted`]. Once claimed, await this
    /// receipt before editing or retrying the message. A dropped receipt does not
    /// cancel the input. Errors mean the user message was not committed.
    ///
    /// Concurrent preparations are serialized in polling order. All available
    /// messages are committed FIFO before compaction and the next provider request;
    /// the last explicit model selection governs that request. Ordinary `prompt`
    /// calls retain their turn-completion semantics.
    pub async fn enqueue_prompt_with_options(
        &self,
        text: impl Into<String>,
        paths: &[PathBuf],
        options: PromptOptions,
        token: QueuedPromptToken,
    ) -> Result<(), HarnessError> {
        self.enqueue_prompts_with_options(vec![QueuedPrompt {
            text: text.into(),
            paths: paths.to_vec(),
            options,
            token,
        }])
        .await
        .pop()
        .expect("one queued prompt has one receipt")
    }

    /// Prepare and enqueue a group atomically at a model-request boundary.
    /// Results correspond to input order and acknowledge history commits, not
    /// turn completion. Invalid or cancelled items do not prevent the remaining
    /// items from committing FIFO. The whole group is prepared before it becomes
    /// visible to the runtime, even when preparing attachments yields.
    pub async fn enqueue_prompts_with_options(
        &self,
        inputs: Vec<QueuedPrompt>,
    ) -> Vec<Result<(), HarnessError>> {
        let preparation = self.enqueue_preparation.lock().await;
        let mut batch = Vec::with_capacity(inputs.len());
        let mut receipts = Vec::with_capacity(inputs.len());
        for input in inputs {
            if input.token.is_cancelled() {
                receipts.push(Err(HarnessError::Interrupted));
                continue;
            }
            match self
                .prepare_prompt(input.text, &input.paths, &input.options)
                .await
            {
                Ok(content) => {
                    let (committed, receipt) = oneshot::channel();
                    batch.push(queue::QueuedInput {
                        content,
                        model: input.options.model,
                        token: input.token,
                        committed,
                    });
                    receipts.push(Ok(receipt));
                }
                Err(error) => receipts.push(Err(error)),
            }
        }
        if !batch.is_empty()
            && !self
                .runtime
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
        {
            // A failed send drops every sender, resolving each receipt as stopped.
            let _ = self.root_tx.send(AgentCommand::QueuedInputs(batch)).await;
        } else {
            drop(batch);
        }
        drop(preparation);
        let mut results = Vec::with_capacity(receipts.len());
        for receipt in receipts {
            results.push(match receipt {
                Ok(receipt) => receipt.await.unwrap_or(Err(HarnessError::AgentStopped)),
                Err(error) => Err(error),
            });
        }
        results
    }

    async fn prepare_prompt(
        &self,
        text: String,
        paths: &[PathBuf],
        options: &PromptOptions,
    ) -> Result<Vec<UserContent>, HarnessError> {
        if let Some(model) = &options.model
            && !self.runtime.harness.model_profiles.contains_key(model)
        {
            return Err(HarnessError::UnknownModelProfile(model.clone()));
        }
        if paths.len() > MAX_IMAGES_PER_SUBMISSION {
            return Err(HarnessError::ImageLimit);
        }
        let mut content = vec![UserContent::Text { text }];
        let mut total = 0_u64;
        for path in paths {
            let absolute = contained_path(&self.runtime.harness.workspace, path).await?;
            let bytes = fs::read(&absolute).await?;
            let length = u64::try_from(bytes.len()).map_err(|_| HarnessError::ImageLimit)?;
            if length > MAX_IMAGE_BYTES {
                return Err(HarnessError::ImageLimit);
            }
            total = total.saturating_add(length);
            if total > MAX_IMAGE_BYTES_PER_SUBMISSION {
                return Err(HarnessError::ImageLimit);
            }
            let media_type = image_media_type(&absolute).ok_or(HarnessError::UnsupportedImage)?;
            let image = self
                .runtime
                .store
                .import_blob(
                    &bytes,
                    absolute
                        .file_name()
                        .map_or_else(|| "image".to_owned(), |name| name.to_string_lossy().into()),
                    media_type.to_owned(),
                )
                .await?;
            content.push(UserContent::Image { image });
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
            .map_err(HarnessError::Agent)
    }

    pub async fn shutdown(&self) -> Result<(), HarnessError> {
        self.runtime
            .shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
        self.runtime.interrupt_tree(&self.root).await;
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
        Ok(())
    }

    pub async fn interrupt(&self) -> usize {
        self.runtime.interrupt_tree(&self.root).await
    }
}

async fn contained_path(workspace: &Path, requested: &Path) -> Result<PathBuf, HarnessError> {
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        workspace.join(requested)
    };
    let canonical = fs::canonicalize(candidate).await?;
    if !canonical.starts_with(workspace) {
        return Err(HarnessError::OutsideWorkspace);
    }
    Ok(canonical)
}

fn image_media_type(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;

    #[tokio::test]
    async fn compaction_resume_restores_todos_and_the_first_provider_request() {
        use crate::agent::TodoStatus;

        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let reconciled = vec![
            TodoItem {
                text: "Inspect queue".into(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                text: "Verify queue fix".into(),
                status: TodoStatus::Pending,
            },
        ];
        let summary = json!({
            "objective":"Preserve the queue investigation", "user_instructions":[],
            "session_rules":[], "plan":["Verify the queue fix"], "findings":[],
            "open_issues":[], "running_work":[], "completed_work":[], "decisions":[],
            "recovery_details":[], "jobs":[], "additional_context":[],
            "todo_reconciliation":["Inspection finished"], "todos":reconciled,
            "resumption_point":"Run the focused verification", "next_actions":[]
        });
        let harness = test_builder(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    answer(summary.to_string()),
                    answer("checkpoint installed"),
                    answer("resumed"),
                ],
            ),
        )
        .model_profile(
            "test",
            ModelProfile {
                provider: "test".into(),
                model: "test".into(),
                reasoning: None,
                max_context: 64_000,
                max_output: 4096,
                supports_images: false,
            },
        )
        .build()
        .await
        .unwrap();
        let session = harness.new_session().await.unwrap();
        session
            .runtime
            .todos
            .replace(
                &session.root,
                vec![TodoItem {
                    text: "Inspect queue".into(),
                    status: TodoStatus::InProgress,
                }],
            )
            .await
            .unwrap();
        let child = session.root.child(1);
        let child_todos = vec![TodoItem {
            text: "Independent child work".into(),
            status: TodoStatus::InProgress,
        }];
        session
            .runtime
            .todos
            .replace(&child, child_todos.clone())
            .await
            .unwrap();
        // Exceed the real runtime threshold and the verbatim retention tail.
        session
            .runtime
            .commit(
                &session.root,
                Message::Assistant(vec![AssistantContent::text(
                    "history",
                    0,
                    "research ".repeat(40_000),
                )]),
            )
            .await
            .unwrap();
        assert_eq!(
            session.prompt("Compact this investigation.").await.unwrap(),
            "checkpoint installed"
        );
        let checkpoint = session
            .runtime
            .store
            .records()
            .await
            .into_iter()
            .find_map(|record| match record.event {
                SessionEvent::Compaction { checkpoint } => Some(checkpoint),
                _ => None,
            })
            .expect("the real turn installed a checkpoint");
        assert_eq!(checkpoint.todos, reconciled);
        let prior_request = {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 2);
            assert!(captured[0].response_schema.is_some());
            captured[1].clone()
        };
        let id = session.id();
        shutdown_session(session).await;

        let resumed = harness.resume_session(id).await.unwrap();
        assert_eq!(
            resumed
                .runtime
                .todos
                .inspect(&resumed.root, None)
                .await
                .unwrap()
                .items,
            reconciled
        );
        assert_eq!(
            resumed
                .runtime
                .todos
                .inspect(&child, None)
                .await
                .unwrap()
                .items,
            child_todos
        );
        assert_eq!(
            resumed
                .prompt("Continue the preserved task.")
                .await
                .unwrap(),
            "resumed"
        );
        {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 3, "exactly one resumed provider invocation");
            let first = &captured[2];
            assert_eq!(first.messages.first(), Some(&checkpoint.message));
            assert_eq!(first.system, prior_request.system);
            assert_eq!(first.tools, prior_request.tools);
            assert_eq!(first.model, prior_request.model);
            assert!(
                first.response_schema.is_none(),
                "do not leak the summary schema"
            );
            let runtime = request_runtime_state(first);
            assert!(runtime.contains("Inspect queue") && runtime.contains("completed"));
            assert!(runtime.contains("Verify queue fix") && runtime.contains("pending"));
        }
        shutdown_session(resumed).await;
    }

    #[tokio::test]
    async fn switching_models_preserves_image_history_and_reports_unsupported_images() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let image = workspace.path().join("sample.png");
        fs::write(&image, b"image fixture").await.unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let provider = scripted_provider(
            &requests,
            [
                response(vec![AssistantContent::text("answer", 0, "Image received")]),
                response(vec![AssistantContent::text(
                    "answer",
                    0,
                    "Image still present",
                )]),
            ],
        );
        let harness = test_builder(workspace.path(), sessions.path(), provider)
            .model_profile(
                "vision",
                ModelProfile {
                    provider: "test".into(),
                    model: "vision-model".into(),
                    reasoning: None,
                    max_context: 128_000,
                    max_output: 4096,
                    supports_images: true,
                },
            )
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let before = session.runtime.store.records().await.len();
        assert!(
            session
                .prompt_with_options(
                    "Missing image",
                    &[workspace.path().join("missing.png")],
                    PromptOptions {
                        model: Some("vision".into())
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(session.runtime.store.records().await.len(), before);
        session
            .prompt_with_options(
                "Look at this",
                &[image],
                PromptOptions {
                    model: Some("vision".into()),
                },
            )
            .await
            .unwrap();
        let error = session
            .prompt_with_options(
                "Keep going",
                &[],
                PromptOptions {
                    model: Some("test".into()),
                },
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("does not support image"),
            "{error}"
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
        session
            .prompt_with_options(
                "Use vision again",
                &[],
                PromptOptions {
                    model: Some("vision".into()),
                },
            )
            .await
            .unwrap();
        {
            let requests = requests.lock().unwrap();
            assert!(contains_images(&requests[1].messages));
        }
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn continuing_interrupted_turn_retains_input_once_and_shutdown_is_idempotent() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(BlockingFirstProvider {
                calls: Arc::new(AtomicUsize::new(0)),
                requests: requests.clone(),
                release: Arc::new(tokio::sync::Semaphore::new(0)),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let task = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("retained input").await }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while requests.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        session.interrupt().await;
        assert!(task.await.unwrap().is_err());
        session
            .record_status(session.root.clone(), "Interrupted".into())
            .await
            .unwrap();
        assert_eq!(session.continue_turn().await.unwrap(), "jobs handled");
        let records = session.runtime.store.records().await;
        assert_eq!(records.iter().filter(|record| matches!(&record.event,
            SessionEvent::MessageCommitted { message: Message::User(blocks) }
                if blocks.iter().any(|block| matches!(block, UserContent::Text { text } if text == "retained input"))
        )).count(), 1);
        assert!(records.iter().any(|record| matches!(&record.event, SessionEvent::Status { message } if message == "Interrupted")));
        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].messages, captured[1].messages);
        session.shutdown().await.unwrap();
        tokio::task::yield_now().await;
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn script_messages_preserve_order_across_calls_and_cancel_waiting_receivers() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness =
            test_harness(workspace.path(), sessions.path(), Arc::new(HangingProvider)).await;
        let session = harness.new_session().await.unwrap();
        let running = session.runtime.executor.execute(
            session.root.clone(), "script",
            json!({"source": "const messages=[]; for(let i=0;i<4;i++) messages.push(await receive()); return {messages, notifyType:typeof notify};", "bg":true}),
            None,
        ).await.unwrap();
        let id = running.job.get();
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
        assert_eq!(
            queued.value["value"]["accepted"],
            json!(vec![json!({"accepted":true}); 3])
        );
        assert_eq!(queued.value["value"]["state"], "running");
        session
            .run_script(format!("return tool.job({id}).send({{value:\"last\"}});"))
            .await
            .unwrap();
        session
            .runtime
            .jobs
            .wait(running.job, None, true)
            .await
            .unwrap();
        let completed = session
            .run_script(format!("return tool.job({id}).output();"))
            .await
            .unwrap();
        assert_eq!(completed.value["value"]["state"], "completed");
        assert_eq!(
            completed.value["value"]["result"]["value"],
            json!({
                "messages":[{"text":"hello 🌏", "nested":[1,true]}, null, false, "last"],
                "notifyType":"undefined",
            })
        );

        let waiting = session
            .runtime
            .executor
            .execute(
                session.root.clone(),
                "script",
                json!({"source":"return await receive();", "bg":true}),
                None,
            )
            .await
            .unwrap();
        let id = waiting.job.get();
        session
            .run_script(format!(
                "await tool.job({id}).output(); return tool.job({id}).cancel();"
            ))
            .await
            .unwrap();
        session
            .runtime
            .jobs
            .wait(waiting.job, None, true)
            .await
            .unwrap();
        let cancelled = session
            .run_script(format!("return tool.job({id}).output();"))
            .await
            .unwrap();
        assert_eq!(cancelled.value["value"]["state"], "cancelled");
    }
}
