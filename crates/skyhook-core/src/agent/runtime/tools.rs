//! Agent-specific tools layered on top of the general coding tool set.

use std::{
    path::PathBuf,
    sync::{Arc, OnceLock, Weak},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::{
    agent::{Question, TodoItem, todo::TodoStore},
    provider::protocol::UserContent,
    session::SessionEvent,
    tool::{RegistryError, ToolError, ToolOptions, ToolRegistryBuilder, policy::Capability},
};

use super::{AgentCommand, AgentLaunch, QueuedPromptToken, SessionRuntime, queue::QueuedInput};

/// Connect only root-eligible MCP servers; adapters enforce per-agent gates later.
pub(super) async fn connect_mcp(
    builder: &mut ToolRegistryBuilder,
    harness: &super::HarnessInner,
    store: &crate::session::SessionStore,
) -> (Arc<crate::mcp::manager::McpManager>, Vec<String>) {
    let capabilities = harness.capabilities.for_agent(harness.max_child_depth);
    let configs = harness
        .mcp
        .iter()
        .filter(|(_, config)| {
            capabilities.contains(Capability::Mcp)
                && config
                    .capabilities
                    .iter()
                    .all(|cap| capabilities.contains(*cap))
        })
        .map(|(name, config)| (name.clone(), config.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let manager = Arc::new(
        crate::mcp::manager::McpManager::connect(
            &configs,
            tokio_util::sync::CancellationToken::new(),
        )
        .await,
    );
    let mut warnings = manager.warnings().to_vec();
    warnings.extend(crate::mcp::adapter::register(
        builder,
        manager.clone(),
        &configs,
        store.clone(),
    ));
    (manager, warnings)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AgentArgs {
    /// Task and context for the child.
    pub(super) prompt: String,
    /// Initial todo items, using the same format as todo.items.
    pub(super) todos: Option<Vec<TodoItem>>,
    /// Delegation depth available to the child; must be less than your available_depth.
    #[serde(default)]
    pub(super) depth: usize,
    /// Model override; omitted/null inherits the parent's active model.
    pub(super) model: Option<String>,
    /// Execution target; defaults to the parent's.
    #[schemars(skip)]
    pub(super) target: Option<String>,
    /// Child workspace override: absolute, or relative to the workspace selected by target.
    pub(super) workspace: Option<PathBuf>,
}

#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
enum TodoOutput {
    Updated { updated: bool },
    Items { items: Vec<TodoItem> },
}

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
) -> Result<(), RegistryError> {
    register_wait(builder, runtime_slot.clone())?;
    register_ask(builder, runtime_slot.clone())?;
    register_todo(builder, runtime_slot.clone())?;
    register_child_agent(builder, runtime_slot)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TodoArgs {
    /// Replace your entire list. An empty array clears it. Cannot be combined with job.
    items: Option<Vec<TodoItem>>,
    /// Inspect a descendant's list using its agent job ID. Omit to read your own list.
    job: Option<crate::identity::JobId>,
}

fn register_todo(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
) -> Result<(), RegistryError> {
    builder.register::<TodoArgs, TodoOutput, _, _>(
        "todo",
        "Read your list, replace it with items, or inspect a descendant by agent job ID. Only the owner can edit.",
        ToolOptions::default(),
        move |context, input| {
            let runtime = runtime_slot.get().and_then(Weak::upgrade);
            async move {
                let runtime = runtime.ok_or_else(runtime_unavailable)?;
                if let Some(items) = input.items {
                    if input.job.is_some() {
                        return Err(ToolError::InvalidArguments("items and job cannot be combined".to_owned()));
                    }
                    TodoStore::validate(&items)?;
                    runtime.todos.replace(&context.agent, items).await.map_err(|error| tool_error(&error))?;
                    Ok(TodoOutput::Updated { updated: true })
                } else {
                    Ok(TodoOutput::Items { items: runtime.todos.inspect(&context.agent, input.job).await?.items })
                }
            }
        },
    )?;
    Ok(())
}

fn register_wait(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
) -> Result<(), RegistryError> {
    builder.register::<super::wait::WaitArgs, super::wait::WaitOutput, _, _>(
        "wait",
        "Wait for any notification or input relevant to this agent. Optional timeout is a positive integer number of seconds; omitted/null waits indefinitely. Returns {reason: event|timeout}. Does not consume notifications or retrieve output; use job_output to inspect saved output. Cancellation interrupts the wait. A timeout ends this wait, not background work; wait again if still dependent on it.",
        ToolOptions::default(),
        move |context, args| {
            let runtime = runtime_slot.get().and_then(Weak::upgrade);
            async move {
                runtime.ok_or_else(runtime_unavailable)?.wait_for_event(&context, args).await
            }
        },
    )?;
    Ok(())
}

fn register_ask(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
) -> Result<(), RegistryError> {
    builder.register::<Question, Value, _, _>(
        "ask",
        "Ask one structured question. Issue independent questions concurrently; the runtime merges calls that become ready together. Use bg:true to continue independent work while awaiting an answer; inspect the returned job with job_output.",
        ToolOptions::default().background().input().requires_for_root(Capability::Interactive),
        move |context, input| {
            let runtime = runtime_slot.get().and_then(Weak::upgrade);
            async move {
                let runtime = runtime.ok_or_else(runtime_unavailable)?;
                let question_id = format!("q-{}", context.job);
                let questions = vec![input.clone()];
                runtime
                    .store
                    .append(
                        context.agent.clone(),
                        SessionEvent::QuestionOpened {
                            job: context.job,
                            question_id: question_id.clone(),
                            questions: serde_json::to_value(&questions)?,
                        },
                    )
                    .await
                    .map_err(|error| tool_error(&error))?;
                let answers = runtime.questions.coordinate_question(context.clone(), input).await?;
                runtime
                    .store
                    .append(
                        context.agent,
                        SessionEvent::QuestionResolved {
                            job: context.job,
                            question_id,
                            answers: answers.clone(),
                        },
                    )
                    .await
                    .map_err(|error| tool_error(&error))?;
                Ok(answers)
            }
        },
    )?;
    Ok(())
}

fn register_child_agent(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
) -> Result<(), RegistryError> {
    builder.register::<AgentArgs, String, _, _>(
        "agent",
        "Start a child agent. Send follow-ups or answers with tool.job(id).send({value: ...}). Questions pause the child; follow-ups arrive automatically at its next model-request boundary. Replies arrive as events. Sending input to a completed child resumes its retained history under the same job ID.",
        ToolOptions::default()
            .named()
            .requires(Capability::Agents)
            .conditional_input(
                "target",
                Capability::Targets,
                json!({
                    "type": ["string", "null"],
                    "description": "Child target; omitted inherits."
                }),
            )
            .background()
            .input(),
        move |context, input| {
            let runtime = runtime_slot.get().and_then(Weak::upgrade);
            async move {
                let runtime = runtime.ok_or_else(runtime_unavailable)?;
                let available_depth = runtime.available_depth(&context.agent);
                let todos = input.todos;
                if let Some(items) = &todos {
                    TodoStore::validate(items)?;
                }
                if input.depth >= available_depth {
                    return Err(ToolError::InvalidArguments(format!(
                        "depth must be less than the caller's available depth of {available_depth}"
                    )));
                }
                let child = runtime.next_child(&context.agent).await;
                let model = input.model
                    .or_else(|| runtime.agents.read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&context.agent).map(|agent| agent.model_profile.clone()))
                    .ok_or_else(|| ToolError::Failed("parent agent is no longer running".into()))?;
                let target = input.target.as_deref().unwrap_or(&context.caller_location.target);
                let definition = if target == crate::target::ROOT_TARGET {
                    None
                } else {
                    Some(runtime.router.targets().get(target).await.map_err(|error| tool_error(&error))?)
                };
                let mut location = crate::execution::ExecutionLocation::select(
                    &context.caller_location,
                    &runtime.harness.workspace,
                    input.target.as_deref(),
                    definition.as_ref().map(|definition| definition.workspace.as_path()),
                );
                if let Some(workspace) = input.workspace {
                    if workspace.as_os_str().is_empty() {
                        return Err(ToolError::InvalidArguments("workspace cannot be empty".to_owned()));
                    }
                    location.workspace = location.workspace.join(workspace);
                }
                let sender = runtime.spawn_agent(AgentLaunch {
                    id: child.clone(),
                    owner_job: Some(context.job),
                    model_profile: model,
                    todos,
                    available_depth: input.depth,
                    location,
                }).await.map_err(|error| tool_error(&error))?;
                // Only this live child session may opt its completed job into resumption.
                // Keep a weak runtime reference: jobs must not retain their own manager.
                let resume_runtime = Arc::downgrade(&runtime);
                let resume_child = child.clone();
                let resume_sender = sender.clone();
                let authorization = context.authorization.clone();
                let execution_location = context.execution_location.clone();
                let caller_location = context.caller_location.clone();
                runtime.jobs.set_resume_handler(context.job, Arc::new(move |value, input| {
                    let runtime = resume_runtime.upgrade();
                    let child = resume_child.clone();
                    let sender = resume_sender.clone();
                    let authorization = authorization.clone();
                    let execution_location = execution_location.clone();
                    let caller_location = caller_location.clone();
                    Box::pin(async move {
                        let runtime = runtime.ok_or_else(runtime_unavailable)?;
                        if runtime.shutting_down.load(std::sync::atomic::Ordering::Acquire) {
                            return Err(ToolError::Cancelled);
                        }
                        let context = crate::tool::ToolContext::new(
                            authorization, execution_location, caller_location, input, runtime.jobs.clone(),
                        );
                        let text = run_child_request(
                            &runtime, &context, &child, &sender,
                            vec![UserContent::ParentInput { text: format!("Owner input: {value}") }],
                        ).await?;
                        Ok(crate::tool::ToolOutput::new(json!(text)))
                    })
                })).await.map_err(|error| tool_error(&error))?;
                run_child_request(
                    &runtime, &context, &child, &sender,
                    vec![UserContent::Text { text: input.prompt }],
                ).await
            }
        },
    )?;
    Ok(())
}

async fn run_child_request(
    runtime: &Arc<SessionRuntime>,
    context: &crate::tool::ToolContext,
    child: &crate::identity::AgentId,
    sender: &super::AgentSender,
    content: Vec<UserContent>,
) -> Result<String, ToolError> {
    let completion_gate = runtime
        .agents
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(child)
        .ok_or_else(|| ToolError::Failed("child agent stopped".into()))?
        .completion_gate
        .clone();
    *completion_gate.lock().await = true;
    let (done_tx, mut done_rx) = oneshot::channel();
    sender
        .send(AgentCommand::Input {
            model: None,
            content,
            done: Some(done_tx),
        })
        .await
        .map_err(|_| ToolError::Failed("child agent stopped".to_owned()))?;
    loop {
        tokio::select! {
            result = &mut done_rx => {
                let text = result
                    .map_err(|_| ToolError::Failed("child agent stopped".to_owned()))?
                    .map_err(ToolError::Failed)?;
                let mut content = Vec::new();
                for value in context.drain_input_or_close().await {
                    content.push(UserContent::ParentInput { text: format!("Owner input: {value}") });
                }
                if content.is_empty() { return Ok(text); }
                let (done, next) = oneshot::channel();
                done_rx = next;
                *completion_gate.lock().await = true;
                sender.send(AgentCommand::Input { model: None, content, done: Some(done) }).await
                    .map_err(|_| ToolError::Failed("child agent stopped".into()))?;
            }
            value = context.receive() => {
                let value = match value {
                    Ok(value) => value,
                    Err(error) => {
                        runtime.questions.cancel_child_question(context.job).await;
                        runtime.interrupt_tree(child).await;
                        return Err(error);
                    }
                };
                if !runtime.questions.answer_child_question(context.job, value.clone()).await
                    .map_err(|error| tool_error(&error))?
                {
                    let mut active = completion_gate.lock().await;
                    if !*active {
                        let (done, next) = oneshot::channel();
                        done_rx = next;
                        *active = true;
                        sender.send(AgentCommand::Input {
                            model: None,
                            content: vec![UserContent::ParentInput { text: format!("Owner input: {value}") }],
                            done: Some(done),
                        }).await.map_err(|_| ToolError::Failed("child agent stopped".into()))?;
                        continue;
                    }
                    // Owner updates use the same request-boundary mailbox as
                    // queued root prompts, rather than waiting for a new turn.
                    let (committed, _receipt) = oneshot::channel();
                    sender.send(AgentCommand::QueuedInputs(vec![QueuedInput {
                        model: None,
                        content: vec![UserContent::ParentInput { text: format!("Owner input: {value}") }],
                        token: QueuedPromptToken::new(),
                        committed,
                    }])).await.map_err(|_| ToolError::Failed("child agent stopped".to_owned()))?;
                }
            }
        }
    }
}

fn runtime_unavailable() -> ToolError {
    ToolError::Failed("session runtime is unavailable".to_owned())
}

fn tool_error(error: &impl ToString) -> ToolError {
    ToolError::Failed(error.to_string())
}

#[cfg(test)]
mod tests {
    use crate::agent::runtime::tests::*;
    use crate::{execution::ExecutionLocation, mcp::McpServerConfig, tool::policy::CapabilitySet};
    use std::{collections::BTreeMap, path::Path, sync::Mutex as StdMutex, time::Duration};

    fn builder(root: &Path, requests: Arc<StdMutex<Vec<ModelRequest>>>) -> HarnessBuilder {
        test_builder(
            root,
            &root.join("sessions"),
            scripted_provider(&requests, [answer("done"), answer("done")]),
        )
        .max_child_depth(1)
    }
    const FIXTURE: &str = r#"
import json, os, sys
with open(os.environ['MCP_RUNTIME_MARKER'], 'a') as marker:
    marker.write('launched\n')
for line in sys.stdin:
    req = json.loads(line)
    if 'id' not in req:
        continue
    method = req['method']
    if method == 'initialize':
        result = {'protocolVersion':req['params']['protocolVersion'],
                  'capabilities':{'tools':{}},
                  'serverInfo':{'name':'runtime-fixture','version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[{'name':'echo', 'description':'runtime echo',
                  'inputSchema':{'type':'object', 'properties':{'text':{'type':'string'}},
                                 'required':['text'], 'additionalProperties':False}}]}
    elif method == 'tools/call':
        result = {'content':[], 'structuredContent':req['params']['arguments'], 'isError':False}
    else:
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}), flush=True)
"#;

    fn stdio_config(root: &Path, capabilities: Vec<Capability>) -> McpServerConfig {
        serde_json::from_value(json!({
            "transport":"stdio",
            "start_command":["python3", "-u", "-c", FIXTURE],
            "env":{"MCP_RUNTIME_MARKER":root.join("launched")},
            "capabilities":capabilities,
            "startup_timeout_secs":5,
            "call_timeout_secs":5
        }))
        .unwrap()
    }

    fn servers(config: McpServerConfig) -> BTreeMap<String, McpServerConfig> {
        BTreeMap::from([("fixture".into(), config)])
    }

    fn mcp_name(session: &SessionHandle) -> String {
        let names = session
            .runtime
            .executor
            .registry()
            .tools()
            .filter(|tool| tool.name().starts_with("mcp_"))
            .map(|tool| tool.name().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 1, "{:?}", session.startup_warnings());
        names[0].clone()
    }

    #[tokio::test]
    async fn mcp_gates_prevent_stdio_launch_and_http_contact() {
        // Required permissions, effective root depth, and the unconditional MCP gate
        // all apply before transport startup, even with an approve-all policy.
        let mut cases = Vec::new();
        for missing in Capability::ALL {
            let mut capabilities = Capability::ALL.into_iter().collect::<CapabilitySet>();
            capabilities.remove(missing);
            cases.push((capabilities, Capability::ALL.to_vec(), 1));
        }
        cases.push((CapabilitySet::default(), vec![Capability::Agents], 0));
        let mut without_mcp = CapabilitySet::default();
        without_mcp.remove(Capability::Mcp);
        cases.push((without_mcp, vec![], 1));
        cases.push((CapabilitySet::empty(), vec![], 1));
        for (capabilities, required, depth) in cases {
            let root = tempfile::tempdir().unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let http = serde_json::from_value(json!({
                "transport":"streamable_http",
                "url":format!("http://{}/mcp", listener.local_addr().unwrap()),
                "capabilities":required, "startup_timeout_secs":1
            }))
            .unwrap();
            let harness = builder(root.path(), Arc::default())
                .capabilities(capabilities)
                .max_child_depth(depth)
                .policy(Arc::new(crate::tool::policy::AllowAll))
                .mcp(BTreeMap::from([
                    ("stdio".into(), stdio_config(root.path(), required)),
                    ("http".into(), http),
                ]))
                .build()
                .await
                .unwrap();
            let session = harness.new_session().await.unwrap();
            assert!(session.startup_warnings().is_empty());
            assert!(
                !session
                    .runtime
                    .executor
                    .registry()
                    .tools()
                    .any(|tool| tool.name().starts_with("mcp_"))
            );
            shutdown_session(session).await;
            assert!(!root.path().join("launched").exists());
            assert!(
                tokio::time::timeout(Duration::from_millis(30), listener.accept())
                    .await
                    .is_err()
            );
        }
    }
    #[tokio::test]
    async fn empty_capability_requirements_expose_direct_and_script_tools() {
        let root = tempfile::tempdir().unwrap();
        let provider: Arc<StdMutex<Vec<ModelRequest>>> = Arc::default();
        // Empty per-server requirements still need the global gate, but neither
        // Read nor Exec is implicitly required to use an MCP connection.
        let capabilities = [Capability::Mcp].into_iter().collect::<CapabilitySet>();
        let harness = builder(root.path(), provider.clone())
            .capabilities(capabilities.clone())
            .mcp(servers(stdio_config(root.path(), vec![])))
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        assert!(session.startup_warnings().is_empty());
        let name = mcp_name(&session);
        let executor = session
            .runtime
            .executor
            .clone()
            .with_capabilities(capabilities);
        assert_eq!(session.prompt("list your tools").await.unwrap(), "done");
        assert!(
            provider.lock().unwrap()[0]
                .tools
                .iter()
                .any(|tool| tool.name == name)
        );
        let direct = executor
            .execute(session.root.clone(), &name, json!({"text":"direct"}), None)
            .await
            .unwrap();
        assert_eq!(
            direct.output.value["structuredContent"],
            json!({"text":"direct"})
        );
        let script = session
            .run_script(format!("return await tool.{name}({{text:'script'}});"))
            .await
            .unwrap();
        assert_eq!(
            script.value["value"]["structuredContent"],
            json!({"text":"script"})
        );
        // Also test a discovered adapter with empty server requirements. Startup
        // omission alone would not catch a missing adapter-level global gate.
        let disabled = executor.with_capabilities(CapabilitySet::empty());
        assert!(disabled.surface().get(&name).is_none());
        assert!(
            disabled
                .execute(session.root.clone(), &name, json!({"text":"denied"}), None)
                .await
                .is_err()
        );
        assert!(
            disabled
                .execute_script(session.root.clone(), &name, json!({"text":"denied"}), None)
                .await
                .is_err()
        );
        let script = disabled
            .execute(
                session.root.clone(),
                "script",
                json!({"source":format!("return typeof tool.{name};")}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(script.output.value["value"], "undefined");
        shutdown_session(session).await;
        assert_eq!(
            std::fs::read_to_string(root.path().join("launched")).unwrap(),
            "launched\n"
        );
    }

    #[tokio::test]
    async fn depth_zero_child_omits_agent_gated_mcp_without_reconnecting() {
        let root = tempfile::tempdir().unwrap();
        let provider: Arc<StdMutex<Vec<ModelRequest>>> = Arc::default();
        let harness = builder(root.path(), provider.clone())
            .max_child_depth(1)
            .mcp(servers(stdio_config(root.path(), vec![Capability::Agents])))
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let name = mcp_name(&session);
        session.prompt("root request").await.unwrap();
        let child = session
            .run_script("return await tool.agent({prompt:'child request', depth:0});")
            .await
            .unwrap();
        assert_eq!(child.value["value"], "done");
        {
            let requests = provider.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests[0].tools.iter().any(|tool| tool.name == name));
            assert!(!requests[1].tools.iter().any(|tool| tool.name == name));
        }
        let child_executor = session
            .runtime
            .executor
            .clone()
            .with_capabilities(session.runtime.harness.capabilities.for_agent(0));
        assert!(
            child_executor
                .execute(session.root.clone(), &name, json!({"text":"denied"}), None)
                .await
                .is_err()
        );
        let script = child_executor
            .execute(
                session.root.clone(),
                "script",
                json!({"source":format!("return typeof tool.{name};")}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(script.output.value["value"], "undefined");
        shutdown_session(session).await;
        assert_eq!(
            std::fs::read_to_string(root.path().join("launched")).unwrap(),
            "launched\n"
        );
    }

    #[tokio::test]
    async fn mcp_stays_on_host_for_remote_location() {
        let root = tempfile::tempdir().unwrap();
        let harness = builder(root.path(), Arc::default())
            .mcp(servers(stdio_config(root.path(), vec![])))
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let name = mcp_name(&session);
        // This location has no route/worker. A targeted dispatch would fail; Host
        // dispatch must still use the session-owning process and its MCP manager.
        let remote = session
            .runtime
            .executor
            .clone()
            .with_location(ExecutionLocation::named(
                "unconnected-remote",
                PathBuf::from("/remote/workspace"),
            ));
        let output = remote
            .execute(session.root.clone(), &name, json!({"text":"host"}), None)
            .await
            .unwrap();
        assert_eq!(output.output.value["structuredContent"]["text"], "host");
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn failed_startup_is_reported_without_breaking_the_session() {
        let root = tempfile::tempdir().unwrap();
        let mut config = stdio_config(root.path(), vec![]);
        config.start_command = Some(vec![
            root.path()
                .join("nonexistent-mcp")
                .to_string_lossy()
                .into_owned(),
        ]);
        let harness = builder(root.path(), Arc::default())
            .mcp(servers(config))
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.startup_warnings().len(), 1);
        assert!(session.startup_warnings()[0].contains("fixture"));
        assert!(
            !session
                .runtime
                .executor
                .registry()
                .tools()
                .any(|tool| tool.name().starts_with("mcp_"))
        );
        assert_eq!(session.prompt("still usable").await.unwrap(), "done");
        shutdown_session(session).await;
    }

    async fn terminal(
        session: &SessionHandle,
        job: crate::identity::JobId,
    ) -> crate::job::JobEnvelope {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !session
                .runtime
                .jobs
                .snapshot(job)
                .await
                .unwrap()
                .state
                .is_terminal()
            {
                tokio::task::yield_now().await;
            }
            session.runtime.jobs.wait(job, None, true).await.unwrap()
        })
        .await
        .expect("tool test stalled")
    }

    #[tokio::test]
    async fn root_interactive_gate_covers_dispatch_and_nested_scripts() {
        for enabled in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let requests: Arc<StdMutex<Vec<ModelRequest>>> = Arc::default();
            let batches: Arc<StdMutex<Vec<Vec<Question>>>> = Arc::default();
            let mut capabilities = CapabilitySet::default();
            if !enabled {
                capabilities.remove(Capability::Interactive);
            }
            let harness = builder(root.path(), requests.clone())
                .capabilities(capabilities.clone())
                .question_handler(Arc::new(RecordingQuestions {
                    batches: batches.clone(),
                    answer: json!("host-answer"),
                }))
                .build()
                .await
                .unwrap();
            let session = harness.new_session().await.unwrap();
            let executor = session
                .runtime
                .executor
                .clone()
                .with_capabilities(capabilities);
            session.prompt("list tools").await.unwrap();
            assert_eq!(
                requests.lock().unwrap()[0]
                    .tools
                    .iter()
                    .any(|tool| tool.name == "ask"),
                enabled
            );
            assert_eq!(
                session
                    .run_script("return typeof tool.ask;")
                    .await
                    .unwrap()
                    .value["value"],
                if enabled { "function" } else { "undefined" }
            );
            for background in [false, true] {
                for route in ["host", "model", "script"] {
                    let args = json!({"id":"root", "prompt":"question", "bg":background});
                    let result = tokio::time::timeout(Duration::from_secs(5), async {
                        match route {
                            "host" => {
                                executor
                                    .execute(session.root.clone(), "ask", args, None)
                                    .await
                            }
                            "model" => {
                                executor
                                    .execute_model(session.root.clone(), "ask", args, None)
                                    .await
                            }
                            _ => {
                                executor
                                    .execute_script(session.root.clone(), "ask", args, None)
                                    .await
                            }
                        }
                    })
                    .await
                    .unwrap();
                    if enabled {
                        let job = result.unwrap().job;
                        let output = terminal(&session, job).await;
                        assert_eq!(output.output, Some(json!("host-answer")));
                    } else {
                        assert!(result.unwrap_err().to_string().contains("unavailable"));
                    }
                }
                // Nested root asks cannot escape the gate through script jobs.
                let result = executor
                    .execute(
                        session.root.clone(),
                        "script",
                        json!({
                            "source":"return await tool.ask({id:'nested', prompt:'question'});",
                            "bg":background,
                        }),
                        None,
                    )
                    .await;
                if enabled || background {
                    let output = terminal(&session, result.unwrap().job).await;
                    assert_eq!(
                        output.state,
                        if enabled {
                            crate::job::JobState::Completed
                        } else {
                            crate::job::JobState::Failed
                        }
                    );
                    if enabled {
                        assert_eq!(output.output.unwrap()["value"], "host-answer");
                    }
                } else {
                    assert!(matches!(
                        result,
                        Err(crate::tool::executor::ExecutionError::Failed { .. })
                    ));
                }
            }
            assert_eq!(batches.lock().unwrap().len(), if enabled { 8 } else { 0 });
            if !enabled {
                assert!(
                    session
                        .inspect_jobs(&session.root)
                        .await
                        .iter()
                        .all(|job| job.tool != "ask")
                );
            }
            shutdown_session(session).await;
        }
    }
}
