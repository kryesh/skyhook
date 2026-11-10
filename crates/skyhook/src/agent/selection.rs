//! Model and mode selection: adopting an input's selection, applying the configured
//! modes and what sending an agent into one changes, the system prompt and context
//! an agent opens with, and how the configuration has moved on from the model and
//! mode a reopened session recorded.

use super::{AgentContext, Boundary, HarnessError, Selection, SessionRuntime, TurnContext};
use crate::identity::AgentId;
use crate::provider::model::{ModelEntry, ModelRef};
use crate::provider::protocol::SystemSegment;
use crate::session::Message;
use crate::session::{AppliedMode, Command, CoreState, Plan, SessionEvent};
use crate::{config::mode::ModeName, tool::policy::CapabilitySet};

/// A resolved model and mode selection, journaled with the input it applies to.
pub(in crate::agent) struct Selected {
    pub(super) events: Vec<(AgentId, SessionEvent)>,
    pub(super) replacement: Option<AgentContext>,
    pub(super) mode: Option<ModeChange>,
}

impl Selected {
    /// Install what the journal now records: the replacement context and the
    /// mode with its grants.
    pub(super) fn install(self, turn: &mut TurnContext<'_>, context: &mut AgentContext) {
        if let Some(replacement) = self.replacement {
            *context = replacement;
        }
        if let Some(ModeChange {
            mode, capabilities, ..
        }) = self.mode
        {
            (turn.settings.mode, turn.settings.capabilities) = (Some(mode), capabilities);
        }
    }
}

/// A mode application that changes what an agent runs: a different mode, a new
/// edition of its own, or different grants under the live configuration.
pub(super) struct ModeChange {
    pub(super) mode: AppliedMode,
    pub(super) depth: usize,
    pub(super) capabilities: CapabilitySet,
    /// The facts the mode rendered with, which the root's new prompt is built with too.
    pub(super) facts: Option<skyhook_worker::target::Facts>,
}

impl ModeChange {
    pub(super) fn event(&self) -> SessionEvent {
        SessionEvent::ModeChanged {
            mode: self.mode.selection(),
            capabilities: self.capabilities.clone(),
        }
    }
}

impl SessionRuntime {
    /// What sending `agent`, which runs `current`, into the configured `name` changes;
    /// `None` when it already runs the edition it renders now with what it grants now.
    /// Its prompt was rendered for the grant last journaled, so a live ceiling that
    /// narrows or widens that grant, as a host without the user does, changes it too.
    pub(super) fn mode_change(
        &self,
        agent: &AgentId,
        current: Option<&AppliedMode>,
        name: &ModeName,
    ) -> Result<Option<ModeChange>, HarnessError> {
        let held = self.store.read(|state| {
            let location = state.contracts.get(agent)?.location.clone();
            let (depth, _) = self.granted(state, agent)?;
            Some((
                location,
                depth,
                state.agent_states.get(agent)?.capabilities.clone(),
            ))
        });
        let (location, depth, rendered) =
            held.ok_or_else(|| HarnessError::NoRecordedModel(agent.clone()))?;
        // Only a root changes mode, and a root runs on the session host.
        let facts = Some(self.executor.root_facts());
        let (mode, granted) = self.applied(
            name,
            agent.depth(),
            &location,
            depth,
            &self.capabilities,
            facts.clone(),
        )?;
        let unchanged = current == Some(&mode) && rendered == granted;
        Ok((!unchanged).then_some(ModeChange {
            mode,
            depth,
            capabilities: granted,
            facts,
        }))
    }

    /// The mode the root runs next: the one `requested`, else its own as configured
    /// now, or the default mode once the configuration no longer has its own.
    pub(super) fn root_mode(
        &self,
        current: Option<&AppliedMode>,
        requested: Option<ModeName>,
    ) -> Option<ModeName> {
        requested.or_else(|| {
            let current = current?;
            let (modes, default) = self.harness.modes.configured()?;
            let kept = modes.contains_key(&current.name);
            Some(if kept { &current.name } else { default }.clone())
        })
    }

    /// Whether the root runs a mode the configuration has since changed, which
    /// the next work that runs it adopts, or a model it no longer has, which
    /// refuses that work.
    pub(super) fn root_outdated(&self, root: &AgentId) -> bool {
        let held = self.held(root, |contract, _, _| {
            let gone = !self.harness.models.contains_key(&contract.profile.name);
            (contract.mode.clone(), gone)
        });
        let Some((current, gone)) = held else {
            return false;
        };
        let current = current.as_ref();
        gone || self.root_mode(current, None).is_some_and(|name| {
            let change = self.mode_change(root, current, &name);
            !matches!(change, Ok(None))
        })
    }

    /// Warnings that the configuration has moved on from the selection the reopened
    /// `root` last ran under: it no longer has the model, which another must replace
    /// before the root runs again; its mode renders for it otherwise or is gone, which
    /// the next work that runs the root adopts; or its mode no longer renders for it,
    /// which fails that work. A selection the root has replaced warns no more.
    pub(super) fn drift_warnings(&self, root: &AgentId) -> Vec<String> {
        let held = self.held(root, |contract, _, _| {
            (contract.profile.name.clone(), contract.mode.clone())
        });
        let Some((model, running)) = held else {
            return Vec::new();
        };
        let model = (!self.harness.models.contains_key(&model)).then(|| {
            format!(
                "The configuration no longer has model `{model}`, which this session last ran \
                 on; the root agent runs only once another is chosen."
            )
        });
        let Some(running) = running else {
            return model.into_iter().collect();
        };
        let name = &running.name;
        let configured = self.harness.modes.configured();
        let kept = configured.is_some_and(|(modes, _)| modes.contains_key(name));
        let mode = match (kept, configured) {
            (true, _) => match self.mode_change(root, Some(&running), name) {
                Ok(change) => change
                    .is_some_and(|change| change.mode != running)
                    .then(|| {
                        format!(
                            "The configuration has changed mode `{name}` since this session last \
                         ran; the root agent adopts it when it next runs."
                        )
                    }),
                Err(error) => Some(format!(
                    "Mode `{name}`, which this session last ran in, no longer renders for the \
                     root agent: {error}. Running the root agent in it fails until it does."
                )),
            },
            (false, Some((_, default))) => Some(format!(
                "The configuration no longer has mode `{name}`, which this session last ran \
                 in; the root agent moves to the default mode `{default}` when it next runs."
            )),
            (false, None) => Some(format!(
                "The configuration no longer has mode `{name}`, which this session last ran \
                 in; without a default mode, the root agent keeps running it within the \
                 current capabilities."
            )),
        };
        model.into_iter().chain(mode).collect()
    }
}

/// The messages an adoption journals with its selection.
pub(in crate::agent) enum Adopting {
    /// What the agent took itself: a selection alone, or the input that begins
    /// its turn, which commits even when an interrupt stopped that turn first.
    Taken(Option<Message>),
    /// Queued input joining the turn, refused once it is no longer busy.
    Joins(Vec<Message>),
}

impl SessionRuntime {
    /// A model this runtime's catalog serves. Journaled names are checked here on
    /// resume, since the catalog may have changed since they were recorded.
    pub(super) fn model_entry(&self, model: &ModelRef) -> Result<ModelEntry, HarnessError> {
        self.harness
            .models
            .get(model)
            .cloned()
            .ok_or_else(|| HarnessError::UnknownModel(model.clone()))
    }
    /// The model the root has committed to.
    pub(super) fn root_model(&self) -> ModelRef {
        let root = AgentId::root(self.store.id());
        let committed = |state: &CoreState| {
            let held = state.contracts.get(&root);
            held.map(|contract| contract.profile.name.clone())
        };
        let committed = self.store.read(committed);
        committed.expect("the root's journal is held from its start")
    }
    /// The edition of `name` the configuration defines now, as an agent at `depth` and
    /// `location`, delegating `available_depth` deep under `ceiling`, enters it, with what
    /// it grants that agent.
    pub(super) fn applied(
        &self,
        name: &ModeName,
        depth: usize,
        location: &skyhook_tools::target::ExecutionLocation,
        available_depth: usize,
        ceiling: &CapabilitySet,
        facts: Option<skyhook_worker::target::Facts>,
    ) -> Result<(AppliedMode, CapabilitySet), HarnessError> {
        let definition = (self.harness.modes.configured()).and_then(|(modes, _)| modes.get(name));
        let definition = definition.ok_or_else(|| HarnessError::UnknownMode(name.clone()))?;
        let capabilities = definition.grants(ceiling, available_depth);
        let grants = (available_depth, &capabilities);
        let agent = self.prompt_agent(depth, location, grants, Some(name), facts)?;
        let definition = self.harness.prompts.edition(definition, &agent)?.into();
        let name = name.clone();
        Ok((AppliedMode { name, definition }, capabilities))
    }
    /// The contract of an agent at `depth` and `location` holding `grants` in `mode`, as
    /// its prompt describes it.
    fn prompt_agent<'a>(
        &self,
        depth: usize,
        location: &'a skyhook_tools::target::ExecutionLocation,
        (available_depth, capabilities): (usize, &'a CapabilitySet),
        mode: Option<&'a ModeName>,
        facts: Option<skyhook_worker::target::Facts>,
    ) -> Result<crate::prompts::Agent<'a>, HarnessError> {
        let definition = match &location.target {
            skyhook_tools::target::TargetRef::Root => None,
            skyhook_tools::target::TargetRef::Named(name) => {
                Some(self.router.targets(|targets| targets.get(name))?)
            }
        };
        Ok(crate::prompts::Agent {
            depth,
            available_depth,
            capabilities,
            mode,
            workspace: &location.workspace,
            target: &location.target,
            definition,
            facts,
        })
    }
    pub(super) fn system_prompt(
        &self,
        agent: &AgentId,
        location: &skyhook_tools::target::ExecutionLocation,
        available_depth: usize,
        mode: Option<&AppliedMode>,
        capabilities: &CapabilitySet,
        facts: Option<skyhook_worker::target::Facts>,
    ) -> Result<Vec<SystemSegment>, HarnessError> {
        let grants = (available_depth, capabilities);
        let name = mode.map(|mode| &mode.name);
        let agent = self.prompt_agent(agent.depth(), location, grants, name, facts)?;
        let instructions = mode.and_then(|mode| mode.definition.instructions.as_ref());
        let prompts = &self.harness.prompts;
        let text = prompts.system(&agent, instructions, &self.harness.instructions)?;
        Ok(vec![SystemSegment { text, cache: true }])
    }
    /// Open a context for `profile` on the live provider of its model, offering the
    /// live tool surface, or `pinned` tools journaled earlier. A pinned tool runs
    /// against its live definition, which validates the arguments; one the live
    /// surface no longer offers stays visible to the model but fails when called.
    /// While the configuration no longer has the model, only the root opens, with
    /// no provider, until a selection replaces it: a child refused here starts
    /// again on the root's model once it has one.
    pub(super) async fn open_agent_context(
        &self,
        agent: &AgentId,
        profile: crate::session::ProfileSnapshot,
        system: Vec<SystemSegment>,
        capabilities: &CapabilitySet,
        pinned: Option<Vec<std::sync::Arc<crate::tool::catalog::ToolDefinition>>>,
    ) -> Result<AgentContext, HarnessError> {
        let id = crate::provider::protocol::ContextId::from(agent);
        let provider = match self.harness.models.get(&profile.name) {
            Some(entry) => Some(entry.provider.open_context(id)?),
            None if agent.parent().is_none() => None,
            None => return Err(HarnessError::UnknownModel(profile.name)),
        };
        let tools = pinned.unwrap_or_else(|| {
            let executor = self
                .executor
                .clone()
                .with_capabilities(capabilities.clone());
            crate::tool::catalog::definitions(&executor.surface_for_agent(agent))
        });
        let model = crate::session::ModelContext {
            purpose: crate::session::ModelPurpose::Agent,
            profile,
            system,
            tools,
            response_schema: None,
        };
        Ok(AgentContext::open(agent, model, provider, &self.store).await?)
    }
    /// Resolve an input's model and mode, where given and different, without
    /// journaling anything: a rejected selection changes nothing. A mode is the root
    /// agent's capabilities, the tools they allow and its instructions; children it
    /// already started keep their own. The root also adopts the configuration's
    /// current definition of the mode it runs, or the default mode once its own is
    /// gone, when the input names none. An agent whose model the configuration no
    /// longer has is refused unless the input names another, and `images` rejects
    /// an unsupported profile, before any provider context opens.
    // Boxed as a `dyn` future: proving the agent loop `Send` through this body
    // otherwise exceeds the compiler's recursion limit. Exclusive borrows keep it
    // `Send` without requiring the context to be `Sync`.
    pub(in crate::agent) fn select<'a>(
        &'a self,
        turn: &'a mut TurnContext<'_>,
        context: &'a mut AgentContext,
        options: Selection,
        images: bool,
    ) -> futures_util::future::BoxFuture<'a, Result<Selected, HarnessError>> {
        Box::pin(async move {
            let agent = turn.agent;
            // An agent whose model is gone runs only on another that its input names.
            let model = match options.model {
                Some(model) => Some(model.name),
                None => {
                    context.provider()?;
                    None
                }
            };
            let model = model.filter(|name| name != &context.model.profile.name);
            let snapshot = match &model {
                Some(name) => crate::session::ProfileSnapshot {
                    name: name.clone(),
                    profile: self.model_entry(name)?.profile,
                },
                None => context.model.profile.clone(),
            };
            let profile = &snapshot.profile;
            if images && !profile.supports_images {
                return Err(HarnessError::ImagesUnsupported(profile.model.clone()));
            }
            let requested = options.mode.map(|mode| mode.name);
            let current = turn.settings.mode.as_ref();
            let mode = if agent.depth() != 0 {
                // A child keeps the mode it started in.
                None
            } else {
                match self.root_mode(current, requested) {
                    Some(name) => self.mode_change(agent, current, &name)?,
                    None => None,
                }
            };
            let replacement = if let Some(change) = &mode {
                let (mode, capabilities) = (Some(&change.mode), &change.capabilities);
                let (location, facts) = (turn.location, change.facts.clone());
                let system =
                    self.system_prompt(agent, location, change.depth, mode, capabilities, facts)?;
                let context = self
                    .open_agent_context(agent, snapshot.clone(), system, capabilities, None)
                    .await?;
                Some(context)
            } else if model.is_some() {
                let system = context.model.system.clone();
                // The tools stay pinned across a model change.
                let tools = Some(context.model.tools.clone());
                let capabilities = &turn.settings.capabilities;
                let opening =
                    self.open_agent_context(agent, snapshot.clone(), system, capabilities, tools);
                Some(opening.await?)
            } else {
                None
            };
            if let Some(replacement) = &replacement {
                replacement.admits_images()?;
            }
            let mut events = Vec::new();
            if model.is_some() {
                let profile = snapshot;
                events.push((agent.clone(), SessionEvent::ModelChanged { profile }));
            }
            if let Some(change) = &mode {
                events.push((agent.clone(), change.event()));
            }
            Ok(Selected {
                events,
                replacement,
                mode,
            })
        })
    }
    /// Journal `selected` and the messages it applies to in one transaction,
    /// admitted as a submission when there are any, then install them.
    /// `ModelChanged` and `ModeChanged` precede those messages.
    pub(in crate::agent) async fn adopt(
        &self,
        turn: &mut TurnContext<'_>,
        context: &mut AgentContext,
        mut selected: Selected,
        adopting: Adopting,
    ) -> Result<(), HarnessError> {
        let (messages, joins) = match adopting {
            Adopting::Taken(message) => (Vec::from_iter(message), false),
            Adopting::Joins(messages) => (messages, true),
        };
        let command = if messages.is_empty() {
            Command::Journal
        } else {
            Command::Start
        };
        let mut events = std::mem::take(&mut selected.events);
        let messages = messages.into_iter();
        let messages = messages.map(|message| SessionEvent::MessageCommitted { message });
        events.extend(messages.map(|message| (turn.agent.clone(), message)));
        if !events.is_empty() {
            let agent = turn.agent.clone();
            let plan = move |state: &CoreState| {
                if joins && !state.agents.busy(&agent) {
                    return Err(HarnessError::Interrupted);
                }
                Ok(Plan::journal(events))
            };
            self.store.commit(command, plan).await?;
        }
        // No await between installation and its routing projection.
        selected.install(turn, context);
        Ok(())
    }
    /// Deliver the owner's pending events with the selection its next turn makes
    /// unasked, the root's configured mode, in one transaction. A selection that
    /// cannot be prepared delivers nothing: the events stay pending for the input
    /// that resumes the agent. None when nothing was pending, or an interrupt
    /// stopped the turn first, which leaves them pending.
    pub(in crate::agent) async fn deliver_selected(
        &self,
        turn: &mut TurnContext<'_>,
        context: &mut AgentContext,
    ) -> Option<Result<(), HarnessError>> {
        let options = Selection::default();
        let mut selected = match self.select(turn, context, options, false).await {
            Ok(selected) => selected,
            Err(error) => return Some(Err(error)),
        };
        let preceding: Vec<_> = std::mem::take(&mut selected.events)
            .into_iter()
            .map(|(_, event)| event)
            .collect();
        // The delivery commits after the selection, so it is presented under the
        // grants the selection gives.
        let capabilities = (selected.mode.as_ref())
            .map_or(&turn.settings.capabilities, |change| &change.capabilities);
        let viewer = skyhook_tools::diagnostic::DiagnosticViewer::new(
            capabilities.target_view(),
            turn.location,
        );
        let delivery = self.deliver_events(turn.agent, viewer, &preceding, Boundary::Jobs);
        match delivery.await {
            Ok(None) | Err(HarnessError::Interrupted) => None,
            Ok(Some(_)) => {
                selected.install(turn, context);
                Some(Ok(()))
            }
            Err(error) => Some(Err(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::queue::tests::*;
    use crate::agent::tests::*;
    use crate::tests::bounded;

    struct DepthState;

    impl crate::agent::HostState for DepthState {
        fn state<'a>(
            &'a self,
            _: &'a crate::session::ExtensionDb,
            agent: &'a crate::identity::AgentId,
        ) -> crate::agent::HostStateFuture<'a> {
            let depth = agent.depth();
            Box::pin(async move { Some(format!("depth: {depth}").parse().unwrap()) })
        }
    }

    /// The host's persona renders for every agent from its context and the host's
    /// section, and the host's state section reaches every agent's request.
    #[tokio::test]
    async fn persona_and_host_state_reach_root_and_child_requests() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let provider = scripted_provider(
            &requests,
            [
                response(vec![tool_call(
                    0,
                    "delegate",
                    "agent",
                    json!({"prompt": "work"}),
                )]),
                answer("child done"),
                answer("root done"),
            ],
        );
        let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
            .prompts(persona_prompts())
            .session_schema(crate::session::tests::TABLES)
            .host_state(Arc::new(DepthState))
            .build()
            .await
            .unwrap();
        let session = ephemeral_session(&harness).await;
        assert_eq!(
            bounded(session.prompt("delegate")).await.unwrap(),
            "root done"
        );
        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured.len(), 3);
        for (request, (role, depth)) in
            (captured.iter()).zip([("root", 0), ("child", 1), ("root", 0)])
        {
            let persona = format!("Persona of a {role}, custom.");
            assert!(request.system[0].text.contains(&persona), "{persona}");
            assert!(request_runtime_state(request).ends_with(&format!("\nhost:\ndepth: {depth}")));
        }
        shutdown_session(session).await;
    }

    /// A host persona reading the agent's role and the host's `tag`.
    fn persona_prompts() -> crate::prompts::Prompts {
        #[derive(serde::Serialize)]
        struct Host {
            tag: &'static str,
        }
        let persona = "Persona of a {{ role }}, {{ tag }}.";
        let sources = [("host/persona.md", crate::prompts::Source::Template(persona))];
        let host = Host { tag: "custom" };
        let templates = crate::prompts::Templates::with(sources, &host).unwrap();
        let persona = "host/persona.md".parse().unwrap();
        crate::prompts::Prompts::host(&templates, &host, persona).unwrap()
    }

    #[tokio::test]
    async fn running_child_parent_inputs_are_fifo_once_after_tool_or_final_responses() {
        for first_calls_tool in [true, false] {
            running_child_receives_parent_inputs(first_calls_tool).await;
        }
    }

    async fn running_child_receives_parent_inputs(first_calls_tool: bool) {
        let (_root, tracking, session) = start(first_calls_tool).await;
        let runtime = &session.runtime;
        let launch = "return await tool.agent({prompt:'test:child-initial',name:'kid',bg:true});";
        let launched = bounded(session.run_script(launch)).await.unwrap();
        let job: JobId = serde_json::from_value(launched.value["value"]["id"].clone()).unwrap();
        let initial = tracking.request(0).await;
        assert_eq!(texts(initial.messages()), ["test:child-initial"]);
        let child = {
            let agents = runtime.agents(|agents| agents.within(&session.root));
            let mut children = agents.into_iter().filter(|id| id != &session.root);
            let child = children.next().unwrap();
            assert!(children.next().is_none());
            child
        };
        // A send acknowledgement alone does not prove forwarding: observe the child's inbox.
        let forwarded = async |count| {
            let (key, child) = (Key::Agent(child.clone()), child.clone());
            found(&session, [key], move |state| {
                (state.agents.get(&child)?.inbox.len() == count).then_some(())
            })
            .await
        };

        // Exercise the public script send as well as the underlying job mailbox.
        let first = json!({"instruction": "test:parent-one", "nested": [1, true]});
        let second = json!("test:parent-two");
        let send = format!("return (await tool.control({{job:{job}, send:{first}}})).unwrap();");
        let accepted = bounded(session.run_script(send)).await.unwrap();
        assert_eq!(accepted.value["value"], serde_json::Value::Null);
        forwarded(1).await;
        let send = runtime.jobs.send(job, second.clone());
        bounded(send).await.unwrap();
        forwarded(2).await;
        assert_eq!(count(&tracking), 1);

        // Keep wakeups out of the script-driven parent loop; the child mailbox stays live.
        let root_inbox = quiet_root(&session);

        tracking.release(0);
        let next = tracking.request(1).await;
        let (first, second) = (
            format!("Owner input: {first}"),
            format!("Owner input: {second}"),
        );
        assert_eq!(parent_inputs(next.messages()), [first, second]);
        assert_eq!(texts(next.messages()), ["test:child-initial"]);
        if first_calls_tool {
            assert!(next.messages().any(todo_finished));
        } else {
            assert!(next.messages().any(|message| matches!(message,
                Sent::Assistant(items) if items == &vec![AssistantItem::text("text/0", 0, "answer-0")])));
        }
        let state = runtime.jobs.snapshot(job).await.unwrap().state;
        // The child must answer the parent updates first.
        assert!(!state.is_terminal());

        tracking.release(1);
        let completed = bounded(runtime.jobs.wait(job, true)).await.unwrap();
        assert_eq!(completed.state, crate::job::JobState::Completed);
        assert_eq!(completed.output, Some(json!({"reply": "answer-1"})));
        drop(root_inbox);
        stop(&session).await;
        assert_eq!(count(&tracking), 2);
    }

    #[tokio::test]
    async fn queued_fifo_images_and_the_last_model_commit_before_next_request() {
        let (root, tracking, session) = start(true).await;
        let first_png = crate::tests::png(b"first image fixture").await;
        let second_png = crate::tests::png(b"second image fixture").await;
        let image = |name: &str, image: &skyhook_media::Image| skyhook_media::Attachment::Image {
            file: Some(root.path().join(name)),
            image: image.clone(),
        };
        let turn = spawn_prompt(&session, "test:initial");
        let in_flight = tracking.request(0).await;
        assert_eq!(in_flight.model.as_str(), "first-model");

        let first_image = vec![image("first.png", &first_png)];
        let (first, first_cancel) = enqueue(
            &session,
            "test:queued-one",
            first_image,
            Some("queue-test/second"),
        );
        buffered(&session, 1).await;
        let second_image = vec![image("second.png", &second_png)];
        let (second, second_cancel) = enqueue(
            &session,
            "test:queued-two",
            second_image,
            Some("queue-test/third"),
        );
        buffered(&session, 2).await;
        assert!(!first.is_finished() && !second.is_finished());
        assert!(!first_cancel.is_claimed() && !second_cancel.is_claimed());
        assert_eq!(texts(&committed(&session)), ["test:initial"]);
        assert_eq!(count(&tracking), 1);

        tracking.release(0);
        let next = tracking.request(1).await;
        bounded(first).await.unwrap().unwrap();
        bounded(second).await.unwrap().unwrap();
        // Receipts must not wait for the gated next response.
        assert!(!turn.is_finished());
        assert!(first_cancel.is_claimed() && second_cancel.is_claimed());
        // A committed submission cannot be recalled.
        assert!(!first_cancel.cancel() && !second_cancel.cancel());
        assert_eq!(next.model.as_str(), "third-model");
        let expected = ["test:initial", "test:queued-one", "test:queued-two"];
        assert_eq!(texts(next.messages()), expected);
        let blocks = next.messages().flat_map(|message| match message {
            Sent::User(blocks) => blocks.as_slice(),
            _ => &[],
        });
        let images = blocks.filter_map(|block| match block {
            SentPart::Attachment {
                attachment: skyhook_media::AttachmentRef::Image(image),
            } => Some(next.blobs.get(&image.blob).unwrap()),
            _ => None,
        });
        let expected_images = [first_png.bytes(), second_png.bytes()];
        assert_eq!(images.collect::<Vec<_>>(), expected_images);
        // The ongoing tool call must finish normally.
        assert!(next.messages().any(todo_finished));
        // Both batches are one submission: only the selection it ends with is journaled.
        assert_eq!(model_changes(&session), ["queue-test/third"]);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        // Queued inputs must not become additional turns.
        assert_eq!(count(&tracking), 2);
        assert_eq!(texts(&committed(&session)), expected);
    }

    #[tokio::test]
    async fn ordinary_prompt_still_waits_for_its_own_turn() {
        let (_root, tracking, session) = start(true).await;
        let turn = spawn_prompt(&session, "test:initial");
        tracking.request(0).await;
        let ordinary = spawn_prompt(&session, "test:ordinary");
        buffered(&session, 1).await;
        tracking.release(0);
        let next = tracking.request(1).await;
        assert_eq!(texts(next.messages()), ["test:initial"]);
        assert!(!ordinary.is_finished() && !turn.is_finished());
        tracking.release(1);
        assert_eq!(bounded(turn).await.unwrap().unwrap(), "answer-1");
        assert_eq!(bounded(ordinary).await.unwrap().unwrap(), "answer-2");
        let last = tracking.request(2).await;
        assert_eq!(texts(last.messages()), ["test:initial", "test:ordinary"]);
        stop(&session).await;
        assert_eq!(count(&tracking), 3);
    }
}
