//! Child-agent launch, tool dispatch, and model context selection.

use super::*;

impl SessionRuntime {
    pub(super) async fn spawn_agent(
        self: &Arc<Self>,
        launch: AgentLaunch,
    ) -> Result<AgentSender, HarnessError> {
        let AgentLaunch {
            id,
            owner_job,
            model_profile,
            todos,
            available_depth,
            location,
        } = launch;
        if id.depth() > self.harness.max_child_depth {
            return Err(HarnessError::ChildDepth);
        }
        let remaining_depth = self.harness.max_child_depth.saturating_sub(id.depth());
        if available_depth > remaining_depth {
            return Err(HarnessError::ChildDepth);
        }
        let capabilities = self.harness.capabilities.for_agent(available_depth);
        let (profile, system) = self
            .resolve_agent(
                &model_profile,
                &id,
                &location,
                available_depth,
                &capabilities,
            )
            .await?;
        let context = self
            .open_agent_context(&id, profile, system, &capabilities, true)
            .await?;
        self.store
            .append(
                id.clone(),
                SessionEvent::AgentStarted {
                    parent: id.parent(),
                    owner_job,
                    model_profile: model_profile.clone(),
                    max_context: Some(context.profile.max_context),
                    location: location.clone(),
                },
            )
            .await?;
        if let Some(job) = owner_job {
            self.jobs.set_agent_location(job, location.clone()).await?;
        }
        self.todos.register(id.clone(), owner_job, todos).await?;
        let (tx, rx) = mpsc::channel(AGENT_CHANNEL_CAPACITY);
        let tx = AgentSender::new(tx);
        {
            let mut agents = self
                .agents
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Serialize this check with shutdown's agent snapshot. A child whose
            // provider initialization raced shutdown must not leave an idle loop.
            if self
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(HarnessError::Interrupted);
            }
            agents.insert(
                id.clone(),
                LiveAgent {
                    model_profile: model_profile.clone(),
                    sender: tx.clone(),
                    cancellation: CancellationToken::new(),
                    available_depth,
                    completion_gate: Arc::new(Mutex::new(true)),
                },
            );
        }
        self.activity(&id, AgentActivity::Idle);
        let runtime = self.clone();
        tokio::spawn(async move {
            runtime
                .run_agent(AgentLoop {
                    id,
                    owner_job,
                    context,
                    model_profile,
                    location,
                    capabilities,
                    rx,
                })
                .await;
        });
        Ok(tx)
    }

    pub(super) async fn execute_call(
        &self,
        agent: &AgentId,
        parent: Option<JobId>,
        call: &ToolCall,
        origin: u64,
        location: &crate::execution::ExecutionLocation,
        capabilities: &CapabilitySet,
    ) -> ToolResult {
        let call = call.clone();
        let result = self
            .executor
            .clone()
            .with_location(location.clone())
            .with_capabilities(capabilities.clone())
            .with_model_origin(crate::session::ModelCallOrigin {
                message: origin,
                call_id: call.id.clone(),
            })
            .execute_model(agent.clone(), &call.name, call.arguments.clone(), parent)
            .await;
        let mut result = match result {
            Ok(result) => {
                let is_error = call.name != "job_output"
                    && result
                        .output
                        .value
                        .get("state")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|state| {
                            matches!(state, "failed" | "cancelled" | "interrupted")
                        });
                ToolResult {
                    call_id: call.id,
                    name: call.name,
                    result: result.output.value,
                    images: result.output.images,
                    is_error,
                }
            }
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
                    call_id: call.id,
                    name: call.name,
                    result,
                    images,
                    is_error: true,
                }
            }
        };
        // Commit the same compact presentation that the model and UI display.
        crate::job::omit_null_fields(&mut result.result);
        result
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

    pub(super) async fn open_agent_context(
        &self,
        agent: &AgentId,
        profile: ModelProfile,
        system: Vec<SystemSegment>,
        capabilities: &CapabilitySet,
        restore_meter: bool,
    ) -> Result<AgentContext, HarnessError> {
        let factory = self
            .harness
            .providers
            .get(&profile.provider)
            .ok_or_else(|| HarnessError::UnknownProvider(profile.provider.clone()))?;
        let template = ModelRequest {
            model: profile.model.clone(),
            system,
            messages: Vec::new(),
            tools: self
                .executor
                .clone()
                .with_capabilities(capabilities.clone())
                .surface_for_agent(agent)
                .definitions(),
            reasoning: profile.reasoning.clone(),
            response_schema: None,
            max_output_tokens: Some(profile.max_output),
            correlation: Some(agent.to_string()),
        };
        AgentContext::open(
            agent,
            profile,
            template,
            factory.as_ref(),
            &self.store.records().await,
            restore_meter,
        )
    }
}
impl SessionRuntime {
    pub(super) async fn next_child(&self, parent: &AgentId) -> AgentId {
        let mut counters = self.child_counters.write().await;
        let counter = counters.entry(parent.clone()).or_insert(0);
        *counter = counter.saturating_add(1);
        parent.child(*counter)
    }

    pub(super) fn available_depth(&self, agent: &AgentId) -> usize {
        self.agents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(agent)
            .map_or(0, |agent| agent.available_depth)
    }

    pub(super) fn agent_sender(&self, id: &AgentId) -> Option<AgentSender> {
        self.agents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .map(|agent| agent.sender.clone())
    }

    pub(super) fn begin_turn(&self, id: &AgentId) -> CancellationToken {
        let cancellation = CancellationToken::new();
        self.agents
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(id)
            .expect("running agents are registered")
            .cancellation = cancellation.clone();
        cancellation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;

    #[tokio::test]
    async fn committed_tool_history_omits_null_fields() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    response(vec![AssistantContent::tool_call(
                        "tool-0",
                        0,
                        ToolCall {
                            id: "script-call".into(),
                            name: "script".into(),
                            arguments: json!({"source": "return {error: null, nested: {absent: null, ok: false}, array: [null, 0]};"}),
                        },
                    )]),
                    response(vec![AssistantContent::text("answer", 0, "done")]),
                ],
            ),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("run").await.unwrap(), "done");
        let records = session.runtime.store.records().await;
        let results = records
            .iter()
            .find_map(|record| match &record.event {
                SessionEvent::MessageCommitted {
                    message: Message::Tool(results),
                } => Some(results),
                _ => None,
            })
            .expect("committed tool result");
        assert_eq!(
            results[0].result["result"]["value"],
            json!({
                "nested": {"ok": false},
                "array": [null, 0]
            })
        );
        let requests = requests.lock().unwrap();
        let Message::Tool(sent) = request_history(&requests[1]).last().unwrap() else {
            panic!("model tool result");
        };
        assert_eq!(sent, results);
    }

    #[tokio::test]
    async fn child_first_request_includes_parent_supplied_todos() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let todos = json!([
            {"text": "Inspect the implementation", "status": "completed"},
            {"text": "Make the change", "status": "in_progress"},
            {"text": "Run relevant checks", "status": "pending"},
            {"text": "Check \"quoted\" text\nand Unicode: café", "status": "pending"},
            {"text": "Keep the original order", "status": "completed"}
        ]);
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    response(vec![AssistantContent::tool_call(
                        "tool-0",
                        0,
                        ToolCall {
                            id: "delegate".to_owned(),
                            name: "agent".to_owned(),
                            arguments: json!({"prompt": "work", "todos": todos}),
                        },
                    )]),
                    response(vec![AssistantContent::text(
                        "answer",
                        0,
                        "child done".to_owned(),
                    )]),
                    response(vec![AssistantContent::text(
                        "answer",
                        0,
                        "root done".to_owned(),
                    )]),
                ],
            ),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("delegate").await.unwrap(), "root done");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let child_request = &requests[1];
        assert!(
            child_request.system[0]
                .text
                .starts_with(prompt::CHILD_PROMPT)
        );
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
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    response(vec![AssistantContent::tool_call(
                        "tool-0",
                        0,
                        ToolCall {
                            id: "agent-too-deep".to_owned(),
                            name: "agent".to_owned(),
                            arguments: json!({"prompt":"too deep", "depth":4}),
                        },
                    )]),
                    response(vec![AssistantContent::text(
                        "answer",
                        0,
                        "root done".to_owned(),
                    )]),
                ],
            ),
        )
        .await;

        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("overdelegate").await.unwrap(), "root done");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "an over-budget child must not start");
        let Message::Tool(results) = request_history(&requests[1]).last().unwrap() else {
            panic!("root must receive the failed agent result");
        };
        assert!(results[0].is_error);
        assert!(
            results[0].result["error"]
                .as_str()
                .is_some_and(|error| error.contains("available depth of 4"))
        );
    }

    #[tokio::test]
    async fn leaf_children_cannot_invoke_agent_directly_or_from_scripts() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(
                &requests,
                [
                    response(vec![AssistantContent::tool_call(
                        "tool-0",
                        0,
                        ToolCall {
                            id: "root-agent".to_owned(),
                            name: "agent".to_owned(),
                            arguments: json!({"prompt":"try hidden delegation"}),
                        },
                    )]),
                    response(vec![
                        AssistantContent::tool_call(
                            "tool-0",
                            0,
                            ToolCall {
                                id: "hidden-agent".to_owned(),
                                name: "agent".to_owned(),
                                arguments: json!({"prompt":"escape"}),
                            },
                        ),
                        AssistantContent::tool_call(
                            "tool-1",
                            1,
                            ToolCall {
                                id: "script-agent".to_owned(),
                                name: "script".to_owned(),
                                arguments: json!({
                                    "source":"return tool.agent({prompt: 'escape'});"
                                }),
                            },
                        ),
                    ]),
                    response(vec![AssistantContent::text(
                        "answer",
                        0,
                        "child done".to_owned(),
                    )]),
                    response(vec![AssistantContent::text(
                        "answer",
                        0,
                        "root done".to_owned(),
                    )]),
                ],
            ),
        )
        .await;

        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("delegate").await.unwrap(), "root done");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4, "hidden delegation must not start agents");
        let Message::Tool(results) = request_history(&requests[2]).last().unwrap() else {
            panic!("child must receive both failed tool results");
        };
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| result.is_error));
        assert!(
            results[0].result["error"]
                .as_str()
                .is_some_and(|error| error.contains("unavailable"))
        );
    }
}
