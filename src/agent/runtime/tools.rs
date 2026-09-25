//! Agent-specific tools layered on top of the general coding tool set.
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, OnceLock, Weak},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::{
    agent::{Question, TodoItem, todo::TodoStore},
    provider::profile::ModelProfile,
    session::UserPart,
    tool::{
        RegistryError, ToolError, ToolOptions, ToolRegistryBuilder,
        diagnostic::{Effects, FailureSite, Operation, Subject},
        policy::{Capability, CapabilitySet, Mode},
    },
};

use super::{AgentCommand, AgentLaunch, SessionRuntime, TurnFailure, queue::QueuedInput};

/// Connect only root-eligible MCP servers; adapters enforce per-agent gates later.
pub(super) async fn connect_mcp(
    builder: &mut ToolRegistryBuilder,
    harness: &super::HarnessInner,
    capabilities: &crate::tool::policy::CapabilitySet,
    store: &crate::session::SessionStore,
) -> (Arc<crate::mcp::manager::McpManager>, Vec<String>) {
    let capabilities = capabilities.for_agent(harness.max_child_depth);
    let manager = Arc::new(
        crate::mcp::manager::McpManager::connect(
            &harness.mcp,
            &capabilities,
            tokio_util::sync::CancellationToken::new(),
        )
        .await,
    );
    let mut warnings = manager.warnings().to_vec();
    warnings.extend(crate::mcp::adapter::register(
        builder,
        manager.clone(),
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
    #[schemars(skip)]
    pub(super) model: Option<String>,
    /// Mode for the child; omitted/null inherits the parent's capabilities.
    #[schemars(skip)]
    pub(super) mode: Option<String>,
    /// Execution target; defaults to the parent's.
    #[schemars(skip)]
    pub(super) target: Option<crate::target::TargetRef>,
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
    models: &BTreeMap<String, ModelProfile>,
    modes: &indexmap::IndexMap<String, Mode>,
) -> Result<(), RegistryError> {
    register_wait(builder, runtime_slot.clone())?;
    register_ask(builder, runtime_slot.clone())?;
    register_todo(builder, runtime_slot.clone())?;
    register_child_agent(builder, runtime_slot, models, modes)
}

/// Whether an agent holding `capabilities` may give a child this mode: it carries a
/// hint and grants nothing the agent lacks.
fn offers_mode(mode: &Mode, capabilities: &CapabilitySet) -> bool {
    let held = |capability: &Capability| capabilities.contains(*capability);
    mode.hint.is_some() && mode.capabilities.iter().all(held)
}

/// A nullable choice among `(name, description)` pairs; None without any.
fn choice_input(summary: &str, choices: &[(&str, String)]) -> Option<Value> {
    if choices.is_empty() {
        return None;
    }
    let names = choices.iter().map(|(name, _)| json!(name));
    let names: Vec<Value> = names.chain([Value::Null]).collect();
    let lines = choices
        .iter()
        .map(|(name, text)| format!("\n- {name}{text}"));
    let description = format!("{summary}{}", lines.collect::<String>());
    Some(json!({"type": ["string", "null"], "enum": names, "description": description}))
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
                        return Err(invalid_argument("job", "items and job cannot be combined"));
                    }
                    TodoStore::validate(&items)?;
                    runtime.todos.replace(context.agent(), items).await.map_err(|error| {
                        harness_error(error.into())
                            .operation(Operation::Save, Subject::Label(format!("todos for agent {}", context.agent()))).effects(Effects::Unknown)
                    })?;
                    Ok(TodoOutput::Updated { updated: true })
                } else {
                    Ok(TodoOutput::Items { items: runtime.todos.inspect(context.agent(), input.job).await?.items })
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
        "Wait for any notification or input relevant to this agent. Optional timeout is a positive integer number of seconds; omitted/null waits indefinitely. Returns {reason: event|timeout}. Does not consume notifications or retrieve output; use jobs to inspect saved output. Cancellation interrupts the wait. A timeout ends this wait, not background work; wait again if still dependent on it.",
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
        "Ask one structured question. Issue independent questions concurrently; the runtime merges calls that become ready together. Use bg:true to continue independent work while awaiting an answer; inspect the returned job with jobs.",
        ToolOptions::default().job_role(crate::job::JobRole::Question).background().input().requires_for_root(Capability::Interactive),
        move |context, input| {
            let runtime = runtime_slot.get().and_then(Weak::upgrade);
            async move {
                let runtime = runtime.ok_or_else(runtime_unavailable)?;
                runtime.questions.coordinate_question(context, input).await
            }
        },
    )?;
    Ok(())
}

fn register_child_agent(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
    models: &BTreeMap<String, ModelProfile>,
    modes: &indexmap::IndexMap<String, Mode>,
) -> Result<(), RegistryError> {
    let hinted = |(name, profile): (&String, &ModelProfile)| {
        let hint = profile.hint.as_ref()?;
        Some((name.clone(), format!(": {hint}")))
    };
    let models: Vec<_> = models.iter().filter_map(hinted).collect();
    let modes = modes.clone();
    builder.register::<AgentArgs, String, _, _>(
        "agent",
        "Start a child agent. Names are unique among your own installed children, including terminal children; other callers may reuse the same names. Send follow-ups or answers with tool.job(id).send({value: ...}). Questions pause the child; follow-ups arrive automatically at its next model-request boundary. Replies arrive as events. Sending input to a completed child resumes its retained history under the same job ID.",
        ToolOptions::default().job_role(crate::job::JobRole::Agent)
            .named()
            .requires(Capability::Agents)
            .conditional_input("target", Capability::Targets, crate::target::TargetRef::schema())
            .computed_input("model", move |_| {
                let choices: Vec<_> = models.iter().map(|(name, text)| (name.as_str(), text.clone())).collect();
                choice_input("Model for the child; omitted inherits yours.", &choices)
            })
            .computed_input("mode", move |capabilities| {
                let offered = modes.iter().filter(|(_, mode)| offers_mode(mode, capabilities));
                let choices: Vec<_> = offered
                    .map(|(name, mode)| {
                        let granted: Vec<_> = mode.capabilities.iter().map(|capability| capability.as_str()).collect();
                        let granted = if granted.is_empty() { "none".to_owned() } else { granted.join(", ") };
                        let hint = mode.hint.as_deref().unwrap_or_default();
                        (name.as_str(), format!(" [{granted}]: {hint}"))
                    })
                    .collect();
                choice_input("Mode limiting what the child can do; omitted inherits what you can.", &choices)
            })
            .background()
            .input(),
        move |context, input| {
            let runtime = runtime_slot.get().and_then(Weak::upgrade);
            async move {
                let runtime = runtime.ok_or_else(runtime_unavailable)?;
                let hinted = |name: &String| runtime.harness.model_profiles.get(name).is_some_and(|profile| profile.hint.is_some());
                if let Some(name) = input.model.as_ref().filter(|name| !hinted(name)) {
                    return Err(invalid_argument("model", format!("unknown model `{name}`")));
                }
                let capabilities = match &input.mode {
                    None => context.capabilities().clone(),
                    Some(name) => {
                        let offered = |mode: &&Mode| offers_mode(mode, context.capabilities());
                        if runtime.modes.get(name).filter(offered).is_none() {
                            return Err(invalid_argument("mode", format!("unknown mode `{name}`")));
                        }
                        // Interaction follows the caller, as the root's follows the host.
                        let granted = runtime.mode_capabilities(name).map_err(|error| harness_error(error)
                            .operation(Operation::Prepare, Subject::argument(["mode"])).effects(Effects::NotStarted))?;
                        &granted & context.capabilities()
                    }
                };
                let available_depth = runtime.available_depth(context.agent());
                let todos = input.todos;
                if let Some(items) = &todos {
                    TodoStore::validate(items)?;
                }
                if input.depth >= available_depth {
                    return Err(invalid_argument("depth", format!(
                        "depth must be less than the caller's available depth of {available_depth}"
                    )));
                }
                // A reused name is a follow-up aimed at the wrong tool.
                if let Some(name) = runtime.jobs.metadata(context.job()).await.map_err(|error| harness_error(error.into())
                    .operation(Operation::Inspect, Subject::Job(context.job())).effects(Effects::Unchanged))?.name
                    && let Some(id) = runtime.jobs.child_name_owner(context.agent(), &name, context.job()).await
                {
                    return Err(ToolError::invalid_arguments(format!(
                        "child `{name}` already exists as job {id}; message it with tool.job({id}).send({{value: ...}}), or pick another name"
                    )).operation(Operation::Validate, Subject::Job(id)).effects(Effects::NotStarted));
                }
                let child = runtime.next_child(context.agent()).await;
                let model = input.model
                    .or_else(|| runtime.agents()
                        .get(context.agent()).map(|agent| agent.model_profile.clone()))
                    .ok_or_else(|| ToolError::failed("parent agent is no longer running")
                        .operation(Operation::Lookup, Subject::Label(format!("parent agent {}", context.agent()))).effects(Effects::NotStarted))?;
                let mut location = crate::target::select_location(
                    context.caller_location(),
                    &runtime.harness.workspace,
                    input.target.as_ref(),
                    context.capabilities(),
                    Some(&runtime.router),
                ).await.map_err(|error| error
                    .operation(Operation::Lookup, Subject::argument(["target"])).effects(Effects::NotStarted))?
                    .location;
                if let Some(workspace) = input.workspace {
                    if workspace.as_os_str().is_empty() {
                        return Err(invalid_argument("workspace", "workspace cannot be empty"));
                    }
                    location.workspace = location.workspace.join(workspace);
                }
                let sender = runtime.spawn_agent(AgentLaunch {
                    id: child.clone(),
                    owner_job: Some(context.job()),
                    model_profile: model,
                    todos,
                    available_depth: input.depth,
                    location,
                    mode: input.mode,
                    capabilities,
                }).await.map_err(|error| harness_error(error)
                    .operation(Operation::Create, Subject::Label(format!("child agent {child}"))).effects(Effects::Unknown))?;
                // Associate immediately so a failed first turn is selectable for retry
                // even when it never emitted visible assistant text.
                runtime.jobs.set_child_agent(context.job(), child.clone()).await.map_err(|error| harness_error(error.into())
                    .operation(Operation::Save, Subject::Job(context.job())).effects(Effects::Started))?;
                // Only this child session may opt its terminal job into resumption.
                let handler = child_resume_handler(
                    Arc::downgrade(&runtime),
                    child.clone(),
                    context.job_subject().clone(),
                    context.execution_location().clone(),
                    context.caller_location().clone(),
                );
                runtime.jobs.set_resume_handler(context.job(), handler).await.map_err(|error| harness_error(error.into())
                    .operation(Operation::Prepare, Subject::Job(context.job())).effects(Effects::Started))?;
                run_child_request(
                    &runtime, &context, &child, &sender,
                    vec![UserPart::Text { text: input.prompt }],
                ).await
            }
        },
    )?;
    Ok(())
}

/// Resume a retained child with owner input. A child whose loop is gone, such as
/// after a restart, is started again under its journaled contract. The runtime
/// reference is weak: jobs must not retain their own manager.
pub(super) fn child_resume_handler(
    runtime: Weak<SessionRuntime>,
    child: crate::identity::AgentId,
    authorization: crate::tool::authorization::AuthorizationSubject,
    execution_location: crate::execution::ExecutionLocation,
    caller_location: crate::execution::ExecutionLocation,
) -> crate::job::ResumeHandler {
    Arc::new(move |value, input| {
        let runtime = runtime.upgrade();
        let child = child.clone();
        let authorization = authorization.clone();
        let execution_location = execution_location.clone();
        let caller_location = caller_location.clone();
        Box::pin(async move {
            let runtime = runtime.ok_or_else(runtime_unavailable)?;
            if runtime
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(ToolError::cancelled()
                    .operation(
                        Operation::Prepare,
                        Subject::Label(format!("child agent {child}")),
                    )
                    .at(FailureSite::Host)
                    .effects(Effects::NotStarted));
            }
            let sender = match runtime.agent_sender(&child) {
                Some(sender) => sender,
                None => runtime
                    .spawn_agent(AgentLaunch {
                        id: child.clone(),
                        owner_job: Some(authorization.job),
                        // The journaled contract supplies the profile, depth and location.
                        model_profile: String::new(),
                        todos: None,
                        available_depth: 0,
                        location: execution_location.clone(),
                        mode: None,
                        // The journaled contract narrows this.
                        capabilities: runtime.capabilities.clone(),
                    })
                    .await
                    .map_err(|error| {
                        harness_error(error)
                            .operation(
                                Operation::Create,
                                Subject::Label(format!("retained child agent {child}")),
                            )
                            .at(FailureSite::Host)
                            .effects(Effects::Unknown)
                    })?,
            };
            let context = crate::tool::ToolContext::new(
                authorization,
                execution_location,
                caller_location,
                input,
                runtime.jobs.clone(),
            );
            let content = value.iter().map(owner_input).collect();
            let text = run_child_request(&runtime, &context, &child, &sender, content).await?;
            Ok(crate::tool::ToolOutput::new(json!(text)))
        })
    })
}

fn owner_input(value: &serde_json::Value) -> UserPart {
    let text = format!("Owner input: {value}");
    UserPart::ParentInput { text }
}

/// Start a child turn; the receiver resolves with its answer.
async fn send_child_input(
    sender: &super::AgentSender,
    content: Vec<UserPart>,
) -> Result<oneshot::Receiver<Result<String, TurnFailure>>, ToolError> {
    let (done, received) = oneshot::channel();
    let input = AgentCommand::Input {
        options: Default::default(),
        content,
        done: Some(done),
    };
    let sent = sender.send(input).await;
    sent.map_err(|_| ToolError::failed("child agent stopped before accepting input"))?;
    Ok(received)
}

async fn run_child_request(
    runtime: &Arc<SessionRuntime>,
    context: &crate::tool::ToolContext,
    child: &crate::identity::AgentId,
    sender: &super::AgentSender,
    content: Vec<UserPart>,
) -> Result<String, ToolError> {
    let job = context.job();
    // Resumption runs this outside dispatch, which would otherwise bind the host site.
    let failure = |operation, effects| {
        move |error: ToolError| {
            error
                .operation(operation, Subject::Job(job))
                .at(FailureSite::Host)
                .effects(effects)
        }
    };
    let completion_gate = runtime
        .agents()
        .get(child)
        .ok_or_else(|| {
            failure(Operation::Lookup, Effects::NotStarted)(ToolError::failed(
                "child agent stopped",
            ))
        })?
        .control
        .completion_gate
        .clone();
    *completion_gate.lock().await = true;
    let mut done_rx = send_child_input(sender, content)
        .await
        .map_err(failure(Operation::Send, Effects::NotStarted))?;
    loop {
        tokio::select! {
            result = &mut done_rx => {
                let text = result
                    .map_err(|_| failure(Operation::Receive, Effects::Started)(ToolError::failed("child agent stopped before returning a result")))?
                    .map_err(|error| failure(Operation::Wait, Effects::Started)(match error {
                        TurnFailure::Interrupted => ToolError::interrupted(),
                        other => ToolError::failed(other),
                    }))?;
                let inputs = context.drain_input_or_close().await;
                if inputs.is_empty() { return Ok(text); }
                // Owner input continues this invocation instead of finishing the job, so
                // the answer above, published without a wake, needs its wake here.
                runtime.jobs.notify_owner(context.job()).await;
                *completion_gate.lock().await = true;
                let content = inputs.iter().map(owner_input).collect();
                done_rx = send_child_input(sender, content).await.map_err(|error| failure(Operation::Send, Effects::NotStarted)(error)
                    .with_result(crate::tool::ToolOutput::new(json!(text))))?;
            }
            value = context.receive() => {
                let value = match value {
                    Ok(value) => value,
                    Err(error) => {
                        runtime.questions.cancel_child_question(context.job()).await;
                        runtime.interrupt_tree(child).await;
                        return Err(failure(Operation::Receive, Effects::Started)(error));
                    }
                };
                if !runtime.questions.answer_child_question(context.job(), value.clone()).await
                    .map_err(|error| failure(Operation::Send, Effects::Unknown)(harness_error(error)))?
                {
                    let mut active = completion_gate.lock().await;
                    if !*active {
                        // The child already resolved this invocation; owner input restarts
                        // it, so an answer published without a wake needs its wake here.
                        runtime.jobs.notify_owner(context.job()).await;
                        *active = true;
                        done_rx = send_child_input(sender, vec![owner_input(&value)]).await.map_err(failure(Operation::Send, Effects::NotStarted))?;
                        continue;
                    }
                    // Owner updates use the same request-boundary mailbox as
                    // queued root prompts, rather than waiting for a new turn.
                    let (committed, _receipt) = oneshot::channel();
                    sender.send(AgentCommand::QueuedInputs(vec![QueuedInput {
                        options: Default::default(),
                        content: vec![owner_input(&value)],
                        cancellation: Default::default(),
                        committed,
                    }])).await.map_err(|_| failure(Operation::Send, Effects::NotStarted)(ToolError::failed("child agent stopped before accepting owner input")))?;
                }
            }
        }
    }
}

fn invalid_argument(argument: &str, message: impl std::fmt::Display) -> ToolError {
    ToolError::invalid_arguments(message)
        .operation(Operation::Validate, Subject::argument([argument]))
        .effects(Effects::NotStarted)
}

fn runtime_unavailable() -> ToolError {
    ToolError::failed("session runtime is unavailable")
        .operation(
            Operation::Lookup,
            Subject::Label("session runtime".to_owned()),
        )
        // Also returned by resumption, which dispatch does not bind.
        .at(FailureSite::Host)
        .effects(Effects::NotStarted)
}

pub(super) fn harness_error(error: crate::agent::HarnessError) -> ToolError {
    use crate::agent::HarnessError;
    match error {
        HarnessError::Interrupted => ToolError::interrupted(),
        HarnessError::Io(error) => ToolError::io(error),
        HarnessError::Session(error) => error.into(),
        HarnessError::Job(error) => error.into(),
        HarnessError::Execution(error) => error.into_tool_error(),
        error => ToolError::failed(error),
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::runtime::tests::*;
    use crate::{execution::ExecutionLocation, mcp::McpServerConfig, tool::policy::CapabilitySet};
    use std::{collections::BTreeMap, path::Path};

    fn builder(root: &Path, requests: Requests) -> HarnessBuilder {
        let provider = scripted_provider(&requests, [answer("done"), answer("done")]);
        test_builder(root, &root.join("sessions"), provider, false).max_child_depth(1)
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

    fn mcp_names(session: &SessionHandle) -> Vec<String> {
        let tools = session.runtime.executor.registry().tools();
        let names = tools.map(|tool| tool.name().to_owned());
        names.filter(|name| name.starts_with("mcp_")).collect()
    }

    fn mcp_name(session: &SessionHandle) -> String {
        let names = mcp_names(session);
        assert_eq!(names.len(), 1, "{:?}", session.startup_warnings());
        names[0].clone()
    }

    /// `typeof tool.<name>` inside a script run by `executor`.
    async fn script_type(executor: &ToolExecutor, session: &SessionHandle, name: &str) -> String {
        let source = json!({"source":format!("return typeof tool.{name};")});
        let script = executor.execute(session.root.clone(), "script", source, None);
        let output = script.await.unwrap().output.value;
        output["value"].as_str().unwrap().to_owned()
    }

    fn launched(root: &Path) -> String {
        std::fs::read_to_string(root.join("launched")).unwrap()
    }

    #[tokio::test]
    async fn mcp_gates_prevent_stdio_launch_and_http_contact() {
        // Required permissions, effective root depth, and the unconditional MCP gate
        // all apply before transport startup, even with an approve-all policy.
        let mut cases = vec![(CapabilitySet::default(), vec![Capability::Agents], 0)];
        // The global gate, the depth-dependent capability, and two ordinary ones.
        for missing in [
            Capability::Mcp,
            Capability::Agents,
            Capability::Exec,
            Capability::Network,
        ] {
            let mut capabilities = Capability::ALL.into_iter().collect::<CapabilitySet>();
            capabilities.remove(missing);
            cases.push((capabilities, Capability::ALL.to_vec(), 1));
        }
        let mut without_mcp = CapabilitySet::default();
        without_mcp.remove(Capability::Mcp);
        cases.push((without_mcp, vec![], 1));
        cases.push((CapabilitySet::empty(), vec![], 1));
        for (capabilities, required, depth) in cases {
            let root = tempfile::tempdir().unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let http = serde_json::from_value(json!({
                "transport":"streamable_http",
                "url":format!("http://{}/mcp", listener.local_addr().unwrap()),
                "capabilities":required, "startup_timeout_secs":1
            }))
            .unwrap();
            let mcp = BTreeMap::from([
                ("stdio".into(), stdio_config(root.path(), required)),
                ("http".into(), http),
            ]);
            let harness = builder(root.path(), Requests::default())
                .capabilities(capabilities)
                .max_child_depth(depth)
                .policy(Arc::new(crate::tool::policy::AllowAll))
                .mcp(mcp)
                .build()
                .await
                .unwrap();
            let session = harness.new_session().await.unwrap();
            assert!(session.startup_warnings().is_empty());
            assert!(mcp_names(&session).is_empty());
            shutdown_session(session).await;
            assert!(!root.path().join("launched").exists());
            // Startup has finished, so any contact is already in the backlog.
            let accepted = listener.accept().unwrap_err();
            assert_eq!(accepted.kind(), std::io::ErrorKind::WouldBlock);
        }
    }

    #[tokio::test]
    async fn empty_capability_requirements_expose_direct_and_script_tools() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        // Empty per-server requirements still need the global gate, but neither
        // Read nor Exec is implicitly required to use an MCP connection.
        let capabilities = [Capability::Mcp].into_iter().collect::<CapabilitySet>();
        let harness = builder(root.path(), requests.clone())
            .capabilities(capabilities.clone())
            .mcp(servers(stdio_config(root.path(), vec![])))
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        assert!(session.startup_warnings().is_empty());
        let name = mcp_name(&session);
        let executor = session.runtime.executor.clone();
        let executor = executor.with_capabilities(capabilities);
        assert_eq!(session.prompt("list your tools").await.unwrap(), "done");
        let tools = requests.lock().unwrap()[0].tools.clone();
        assert!(tools.iter().any(|tool| tool.name == name));
        let direct = executor.execute(session.root.clone(), &name, json!({"text":"direct"}), None);
        let direct = direct.await.unwrap().output.value;
        assert_eq!(direct["structuredContent"], json!({"text":"direct"}));
        let script = format!("return await tool.{name}({{text:'script'}});");
        let script = session.run_script(script).await.unwrap().value;
        let found = &script["value"]["result"]["structuredContent"];
        assert_eq!(*found, json!({"text":"script"}));
        // Also test a discovered adapter with empty server requirements. Startup
        // omission alone would not catch a missing adapter-level global gate.
        let disabled = executor.with_capabilities(CapabilitySet::empty());
        assert!(disabled.surface().get(&name).is_none());
        let denied = json!({"text":"denied"});
        let direct = disabled.execute(session.root.clone(), &name, denied.clone(), None);
        assert!(direct.await.is_err());
        let scripted = disabled.execute_script(session.root.clone(), &name, denied, None);
        assert!(scripted.await.is_err());
        assert_eq!(script_type(&disabled, &session, &name).await, "undefined");
        shutdown_session(session).await;
        assert_eq!(launched(root.path()), "launched\n");
    }

    /// An agent may name only hinted models, and hinted modes within what it holds
    /// itself; with none to name, the input is absent. A chosen mode is the child's.
    #[tokio::test]
    async fn children_take_only_hinted_models_and_modes_their_parent_could_hold() {
        use crate::tool::policy::Mode;
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let agent_input = |index: usize, name: &str| {
            let requests = requests.lock().unwrap();
            let agent = requests[index]
                .tools
                .iter()
                .find(|tool| tool.name == "agent");
            agent.unwrap().input_schema["properties"][name].clone()
        };
        let plain = builder(root.path(), requests.clone())
            .build()
            .await
            .unwrap();
        let session = plain.new_session().await.unwrap();
        session.prompt("root request").await.unwrap();
        assert!(agent_input(0, "model").is_null() && agent_input(0, "mode").is_null());
        shutdown_session(session).await;
        requests.lock().unwrap().clear();

        let mode = |capabilities: &[Capability], hint: Option<&str>| Mode {
            capabilities: capabilities.to_vec(),
            instructions: Some(format!("Holds {}.", capabilities.len())),
            hint: hint.map(str::to_owned),
        };
        let (read, agents) = (Capability::Read, Capability::Agents);
        let modes = [
            (
                "work",
                mode(&[read, Capability::Write, agents], Some("Works")),
            ),
            ("scout", mode(&[read, agents], Some("Delegates reading"))),
            ("idle", mode(&[], Some("Thinks"))),
            (
                "online",
                mode(&[read, Capability::Network], Some("Looks things up")),
            ),
            ("secret", mode(&[read], None)),
        ];
        let cheap = ModelProfile {
            hint: Some("Cheap".into()),
            ..ModelProfile::new("test", "cheap", None, 128_000, 4096, false)
        };
        let provider = scripted_provider(&requests, (0..3).map(|_| answer("done")));
        let harness = test_builder(root.path(), &root.path().join("hinted"), provider, false)
            .max_child_depth(2)
            .model_profile("cheap", cheap)
            .modes(modes.map(|(name, mode)| (name.to_owned(), mode)).into())
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        session.prompt("root request").await.unwrap();
        // `work` holds no network, and `secret` has no hint.
        assert_eq!(agent_input(0, "model")["enum"], json!(["cheap", null]));
        let offered = agent_input(0, "mode");
        assert_eq!(offered["enum"], json!(["work", "scout", "idle", null]));
        let description = offered["description"].as_str().unwrap();
        assert!(description.contains("\n- scout [read, agents]: Delegates reading"));
        assert!(description.contains("\n- idle [none]: Thinks"));
        assert!(!description.contains("interactive"));

        let refusals = [
            ("{mode:'secret'}", "unknown mode `secret`"),
            ("{mode:'online'}", "unknown mode `online`"),
            ("{model:'test'}", "unknown model `test`"),
        ];
        for (refused, reason) in refusals {
            let script =
                format!("return (await tool.agent({{prompt:'no', ...{refused}}})).unwrap();");
            let error = session.run_script(script).await.unwrap_err().to_string();
            assert!(error.contains(reason), "{refused}: {error}");
        }
        let child = "return await tool.agent({prompt:'go', depth:1, mode:'scout', model:'cheap'});";
        assert_eq!(
            session.run_script(child).await.unwrap().value["value"]["result"],
            "done"
        );
        // The child holds less than its parent, so it is offered less.
        assert_eq!(
            agent_input(1, "mode")["enum"],
            json!(["scout", "idle", null])
        );
        let child = requests.lock().unwrap()[1].clone();
        assert_eq!(child.model, "cheap");
        assert!(
            child.system[0]
                .text
                .contains("<mode name=\"scout\">\nHolds 2.")
        );
        let records = session.runtime.store.records().await;
        let started = events!(&records, SessionEvent::AgentStarted { mode, capabilities, .. } => (mode.clone().map(|mode| mode.name), capabilities.clone()));
        // Interaction follows the parent; the mode grants the rest.
        let held = vec![read, agents, Capability::Interactive];
        assert_eq!(started[1], (Some("scout".to_owned()), held));
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn child_target_lookup_preserves_safe_admission_diagnostics() {
        use crate::tool::diagnostic::{Cause, Effects, Operation, Subject};

        let root = tempfile::tempdir().unwrap();
        let harness = builder(root.path(), Requests::default())
            .capabilities(Capability::ALL.into_iter().collect())
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        for inherited in [false, true] {
            let mut executor = session.runtime.executor.clone();
            let mut args = json!({"prompt":"no"});
            if inherited {
                executor = executor
                    .with_capabilities(CapabilitySet::default())
                    .with_location(ExecutionLocation::named(
                        "private-missing-target".parse().unwrap(),
                        root.path().to_path_buf(),
                    ));
            } else {
                executor = executor.with_capabilities(Capability::ALL.into_iter().collect());
                args["target"] = json!("private-missing-target");
            }
            let error = bounded(executor.execute(session.root.clone(), "agent", args, None))
                .await
                .unwrap_err();
            let diagnostic = error.diagnostic();
            assert_eq!(
                diagnostic.cause,
                Cause::InvalidArguments("unknown target".into())
            );
            assert_eq!(diagnostic.context.operation, Operation::Lookup);
            assert_eq!(diagnostic.context.subject, Subject::argument(["target"]));
            assert_eq!(diagnostic.context.effects, Effects::NotStarted);
            assert!(
                !diagnostic
                    .render(&CapabilitySet::default())
                    .contains("private-missing-target")
            );
        }
        shutdown_session(session).await;
    }

    /// A child in a mode is held to it: depth still withholds `agents`, a mode it could
    /// not hold is unknown to it, and a restart returns it to the mode as pinned.
    #[tokio::test]
    async fn a_child_keeps_its_mode_through_depth_delegation_and_restart() {
        use crate::tool::policy::Mode;
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let mode = |capabilities: &[Capability], instructions: &str| Mode {
            capabilities: capabilities.to_vec(),
            instructions: Some(instructions.to_owned()),
            hint: Some("Hinted".into()),
        };
        let (read, write, agents) = (Capability::Read, Capability::Write, Capability::Agents);
        let build = |scout: Mode, responses: Vec<Vec<ResponseEvent>>| {
            let provider = scripted_provider(&requests, responses);
            let modes = [
                ("work", mode(&[read, write, agents], "Work.")),
                ("scout", scout),
            ];
            test_builder(root.path(), &sessions, provider, false)
                .max_child_depth(2)
                .modes(modes.map(|(name, mode)| (name.to_owned(), mode)).into())
                .build()
        };
        let scout = mode(&[read, agents], "Scout.");
        let widen = tool_call(0, "widen", "agent", json!({"prompt": "no", "mode": "work"}));
        let responses = vec![answer("leaf"), response(vec![widen]), answer("held")];
        let session = build(scout.clone(), responses).await.unwrap();
        let session = session.new_session().await.unwrap();
        let spawn = |depth: usize| {
            format!("return await tool.agent({{prompt:'go', mode:'scout', depth:{depth}}});")
        };
        assert_eq!(
            session.run_script(spawn(0)).await.unwrap().value["value"]["result"],
            "leaf"
        );
        assert_eq!(
            session.run_script(spawn(1)).await.unwrap().value["value"]["result"],
            "held"
        );
        let records = session.runtime.store.records().await;
        let started = events!(&records, SessionEvent::AgentStarted { mode: Some(mode), capabilities, .. } if mode.name == "scout" => capabilities.clone());
        let interactive = Capability::Interactive;
        assert_eq!(
            started,
            [vec![read, interactive], vec![read, agents, interactive]]
        );
        let job = events!(&records, SessionEvent::JobCreated { job, tool, .. } if tool == "agent" => *job)
            [1];
        {
            let captured = requests.lock().unwrap();
            let offers = |index: usize| {
                captured[index]
                    .tools
                    .iter()
                    .any(|tool| tool.name == "agent")
            };
            assert_eq!((offers(0), offers(1)), (false, true));
            let Some(crate::provider::protocol::Message::Tool(results)) =
                captured[2].history.last()
            else {
                panic!("expected the refused delegation");
            };
            let refusal = results[0].result.to_string();
            assert!(
                results[0].is_error && refusal.contains("unknown mode `work`"),
                "{refusal}"
            );
        }
        let id = session.id();
        shutdown_session(session).await;

        // The configuration now widens `scout`; the session keeps the one it pinned.
        let widened = mode(&[read, write, agents], "Changed.");
        let harness = build(widened, vec![answer("again")]).await.unwrap();
        let resumed = harness.resume_session(id).await.unwrap();
        resumed.runtime.jobs.send(job, json!("more")).await.unwrap();
        assert_eq!(
            terminal(&resumed, job).await.state,
            crate::job::JobState::Completed
        );
        let child = requests.lock().unwrap().last().unwrap().clone();
        assert!(
            child.system[0].text.contains("Scout.") && !child.system[0].text.contains("Changed.")
        );
        assert!(!child.tools.iter().any(|tool| tool.name == "write"));
        let agent = child
            .tools
            .iter()
            .find(|tool| tool.name == "agent")
            .unwrap();
        assert_eq!(
            agent.input_schema["properties"]["mode"]["enum"],
            json!(["scout", null])
        );
        shutdown_session(resumed).await;
        // As decoded from the journal: pinned once, with its hint, by the first child.
        let (_store, records) = crate::session::SessionStore::open(&sessions, id)
            .await
            .unwrap();
        let pinned = events!(&records, SessionEvent::AgentStarted { mode: Some(mode), .. } if mode.name == "scout" => mode.definition.clone());
        assert_eq!(pinned, [Some(scout), None]);
    }

    /// Children starting at once in a mode the session has not used pin it once.
    #[tokio::test(flavor = "multi_thread")]
    async fn children_starting_together_pin_their_mode_once() {
        use crate::tool::policy::Mode;
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let mode = |capabilities: &[Capability]| Mode {
            capabilities: capabilities.to_vec(),
            instructions: None,
            hint: Some("Hinted".into()),
        };
        let modes = [
            ("work", mode(&[Capability::Read, Capability::Agents])),
            ("look", mode(&[Capability::Read])),
        ];
        let provider = scripted_provider(&requests, (0..8).map(|_| answer("done")));
        let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
            .max_child_depth(1)
            .modes(modes.map(|(name, mode)| (name.to_owned(), mode)).into())
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let script = "const all = [...Array(8)].map(() => tool.agent({prompt:'go', mode:'look'})); \
                      return (await Promise.all(all)).length;";
        assert_eq!(session.run_script(script).await.unwrap().value["value"], 8);
        let records = session.runtime.store.records().await;
        let looks = events!(&records, SessionEvent::AgentStarted { mode: Some(mode), .. } if mode.name == "look" => mode.definition.is_some());
        assert_eq!(
            (looks.len(), looks.iter().filter(|pinned| **pinned).count()),
            (8, 1)
        );
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn depth_zero_child_omits_agent_gated_mcp_without_reconnecting() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let harness = builder(root.path(), requests.clone())
            .mcp(servers(stdio_config(root.path(), vec![Capability::Agents])))
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let name = mcp_name(&session);
        session.prompt("root request").await.unwrap();
        let child =
            session.run_script("return await tool.agent({prompt:'child request', depth:0});");
        assert_eq!(child.await.unwrap().value["value"]["result"], "done");
        {
            let requests = requests.lock().unwrap();
            let offered = |index: usize| requests[index].tools.iter().any(|tool| tool.name == name);
            assert_eq!((requests.len(), offered(0), offered(1)), (2, true, false));
        }
        let child_capabilities = session.runtime.capabilities.for_agent(0);
        let child_executor = session.runtime.executor.clone();
        let child_executor = child_executor.with_capabilities(child_capabilities);
        let denied =
            child_executor.execute(session.root.clone(), &name, json!({"text":"denied"}), None);
        assert!(denied.await.is_err());
        let found = script_type(&child_executor, &session, &name).await;
        assert_eq!(found, "undefined");
        shutdown_session(session).await;
        assert_eq!(launched(root.path()), "launched\n");
    }

    #[tokio::test]
    async fn mcp_stays_on_host_for_remote_location_and_failed_startup_is_reported() {
        let root = tempfile::tempdir().unwrap();
        let harness = builder(root.path(), Requests::default())
            .mcp(servers(stdio_config(root.path(), vec![])))
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let name = mcp_name(&session);
        // This location has no route/worker. A targeted dispatch would fail; Host
        // dispatch must still use the session-owning process and its MCP manager.
        let location = ExecutionLocation::named(
            "unconnected-remote".parse().unwrap(),
            PathBuf::from("/remote/workspace"),
        );
        let remote = session.runtime.executor.clone().with_location(location);
        let output = remote.execute(session.root.clone(), &name, json!({"text":"host"}), None);
        let output = output.await.unwrap().output.value;
        assert_eq!(output["structuredContent"]["text"], "host");
        shutdown_session(session).await;

        let missing = root.path().join("nonexistent-mcp");
        let config = json!({"transport": "stdio", "start_command": [missing.to_string_lossy()]});
        let harness = builder(root.path(), Requests::default())
            .mcp(servers(serde_json::from_value(config).unwrap()))
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.startup_warnings().len(), 1);
        assert!(session.startup_warnings()[0].contains("fixture"));
        assert!(mcp_names(&session).is_empty());
        assert_eq!(session.prompt("still usable").await.unwrap(), "done");
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn root_interactive_gate_covers_dispatch_and_nested_scripts() {
        for enabled in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let requests = Requests::default();
            let batches: Arc<StdMutex<Vec<Vec<Question>>>> = Arc::default();
            let mut capabilities = CapabilitySet::default();
            if !enabled {
                capabilities.remove(Capability::Interactive);
            }
            let questions = RecordingQuestions {
                batches: batches.clone(),
                answer: json!("host-answer"),
                ..Default::default()
            };
            let harness = builder(root.path(), requests.clone())
                .capabilities(capabilities.clone())
                .question_handler(Arc::new(questions))
                .build()
                .await
                .unwrap();
            let session = harness.new_session().await.unwrap();
            let executor = session.runtime.executor.clone();
            let executor = executor.with_capabilities(capabilities);
            session.prompt("list tools").await.unwrap();
            let tools = requests.lock().unwrap()[0].tools.clone();
            assert_eq!(tools.iter().any(|tool| tool.name == "ask"), enabled);
            let ask_type = session.run_script("return typeof tool.ask;").await.unwrap();
            let found = &ask_type.value["value"];
            assert_eq!(*found, if enabled { "function" } else { "undefined" });
            for background in [false, true] {
                for route in ["host", "model", "script"] {
                    let args = json!({"id":"root", "prompt":"question", "bg":background});
                    let agent = session.root.clone();
                    let result = bounded(async {
                        match route {
                            "host" => executor.execute(agent, "ask", args, None).await,
                            "model" => executor.execute_model(agent, "ask", args, None).await,
                            _ => executor.execute_script(agent, "ask", args, None).await,
                        }
                    })
                    .await;
                    if enabled {
                        let output = terminal(&session, result.unwrap().job).await;
                        assert_eq!(output.output, Some(json!("host-answer")));
                    } else {
                        assert!(result.unwrap_err().to_string().contains("unavailable"));
                    }
                }
                // Nested root asks cannot escape the gate through script jobs.
                let source = "return (await tool.ask({id:'nested', prompt:'question'})).unwrap();";
                let arguments = json!({"source":source, "bg":background});
                let result = executor
                    .execute(session.root.clone(), "script", arguments, None)
                    .await;
                if !enabled && !background {
                    assert!(matches!(
                        result.unwrap_err().diagnostic().cause,
                        crate::tool::diagnostic::Cause::Message(_)
                    ));
                    continue;
                }
                let output = terminal(&session, result.unwrap().job).await;
                if enabled {
                    assert_eq!(output.state, crate::job::JobState::Completed);
                    assert_eq!(output.output.unwrap()["value"], "host-answer");
                } else {
                    assert_eq!(output.state, crate::job::JobState::Failed);
                }
            }
            assert_eq!(batches.lock().unwrap().len(), if enabled { 8 } else { 0 });
            if !enabled {
                let jobs = session.inspect_jobs(&session.root).await;
                assert!(jobs.iter().all(|job| job.tool != "ask"));
            }
            shutdown_session(session).await;
        }
    }
}
