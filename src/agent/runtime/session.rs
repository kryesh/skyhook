//! Public session submission, inspection, and shutdown API.

use super::*;
use crate::session::{SessionTitle, TitleSource};

impl Harness {
    pub async fn new_session(&self) -> Result<SessionHandle, HarnessError> {
        let store = SessionStore::create(&self.inner.session_root).await?;
        let jobs = JobManager::new(store.clone());
        let open = Arc::new(Open(store.clone(), jobs.supervisor().clone()));
        let runtime = SessionRuntime::build(self.inner.clone(), store, jobs, &[]).await?;
        runtime.start_root(runtime.new_root()?, open).await
    }
}
impl SessionHandle {
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.root.session()
    }

    pub fn root_agent(&self) -> &AgentId {
        &self.root
    }

    /// Observe without a gap between the initial snapshot and subsequent updates.
    #[must_use]
    pub fn observe(&self) -> Observation {
        self.runtime.store.observe()
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
        let depth = (self.runtime.delegation(&self.root)).map_or(0, |(_, depth)| depth);
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
        self.running().await?;
        let root = self.root.clone();
        let plan = move |state: &crate::session::CoreState| {
            let source = TitleSource::Prompt;
            let titled = (!state.titled).then(|| (root, SessionEvent::TitleSet { title, source }));
            let entries = titled.into_iter().collect();
            Ok::<_, SessionError>(Plan::journal(entries))
        };
        Ok(self.runtime.store.commit(Command::Journal, plan).await?)
    }

    /// Give the session the user's title, or with `None` return it to its
    /// automatic title.
    pub async fn rename(&self, title: Option<String>) -> Result<(), HarnessError> {
        self.running().await?;
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
        self.running().await?;
        self.runtime
            .store
            .append(agent, SessionEvent::Status { message })
            .await?;
        Ok(())
    }

    #[must_use]
    /// `agent`'s jobs, read as the session's observation reads them.
    pub fn inspect_jobs(&self, agent: &AgentId) -> Vec<crate::job::JobEnvelope> {
        let mut jobs = self.runtime.jobs.list(agent);
        let stopped = |state: &CoreState| state.phase.stopped_at(&state.records);
        if let Some(at) = self.runtime.store.read(stopped) {
            jobs.iter_mut().for_each(|job| {
                crate::agent::observation::read_job(job, at);
            });
        }
        jobs
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

    /// Inspect output and hydrate eligible automatic capture text: a running
    /// job's from each capture's last page.
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

    pub async fn cancel_job(&self, job: JobId) -> Result<crate::job::JobEnvelope, HarnessError> {
        self.running().await?;
        Ok(self.runtime.jobs.cancel(job).await?)
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
    /// request). Retained interrupted children restart first, and those stopped
    /// before they began begin; a root still waiting on them is left alone and
    /// woken by their ordinary completion.
    pub async fn continue_turn_with(
        &self,
        options: Selection,
    ) -> Result<ContinueOutcome, HarnessError> {
        self.outcome(self.continue_with(options).await)
    }

    async fn continue_with(&self, options: Selection) -> Result<ContinueOutcome, HarnessError> {
        self.admit_selection(&options)?;
        self.running().await?;
        // An immediate resume must not miss children still journaling.
        self.runtime.settle_interrupts().await;
        let (runtime, root) = (self.runtime.clone(), self.root.clone());
        let selection_applied = options.model.is_some() || options.mode.is_some();
        let (done, answer) = oneshot::channel();
        let restart = move |state: &CoreState| {
            let (running, restart) = runtime.jobs.continue_resumable(state);
            Ok::<_, HarnessError>(Plan::new(running, move |state, _| {
                state.agents.flow = Flow::Open;
                let resumed = restart(state);
                // A held wait resumes with its children: busy as they restart.
                let waiting = state.agents.turned(&Turn::Held);
                for agent in &waiting {
                    state
                        .agents
                        .get_mut(agent)
                        .expect("listed agents run")
                        .set_turn(Turn::Busy);
                }
                // A root that stopped short of its answer goes on from where it did.
                let parked = (state.agents.get(&root))
                    .is_some_and(|facts| matches!(facts.turn, Turn::Parked(_)));
                let input = AgentCommand::Input {
                    options,
                    content: Vec::new(),
                    done: Some(RequestCompletion::Root(done)),
                };
                let continued = (parked && state.agents.send(&root, input)).then_some(answer);
                (resumed, continued)
            }))
        };
        let continued = self.runtime.store.commit(Command::Continue, restart);
        let (children_resumed, continued) = continued.await?;
        // Child-only recovery deliberately leaves a live/waiting root request
        // untouched. Its normal delivery path observes the replacement result.
        // A model or mode change cannot apply here: only a continued root turn adopts
        // one, so report it as unapplied rather than dropping it silently.
        let Some(answer) = continued else {
            return Ok(ContinueOutcome {
                answer: None,
                children_resumed,
                selection_applied: false,
            });
        };
        let answer = answer.await.map_err(|_| HarnessError::AgentStopped)??;
        Ok(ContinueOutcome {
            answer: Some(answer),
            children_resumed,
            selection_applied,
        })
    }

    /// Execute a JavaScript workflow through the session's registered `script` tool.
    pub async fn run_script(
        &self,
        source: impl Into<String>,
    ) -> Result<crate::tool::ToolOutput, HarnessError> {
        let source = source.into();
        let run = async {
            self.running().await?;
            // A host script is the root agent's work: it runs under the root's current mode.
            let root = (self.runtime).agents(|agents| {
                agents.get(&self.root)?;
                Some(
                    self.runtime
                        .resumed_contract(agents, &self.root)?
                        .capabilities,
                )
            });
            let capabilities = root.ok_or_else(|| {
                (self.runtime.store).read(|state| super::refused_input(&state.phase))
            })?;
            let executor = self
                .runtime
                .executor
                .clone()
                .with_capabilities(capabilities);
            let source = json!({"source": source});
            let result = executor.execute(self.root.clone(), "script", source, None);
            Ok(result.await?.output)
        };
        self.outcome(run.await)
    }

    /// Submit a user message and wait for the resulting turn to finish.
    /// Explicit request-boundary enqueues may change the model during that turn.
    pub async fn prompt_with_options(
        &self,
        text: impl Into<String>,
        attachments: &[crate::media::Attachment],
        options: Selection,
    ) -> Result<String, HarnessError> {
        let submitted = async {
            let content = (self.prepare_prompt(text.into(), attachments, &options)).await?;
            let (done, answer) = oneshot::channel();
            let input = AgentCommand::Input {
                options,
                content,
                done: Some(RequestCompletion::Root(done)),
            };
            self.runtime.redirect(&self.root, input).await?;
            answer.await.map_err(|_| HarnessError::AgentStopped)?
        };
        self.outcome(submitted.await)
    }

    /// What a call the session's failure cut short returns: that failure,
    /// however the call itself ended.
    fn outcome<T>(&self, result: Result<T, HarnessError>) -> Result<T, HarnessError> {
        let failed = || self.runtime.store.read(|state| state.phase.failed());
        result.map_err(|error| failed().map_or_else(Into::into, |()| error))
    }

    /// Where the session is in its life.
    #[must_use]
    pub fn phase(&self) -> crate::session::SessionPhase {
        self.runtime.store.phase()
    }

    pub(super) async fn prepare_prompt(
        &self,
        text: String,
        attachments: &[crate::media::Attachment],
        options: &Selection,
    ) -> Result<Vec<UserPart>, HarnessError> {
        use crate::media::Attachment;
        self.admit_selection(options)?;
        self.running().await?;
        self.runtime.store.admits(Command::Submit)?;
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

    /// Stop runtime producers and drain their accepted work. The journal stays
    /// open so a host can record its final status after observing shutdown errors,
    /// then [`close`](Self::close) it.
    pub async fn shutdown(&self) -> Result<(), HarnessError> {
        // What started or resumed before this is running work; nothing does after
        // it. Closing stops the session as a crash would, in one commit: every turn
        // is interrupted, every child invocation still live ends interrupted and
        // stays resumable, the waits held on them are released, and only other
        // running work is cancelled.
        let (runtime, root) = (self.runtime.clone(), self.root.clone());
        let stop = move |state: &CoreState| {
            // A viewed session runs nothing, so it stops nothing.
            if !state.phase.runs() {
                return Ok(Plan::new(Vec::new(), |_, _| true));
            }
            let agents = state.agents.within(&root);
            let released = crate::job::release_records(&state.jobs.releases(&root, &agents));
            Ok::<_, SessionError>(Plan::new(released, move |state, _| {
                runtime.stop_turns(state, &root);
                drop(runtime.jobs.conclude(state, crate::job::Concluding::Every));
                runtime.jobs.cancel_running(state);
                let supervisor = runtime.jobs.supervisor().clone();
                state.defer(move || {
                    agents
                        .iter()
                        .for_each(|agent| supervisor.cancel_turn(agent))
                });
                false
            }))
        };
        let stopped = async {
            let viewed = (self.runtime.store)
                .commit(Command::BeginShutdown, stop)
                .await?;
            if viewed {
                return Ok(());
            }
            // Each loop ends as it finds its turn refused, and each cancellation settles.
            let quiescent = |state: &mut CoreState| state.agents.is_empty().then_some(());
            self.runtime.store.until(quiescent).await?;
            Ok::<_, HarnessError>(self.runtime.jobs.cancel_and_drain().await?)
        };
        // A failed session's work is already stopped; its connections still close.
        let stopped = stopped.await;
        self.runtime.mcp.shutdown().await;
        self.runtime.router.shutdown().await;
        stopped?;
        Ok(self.runtime.store.drain().await?)
    }

    /// Close the journal of a session already shut down, so it can be opened
    /// again at once rather than when every handle has dropped.
    pub async fn close(&self) -> Result<(), HarnessError> {
        Ok(self.runtime.store.close().await?)
    }

    /// Interrupt active turns while retaining child jobs for retry, and report
    /// how many turns and unbegun child invocations that stopped. Having stopped
    /// any, the session is paused: deliveries start no turn until `continue` or
    /// new input. Explicit `job_cancel` remains the non-resumable cancellation path.
    pub async fn interrupt(&self) -> usize {
        let (runtime, root) = (self.runtime.clone(), self.root.clone());
        let interrupt = move |state: &mut CoreState| runtime.stop_turns(state, &root);
        self.runtime.store.live(interrupt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;

    /// Once closing has begun, a host script is refused before its job exists,
    /// and input the root holds, or never takes, as the session's refusal, not
    /// as a stopped agent.
    #[tokio::test]
    async fn shutdown_drains_before_final_host_status_without_closing_journal() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, Arc::new(HangingProvider)).await;
        let session = harness.new_session().await.unwrap();
        let result = session.run_script("console.log('completed'); return 42;");
        assert_eq!(result.await.unwrap().value["value"], 42);
        let begin = Command::BeginShutdown;
        (session.runtime.store).enter(begin).await.unwrap();
        let closing = crate::session::SessionPhase::ShuttingDown;
        let refused = |result: Result<_, HarnessError>| matches!(result, Err(HarnessError::Session(SessionError::Refused(phase))) if phase == closing);
        assert!(refused(session.run_script("return 1;").await.map(drop)));
        // Once held by the root's loop, then never taken once it ended.
        for _ in 0..2 {
            assert!(refused(bounded(session.prompt("")).await.map(drop)));
        }
        session.shutdown().await.unwrap();
        // Hosts record final status after shutdown so drain errors are not hidden.
        let status = session.record_status(session.root.clone(), "Completed".into());
        status.await.unwrap();
        let durable = SessionStore::read_records(&sessions, session.id()).await;
        let durable = durable.unwrap();
        assert!(matches!(&durable.last().unwrap().event,
            SessionEvent::Status { message } if message == "Completed"));
        assert_eq!(session.runtime.store.records(), durable);
    }

    /// A caller that gives up on a new session, at any of its awaits, leaves it
    /// closed: a session that journaled nothing is removed, and one whose root
    /// started stays, released for another open.
    #[tokio::test]
    async fn a_new_session_given_up_on_is_closed() {
        use std::task::{Context, Poll, Wake, Waker};
        struct Woken(tokio::sync::Notify);
        impl Wake for Woken {
            fn wake(self: Arc<Self>) {
                self.0.notify_one();
            }
        }
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, Arc::new(HangingProvider)).await;
        let closed = async || loop {
            let mut open = 0;
            for entry in std::fs::read_dir(&sessions).into_iter().flatten() {
                let name = entry.unwrap().file_name();
                let id = name.to_str().unwrap().parse().unwrap();
                let records = SessionStore::read_records(&sessions, id).await;
                let journaled = records.is_ok_and(|records| !records.is_empty());
                if !journaled || SessionStore::is_open(&sessions, id).await.unwrap() {
                    open += 1;
                }
            }
            if open == 0 {
                return;
            }
            tokio::task::yield_now().await;
        };
        for awaits in 0.. {
            let woken = Arc::new(Woken(tokio::sync::Notify::new()));
            let waker = Waker::from(woken.clone());
            let mut start = Box::pin(harness.new_session());
            let mut started = None;
            for _ in 0..awaits {
                match start.as_mut().poll(&mut Context::from_waker(&waker)) {
                    Poll::Ready(session) => {
                        started = Some(session.unwrap());
                        break;
                    }
                    Poll::Pending => bounded(woken.0.notified()).await,
                }
            }
            drop(start);
            let finished = started.is_some();
            if let Some(session) = started {
                shutdown_session(session).await;
            }
            bounded(closed()).await;
            if finished {
                return;
            }
        }
    }

    /// A saved session's root grants a mode what its recorded depth allows within
    /// the current limit, the same while viewed as once running.
    #[tokio::test]
    async fn viewed_and_running_roots_grant_a_mode_the_same_delegation() {
        use crate::tool::policy::{Capability, Mode};
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let mode = mode_name("delegate");
        let harness = async |depth| {
            let delegate = Mode {
                capabilities: vec![Capability::Read, Capability::Agents],
                instructions: None,
                hint: None,
            };
            let provider = scripted_provider(&Requests::default(), []);
            (test_builder(root.path(), &sessions, provider, false))
                .max_child_depth(depth)
                .modes([(mode.clone(), delegate)].into_iter().collect())
                .build()
                .await
                .unwrap()
        };
        let session = harness(2).await.new_session().await.unwrap();
        let id = session.id();
        shutdown_session(session).await;
        for depth in [2, 0] {
            let reopened = harness(depth).await.resume_session(id).await.unwrap();
            let viewed = reopened.mode_capabilities(&mode).unwrap();
            assert_eq!(viewed.contains(Capability::Agents), depth > 0);
            assert_eq!(reopened.phase(), crate::session::SessionPhase::Viewing);
            reopened.running().await.unwrap();
            assert_eq!(reopened.mode_capabilities(&mode).unwrap(), viewed);
            shutdown_session(reopened).await;
        }
    }

    /// Dropping the last handle stops the session's work where it stands, as a
    /// crash would, a worker that ignores its cancellation included.
    #[tokio::test]
    async fn dropping_the_last_handle_stops_a_worker_that_ignores_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, Arc::new(HangingProvider)).await;
        let session = harness.new_session().await.unwrap();
        let jobs = session.runtime.jobs.clone();
        let spec = crate::job::JobSpec::test(session.root.clone(), "stubborn");
        let (_input, worker) = jobs.test_running(spec).await.split();
        let ignores = std::future::pending::<crate::job::JobOutcome>();
        worker.launch(ignores, "tool handler panicked");
        drop(session);
        bounded(jobs.supervisor().drain()).await;
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

        let records = session.runtime.store.records();
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
        let records = resumed.runtime.store.records();
        assert_eq!(count!(&records, SessionEvent::ModeChanged { .. }), 1);

        // Continuing a failed turn can change the mode it continues in.
        assert!(resumed.prompt("fails").await.is_err());
        let look = resumed.selection(None, Some(&mode_name("look"))).unwrap();
        let outcome = resumed.continue_turn_with(look).await.unwrap();
        assert_eq!(outcome.answer.as_deref(), Some("continued"));
        assert!(outcome.selection_applied);
        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured.last().unwrap().system, looking.system);
        let records = resumed.runtime.store.records();
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
        let turn = spawn_prompt(&session, "delegate, then write");
        tracking.request(0).await;
        // Queued while the root's first request is in flight; consumed at its next one.
        let queued = QueuedPrompt {
            text: "only look from here".into(),
            attachments: Vec::new(),
            options: session.selection(None, Some(&mode_name("look"))).unwrap(),
            cancellation: Default::default(),
        };
        let mut receipt = Box::pin(enqueue_prompts(&session, vec![queued]));
        let sent = async || inbox(&session, &session.root) > 0;
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
        let records = session.runtime.store.records();
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
        let records = session.runtime.store.records();
        assert_eq!(count!(&records, SessionEvent::ModeChanged { .. }), 1);
        session.shutdown().await.unwrap();
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
        let records = session.runtime.store.records();
        assert_eq!(summary.entries, records.len() as u64);
        assert_eq!(
            summary.last_millis,
            records.last().unwrap().timestamp_millis
        );
        shutdown_session(session).await;
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
        let before = session.runtime.store.records().len();
        let oversized = crate::media::Attachment::Image {
            file: None,
            image: crate::tests::png(&vec![0; MAX_IMAGE_BYTES as usize]),
        };
        let oversized = [oversized];
        let rejected =
            session.prompt_with_options("Oversized image", &oversized, model("test/vision"));
        assert!(matches!(rejected.await, Err(HarnessError::ImageLimit)));
        assert_eq!(session.runtime.store.records().len(), before);
        let images = [image];
        let direct = session.prompt_with_options("Look", &images, Selection::default());
        let error = direct.await.unwrap_err();
        assert!(
            matches!(error, HarnessError::ImagesUnsupported(_)),
            "{error}"
        );
        assert_eq!(session.runtime.store.records().len(), before);
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
        let task = spawn_prompt(&session, "retained input");
        let mut events = session.runtime.store.observe().updates;
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
        let records = session.runtime.store.records();
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
        let records = session.runtime.store.records();
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
    #[tokio::test]
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
        let parent = spawn_prompt(&session, "delegate");
        provider.request(2).await;
        // The grandchild is interrupted; the child and the root holding it keep waiting.
        assert_eq!(session.interrupt().await, 3);
        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "");
        provider.request(4).await;
        let child = session.root.child(1);
        assert_eq!(turn(&session, &child), Turn::Busy);
        // The child's resumed turn is in a model request, so it is interrupted; the
        // root holding it shows interrupted again.
        assert_eq!(session.interrupt().await, 2);
        assert_eq!(turn(&session, &child), INTERRUPTED);
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
        let task = spawn_prompt(&session, "start");
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
        // The completion does not wake the interrupted root, whose turn would spend
        // the scripted answer: it reaches the model with the next request instead.
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
