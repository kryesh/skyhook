//! Child-agent launch, tool dispatch, and model context selection.

use super::*;

// One owner binds the resolved provider context, admitted capabilities, loop
// controls and initial journal publication to the same launch identity. There
// is no config lookup or reconstruction after preparation.
struct PreparedAgentLaunch {
    runtime: Arc<SessionRuntime>,
    agent_loop: AgentLoop,
    sender: AgentSender,
    todos: Option<Vec<TodoItem>>,
    available_depth: usize,
    /// Journaled before: resume under that contract without starting it again.
    resumed: bool,
    /// The session's first agent, which commits the session start with it.
    first: bool,
}

impl PreparedAgentLaunch {
    async fn prepare(
        runtime: Arc<SessionRuntime>,
        launch: AgentLaunch,
    ) -> Result<Self, HarnessError> {
        let AgentLaunch {
            id,
            owner_job,
            mut model_profile,
            todos,
            mut available_depth,
            mut location,
        } = launch;
        // An agent that started before resumes under its journaled contract. Live
        // configuration may narrow its capabilities but never widen them.
        let records = runtime.store.records().await;
        let first = records.is_empty();
        let recorded = recorded_contract(&records, &id, &runtime.harness.capabilities);
        drop(records);
        let resumed = recorded.is_some();
        if id.depth() > runtime.harness.max_child_depth {
            return Err(HarnessError::ChildDepth);
        }
        let remaining_depth = runtime.harness.max_child_depth.saturating_sub(id.depth());
        if let Some(recorded) = &recorded {
            // A lowered live limit narrows a resumed agent rather than refusing it.
            available_depth = recorded.available_depth.min(remaining_depth);
            location.clone_from(&recorded.location);
            model_profile.clone_from(&recorded.profile.name);
        }
        if available_depth > remaining_depth {
            return Err(HarnessError::ChildDepth);
        }
        let allowed = runtime.harness.capabilities.for_agent(available_depth);
        let capabilities = match &recorded {
            Some(recorded) => recorded
                .capabilities
                .iter()
                .filter(|capability| allowed.contains(*capability))
                .collect(),
            None => allowed,
        };
        let (profile, system, tools) = match recorded {
            Some(RecordedContract {
                profile,
                system: Some(system),
                tools,
                ..
            }) => (profile.profile, system, tools),
            recorded => {
                let (live, system) = runtime
                    .resolve_agent(
                        &model_profile,
                        &id,
                        &location,
                        available_depth,
                        &capabilities,
                    )
                    .await?;
                let profile = recorded.map_or(live, |recorded| recorded.profile.profile);
                (profile, system, None)
            }
        };
        let context = runtime
            .open_agent_context(&id, profile, system, &capabilities, tools, true)
            .await?;
        let (tx, rx) = mpsc::channel(AGENT_CHANNEL_CAPACITY);
        let sender = AgentSender::new(tx);
        Ok(Self {
            runtime,
            agent_loop: AgentLoop {
                control: AgentControl::new(),
                id,
                owner_job,
                context,
                model_profile,
                location,
                capabilities,
                rx,
            },
            sender,
            todos,
            available_depth,
            resumed,
            first,
        })
    }

    async fn install(self) -> Result<AgentSender, HarnessError> {
        let Self {
            runtime,
            agent_loop,
            sender,
            todos,
            available_depth,
            resumed,
            first,
        } = self;
        if !resumed {
            let started = SessionEvent::AgentStarted {
                parent: agent_loop.id.parent(),
                owner_job: agent_loop.owner_job,
                profile: Some(crate::session::ProfileSnapshot {
                    name: agent_loop.model_profile.clone(),
                    profile: agent_loop.context.profile.clone(),
                }),
                available_depth: u32::try_from(available_depth).unwrap_or(u32::MAX),
                capabilities: agent_loop.capabilities.iter().collect(),
                location: agent_loop.location.clone(),
            };
            let harness = &runtime.harness;
            // Every entry references an agent, so the session starts with its root.
            let session = first.then(|| SessionEvent::SessionStarted {
                targets: harness.target_definitions.clone(),
                capabilities: harness.capabilities.iter().collect(),
                max_child_depth: u32::try_from(harness.max_child_depth).unwrap_or(u32::MAX),
            });
            let events = session.into_iter().chain([started]);
            let id = &agent_loop.id;
            runtime
                .store
                .append_all(events.map(|event| (id.clone(), event)).collect())
                .await?;
        }
        if let Some(job) = agent_loop.owner_job {
            runtime
                .jobs
                .set_agent_location(job, agent_loop.location.clone())
                .await?;
        }
        runtime
            .todos
            .register(agent_loop.id.clone(), agent_loop.owner_job, todos)
            .await?;
        {
            let mut agents = runtime.agents_mut();
            // Serialize this check with shutdown's agent snapshot. A child whose
            // provider initialization raced shutdown must not leave an idle loop.
            if runtime.shutting_down.load(Ordering::Acquire) {
                return Err(HarnessError::Interrupted);
            }
            agents.insert(
                agent_loop.id.clone(),
                LiveAgent {
                    model_profile: agent_loop.model_profile.clone(),
                    sender: sender.clone(),
                    cancellation: CancellationToken::new(),
                    control: agent_loop.control.clone(),
                    available_depth,
                },
            );
        }
        runtime.activity(&agent_loop.id, AgentActivity::Idle);
        tokio::spawn(async move { runtime.run_agent(agent_loop).await });
        Ok(sender)
    }
}

impl SessionRuntime {
    pub(super) async fn spawn_agent(
        self: &Arc<Self>,
        launch: AgentLaunch,
    ) -> Result<AgentSender, HarnessError> {
        PreparedAgentLaunch::prepare(self.clone(), launch)
            .await?
            .install()
            .await
    }

    pub(super) fn execute_call(
        &self,
        agent: &AgentId,
        parent: Option<JobId>,
        call: &ToolCall,
        origin: u64,
        location: &crate::execution::ExecutionLocation,
        capabilities: &CapabilitySet,
    ) -> futures_util::future::BoxFuture<'static, ToolResult> {
        let call = call.clone();
        let agent = agent.clone();
        let executor = self
            .executor
            .clone()
            .with_location(location.clone())
            .with_capabilities(capabilities.clone())
            .with_model_origin(crate::session::ModelCallOrigin {
                message: origin,
                call_id: call.id().to_owned(),
            });
        // Tool dispatch owns its execution inputs. Erasing this future separates
        // the driver's Send proof from the nested supervised executor graph.
        Box::pin(async move {
            let result = executor
                .execute_model(
                    agent,
                    call.name(),
                    serde_json::Value::Object(call.arguments().clone()),
                    parent,
                )
                .await;
            let mut result = match result {
                Ok(result) => ToolResult {
                    call_id: call.id().to_owned(),
                    name: call.name().to_owned(),
                    result: result.output.value,
                    images: result.output.images,
                    is_error: result.is_error,
                },
                Err(error) => {
                    let failure = error.into_failure();
                    let mut result = json!({"error": failure.message});
                    if let Some(denial) = failure.denial {
                        result["code"] = json!(denial.code);
                        result["executed"] = json!(denial.executed);
                    }
                    let images = if let Some(output) = failure.output {
                        result["output"] = output.value;
                        output.images
                    } else {
                        Vec::new()
                    };
                    ToolResult {
                        call_id: call.id().to_owned(),
                        name: call.name().to_owned(),
                        result,
                        images,
                        is_error: true,
                    }
                }
            };
            // Commit the same compact presentation that the model and UI display.
            crate::job::omit_null_fields(&mut result.result);
            result
        })
    }

    pub(super) async fn resolve_agent(
        &self,
        model_profile: &str,
        agent: &AgentId,
        location: &crate::execution::ExecutionLocation,
        available_depth: usize,
        capabilities: &CapabilitySet,
    ) -> Result<(ModelProfile, Vec<SystemSegment>), HarnessError> {
        let profile = self
            .harness
            .model_profiles
            .get(model_profile)
            .cloned()
            .ok_or_else(|| HarnessError::UnknownModelProfile(model_profile.to_owned()))?;
        let target = if location.is_root() {
            None
        } else {
            Some(self.router.targets().get(&location.target).await?)
        };
        let system = vec![prompt::system_segment(
            &self.harness.instructions,
            agent,
            location,
            target.as_ref(),
            available_depth,
            capabilities,
        )];
        Ok((profile, system))
    }

    /// Open a context offering the live tool surface, or `pinned` tools journaled
    /// earlier. A pinned tool whose live definition differs or is gone stays
    /// visible to the model but is unavailable to call.
    pub(super) async fn open_agent_context(
        &self,
        agent: &AgentId,
        profile: ModelProfile,
        system: Vec<SystemSegment>,
        capabilities: &CapabilitySet,
        pinned: Option<Vec<crate::provider::protocol::ToolDefinition>>,
        restore_meter: bool,
    ) -> Result<AgentContext, HarnessError> {
        let factory = self
            .harness
            .providers
            .get(&profile.provider)
            .ok_or_else(|| HarnessError::UnknownProvider(profile.provider.clone()))?;
        let live = self
            .executor
            .clone()
            .with_capabilities(capabilities.clone())
            .surface_for_agent(agent)
            .definitions();
        let (tools, unavailable) = match pinned {
            // A pinned tool the live surface no longer offers cannot run. One it still
            // offers runs against its live definition, which validates the arguments.
            Some(pinned) => {
                let unavailable = pinned
                    .iter()
                    .filter(|tool| !live.iter().any(|live| live.name == tool.name))
                    .map(|tool| tool.name.clone())
                    .collect();
                (pinned, unavailable)
            }
            None => (live, Default::default()),
        };
        let template = ModelRequest {
            model: profile.model.clone(),
            system,
            history: Vec::new(),
            tail: Vec::new(),
            history_lifetime: Default::default(),
            tools,
            reasoning: profile.reasoning.clone(),
            response_schema: None,
            max_output_tokens: Some(profile.max_output),
            correlation: Some(agent.to_string()),
            blobs: Default::default(),
        };
        let mut context = AgentContext::open(
            agent,
            profile,
            template.try_into()?,
            factory.as_ref(),
            &self.store.records().await,
            restore_meter,
        )?;
        context.unavailable_tools = Arc::new(unavailable);
        Ok(context)
    }
}

impl SessionRuntime {
    /// Give restored agent jobs a handler that resumes their child, starting it
    /// again under its journaled contract when its loop is gone.
    pub(super) async fn install_retained_children(self: &Arc<Self>) {
        let records = self.store.records().await;
        for retained in self.jobs.retained_children().await {
            let live = &self.harness.capabilities;
            let Some(mut owner) = recorded_contract(&records, &retained.owner, live) else {
                continue;
            };
            // As when the owner itself resumes: its depth never exceeds the live limit.
            let remaining = self
                .harness
                .max_child_depth
                .saturating_sub(retained.owner.depth());
            let allowed = live.for_agent(owner.available_depth.min(remaining));
            owner.capabilities = owner
                .capabilities
                .iter()
                .filter(|capability| allowed.contains(*capability))
                .collect();
            let authorization = crate::tool::authorization::AuthorizationSubject {
                agent: retained.owner,
                job: retained.job,
                parent: retained.parent,
                scope: retained.scope,
                capabilities: owner.capabilities,
                cancellation: retained.cancellation,
            };
            let handler = super::tools::child_resume_handler(
                Arc::downgrade(self),
                retained.child,
                authorization,
                retained.location,
                owner.location,
            );
            let _ = self.jobs.set_resume_handler(retained.job, handler).await;
        }
    }
}

/// The contract an agent was journaled under: its start, latest applied profile,
/// and the prompt and tools of its latest agent model context.
struct RecordedContract {
    profile: crate::session::ProfileSnapshot,
    available_depth: usize,
    /// The journaled set narrowed to the live configuration, never widened.
    capabilities: CapabilitySet,
    location: crate::execution::ExecutionLocation,
    system: Option<Vec<SystemSegment>>,
    tools: Option<Vec<crate::provider::protocol::ToolDefinition>>,
}

fn recorded_contract(
    records: &[EventRecord],
    agent: &AgentId,
    live: &CapabilitySet,
) -> Option<RecordedContract> {
    let mut contract: Option<RecordedContract> = None;
    for record in records.iter().filter(|record| &record.agent == agent) {
        match &record.event {
            SessionEvent::AgentStarted {
                profile: Some(profile),
                available_depth,
                capabilities,
                location,
                ..
            } => {
                contract = Some(RecordedContract {
                    profile: profile.clone(),
                    available_depth: *available_depth as usize,
                    capabilities: capabilities
                        .iter()
                        .copied()
                        .filter(|capability| live.contains(*capability))
                        .collect(),
                    location: location.clone(),
                    system: None,
                    tools: None,
                });
            }
            SessionEvent::ModelChanged { profile } => {
                if let Some(contract) = &mut contract {
                    contract.profile = profile.clone();
                }
            }
            SessionEvent::ModelContext { context }
                if context.purpose == crate::session::ModelPurpose::Agent =>
            {
                if let Some(contract) = &mut contract {
                    contract.system = Some(context.system.clone());
                    contract.tools = Some(context.tools.clone());
                }
            }
            _ => {}
        }
    }
    contract
}
impl SessionRuntime {
    pub(super) async fn next_child(&self, parent: &AgentId) -> AgentId {
        let mut counters = self.child_counters.write().await;
        let counter = counters.entry(parent.clone()).or_insert(0);
        *counter = counter.saturating_add(1);
        parent.child(*counter)
    }

    pub(super) fn available_depth(&self, agent: &AgentId) -> usize {
        self.agents()
            .get(agent)
            .map_or(0, |agent| agent.available_depth)
    }

    pub(super) fn agent_sender(&self, id: &AgentId) -> Option<AgentSender> {
        self.agents().get(id).map(|agent| agent.sender.clone())
    }

    /// Register this agent's turn cancellation, or decline once stopping. Checked
    /// under the `agents` write lock: a token minted before `interrupt_tree`'s
    /// snapshot is cancelled by it, and none can be minted after.
    pub(super) fn begin_turn(&self, id: &AgentId) -> Option<CancellationToken> {
        let cancellation = CancellationToken::new();
        let mut agents = self.agents_mut();
        if self.shutting_down.load(Ordering::Acquire) {
            return None;
        }
        let agent = agents.get_mut(id).expect("running agents are registered");
        agent
            .control
            .retryable_interrupt
            .store(false, Ordering::Release);
        agent.cancellation = cancellation.clone();
        Some(cancellation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;

    fn last_tool_results(request: &ModelRequest) -> &[ToolResult] {
        let Some(Message::Tool(results)) = request.history.last() else {
            panic!("expected tool results at the end of the history");
        };
        results
    }

    #[tokio::test]
    async fn committed_tool_history_omits_null_fields() {
        let source = "return {error: null, nested: {absent: null, ok: false}, array: [null, 0]};";
        let call = tool_call(0, "script-call", "script", json!({ "source": source }));
        let (_root, requests, session) =
            scripted_session([response(vec![call]), answer("done")]).await;
        assert_eq!(session.prompt("run").await.unwrap(), "done");
        let records = session.runtime.store.records().await;
        let results = events!(&records, SessionEvent::MessageCommitted { message: Message::Tool(results) } => results);
        let value = &results[0][0].result["result"]["value"];
        assert_eq!(value, &json!({"nested": {"ok": false}, "array": [null, 0]}));
        assert_eq!(last_tool_results(&requests.lock().unwrap()[1]), results[0]);
    }

    #[tokio::test]
    async fn child_first_request_includes_parent_supplied_todos() {
        let todos = json!([
            {"text": "Inspect the implementation", "status": "completed"},
            {"text": "Make the change", "status": "in_progress"},
            {"text": "Run relevant checks", "status": "pending"},
            {"text": "Check \"quoted\" text\nand Unicode: café", "status": "pending"},
            {"text": "Keep the original order", "status": "completed"}
        ]);
        let arguments = json!({"prompt": "work", "todos": todos});
        let (_root, requests, session) = scripted_session([
            response(vec![tool_call(0, "delegate", "agent", arguments)]),
            answer("child done"),
            answer("root done"),
        ])
        .await;
        assert_eq!(session.prompt("delegate").await.unwrap(), "root done");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let child_request = &requests[1];
        let system = &child_request.system[0].text;
        assert!(system.starts_with(prompt::CHILD_PROMPT));
        let state = request_runtime_state(child_request);
        let (_, sections) = state.split_once('\n').unwrap();
        assert_eq!(
            sections,
            concat!(
                "todos:\n",
                "completed:\n",
                "  \"Inspect the implementation\"\n",
                "in_progress:\n",
                "  \"Make the change\"\n",
                "pending:\n",
                "  \"Run relevant checks\"\n",
                "  \"Check \\\"quoted\\\" text\\nand Unicode: café\"\n",
                "completed:\n",
                "  \"Keep the original order\"",
            )
        );
    }

    #[tokio::test]
    async fn delegated_depth_cannot_exceed_the_callers_budget() {
        let arguments = json!({"prompt":"too deep", "depth":4});
        let (_root, requests, session) = scripted_session([
            response(vec![tool_call(0, "agent-too-deep", "agent", arguments)]),
            answer("root done"),
        ])
        .await;
        assert_eq!(session.prompt("overdelegate").await.unwrap(), "root done");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "an over-budget child must not start");
        let results = last_tool_results(&requests[1]);
        assert!(results[0].is_error);
        let error = results[0].result["error"].as_str().unwrap();
        assert!(error.contains("available depth of 4"));
    }

    #[tokio::test]
    async fn leaf_children_cannot_invoke_agent_directly_or_from_scripts() {
        let script = json!({"source":"return tool.agent({prompt: 'escape'});"});
        let hidden = json!({"prompt":"try hidden delegation"});
        let (_root, requests, session) = scripted_session([
            response(vec![tool_call(0, "root-agent", "agent", hidden)]),
            response(vec![
                tool_call(0, "hidden-agent", "agent", json!({"prompt":"escape"})),
                tool_call(1, "script-agent", "script", script),
            ]),
            answer("child done"),
            answer("root done"),
        ])
        .await;
        assert_eq!(session.prompt("delegate").await.unwrap(), "root done");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4, "hidden delegation must not start agents");
        let results = last_tool_results(&requests[2]);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| result.is_error));
        let error = results[0].result["error"].as_str().unwrap();
        assert!(error.contains("unavailable"));
    }

    #[tokio::test]
    async fn launch_faults_never_install_a_live_agent() {
        use crate::session::AppendBoundary;
        for seed_fault in [false, true] {
            for boundary in AppendBoundary::ALL {
                let (_root, _, session) = scripted_session([]).await;
                let runtime = &session.runtime;
                let todos = Some(vec![todo("seed", crate::agent::TodoStatus::Pending)]);
                let id = runtime.next_child(&session.root).await;
                let launch = AgentLaunch {
                    todos,
                    ..child_launch(&session, id, None)
                };
                let launch = PreparedAgentLaunch::prepare(runtime.clone(), launch)
                    .await
                    .unwrap();
                let agent = launch.agent_loop.id.clone();
                let sender = launch.sender.clone();
                let worker = if seed_fault {
                    let store = &runtime.store;
                    let (reached, resume) =
                        store.pause_append_at(AppendBoundary::Publication).await;
                    let worker = tokio::spawn(launch.install());
                    bounded(reached).await.unwrap();
                    // Queue the next fault on the writer's FIFO mutex before letting
                    // AgentStarted commit; TodosReplaced is the next launch append.
                    let fault = runtime.store.fail_append_at(boundary);
                    tokio::pin!(fault);
                    assert!(futures_util::poll!(fault.as_mut()).is_pending());
                    resume.send(()).unwrap();
                    bounded(fault).await;
                    worker
                } else {
                    runtime.store.fail_append_at(boundary).await;
                    tokio::spawn(launch.install())
                };
                assert!(bounded(worker).await.unwrap().is_err());
                bounded(sender.closed()).await;
                assert!(!runtime.agents.read().unwrap().contains_key(&agent));
                let seeded = runtime.todos.inspect(&agent, None).await.unwrap();
                assert!(seeded.items.is_empty());
                // A failed append may leave a replayable prefix, never a live agent.
                let visible = runtime.store.records().await;
                let prefix = visible.iter().filter(|r| r.agent == agent).count();
                assert_eq!(prefix, usize::from(seed_fault));
                let _ = bounded(session.shutdown()).await;
            }
        }
    }
}
