//! Public session submission, inspection, and shutdown API.

use super::*;
use crate::session::{SessionTitle, TitleSource};

impl Harness {
    pub async fn new_session(&self) -> Result<SessionHandle, HarnessError> {
        let store = SessionStore::create(&self.inner.session_root).await?;
        let jobs = JobManager::new(store.clone());
        let runtime = SessionRuntime::build(self.inner.clone(), store, jobs, &[]).await?;
        runtime.start_root(runtime.new_root()?).await
    }

    pub async fn resume_session(&self, id: SessionId) -> Result<SessionHandle, HarnessError> {
        let (store, mut records) = SessionStore::open(&self.inner.session_root, id).await?;
        // Settlement closes only what this already reads as stopped.
        let stopped = store.stopped_turn().await?;
        let reopened = SessionEvent::SessionReopened;
        records.push(store.append(AgentId::root(id), reopened).await?);
        let jobs = JobManager::restore(store.clone(), &records).await?;
        let runtime = SessionRuntime::build(self.inner.clone(), store, jobs, &records).await?;
        let resumed = stopped.map_or(Resumed::Idle, Resumed::Parked);
        runtime.settle_interrupted_work(&records).await?;
        runtime.start_root(AgentLaunch::Resume(resumed)).await
    }
}
impl SessionHandle {
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.root.session()
    }

    #[must_use]
    /// The session's journal, for hosts that record their own events beside the
    /// runtime's.
    pub fn store(&self) -> &crate::session::SessionStore {
        &self.runtime.store
    }

    pub fn root_agent(&self) -> &AgentId {
        &self.root
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

    /// Startup outcome of every configured MCP server.
    pub fn mcp_servers(&self) -> std::collections::BTreeMap<String, crate::mcp::McpServerStatus> {
        self.runtime.mcp.servers()
    }

    /// The modes the root agent can be sent into: the configured ones, except that a
    /// mode the session has used keeps the definition it was used under.
    pub fn modes(&self) -> &indexmap::IndexMap<ModeName, crate::tool::policy::Mode> {
        &self.runtime.modes
    }

    /// What sending the root agent into `mode` would grant it.
    pub fn mode_capabilities(&self, mode: &ModeName) -> Option<crate::tool::policy::CapabilitySet> {
        let depth = self.runtime.available_depth(&self.root);
        let granted = self.runtime.mode_capabilities(mode).ok()?;
        Some(granted.for_agent(depth))
    }

    /// Model configuration, instruction, and skill diagnostics that must not
    /// write directly to a terminal.
    pub fn warnings(&self) -> &[String] {
        &self.runtime.harness.warnings
    }

    pub fn directory(&self) -> &Path {
        self.runtime.store.directory()
    }

    /// Name the session after its first prompt; a session already titled keeps
    /// its title.
    pub async fn set_title(&self, title: String) -> Result<(), HarnessError> {
        let titled = |records: &[EventRecord]| {
            (records.iter()).any(|record| matches!(record.event, SessionEvent::TitleSet { .. }))
        };
        let store = &self.runtime.store;
        if !store
            .visit_records_after(RecordSeq::default(), titled)
            .await
        {
            store
                .append(
                    self.root.clone(),
                    SessionEvent::TitleSet {
                        title,
                        source: TitleSource::Prompt,
                    },
                )
                .await?;
        }
        Ok(())
    }

    /// Give the session the user's title, or with `None` return it to its
    /// automatic title.
    pub async fn rename(&self, title: Option<String>) -> Result<(), HarnessError> {
        let event = match title {
            Some(title) => SessionEvent::TitleSet {
                title,
                source: TitleSource::User,
            },
            None => SessionEvent::TitleCleared,
        };
        self.runtime.store.append(self.root.clone(), event).await?;
        Ok(())
    }

    /// The title the session list shows.
    pub async fn title(&self) -> Result<Option<SessionTitle>, HarnessError> {
        Ok(self.runtime.store.title().await?)
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

    /// Host inspection is not bound to any turn: interrupting the agent never
    /// aborts a page the host is reading.
    pub async fn inspect_output(
        &self,
        query: crate::job::JobOutputQuery,
    ) -> Result<serde_json::Value, crate::tool::ToolError> {
        self.runtime
            .jobs
            .inspect_output(query, CancellationToken::new(), &self.runtime.capabilities)
            .await
    }

    /// Inspect output and hydrate eligible automatic capture text.
    pub async fn inspect_output_with_captures(
        &self,
        query: crate::job::JobOutputQuery,
    ) -> Result<crate::job::PresentedOutput, crate::tool::ToolError> {
        self.runtime
            .jobs
            .inspect_output_with_captures(
                query,
                CancellationToken::new(),
                &self.runtime.capabilities,
            )
            .await
    }

    /// Selectable saved-output pointers one level below `parent`, from its child
    /// at `index`, unaffected by presentation-only wrappers.
    pub async fn inspect_output_fields(
        &self,
        job: JobId,
        parent: crate::job::FieldPointer,
        index: usize,
    ) -> Result<crate::job::OutputFields, crate::tool::ToolError> {
        self.runtime
            .jobs
            .inspect_output_fields(job, parent, index)
            .await
    }

    pub async fn cancel_job(
        &self,
        job: JobId,
    ) -> Result<crate::job::JobEnvelope, crate::job::JobError> {
        self.runtime.jobs.cancel(job).await
    }

    #[must_use]
    pub fn tools(&self) -> &ToolRegistry {
        self.runtime.executor.registry()
    }

    pub async fn prompt(&self, text: impl Into<String>) -> Result<String, HarnessError> {
        self.prompt_with_options(text, &[], Selection::default())
            .await
    }

    /// `continue_turn_with` the active selection, returning the answer (empty if none).
    pub async fn continue_turn(&self) -> Result<String, HarnessError> {
        let outcome = self.continue_turn_with(Selection::default()).await?;
        Ok(outcome.answer.unwrap_or_default())
    }

    /// Continue a failed or interrupted turn without duplicating its input,
    /// optionally on another model (a refusal is deterministic for a given
    /// request). Retained interrupted children restart first; a root still waiting
    /// on them is left alone and woken by their ordinary completion.
    pub async fn continue_turn_with(
        &self,
        options: Selection,
    ) -> Result<ContinueOutcome, HarnessError> {
        self.admit_selection(&options)?;
        // An immediate resume must not miss children still journaling.
        self.runtime.settle_interrupts().await;
        // Admitted before closing begins, or refused before anything changes.
        // Closing waits for what this restarts to be running, so no longer.
        let admitted = self.runtime.jobs.admit_resumption();
        let admitted = admitted.ok_or(HarnessError::AgentStopped)?;
        // A held wait resumes with its children. Mark it busy before they restart,
        // while its turn cannot move on.
        let agents: Vec<_> = (self.runtime.agents().iter())
            .map(|(agent, live)| (agent.clone(), live.control.clone()))
            .collect();
        for (agent, control) in agents {
            if control.turn_from(&[TurnState::Held], TurnState::Busy) {
                let waiting = crate::agent::AgentActivity::WaitingChildren;
                self.runtime.activity(&agent, waiting);
            }
        }
        let jobs = &self.runtime.jobs;
        let children_resumed = jobs.continue_resumable_children(&admitted).await?;
        drop(admitted);
        let root = self
            .runtime
            .agents()
            .get(&self.root)
            .map(|live| live.control.clone());
        let parked = root.filter(|root| root.turn() == TurnState::Parked);
        let holding = self.runtime.jobs.live_work(&self.root).await.holding;
        if let Some(root) = &parked
            && holding.is_some()
        {
            // A cancelled turn still waiting on restarted children ends with them.
            root.set_turn(TurnState::Busy);
            self.runtime
                .activity(&self.root, crate::agent::AgentActivity::WaitingChildren);
        }
        if parked.is_some() && holding.is_none() {
            // An independently failed root has no live wait to preserve; continue
            // it after scheduling descendant recovery.
            let selection_applied = options.model.is_some() || options.mode.is_some();
            let answer = self.submit(Vec::new(), options).await?;
            return Ok(ContinueOutcome {
                answer: Some(answer),
                children_resumed,
                selection_applied,
            });
        }
        // Child-only recovery deliberately leaves a live/waiting root request
        // untouched. Its normal delivery path observes the replacement result.
        // A model or mode change cannot apply here: only a continued root turn adopts
        // one, so report it as unapplied rather than dropping it silently.
        Ok(ContinueOutcome {
            answer: None,
            children_resumed,
            selection_applied: false,
        })
    }

    /// Execute a JavaScript workflow through the session's registered `script` tool.
    pub async fn run_script(
        &self,
        source: impl Into<String>,
    ) -> Result<crate::tool::ToolOutput, HarnessError> {
        // A host script is the root agent's work: it runs under the root's current mode.
        let root = self
            .runtime
            .agents()
            .get(&self.root)
            .map(|root| root.capabilities.clone());
        let capabilities = root.ok_or(HarnessError::AgentStopped)?;
        let result = self
            .runtime
            .executor
            .clone()
            .with_capabilities(capabilities)
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
        options: Selection,
    ) -> Result<String, HarnessError> {
        let content = self
            .prepare_prompt(text.into(), attachments, &options)
            .await?;
        self.runtime.redirect(&self.root, true).await;
        self.submit(content, options).await
    }

    pub(super) async fn prepare_prompt(
        &self,
        text: String,
        attachments: &[crate::media::Attachment],
        options: &Selection,
    ) -> Result<Vec<UserPart>, HarnessError> {
        use crate::media::Attachment;
        self.admit_selection(options)?;
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
        let mut content = vec![UserPart::Text { text }];
        for attachment in attachments {
            let attachment = self.runtime.store.store_attachment(attachment).await?;
            content.push(UserPart::Attachment { attachment });
        }
        Ok(content)
    }

    /// The selection a message or continued turn can carry: a `provider/model` the
    /// harness has and one of this session's modes, each omitted to retain the
    /// active one. Rejected here, before anything is stored.
    pub fn selection(
        &self,
        model: Option<&ModelRef>,
        mode: Option<&ModeName>,
    ) -> Result<Selection, HarnessError> {
        let runtime = self.runtime.instance;
        let model = match model {
            Some(name) if !self.runtime.harness.models.contains_key(name) => {
                return Err(HarnessError::UnknownModel(name.clone()));
            }
            Some(name) => Some(SessionModel {
                runtime,
                name: name.clone(),
            }),
            None => None,
        };
        let mode = match mode {
            Some(name) if self.runtime.modes.contains_key(name) => Some(SessionMode {
                runtime,
                name: name.clone(),
            }),
            Some(name) => return Err(HarnessError::UnknownMode(name.clone())),
            None => None,
        };
        Ok(Selection { model, mode })
    }

    /// A selection another runtime instance issued, even for this session before a
    /// resume, proves nothing about this instance's catalog.
    fn admit_selection(&self, options: &Selection) -> Result<(), HarnessError> {
        let runtime = self.runtime.instance;
        if let Some(model) = &options.model
            && model.runtime != runtime
        {
            return Err(HarnessError::UnknownModel(model.name.clone()));
        }
        if let Some(mode) = &options.mode
            && mode.runtime != runtime
        {
            return Err(HarnessError::UnknownMode(mode.name.clone()));
        }
        Ok(())
    }

    async fn submit(
        &self,
        content: Vec<UserPart>,
        options: Selection,
    ) -> Result<String, HarnessError> {
        let (done_tx, done_rx) = oneshot::channel();
        self.root_tx
            .send(AgentCommand::Input {
                options,
                content,
                done: Some(RequestCompletion::Root(done_tx)),
            })
            .await
            .map_err(|_| HarnessError::AgentStopped)?;
        done_rx.await.map_err(|_| HarnessError::AgentStopped)?
    }

    /// Stop runtime producers and drain their accepted work. The journal stays
    /// open so a host can record its final status after observing shutdown errors,
    /// then [`close`](Self::close) it.
    pub async fn shutdown(&self) -> Result<(), HarnessError> {
        // What resumed before this is running work; nothing resumes after it.
        self.runtime.jobs.close_resumption().await;
        self.runtime
            .shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
        // Interruptions stay resumable. One accepted just before closing may still
        // be unwinding, and must not be taken for running work and cancelled.
        self.runtime.conclude_interrupts().await;
        self.runtime
            .interrupt_tree(&self.root, CancelScope::Running)
            .await;
        // Completed children retain idle loops for resumption, and must also stop.
        let agents = (self.runtime.agents().iter())
            .map(|(id, agent)| (id.clone(), agent.sender.clone()))
            .collect::<Vec<_>>();
        for (agent, sender) in &agents {
            // Interruptions stay resumable, so a turn held on one is released
            // rather than cancelled.
            self.runtime.jobs.release_held(agent).await;
            let _ = sender.send(AgentCommand::Shutdown).await;
        }
        self.runtime.jobs.cancel_and_drain().await?;
        for (_, sender) in agents {
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

    /// Close the journal of a session already shut down, so it can be opened
    /// again at once rather than when every handle has dropped.
    pub async fn close(&self) -> Result<(), HarnessError> {
        Ok(self.runtime.store.close().await?)
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
    use crate::session::tests::{attempt, requested};

    #[tokio::test]
    async fn shutdown_drains_before_final_host_status_without_closing_journal() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, Arc::new(HangingProvider)).await;
        let session = harness.new_session().await.unwrap();
        let result = session.run_script("console.log('completed'); return 42;");
        assert_eq!(result.await.unwrap().value["value"], 42);
        session.shutdown().await.unwrap();
        // Hosts record final status after shutdown so drain errors are not hidden.
        let status = session.record_status(session.root.clone(), "Completed".into());
        status.await.unwrap();
        let durable = SessionStore::read_records(&sessions, session.id()).await;
        let durable = durable.unwrap();
        assert!(matches!(&durable.last().unwrap().event,
            SessionEvent::Status { message } if message == "Completed"));
        assert_eq!(session.runtime.store.records().await, durable);
    }

    /// A crash can leave an attempt without an outcome and a committed call
    /// without a result. Resume settles both once, before any agent runs.
    #[tokio::test]
    async fn resume_settles_interrupted_attempts_and_calls_exactly_once() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [answer("first")]);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("hello").await.unwrap(), "first");
        let records = session.runtime.store.records().await;
        let context = events!(&records, SessionEvent::ModelContext { .. } => ());
        assert_eq!(context.len(), 1);
        let context = records
            .iter()
            .find(|record| matches!(record.event, SessionEvent::ModelContext { .. }))
            .unwrap()
            .sequence;
        let (id, root_agent) = (session.id(), session.root.clone());
        shutdown_session(session).await;
        // What a process killed mid-turn leaves behind.
        let (store, _) = SessionStore::open(&sessions, id).await.unwrap();
        let call = ToolCall::new("orphan", "read", json!({"path": "file"})).unwrap();
        let assistant = Message::Assistant(vec![AssistantItem::tool_call("orphan", 0, call)]);
        store
            .append(
                root_agent.clone(),
                SessionEvent::MessageCommitted { message: assistant },
            )
            .await
            .unwrap();
        let request = store.append(root_agent.clone(), requested(context)).await;
        let request = request.unwrap();
        let attempt = attempt(request.sequence.request(), 1);
        store.append(root_agent, attempt).await.unwrap();
        drop(store);

        for resume in 0..2 {
            let resumed = harness.resume_session(id).await.unwrap();
            let records = resumed.runtime.store.records().await;
            let interrupted = events!(&records,
                SessionEvent::ModelAttemptInterrupted(attempt) => (attempt.request, attempt.attempt));
            assert_eq!(interrupted, [(request.sequence.request(), 1)]);
            let results = events!(&records,
                SessionEvent::MessageCommitted { message: Message::Tool(results) } => results.clone());
            assert_eq!(results.len(), 1);
            assert_eq!(
                (results[0][0].call_id.as_str(), results[0][0].is_error),
                ("orphan", true)
            );
            // The settled turn stays continuable on every resume, not only the
            // one that settled it.
            assert_eq!(turn(&resumed, &resumed.root), TurnState::Parked, "{resume}");
            shutdown_session(resumed).await;
        }
    }

    /// A crash during a retry backoff, or between a request and its first attempt,
    /// leaves a request waiting for an attempt that never comes. Resume journals its
    /// reopening, then settles the request as interrupted, once, like an open
    /// attempt. What it closes, a running job too, is dated when the crashed process
    /// last did something, so reopening leaves the session's last activity alone,
    /// and so does closing it again with the job still interrupted.
    #[tokio::test]
    async fn resume_settles_requests_left_waiting_for_an_attempt() {
        use crate::session::{AttemptRef, EventRecord, RequestLedger, RequestPhase, RequestSeq};
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [answer("first")]);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("hello").await.unwrap(), "first");
        let records = session.runtime.store.records().await;
        let context = records
            .iter()
            .find(|record| matches!(record.event, SessionEvent::ModelContext { .. }))
            .unwrap()
            .sequence;
        let root_agent = session.root.clone();
        let id = session.id();
        shutdown_session(session).await;
        let settled = |records: &[EventRecord], request: RequestSeq| {
            let mut ledger = RequestLedger::default();
            for record in records {
                ledger.observe(record);
            }
            ledger.get(request).unwrap().phase.clone()
        };
        let interrupted = |records: &[EventRecord]| count!(records, SessionEvent::AgentInterrupted);
        // The one reopening, on the root, is journaled ahead of what resume settles.
        let reopened_after = |records: &[EventRecord], last: &EventRecord| {
            let resumed =
                &records[records.partition_point(|record| record.sequence <= last.sequence)..];
            assert_eq!(count!(resumed, SessionEvent::SessionReopened), 1);
            assert!(matches!(resumed[0].event, SessionEvent::SessionReopened));
            assert_eq!(resumed[0].agent, root_agent);
        };

        // What a killed process leaves behind: a running job, and a request failed
        // once, with the retry scheduled but never started.
        let (store, _) = SessionStore::open(&sessions, id).await.unwrap();
        let job = crate::identity::JobId::new(1).unwrap();
        use crate::job::JobTransition::{AwaitingApproval, Running};
        let created = SessionEvent::JobCreated {
            job,
            parent: None,
            origin: None,
            tool: "exec".into(),
            role: crate::job::JobRole::Tool,
            name: None,
            arguments: serde_json::json!({}),
            output_schema: None,
            accepts_input: false,
            background: false,
            location: crate::execution::ExecutionLocation::root(root.path().to_owned()),
        };
        let started =
            [AwaitingApproval, Running].map(|state| SessionEvent::JobStateChanged { job, state });
        let running = [created].into_iter().chain(started);
        let running = running.map(|event| (root_agent.clone(), event)).collect();
        store.append_all(running).await.unwrap();
        let retrying = store
            .append(root_agent.clone(), requested(context))
            .await
            .unwrap();
        let attempt = AttemptRef {
            request: retrying.sequence.request(),
            attempt: 1,
        };
        let started = SessionEvent::ModelAttemptStarted(attempt);
        store.append(root_agent.clone(), started).await.unwrap();
        let lost = "connection lost".into();
        let failed = SessionEvent::ModelFailed {
            attempt,
            failure: crate::agent::Failure::Provider(
                lost,
                crate::provider::ProviderErrorKind::Transport,
            ),
        };
        let failed = store.append(root_agent.clone(), failed).await.unwrap();
        let scheduled = SessionEvent::ModelRecoveryScheduled {
            failure: failed.sequence,
            delay_millis: 60_000,
        };
        let last = store.append(root_agent.clone(), scheduled).await.unwrap();
        let before = interrupted(&store.records().await);
        drop(store);
        let last_activity = async || {
            let summary = SessionStore::summary(&sessions, id).await.unwrap();
            summary.last_millis
        };
        assert_eq!(last_activity().await, last.timestamp_millis);
        // Reopen strictly later, so a repair dated now would show.
        bounded(async {
            while chrono::Utc::now().timestamp_millis() <= last.timestamp_millis {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let resumed = harness.resume_session(id).await.unwrap();
        let records = resumed.runtime.store.records().await;
        let request = retrying.sequence.request();
        let cut = |attempt| RequestPhase::Interrupted {
            attempt,
            at: last.timestamp_millis,
        };
        assert_eq!(settled(&records, request), cut(Some(1)));
        assert_eq!(interrupted(&records), before + 1);
        reopened_after(&records, &last);
        let repairs = records.iter().filter(|record| {
            record.sequence > last.sequence
                && matches!(
                    record.event,
                    SessionEvent::JobFinished { .. } | SessionEvent::AgentInterrupted
                )
        });
        let repaired = repairs
            .map(|record| record.timestamp_millis)
            .collect::<Vec<_>>();
        assert_eq!(repaired, [last.timestamp_millis; 2]);
        assert_eq!(last_activity().await, last.timestamp_millis);
        shutdown_session(resumed).await;
        assert_eq!(last_activity().await, last.timestamp_millis);

        // Requested, and never attempted.
        let (store, _) = SessionStore::open(&sessions, id).await.unwrap();
        let waiting = store
            .append(root_agent.clone(), requested(context))
            .await
            .unwrap();
        let before = interrupted(&store.records().await);
        drop(store);
        let resumed = harness.resume_session(id).await.unwrap();
        let records = resumed.runtime.store.records().await;
        let request = waiting.sequence.request();
        let cut = RequestPhase::Interrupted {
            attempt: None,
            at: waiting.timestamp_millis,
        };
        assert_eq!(settled(&records, request), cut);
        assert_eq!(interrupted(&records), before + 1);
        reopened_after(&records, &waiting);
        shutdown_session(resumed).await;

        // Nothing left to settle: an attempt its agent's interruption closed without
        // an outcome of its own is settled already, so a further resume settles
        // nothing, yet the interrupted turn can still be continued.
        let (store, _) = SessionStore::open(&sessions, id).await.unwrap();
        let closed = store
            .append(root_agent.clone(), requested(context))
            .await
            .unwrap();
        let attempt = AttemptRef {
            request: closed.sequence.request(),
            attempt: 1,
        };
        let started = SessionEvent::ModelAttemptStarted(attempt);
        store.append(root_agent.clone(), started).await.unwrap();
        let stopped = SessionEvent::AgentInterrupted;
        store.append(root_agent.clone(), stopped).await.unwrap();
        let before = interrupted(&store.records().await);
        drop(store);
        let resumed = harness.resume_session(id).await.unwrap();
        assert_eq!(interrupted(&resumed.runtime.store.records().await), before);
        assert_eq!(turn(&resumed, &resumed.root), TurnState::Parked);
        shutdown_session(resumed).await;
    }

    /// A session whose root is held on a child it interrupted mid-response. The
    /// child's retry answers next, then the root's continuation.
    async fn held_on_interrupted_child(
        root: &Path,
    ) -> (
        Harness,
        SessionHandle,
        Requests,
        tokio::task::JoinHandle<Result<String, HarnessError>>,
        JobId,
    ) {
        let child = json!({"prompt": "child task", "depth": 0});
        let steps = [
            Step::new(response(vec![tool_call(0, "agent-0", "agent", child)])),
            Step::new(Vec::new()).midstream(),
            Step::new(answer("child recovered")),
            Step::new(answer("root continued")),
        ];
        let requests = Requests::default();
        let provider = Script::new(steps, &requests);
        let harness = test_harness(root, &root.join("sessions"), provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let parent = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("delegate").await }
        });
        provider.request(1).await;
        root_waiting(&session).await;
        assert_eq!(session.interrupt().await, 2);
        assert_eq!(turn(&session, &session.root), TurnState::Held);
        let job = session.inspect_jobs(&session.root).await[0].id;
        (harness, session, requests, parent, job)
    }

    /// Closing ends running work, not an interruption, even one closing follows
    /// at once: a turn held on an interrupted child is released, and when the
    /// session reopens the child resumes and its result reaches the parent.
    #[tokio::test(start_paused = true)]
    async fn shutdown_leaves_interrupted_children_resumable() {
        use crate::job::JobState;
        let root = tempfile::tempdir().unwrap();
        let (harness, session, requests, parent, job) =
            held_on_interrupted_child(root.path()).await;
        let id = session.id();
        shutdown_session(session).await;
        assert!(bounded(parent).await.unwrap().is_err());

        let resumed = harness.resume_session(id).await.unwrap();
        let state = resumed.runtime.jobs.snapshot(job).await.unwrap().state;
        assert_eq!(state, JobState::Interrupted);
        resumed.runtime.jobs.send(job, json!("more")).await.unwrap();
        until(&resumed, job, |job| job.state == JobState::Completed).await;
        assert_eq!(resumed.continue_turn().await.unwrap(), "root continued");
        let last = requests.lock().unwrap().last().unwrap().request.clone();
        assert!(format!("{last:?}").contains("child recovered"));
        shutdown_session(resumed).await;
    }

    /// Closing a session whose interruption has settled, with nothing left
    /// running, leaves its last activity where it was.
    #[tokio::test(start_paused = true)]
    async fn closing_a_settled_interruption_is_not_activity() {
        use crate::job::JobState;
        let root = tempfile::tempdir().unwrap();
        let (_harness, session, _requests, parent, job) =
            held_on_interrupted_child(root.path()).await;
        until(&session, job, |job| job.state == JobState::Interrupted).await;
        let sessions = root.path().join("sessions");
        let id = session.id();
        let last_activity = async || {
            SessionStore::summary(&sessions, id)
                .await
                .unwrap()
                .last_millis
        };
        let before = last_activity().await;
        // Close strictly later, so an entry dated now would show.
        bounded(async {
            while chrono::Utc::now().timestamp_millis() <= before {
                tokio::task::yield_now().await;
            }
        })
        .await;
        shutdown_session(session).await;
        assert!(bounded(parent).await.unwrap().is_err());
        assert_eq!(last_activity().await, before);
    }

    /// Closing stops agents in no particular order. A child stopping before the
    /// turn held on it is released leaves that turn releasable.
    #[tokio::test(start_paused = true)]
    async fn shutdown_releases_a_turn_whose_interrupted_child_stopped_first() {
        use crate::job::JobState;
        let root = tempfile::tempdir().unwrap();
        let (_harness, session, _requests, parent, job) =
            held_on_interrupted_child(root.path()).await;
        until(&session, job, |job| job.state == JobState::Interrupted).await;
        let child = session.root.child(1);
        let sender = session.runtime.agent_sender(&child).unwrap();
        (session.runtime.shutting_down).store(true, std::sync::atomic::Ordering::Release);
        sender.send(AgentCommand::Shutdown).await.unwrap();
        bounded(sender.closed()).await;
        shutdown_session(session).await;
        assert!(bounded(parent).await.unwrap().is_err());
    }

    /// A continue waiting for an interruption to publish while closing publishes
    /// it finds the session closed: it journals nothing, and the interruption
    /// resumes when the session reopens.
    #[tokio::test(start_paused = true)]
    async fn a_continue_pending_across_shutdown_leaves_the_interruption_resumable() {
        use crate::job::JobState;
        let child_task = json!({"prompt": "child task", "depth": 0});
        let delegate = Step::new(response(vec![tool_call(0, "agent-0", "agent", child_task)]));
        let recovered = (0..4).map(|_| Step::new(answer("recovered")));
        let steps = [delegate, Step::new(Vec::new()).midstream()];
        let provider = Script::new(steps.into_iter().chain(recovered), &Requests::default());
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let parent = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("delegate").await }
        });
        provider.request(1).await;
        root_waiting(&session).await;
        let job = session.inspect_jobs(&session.root).await[0].id;
        // The child's interruption publishes only once its turn completes.
        let child = session.root.child(1);
        let gate = session.runtime.agents()[&child].control.invocation.clone();
        let completing = gate.lock().await;
        assert_eq!(session.interrupt().await, 2);
        let mut continued = Box::pin(session.continue_turn());
        assert!(futures_util::poll!(continued.as_mut()).is_pending());
        let shutdown = tokio::spawn({
            let session = session.clone();
            async move { session.shutdown().await }
        });
        until(&session, job, |job| job.state == JobState::Interrupted).await;
        drop(completing);
        bounded(shutdown).await.unwrap().unwrap();
        assert!(bounded(parent).await.unwrap().is_err());
        let closed = session.runtime.store.records().await;
        let refused = bounded(continued).await;
        assert!(
            matches!(refused, Err(HarnessError::AgentStopped)),
            "{refused:?}"
        );
        assert_eq!(session.runtime.store.records().await, closed);
        session.close().await.unwrap();

        let resumed = harness.resume_session(session.id()).await.unwrap();
        let state = resumed.runtime.jobs.snapshot(job).await.unwrap().state;
        assert_eq!(state, JobState::Interrupted);
        bounded(resumed.continue_turn()).await.unwrap();
        assert_eq!(terminal(&resumed, job).await.state, JobState::Completed);
        shutdown_session(resumed).await;
    }

    /// Closing frees the session for another open while its handle lives on, and
    /// work still winding down behind it can no longer write.
    #[tokio::test]
    async fn a_closed_session_reopens_while_its_handle_lives() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, Arc::new(HangingProvider)).await;
        let session = harness.new_session().await.unwrap();
        session.shutdown().await.unwrap();
        session.close().await.unwrap();
        let resumed = harness.resume_session(session.id()).await.unwrap();
        let late = session.record_status(session.root.clone(), "late".into());
        assert!(late.await.is_err());
        shutdown_session(resumed).await;
    }

    /// A turn that failed or was interrupted before the session closed reopens
    /// stopped for that reason and is continued; a finished one leaves nothing to
    /// continue.
    #[tokio::test(start_paused = true)]
    async fn reopened_failed_or_interrupted_turn_continues() {
        use crate::agent::{AgentActivity, Failure, TurnFailure};
        let stopped = async |session: &SessionHandle| {
            let activity = session.observe().await.snapshot.activity;
            match &activity[&session.root].state {
                AgentActivity::Stopped(failure) => Some(failure.clone()),
                _ => None,
            }
        };
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let failure = crate::provider::ProviderErrorKind::Protocol.error("not retried");
        let steps = [
            Step::fail(failure),
            Step::new(answer("recovered")),
            Step::new(answer("never released")).midstream(),
            Step::new(answer("continued")),
        ];
        let script = Script::new(steps, &requests);
        let harness = test_harness(root.path(), &sessions, script.clone()).await;
        let session = harness.new_session().await.unwrap();
        let id = session.id();
        session.prompt("fails").await.unwrap_err();
        shutdown_session(session).await;

        let resumed = harness.resume_session(id).await.unwrap();
        let failed = stopped(&resumed).await;
        assert!(matches!(
            failed,
            Some(TurnFailure::Failed(Failure::Provider(..)))
        ));
        assert_eq!(resumed.continue_turn().await.unwrap(), "recovered");
        let interrupted = resumed.prompt("interrupted");
        let interrupt = async {
            script.held(2).await;
            resumed.interrupt().await;
        };
        let (result, _) = tokio::join!(interrupted, interrupt);
        result.unwrap_err();
        shutdown_session(resumed).await;

        let resumed = harness.resume_session(id).await.unwrap();
        assert_eq!(stopped(&resumed).await, Some(TurnFailure::Interrupted));
        assert_eq!(resumed.continue_turn().await.unwrap(), "continued");
        shutdown_session(resumed).await;

        let resumed = harness.resume_session(id).await.unwrap();
        assert_eq!(stopped(&resumed).await, None);
        assert_eq!(resumed.continue_turn().await.unwrap(), "");
        assert_eq!(requests.lock().unwrap().len(), 4);
        shutdown_session(resumed).await;
    }

    /// A selection proves membership in the runtime instance that issued it; the
    /// same session resumed is another instance with its own catalog.
    #[tokio::test]
    async fn selections_do_not_survive_a_resume() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [answer("first"), answer("second")]);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let session = harness.new_session().await.unwrap();
        let model = session.runtime.harness.default_model.clone();
        let stale = session.selection(Some(&model), None).unwrap();
        let id = session.id();
        shutdown_session(session).await;
        let resumed = harness.resume_session(id).await.unwrap();
        let rejected = resumed.prompt_with_options("hello", &[], stale).await;
        assert!(matches!(rejected, Err(HarnessError::UnknownModel(name)) if name == model));
        let fresh = resumed.selection(Some(&model), None).unwrap();
        let answered = resumed.prompt_with_options("hello", &[], fresh).await;
        assert_eq!(answered.unwrap(), "first");
        shutdown_session(resumed).await;
    }

    /// A resumed agent keeps its journaled prompt, tools and capabilities. Live
    /// configuration narrows them: a pinned tool it no longer allows is shown but
    /// never executed, a newly allowed capability is not granted, and a lower
    /// depth limit applies.
    #[tokio::test]
    async fn resumed_agents_keep_their_journaled_contract_and_config_only_narrows_it() {
        use crate::tool::policy::{Capability, CapabilitySet};
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [answer("first")]);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let session = harness.new_session().await.unwrap();
        session.prompt("hello").await.unwrap();
        let id = session.id();
        shutdown_session(session).await;
        let original = requests.lock().unwrap()[0].clone();

        let mut narrowed = CapabilitySet::default();
        narrowed.remove(Capability::Write);
        narrowed.insert(Capability::Targets);
        let write = json!({"path": "must-not-exist", "content": "unsafe"});
        let responses = [
            response(vec![tool_call(0, "write", "write", write)]),
            answer("done"),
        ];
        let provider = scripted_provider(&requests, responses);
        let resumed = test_builder(root.path(), &sessions, provider, false)
            .capabilities(narrowed)
            // A lowered depth limit narrows the resumed root instead of refusing it.
            .max_child_depth(0)
            .build()
            .await
            .unwrap()
            .resume_session(id)
            .await
            .unwrap();
        assert_eq!(resumed.prompt("write it").await.unwrap(), "done");
        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured[1].tools, original.tools);
        assert_eq!(captured[1].system, original.system);
        let Some(Sent::Tool(results)) = captured[2].history.last() else {
            panic!("expected the write result");
        };
        assert!(results[0].is_error);
        assert!(
            results[0]
                .result
                .to_string()
                .contains("unavailable in this session")
        );
        assert!(!root.path().join("must-not-exist").exists());
        let records = resumed.runtime.store.records().await;
        assert_eq!(count!(&records, SessionEvent::ModelChanged { .. }), 0);
        shutdown_session(resumed).await;
    }

    /// A mode decides the root agent's capabilities, tools and instructions from the
    /// input that selects it. A child gets what its parent holds when it starts.
    #[tokio::test(start_paused = true)]
    async fn modes_switch_the_root_contract_and_survive_resume() {
        use crate::tool::policy::{Capability, CapabilitySet, Mode};
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let mode = |capabilities: &[Capability], instructions: Option<&str>| Mode {
            capabilities: capabilities.to_vec(),
            instructions: instructions.map(|text| text.parse().unwrap()),
            hint: None,
        };
        let look = mode(&[Capability::Read, Capability::Agents], Some("Only look."));
        // The session's ceiling has no targets, so no mode grants them. A mode is a
        // set however it is listed.
        let work = [
            Capability::Write,
            Capability::Read,
            Capability::Agents,
            Capability::Targets,
            Capability::Read,
        ];
        let modes: indexmap::IndexMap<_, _> = [
            (mode_name("look"), look),
            (mode_name("work"), mode(&work, None)),
        ]
        .into();
        let builder = |provider| {
            test_builder(root.path(), &sessions, provider, false)
                .capabilities(CapabilitySet::default())
                .modes(modes.clone())
        };
        let delegate = tool_call(0, "delegate", "agent", json!({"prompt": "work"}));
        let responses = [
            response(vec![delegate]),
            answer("child done"),
            answer("first"),
            answer("second"),
            answer("third"),
        ];
        let provider = scripted_provider(&requests, responses);
        let session = builder(provider)
            .build()
            .await
            .unwrap()
            .new_session()
            .await
            .unwrap();
        assert_eq!(session.prompt("delegate").await.unwrap(), "first");
        let payload = json!({"signature": "bound to the look conversation"});
        let signed = envelope(
            ReplayFormat::Messages,
            "test",
            payload,
            Binding::Conversation,
        );
        // Reasoning rides a turn that says something; alone it would be no turn.
        let signed = AssistantItem::reasoning("signed", 0, "visible", Some(signed));
        let signed = Message::Assistant(vec![signed, AssistantItem::text("t", 1, "noted")]);
        session.runtime.commit(&session.root, signed).await.unwrap();
        let work = session.selection(None, Some(&mode_name("work"))).unwrap();
        let prompt = session.prompt_with_options("write", &[], work.clone());
        assert_eq!(prompt.await.unwrap(), "second");
        let prompt = session.prompt_with_options("again", &[], work);
        assert_eq!(prompt.await.unwrap(), "third");
        let unknown = session.selection(None, Some(&mode_name("missing")));
        assert!(
            matches!(unknown, Err(HarnessError::UnknownMode(mode)) if mode == mode_name("missing"))
        );

        let captured = requests.lock().unwrap().clone();
        let offers = |request: &ModelRequest, tool: &str| {
            request
                .tools
                .iter()
                .any(|definition| definition.name == tool)
        };
        let prompt = |request: &ModelRequest| request.system[0].text.clone();
        let (looking, child, working) = (&captured[0], &captured[1], &captured[3]);
        assert!(prompt(looking).contains("<mode name=\"look\">\nOnly look.\n</mode>"));
        assert!(offers(looking, "read") && !offers(looking, "write"));
        // The child inherits the narrowed set, but a mode's instructions are the root's.
        assert!(!prompt(child).contains("<mode") && !offers(child, "write"));
        assert!(!prompt(working).contains("<mode") && offers(working, "write"));
        assert_eq!(captured[4].system, working.system);
        // The switch replaced the conversation its signed reasoning was bound to.
        let replays = |request: &ModelRequest| {
            let items = request.messages().filter_map(|message| match message {
                Sent::Assistant(items) => Some(items),
                _ => None,
            });
            let signed = items.flatten().find(|item| item.id().as_str() == "signed");
            signed.unwrap().replay().is_some()
        };
        assert!(!replays(working) && !replays(&captured[4]));

        let records = session.runtime.store.records().await;
        let changes = events!(&records, SessionEvent::ModeChanged { mode, capabilities } => (mode.clone(), capabilities.clone()));
        let granted = [
            Capability::Read,
            Capability::Write,
            Capability::Agents,
            Capability::Interactive,
        ];
        // The first use of a mode pins the definition it was used under.
        let pinned = |name: &str| {
            let mut definition = modes[name].clone();
            definition.capabilities.sort();
            definition.capabilities.dedup();
            crate::session::ModeSelection {
                name: mode_name(name),
                definition: Some(definition),
            }
        };
        assert_eq!(changes, [(pinned("work"), granted.to_vec())]);
        let started = events!(&records, SessionEvent::AgentStarted { mode, .. } => mode.clone());
        assert_eq!(started, [Some(pinned("look")), None]);
        let id = session.id();
        shutdown_session(session).await;

        // The launch's default mode does not replace the journaled one.
        let rejected =
            crate::provider::ProviderErrorKind::InvalidRequest.error("scripted rejection");
        let steps = [
            Step::new(answer("resumed")),
            Step::fail(rejected),
            Step::new(answer("continued")),
        ];
        let provider = Script::new(steps, &requests);
        let harness = builder(provider)
            .mode(mode_name("look"))
            .build()
            .await
            .unwrap();
        let resumed = harness.resume_session(id).await.unwrap();
        assert_eq!(resumed.prompt("continue").await.unwrap(), "resumed");
        let captured = requests.lock().unwrap().clone();
        let last = captured.last().unwrap();
        assert_eq!(
            (&last.system, &last.tools),
            (&working.system, &working.tools)
        );
        let records = resumed.runtime.store.records().await;
        assert_eq!(count!(&records, SessionEvent::ModeChanged { .. }), 1);

        // Continuing a failed turn can change the mode it continues in.
        assert!(resumed.prompt("fails").await.is_err());
        let look = resumed.selection(None, Some(&mode_name("look"))).unwrap();
        let outcome = resumed.continue_turn_with(look).await.unwrap();
        assert_eq!(outcome.answer.as_deref(), Some("continued"));
        assert!(outcome.selection_applied);
        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured.last().unwrap().system, looking.system);
        let records = resumed.runtime.store.records().await;
        assert_eq!(count!(&records, SessionEvent::ModeChanged { .. }), 2);
        shutdown_session(resumed).await;
    }

    /// A mode consumed at a request boundary narrows the root's next tool call and its
    /// host scripts, while a child it already started keeps what it was given.
    #[tokio::test(start_paused = true)]
    async fn a_mid_turn_mode_switch_narrows_the_root_but_not_its_running_child() {
        use crate::tool::policy::{Capability, Mode};
        let call = |id: &str, name: &str, arguments: serde_json::Value| {
            let call = ToolCall::new(id, name, arguments).unwrap();
            response(vec![AssistantItem::tool_call(id, 0, call)])
        };
        let write = |path: &str| json!({"path": path, "content": "written"});
        let launch = json!({"prompt": "child task", "model": "test/child", "bg": true});
        let done = || Step::new(answer("done")).model("root");
        let tracking = Script::new(
            [
                Step::new(call("launch", "agent", launch))
                    .model("root")
                    .gated(),
                Step::new(call("child-write", "write", write("child.txt")))
                    .model("child")
                    .gated(),
                Step::new(call("root-write", "write", write("root.txt")))
                    .model("root")
                    .gated(),
                Step::new(answer("child done")).model("child"),
                done().gated(),
                // The child's completion reaches the root as one more request.
                done(),
            ],
            &Default::default(),
        );
        let root = tempfile::tempdir().unwrap();
        let mode = |capabilities: &[Capability]| Mode {
            capabilities: capabilities.to_vec(),
            instructions: None,
            hint: None,
        };
        let work = mode(&[Capability::Read, Capability::Write, Capability::Agents]);
        let profile = |model: &str| ModelProfile {
            hint: Some(model.parse().unwrap()),
            ..crate::tests::profile(model, false)
        };
        let harness = HarnessBuilder::new(root.path())
            .session_root(root.path().join("sessions"))
            .provider(
                provider_name("test"),
                tracking.clone(),
                models([("root", profile("root")), ("child", profile("child"))]),
            )
            .default_model(model_ref("root"))
            .modes(
                [
                    (mode_name("work"), work),
                    (mode_name("look"), mode(&[Capability::Read])),
                ]
                .into(),
            )
            .build()
            .await
            .unwrap();
        let session = Arc::new(harness.new_session().await.unwrap());
        let turn = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("delegate, then write").await }
        });
        tracking.request(0).await;
        // Queued while the root's first request is in flight; consumed at its next one.
        let queued = QueuedPrompt {
            text: "only look from here".into(),
            attachments: Vec::new(),
            options: session.selection(None, Some(&mode_name("look"))).unwrap(),
            cancellation: Default::default(),
        };
        let mut receipt = Box::pin(enqueue_prompts(&session, vec![queued]));
        let sent = async || session.root_tx.capacity() < AGENT_CHANNEL_CAPACITY;
        pending_until(&mut receipt, sent).await;
        tracking.release(0);
        tracking.request(1).await;
        let narrowed = tracking.request(2).await;
        let offered: Vec<_> = narrowed
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(offered.contains(&"read"), "{offered:?}");
        assert!(
            !offered.contains(&"write") && !offered.contains(&"agent"),
            "{offered:?}"
        );
        assert!(bounded(receipt).await.iter().all(Result::is_ok));
        // The model calls `write` regardless: it is refused.
        tracking.release(2);
        tracking.request(4).await;
        assert!(!root.path().join("root.txt").exists());
        // The child started under `work` and still writes. Its reply and completion
        // are both pending before the root's answer, so they reach it together.
        tracking.release(1);
        let records = session.runtime.store.records().await;
        let child = events!(&records, SessionEvent::JobCreated { job, tool, .. } if tool == "agent" => *job)
            [0];
        terminal(&session, child).await;
        assert!(root.path().join("child.txt").exists());
        tracking.release(4);
        bounded(turn).await.unwrap().unwrap();
        let script =
            "return (await tool.write({path: 'script.txt', content: 'written'})).unwrap();";
        assert!(session.run_script(script).await.is_err());
        assert!(!root.path().join("script.txt").exists());
        let records = session.runtime.store.records().await;
        assert_eq!(count!(&records, SessionEvent::ModeChanged { .. }), 1);
        session.shutdown().await.unwrap();
    }

    /// A session never outgrows the ceiling it started with. A mode it has used keeps
    /// the definition it was used under; a mode new to it is pinned on first use.
    #[tokio::test(start_paused = true)]
    async fn resume_keeps_the_session_ceiling_and_its_pinned_modes() {
        use crate::tool::policy::{Capability, CapabilitySet, Mode};
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let mode = |capabilities: &[Capability], instructions: &str| Mode {
            capabilities: capabilities.to_vec(),
            instructions: Some(instructions.parse().unwrap()),
            hint: None,
        };
        let build = |ceiling: CapabilitySet, modes: &[(&str, Mode)], reply: &str| {
            let provider = scripted_provider(&requests, [answer(reply)]);
            let modes = modes.iter().cloned();
            test_builder(root.path(), &sessions, provider, false)
                .capabilities(ceiling)
                .modes(
                    modes
                        .map(|(name, mode)| (name.parse().unwrap(), mode))
                        .collect(),
                )
                .build()
        };
        let look = ("look", mode(&[Capability::Read], "Look."));
        let work = (
            "work",
            mode(&[Capability::Read, Capability::Write], "Work."),
        );
        let narrow: CapabilitySet = [Capability::Read].into_iter().collect();
        let harness = build(narrow, &[look.clone(), work.clone()], "one").await;
        let session = harness.unwrap().new_session().await.unwrap();
        session.prompt("start").await.unwrap();
        let id = session.id();
        shutdown_session(session).await;

        let in_mode = |session: &SessionHandle, mode: &str| {
            session.selection(None, Some(&mode_name(mode))).unwrap()
        };
        let changes = |records: &[EventRecord]| events!(records, SessionEvent::ModeChanged { mode, capabilities } => (mode.clone(), capabilities.clone()));
        // A wider live ceiling does not widen the session: `work` grants no write here.
        let wide = CapabilitySet::default;
        let harness = build(wide(), &[look.clone(), work.clone()], "two")
            .await
            .unwrap();
        let resumed = harness.resume_session(id).await.unwrap();
        let prompt = resumed.prompt_with_options("widen", &[], in_mode(&resumed, "work"));
        assert_eq!(prompt.await.unwrap(), "two");
        let first = crate::session::ModeSelection {
            name: "work".parse().unwrap(),
            definition: Some(work.1.clone()),
        };
        let records = resumed.runtime.store.records().await;
        assert_eq!(changes(&records), [(first.clone(), vec![Capability::Read])]);
        shutdown_session(resumed).await;

        // The configuration now redefines both modes and adds one. The session keeps
        // its own `look`, pins the new `extra` when a message first uses it, and keeps
        // that after it leaves the configuration again.
        let relook = ("look", mode(&[], "Changed."));
        let extra = ("extra", mode(&[Capability::Read], "Extra."));
        for (configured, selected, reply) in [
            (vec![relook.clone(), extra.clone()], "extra", "three"),
            (vec![relook.clone()], "look", "four"),
            (vec![relook], "extra", "five"),
        ] {
            let harness = build(wide(), &configured, reply).await.unwrap();
            let resumed = harness.resume_session(id).await.unwrap();
            let prompt = resumed.prompt_with_options("next", &[], in_mode(&resumed, selected));
            assert_eq!(prompt.await.unwrap(), reply);
            shutdown_session(resumed).await;
        }
        let (_store, records) = SessionStore::open(&sessions, id).await.unwrap();
        let named = |name: &str, definition: Option<&Mode>| crate::session::ModeSelection {
            name: mode_name(name),
            definition: definition.cloned(),
        };
        let read = vec![Capability::Read];
        let expected = [
            (first, read.clone()),
            (named("extra", Some(&extra.1)), read.clone()),
            (named("look", None), read.clone()),
            (named("extra", None), read),
        ];
        assert_eq!(changes(&records), expected);
        let captured = requests.lock().unwrap().clone();
        let prompt = |index: usize| captured[index].system[0].text.clone();
        assert!(prompt(3).contains("Look.") && !prompt(3).contains("Changed."));
        assert!(prompt(4).contains("Extra."));
    }

    /// A retained child accepts owner input after a restart: its loop starts again
    /// under its journaled contract and continues its own history.
    #[tokio::test(start_paused = true)]
    async fn retained_children_resume_after_the_session_restarts() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let delegate = tool_call(0, "delegate", "agent", json!({"prompt": "work"}));
        let responses = [
            response(vec![delegate]),
            answer("child done"),
            answer("root done"),
        ];
        let provider = scripted_provider(&requests, responses);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("delegate").await.unwrap(), "root done");
        let records = session.runtime.store.records().await;
        let job = events!(&records, SessionEvent::JobCreated { job, tool, .. } if tool == "agent" => *job)
            [0];
        let id = session.id();
        shutdown_session(session).await;

        let responses = [answer("child resumed"), answer("root again")];
        let provider = scripted_provider(&requests, responses);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let resumed = harness.resume_session(id).await.unwrap();
        resumed.runtime.jobs.send(job, json!("more")).await.unwrap();
        let finished = terminal(&resumed, job).await;
        assert_eq!(finished.state, crate::job::JobState::Completed);
        let captured = requests.lock().unwrap().clone();
        let child = captured
            .iter()
            .rev()
            .find(|request| request.context != ContextId::from(&resumed.root))
            .expect("the child made a request after the restart");
        let history = serde_json::to_string(&child.history).unwrap();
        assert!(history.contains("work") && history.contains("child done"));
        assert!(history.contains("Owner input"));
        shutdown_session(resumed).await;
    }

    /// A process killed while a foreground child works leaves the root's call
    /// open. Resume settles it with an error result, so nothing waits on the child
    /// any more; retry restarts the child and its answer reaches the root as an event.
    #[tokio::test(start_paused = true)]
    async fn retry_after_a_crash_delivers_a_retained_foreground_childs_answer() {
        for answered in [false, true] {
            crash_then_retry(answered).await;
        }
    }

    async fn crash_then_retry(answered: bool) {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [answer("first")]);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("hello").await.unwrap(), "first");
        // What a process killed while its foreground child worked leaves behind.
        let store = &session.runtime.store;
        let root_agent = session.root.clone();
        let call = ToolCall::new("delegate", "agent", json!({"prompt": "work"})).unwrap();
        let assistant = Message::Assistant(vec![AssistantItem::tool_call("delegate", 0, call)]);
        let launch = store
            .append(
                root_agent.clone(),
                SessionEvent::MessageCommitted { message: assistant },
            )
            .await
            .unwrap();
        let job = crate::identity::JobId::new(1).unwrap();
        let location = crate::execution::ExecutionLocation::root(root.path().to_owned());
        let events = vec![
            (
                root_agent.clone(),
                SessionEvent::JobCreated {
                    job,
                    parent: None,
                    origin: Some(crate::session::ModelCallOrigin {
                        message: launch.sequence.message(),
                        call_id: "delegate".into(),
                    }),
                    tool: "agent".into(),
                    role: crate::job::JobRole::Agent,
                    name: None,
                    arguments: json!({"prompt": "work"}),
                    output_schema: None,
                    accepts_input: true,
                    background: false,
                    location: location.clone(),
                },
            ),
            (
                root_agent.clone(),
                SessionEvent::JobStateChanged {
                    job,
                    state: crate::job::JobTransition::AwaitingApproval,
                },
            ),
            (
                root_agent.clone(),
                SessionEvent::JobStateChanged {
                    job,
                    state: crate::job::JobTransition::Running,
                },
            ),
        ];
        store.append_all(events).await.unwrap();
        let started = crate::session::tests::child_started(Some(job), location);
        store.append(root_agent.child(1), started).await.unwrap();
        if answered {
            // A second crash: the first resume answered the call and retry restarted
            // the child, which was working again when the process died.
            let result = crate::provider::protocol::ToolResult {
                call_id: "delegate".into(),
                name: "agent".into(),
                result: json!({"error": "interrupted while the session was not running"}),
                images: Vec::new(),
                is_error: true,
            };
            let events = vec![
                (
                    root_agent.clone(),
                    SessionEvent::JobFinished {
                        job,
                        state: crate::job::JobEnd::Interrupted,
                        diagnostic: Some(crate::tool::ToolError::interrupted().diagnostic()),
                        output_diagnostic: None,
                        images: Vec::new(),
                    },
                ),
                (
                    root_agent.clone(),
                    SessionEvent::MessageCommitted {
                        message: Message::Tool(vec![result]),
                    },
                ),
                (
                    root_agent.clone(),
                    SessionEvent::JobStateChanged {
                        job,
                        state: crate::job::JobTransition::Running,
                    },
                ),
            ];
            store.append_all(events).await.unwrap();
        }
        let id = session.id();
        shutdown_session(session).await;

        // Steps serve requests in arrival order; the root's continuation and the
        // restarted child race, so every answer reads the same.
        let steps = [(); 3].map(|()| Step::new(answer("recovered")));
        let provider = Script::new(steps, &requests);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let resumed = harness.resume_session(id).await.unwrap();
        assert!(resumed.runtime.jobs.is_background(job).await.unwrap());
        bounded(resumed.continue_turn()).await.unwrap();
        // The child's answer reaches the root as a job event.
        bounded(async {
            loop {
                let delivered = requests.lock().unwrap().iter().any(|request| {
                    let history = rendered(request);
                    history.contains("job_events") && history.contains("recovered")
                });
                if delivered {
                    break;
                }
                poll().await;
            }
        })
        .await;
        let records = resumed.runtime.store.records().await;
        assert_eq!(count!(&records, SessionEvent::AgentStarted { .. }), 2);
        shutdown_session(resumed).await;
    }

    #[tokio::test]
    async fn session_title_is_set_once_and_listed_without_decoding() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [answer("done")]);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let session = harness.new_session().await.unwrap();
        session.prompt("list me by my first prompt").await.unwrap();
        let summary = SessionStore::summary(&sessions, session.id())
            .await
            .unwrap();
        assert_eq!(
            summary.title.map(|title| title.text).as_deref(),
            Some("list me by my first prompt")
        );
        assert_eq!(summary.model, Some(model_ref("test")));
        session.set_title("first title".into()).await.unwrap();
        session.set_title("second title".into()).await.unwrap();
        let summary = SessionStore::summary(&sessions, session.id())
            .await
            .unwrap();
        assert_eq!(
            summary.title.map(|title| title.text).as_deref(),
            Some("first title")
        );
        let records = session.runtime.store.records().await;
        assert_eq!(summary.entries, records.len() as u64);
        assert_eq!(
            summary.last_millis,
            records.last().unwrap().timestamp_millis
        );
        shutdown_session(session).await;
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
        let usage = usage(48_000, 2_000, 3_914);
        completed.insert(completed.len() - 1, ResponseEvent::Usage(usage));
        let responses = [completed, answer(summary.to_string()), answer("resumed")];
        let provider = scripted_provider(&requests, responses);
        let profile = ModelProfile {
            max_context: crate::tests::limit(64_000),
            ..crate::tests::profile("test", false)
        };
        let harness = serving(root.path(), provider, [("test", profile)])
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let todos = &session.runtime.todos;
        let root_todos = vec![todo("Inspect queue", TodoStatus::InProgress)];
        todos
            .replace(&session.root, root_todos, None)
            .await
            .unwrap();
        let child = start_child(&session, 1, None).await;
        let child_todos = vec![todo("Independent child work", TodoStatus::InProgress)];
        todos
            .replace(&child, child_todos.clone(), None)
            .await
            .unwrap();
        // Exceed the retention tail; only the high-usage response triggers the checkpoint.
        let history = AssistantItem::text("history", 0, "research ".repeat(40_000));
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
                Sent::Assistant(items) if items == &vec![AssistantItem::text("answer", 0, "checkpoint installed")])));
            captured[0].clone()
        };
        let id = session.id();
        shutdown_session(session).await;

        let resumed = harness.resume_session(id).await.unwrap();
        let todos = &resumed.runtime.todos;
        let found = todos.inspect(&resumed.root, None).await.unwrap();
        assert_eq!(found, reconciled);
        let found = todos.inspect(&child, None).await.unwrap();
        assert_eq!(found, child_todos);
        assert_eq!(resumed.prompt("Continue.").await.unwrap(), "resumed");
        {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 3, "exactly one resumed provider invocation");
            let first = &captured[2];
            assert_eq!(first.history.first(), Some(&checkpoint.message.render()));
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

    #[tokio::test(start_paused = true)]
    async fn switching_models_preserves_image_history_and_reports_unsupported_images() {
        let root = tempfile::tempdir().unwrap();
        let image = crate::media::Attachment::Image {
            file: Some(root.path().join("sample.png")),
            image: crate::tests::png(b"image fixture"),
        };
        let requests = Requests::default();
        let responses = [answer("Image received"), answer("Image still present")];
        let provider = scripted_provider(&requests, responses);
        let vision = crate::tests::profile("vision-model", true);
        let harness = serving(root.path(), provider, [("vision", vision)])
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let model = |name: &str| {
            session
                .selection(Some(&name.parse().unwrap()), None)
                .unwrap()
        };
        let before = session.runtime.store.records().await.len();
        let oversized = crate::media::Attachment::Image {
            file: None,
            image: crate::tests::png(&vec![0; MAX_IMAGE_BYTES as usize]),
        };
        let oversized = [oversized];
        let rejected =
            session.prompt_with_options("Oversized image", &oversized, model("test/vision"));
        assert!(matches!(rejected.await, Err(HarnessError::ImageLimit)));
        assert_eq!(session.runtime.store.records().await.len(), before);
        let images = [image];
        let direct = session.prompt_with_options("Look", &images, Selection::default());
        let error = direct.await.unwrap_err();
        assert!(
            matches!(error, HarnessError::ImagesUnsupported(_)),
            "{error}"
        );
        assert_eq!(session.runtime.store.records().await.len(), before);
        let with_image = session.prompt_with_options("Look at this", &images, model("test/vision"));
        with_image.await.unwrap();
        let error = session.prompt_with_options("Go on", &[], model("test/test"));
        let error = error.await.unwrap_err();
        assert!(
            matches!(error, HarnessError::ImagesUnsupported(_)),
            "{error}"
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
        let again = session.prompt_with_options("Use vision again", &[], model("test/vision"));
        again.await.unwrap();
        assert!(
            requests.lock().unwrap()[1]
                .history
                .iter()
                .any(|message| matches!(message,
            Sent::User(parts) if parts.iter().any(SentPart::is_image)))
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn continuing_interrupted_turn_retains_input_once_and_shutdown_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        // The first response reports usage, then hangs mid-stream.
        let mut initial = answer("initial");
        let spent = usage(7, 0, 1);
        initial.insert(0, ResponseEvent::Usage(spent));
        let steps = [
            Step::new(initial).midstream(),
            Step::new(answer("jobs handled")),
        ];
        let provider = Script::new(steps, &requests);
        let harness =
            test_harness(root.path(), &root.path().join("sessions"), provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let task = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("retained input").await }
        });
        let mut events = session.runtime.events.observe().updates;
        provider.request(0).await;
        bounded(async {
            while !matches!(
                events.recv().await.unwrap().event,
                RuntimeEvent::ResponseEvent { .. }
            ) {}
        })
        .await;
        session.interrupt().await;
        assert!(task.await.unwrap().is_err());
        // The interrupted stream journals its usage and attempt, and commits nothing.
        let records = session.runtime.store.records().await;
        let recorded = events!(&records, SessionEvent::Usage { usage, .. } => *usage);
        assert_eq!(recorded, [spent]);
        let interrupted = count!(
            &records,
            SessionEvent::ModelAttemptInterrupted(crate::session::AttemptRef { attempt: 1, .. })
        );
        assert_eq!((interrupted, assistant_commits(&records)), (1, 0));
        let status = session.record_status(session.root.clone(), "Interrupted".into());
        status.await.unwrap();
        assert_eq!(session.continue_turn().await.unwrap(), "jobs handled");
        let records = session.runtime.store.records().await;
        let retained = count!(&records, SessionEvent::MessageCommitted { message: Message::User(blocks) }
            if blocks.iter().any(|block| matches!(block, UserPart::Text { text } if text == "retained input")));
        let interrupted =
            count!(&records, SessionEvent::Status { message } if message == "Interrupted");
        assert_eq!((retained, interrupted), (1, 1));
        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured.len(), 2);
        assert!(captured[0].messages().eq(captured[1].messages()));
        session.shutdown().await.unwrap();
        session.shutdown().await.unwrap();
    }

    /// Continue resumes every holder's wait, not only the root's: a nested holder's
    /// resumed turn is busy again, so a second interrupt stops it.
    #[tokio::test(start_paused = true)]
    async fn continue_resumes_nested_holders_so_a_second_interrupt_stops_them() {
        let delegate = |id: &str, depth: u32| {
            let arguments = json!({"prompt": format!("{id} task"), "depth": depth});
            response(vec![tool_call(0, id, "agent", arguments)])
        };
        let steps = [
            Step::new(delegate("child", 1)),
            Step::new(delegate("grandchild", 0)),
            Step::new(Vec::new()).midstream(),
            Step::new(answer("grandchild recovered")),
            Step::new(Vec::new()).midstream(),
            Step::new(answer("child recovered")),
            Step::new(answer("parent done")),
        ];
        let requests = Requests::default();
        let provider = Script::new(steps, &requests);
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let parent = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("delegate").await }
        });
        provider.request(2).await;
        // The grandchild is interrupted; the child and the root holding it keep waiting.
        assert_eq!(session.interrupt().await, 3);
        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "");
        provider.request(4).await;
        let child = session.root.child(1);
        assert_eq!(turn(&session, &child), TurnState::Busy);
        // The child's resumed turn is in a model request, so it is interrupted; the
        // root holding it shows interrupted again.
        assert_eq!(session.interrupt().await, 2);
        assert_eq!(turn(&session, &child), TurnState::Parked);
        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "");
        assert_eq!(bounded(parent).await.unwrap().unwrap(), "parent done");
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_job_finishing_after_an_interrupt_waits_for_the_next_input() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let steps = [
            Step::new(answer("initial")).midstream(),
            Step::new(answer("jobs handled")),
        ];
        let provider = Script::new(steps, &requests);
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let task = tokio::spawn({
            let session = session.clone();
            async move { session.prompt("start").await }
        });
        provider.request(0).await;
        session.interrupt().await;
        assert!(task.await.unwrap().is_err());
        // Background work owned by the interrupted root completes afterwards.
        let arguments = json!({"source": "return 1", "bg": true});
        let executor = &session.runtime.executor;
        let running = executor.execute(session.root.clone(), "script", arguments, None);
        let job = running.await.unwrap().job;
        bounded(session.runtime.jobs.wait_settled(job))
            .await
            .unwrap();
        // Queue the completion's wake ahead of the continue: had it woken the
        // interrupted root, that turn would spend the scripted answer.
        session.root_tx.send(AgentCommand::JobsReady).await.unwrap();
        // The completion reaches the model with the next request instead.
        assert_eq!(session.continue_turn().await.unwrap(), "jobs handled");
        let captured = requests.lock().unwrap().clone();
        let delivered = captured[1].messages().any(|message| {
            matches!(message, Sent::User(blocks) if blocks.iter().any(|block|
                matches!(block, SentPart::Runtime { text } if text.starts_with("<skyhook_job_events>"))))
        });
        assert!(captured.len() == 2 && delivered);
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
      accepted.push((await tool.job({id}).send({{value}})).unwrap());
    }}
    const pending = await tool.jobs({{job:{id}}});
    return {{accepted, state:pending.state}};
    "#
            ))
            .await
            .unwrap();
        let accepted = json!([null, null, null]);
        assert_eq!(queued.value["value"]["accepted"], accepted);
        assert_eq!(queued.value["value"]["state"], "running");
        let last = format!("return tool.job({id}).send({{value:\"last\"}});");
        session.run_script(last).await.unwrap();
        jobs.wait(running, None, true).await.unwrap();
        let output = format!("return tool.jobs({{job:{id}}});");
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
        // The cancel result is the target's view, with no denial code.
        let cancel = format!(
            "await tool.jobs({{job:{id}}}); return (await tool.job({id}).cancel()).unwrap().meta.code === undefined;"
        );
        assert_eq!(
            session.run_script(cancel).await.unwrap().value["value"],
            true
        );
        jobs.wait(waiting, None, true).await.unwrap();
        let output = format!("return tool.jobs({{job:{id}}});");
        let cancelled = session.run_script(output).await.unwrap();
        assert_eq!(cancelled.value["value"]["state"], "cancelled");
    }
}
