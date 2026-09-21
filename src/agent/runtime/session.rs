//! Public session submission, inspection, and shutdown API.

use super::*;

impl Harness {
    pub async fn new_session(&self) -> Result<SessionHandle, HarnessError> {
        let store = SessionStore::create(&self.inner.session_root).await?;
        let runtime = SessionRuntime::build(self.inner.clone(), store, Vec::new()).await?;
        runtime.start_root(None).await
    }

    pub async fn resume_session(&self, id: SessionId) -> Result<SessionHandle, HarnessError> {
        let (store, records) = SessionStore::open(&self.inner.session_root, id).await?;
        let root = AgentId::root(id);
        let selection = crate::session::agent_selection(&records, &root);
        let runtime = SessionRuntime::build(self.inner.clone(), store, records).await?;
        let interrupted = runtime.settle_interrupted_work().await?;
        let session = runtime.start_root(selection).await?;
        // Starting an agent marks it idle; a turn settled here can be continued.
        for agent in &interrupted {
            session
                .runtime
                .activity(agent, crate::agent::AgentActivity::Interrupted);
        }
        Ok(session)
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
    pub fn mcp_servers(&self) -> &std::collections::BTreeMap<String, crate::mcp::McpServerStatus> {
        self.runtime.mcp.servers()
    }

    /// The modes the root agent can be sent into: the configured ones, except that a
    /// mode the session has used keeps the definition it was used under.
    pub fn modes(&self) -> &indexmap::IndexMap<String, crate::tool::policy::Mode> {
        &self.runtime.modes
    }

    /// What sending the root agent into `mode` would grant it.
    pub fn mode_capabilities(&self, mode: &str) -> Option<crate::tool::policy::CapabilitySet> {
        let depth = self.runtime.available_depth(&self.root);
        let granted = self.runtime.mode_capabilities(mode).ok()?;
        Some(granted.for_agent(depth))
    }

    /// Host skill diagnostics that must not write directly to a terminal.
    pub fn warnings(&self) -> &[String] {
        self.runtime.harness.skills.warnings()
    }

    pub fn directory(&self) -> &Path {
        self.runtime.store.directory()
    }

    /// Name the session once; later titles leave the first in place.
    pub async fn set_title(&self, title: String) -> Result<(), HarnessError> {
        let records = self.runtime.store.records().await;
        if records
            .iter()
            .any(|record| matches!(record.event, SessionEvent::TitleSet { .. }))
        {
            return Ok(());
        }
        self.runtime
            .store
            .append(self.root.clone(), SessionEvent::TitleSet { title })
            .await?;
        Ok(())
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
            .inspect_output(query, &self.runtime.capabilities)
            .await
    }

    /// Inspect output and hydrate eligible automatic capture text.
    pub async fn inspect_output_with_captures(
        &self,
        query: crate::job::JobOutputQuery,
    ) -> Result<crate::job::PresentedOutput, crate::tool::ToolError> {
        self.runtime
            .jobs
            .inspect_output_with_captures(query, &self.runtime.capabilities)
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

    #[must_use]
    pub fn tools(&self) -> &ToolRegistry {
        self.runtime.executor.registry()
    }

    pub async fn prompt(&self, text: impl Into<String>) -> Result<String, HarnessError> {
        self.prompt_with_options(text, &[], PromptOptions::default())
            .await
    }

    /// `continue_turn_with` default options, returning the answer (empty if none).
    pub async fn continue_turn(&self) -> Result<String, HarnessError> {
        let outcome = self.continue_turn_with(ContinueOptions::default()).await?;
        Ok(outcome.answer.unwrap_or_default())
    }

    /// Continue a failed or interrupted turn without duplicating its input,
    /// optionally on another model profile (a refusal is deterministic for a given
    /// request). Retained interrupted children restart first; a root still waiting
    /// on them is left alone and woken by their ordinary completion.
    pub async fn continue_turn_with(
        &self,
        options: ContinueOptions,
    ) -> Result<ContinueOutcome, HarnessError> {
        if let Some(model) = &options.model
            && !self.runtime.harness.model_profiles.contains_key(model)
        {
            return Err(HarnessError::UnknownModelProfile(model.clone()));
        }
        if let Some(mode) = &options.mode
            && !self.runtime.modes.contains_key(mode)
        {
            return Err(HarnessError::UnknownMode(mode.clone()));
        }
        let ContinueOptions { model, mode } = options;
        // An immediate resume must not miss children still journaling.
        self.runtime.settle_interrupts().await;
        let children_resumed = self.runtime.jobs.continue_resumable_children().await?;
        let root_retryable = self.runtime.events.retryable(&self.root);
        let holding = self.runtime.jobs.live_work(&self.root).await.holding;
        if root_retryable && holding.is_some() {
            // The root's wait resumes with its restarted children.
            self.runtime
                .activity(&self.root, crate::agent::AgentActivity::WaitingChildren);
        }
        if root_retryable && holding.is_none() {
            // An independently failed root has no live wait to preserve; continue
            // it after scheduling descendant recovery.
            let selection_applied = model.is_some() || mode.is_some();
            let options = PromptOptions { model, mode };
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
        options: PromptOptions,
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
        options: &PromptOptions,
    ) -> Result<Vec<UserContent>, HarnessError> {
        use crate::media::Attachment;
        if let Some(model) = &options.model
            && !self.runtime.harness.model_profiles.contains_key(model)
        {
            return Err(HarnessError::UnknownModelProfile(model.clone()));
        }
        if let Some(mode) = &options.mode
            && !self.runtime.modes.contains_key(mode)
        {
            return Err(HarnessError::UnknownMode(mode.clone()));
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
        options: PromptOptions,
    ) -> Result<String, HarnessError> {
        let (done_tx, done_rx) = oneshot::channel();
        self.root_tx
            .send(AgentCommand::Input {
                options,
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
        self.runtime.interrupt_tree(&self.root).await;
        // Completed children retain idle loops for resumption, and must also stop.
        let senders = self
            .runtime
            .agents()
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
        let store = &session.runtime.store;
        let records = store.records().await;
        let context = events!(&records, SessionEvent::ModelContext { .. } => ());
        assert_eq!(context.len(), 1);
        let context = records
            .iter()
            .find(|record| matches!(record.event, SessionEvent::ModelContext { .. }))
            .unwrap()
            .sequence;
        // What a process killed mid-turn leaves behind.
        let call = ToolCall::new("orphan", "read", json!({"path": "file"})).unwrap();
        let assistant = Message::Assistant(vec![AssistantContent::tool_call("orphan", 0, call)]);
        let root_agent = session.root.clone();
        store
            .append(
                root_agent.clone(),
                SessionEvent::MessageCommitted { message: assistant },
            )
            .await
            .unwrap();
        let request = SessionEvent::ModelRequested {
            context,
            history: Vec::new(),
            tail: Vec::new(),
            history_lifetime: Default::default(),
            purpose: crate::session::ModelPurpose::Agent,
        };
        let request = store.append(root_agent.clone(), request).await.unwrap();
        let attempt = SessionEvent::ModelAttemptStarted {
            request: request.sequence,
            attempt: 1,
        };
        store.append(root_agent, attempt).await.unwrap();
        let id = session.id();
        shutdown_session(session).await;

        for resume in 0..2 {
            let resumed = harness.resume_session(id).await.unwrap();
            let records = resumed.runtime.store.records().await;
            let interrupted = events!(&records,
                SessionEvent::ModelAttemptInterrupted { request, attempt } => (*request, *attempt));
            assert_eq!(interrupted, [(request.sequence, 1)]);
            let results = events!(&records,
                SessionEvent::MessageCommitted { message: Message::Tool(results) } => results.clone());
            assert_eq!(results.len(), 1);
            assert_eq!(
                (results[0][0].call_id.as_str(), results[0][0].is_error),
                ("orphan", true)
            );
            // The resume that settles the turn can continue it.
            assert_eq!(resumed.runtime.events.retryable(&resumed.root), resume == 0);
            shutdown_session(resumed).await;
        }
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
        let Some(Message::Tool(results)) = captured[2].history.last() else {
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
    #[tokio::test]
    async fn modes_switch_the_root_contract_and_survive_resume() {
        use crate::tool::policy::{Capability, CapabilitySet, Mode};
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let mode = |capabilities: &[Capability], instructions: Option<&str>| Mode {
            capabilities: capabilities.to_vec(),
            instructions: instructions.map(str::to_owned),
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
            ("look".to_owned(), look),
            ("work".to_owned(), mode(&work, None)),
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
        let in_mode = |mode: &str| PromptOptions {
            mode: Some(mode.to_owned()),
            ..Default::default()
        };
        assert_eq!(session.prompt("delegate").await.unwrap(), "first");
        let signed = crate::provider::protocol::ReplayEnvelope {
            version: 1,
            protocol: "test".into(),
            model: "test".into(),
            scope: String::new(),
            payload: json!({"signature": "bound to the look conversation"}),
            conversation_bound: true,
        };
        let signed = AssistantContent::reasoning("signed", 0, "visible", Some(signed));
        let signed = Message::Assistant(vec![signed]);
        session.runtime.commit(&session.root, signed).await.unwrap();
        let prompt = session.prompt_with_options("write", &[], in_mode("work"));
        assert_eq!(prompt.await.unwrap(), "second");
        let prompt = session.prompt_with_options("again", &[], in_mode("work"));
        assert_eq!(prompt.await.unwrap(), "third");
        let unknown = session
            .prompt_with_options("no", &[], in_mode("missing"))
            .await;
        assert!(matches!(unknown, Err(HarnessError::UnknownMode(mode)) if mode == "missing"));

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
                Message::Assistant(items) => Some(items),
                _ => None,
            });
            let signed = items.flatten().find(|item| item.id == "signed").cloned();
            signed.unwrap().replay.is_some()
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
                name: name.to_owned(),
                definition: Some(definition),
            }
        };
        assert_eq!(changes, [(pinned("work"), granted.to_vec())]);
        let started = events!(&records, SessionEvent::AgentStarted { mode, .. } => mode.clone());
        assert_eq!(started, [Some(pinned("look")), None]);
        let id = session.id();
        shutdown_session(session).await;

        // The launch's default mode does not replace the journaled one.
        let rejected = crate::provider::ProviderError {
            kind: crate::provider::ProviderErrorKind::InvalidRequest,
            message: "scripted rejection".into(),
            retry_after: None,
        };
        let steps = [
            Step::new(answer("resumed")),
            Step::fail(rejected),
            Step::new(answer("continued")),
        ];
        let provider = Script::new(steps, &requests);
        let harness = builder(provider).mode("look").build().await.unwrap();
        let resumed = harness.resume_session(id).await.unwrap();
        assert_eq!(resumed.prompt("continue").await.unwrap(), "resumed");
        let captured = requests.lock().unwrap().clone();
        let last = captured.last().unwrap();
        assert_eq!(
            (&last.system, &last.tools),
            (&working.system, &working.tools)
        );
        let records = resumed.runtime.store.records().await;
        let mode = crate::session::agent_mode(&records, &resumed.root);
        assert_eq!(mode.as_deref(), Some("work"));
        assert_eq!(count!(&records, SessionEvent::ModeChanged { .. }), 1);

        // Continuing a failed turn can change the mode it continues in.
        assert!(resumed.prompt("fails").await.is_err());
        let in_mode = |mode: &str| ContinueOptions {
            mode: Some(mode.to_owned()),
            ..Default::default()
        };
        let unknown = resumed.continue_turn_with(in_mode("missing")).await;
        assert!(matches!(unknown, Err(HarnessError::UnknownMode(_))));
        let outcome = resumed.continue_turn_with(in_mode("look")).await.unwrap();
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
    #[tokio::test]
    async fn a_mid_turn_mode_switch_narrows_the_root_but_not_its_running_child() {
        use crate::tool::policy::{Capability, Mode};
        let call = |id: &str, name: &str, arguments: serde_json::Value| {
            let call = ToolCall::new(id, name, arguments).unwrap();
            response(vec![AssistantContent::tool_call(id, 0, call)])
        };
        let write = |path: &str| json!({"path": path, "content": "written"});
        let launch = json!({"prompt": "child task", "model": "child", "bg": true});
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
            hint: Some(model.to_owned()),
            ..ModelProfile::new("test", model, None, 128_000, 4096, false)
        };
        let harness = HarnessBuilder::new(root.path())
            .session_root(root.path().join("sessions"))
            .provider("test", tracking.clone())
            .model_profile("root", profile("root"))
            .model_profile("child", profile("child"))
            .default_model_profile("root")
            .modes(
                [
                    ("work".to_owned(), work),
                    ("look".to_owned(), mode(&[Capability::Read])),
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
            options: PromptOptions {
                mode: Some("look".into()),
                ..Default::default()
            },
            cancellation: Default::default(),
        };
        let receipt = tokio::spawn({
            let session = session.clone();
            async move { enqueue_prompts(&session, vec![queued]).await }
        });
        // A spawned enqueue proves nothing: observe the root's mailbox.
        bounded(async {
            while session.root_tx.capacity() == AGENT_CHANNEL_CAPACITY {
                tokio::task::yield_now().await;
            }
        })
        .await;
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
        assert!(bounded(receipt).await.unwrap().iter().all(Result::is_ok));
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
    #[tokio::test]
    async fn resume_keeps_the_session_ceiling_and_its_pinned_modes() {
        use crate::tool::policy::{Capability, CapabilitySet, Mode};
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let mode = |capabilities: &[Capability], instructions: &str| Mode {
            capabilities: capabilities.to_vec(),
            instructions: Some(instructions.to_owned()),
            hint: None,
        };
        let build = |ceiling: CapabilitySet, modes: &[(&str, Mode)], reply: &str| {
            let provider = scripted_provider(&requests, [answer(reply)]);
            let modes = modes.iter().cloned();
            test_builder(root.path(), &sessions, provider, false)
                .capabilities(ceiling)
                .modes(modes.map(|(name, mode)| (name.to_owned(), mode)).collect())
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

        let in_mode = |mode: &str| PromptOptions {
            mode: Some(mode.to_owned()),
            ..Default::default()
        };
        let changes = |records: &[EventRecord]| events!(records, SessionEvent::ModeChanged { mode, capabilities } => (mode.clone(), capabilities.clone()));
        // A wider live ceiling does not widen the session: `work` grants no write here.
        let wide = CapabilitySet::default;
        let harness = build(wide(), &[look.clone(), work.clone()], "two")
            .await
            .unwrap();
        let resumed = harness.resume_session(id).await.unwrap();
        let prompt = resumed.prompt_with_options("widen", &[], in_mode("work"));
        assert_eq!(prompt.await.unwrap(), "two");
        let first = crate::session::ModeSelection {
            name: "work".into(),
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
            let prompt = resumed.prompt_with_options("next", &[], in_mode(selected));
            assert_eq!(prompt.await.unwrap(), reply);
            shutdown_session(resumed).await;
        }
        let (_store, records) = SessionStore::open(&sessions, id).await.unwrap();
        let named = |name: &str, definition: Option<&Mode>| crate::session::ModeSelection {
            name: name.to_owned(),
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
    #[tokio::test]
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
            .find(|request| request.correlation.as_deref() != Some(&resumed.root.to_string()))
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
        let assistant = Message::Assistant(vec![AssistantContent::tool_call("delegate", 0, call)]);
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
                        message: launch.sequence,
                        call_id: "delegate".into(),
                    }),
                    tool: "agent".into(),
                    role: crate::job::JobRole::Agent,
                    name: None,
                    arguments: json!({"prompt": "work"}),
                    output_schema: None,
                    accepts_input: true,
                    background: false,
                    authorization_scope: None,
                    location: location.clone(),
                },
            ),
            (
                root_agent.clone(),
                SessionEvent::JobStateChanged {
                    job,
                    state: crate::job::JobState::Running,
                },
            ),
        ];
        store.append_all(events).await.unwrap();
        let started =
            crate::session::fixture::child_started(Some(root_agent.clone()), Some(job), location);
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
                        state: crate::job::JobState::Interrupted,
                        diagnostic: Some(crate::tool::ToolError::Interrupted.diagnostic()),
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
                        state: crate::job::JobState::Running,
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
                    history.contains("skyhook_job_events") && history.contains("recovered")
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
        assert_eq!(summary.title, None);
        assert_eq!(
            summary.preview.as_deref(),
            Some("list me by my first prompt")
        );
        assert_eq!(summary.model.as_deref(), Some("test"));
        session.set_title("first title".into()).await.unwrap();
        session.set_title("second title".into()).await.unwrap();
        let summary = SessionStore::summary(&sessions, session.id())
            .await
            .unwrap();
        assert_eq!(summary.title.as_deref(), Some("first title"));
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
        let usage = usage(48_000, 2_000, 1_200);
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
        let child = start_child(&session, 1, None).await;
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
        // The first response reports usage, then hangs mid-stream.
        let mut initial = answer("initial");
        let spent = usage(7, 0, 1);
        initial.insert(0, ResponseChunk::UsageUpdated { usage: spent });
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
            SessionEvent::ModelAttemptInterrupted { attempt: 1, .. }
        );
        assert_eq!((interrupted, assistant_commits(&records)), (1, 0));
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
        bounded(async {
            while session.runtime.jobs.has_running(&session.root).await {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(requests.lock().unwrap().len(), 1, "job {job} woke the root");
        // The completion reaches the model with the next request instead.
        assert_eq!(session.continue_turn().await.unwrap(), "jobs handled");
        let captured = requests.lock().unwrap().clone();
        let delivered = captured[1].messages().any(|message| {
            matches!(message, Message::User(blocks) if blocks.iter().any(|block|
                matches!(block, UserContent::Runtime { text } if text.contains("skyhook_job_events"))))
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
      accepted.push((await tool.job({id}).send({{value}})).result);
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
