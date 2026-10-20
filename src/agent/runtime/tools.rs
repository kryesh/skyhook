//! Agent-specific tools layered on top of the general coding tool set.
use std::sync::{Arc, OnceLock, Weak};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::{
    agent::{ModelEntry, Question, ReplaceError, TodoItem},
    identity::{AgentId, JobId},
    provider::profile::ModelRef,
    session::{CoreState, Plan, UserPart},
    tool::{
        AdmissionError, RegistryError, TARGET, ToolError, ToolOptions, ToolRegistryBuilder,
        diagnostic::{Effects, FailureSite, Operation, Subject},
        policy::{Capability, CapabilitySet, Mode, ModeName},
        registry,
    },
};

use super::{
    AgentCommand, AgentLaunch, HarnessError, Invocation, SessionRuntime, TurnFailure,
    dispatch::ChildAnswer, queue::QueuedInput,
};

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
            &harness.workspace,
            tokio_util::sync::CancellationToken::new(),
        )
        .await,
    );
    let mut warnings = manager.warnings();
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
    pub(super) todos: Option<Vec<TodoItem<String>>>,
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
    pub(super) workspace: Option<String>,
}

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
    models: &indexmap::IndexMap<ModelRef, ModelEntry>,
    modes: &indexmap::IndexMap<ModeName, Mode>,
) -> Result<(), RegistryError> {
    let (wait, ask, todo) = (
        runtime_slot.clone(),
        runtime_slot.clone(),
        runtime_slot.clone(),
    );
    builder.register::<super::wait::WaitArgs, super::wait::WaitOutput, _, _>(
        "wait",
        "Wait for any notification or input relevant to this agent. Optional timeout is a positive integer number of seconds; omitted/null waits indefinitely. Returns {reason: event|timeout}. Does not consume notifications or retrieve output; use jobs to inspect saved output. Cancellation interrupts the wait. A timeout ends this wait, not background work; wait again if still dependent on it.",
        ToolOptions::default().job_role(crate::job::JobRole::Wait),
        move |context, args| {
            let runtime = wait.get().and_then(Weak::upgrade);
            async move {
                runtime.ok_or_else(runtime_unavailable)?.wait_for_event(&context, args).await
            }
        },
    )?;
    builder.register::<Question, Value, _, _>(
        "ask",
        "Ask one structured question. Issue independent questions concurrently; the runtime merges calls that become ready together. Use bg:true to continue independent work while awaiting an answer; inspect the returned job with jobs.",
        ToolOptions::default().job_role(crate::job::JobRole::Question).background().input().requires_for_root(Capability::Interactive),
        move |context, input| {
            let runtime = ask.get().and_then(Weak::upgrade);
            async move {
                let runtime = runtime.ok_or_else(runtime_unavailable)?;
                runtime.questions.coordinate_question(context, input).await
            }
        },
    )?;
    builder.register_checked(
        "todo",
        "Read your list, replace it with items, or inspect a descendant by agent job ID. Only the owner can edit.",
        ToolOptions::default().result::<TodoList>(),
        |TodoArgs { items, job }| match (items, job) {
            (Some(_), Some(_)) => Err(AdmissionError::invalid_arguments("items and job cannot be combined")
                .operation(Operation::Validate, Subject::argument(["job"])).effects(Effects::NotStarted)),
            (Some(items), None) => Ok(TodoRequest::Replace(items)),
            (None, job) => Ok(TodoRequest::Inspect(job)),
        },
        move |request| {
            let runtime = todo.get().and_then(Weak::upgrade);
            match request {
                TodoRequest::Replace(items) => registry::AdmittedInvocation::unit(move |context| async move {
                    let runtime = runtime.ok_or_else(runtime_unavailable)?;
                    let items = admit_todos("items", items)?;
                    let issued_by = runtime.jobs.call_origin(context.job()).await;
                    let subject = Subject::Label(format!("todos for agent {}", context.agent()));
                    runtime.store.replace_todos(context.agent(), items, issued_by).await.map_err(|error| match error {
                        ReplaceError::Superseded => ToolError::failed(error).operation(Operation::Save, subject).effects(Effects::Unchanged),
                        ReplaceError::Session(error) => harness_error(error.into()).operation(Operation::Save, subject).effects(Effects::Unknown),
                    })
                }),
                TodoRequest::Inspect(job) => registry::AdmittedInvocation::new(move |context| async move {
                    let runtime = runtime.ok_or_else(runtime_unavailable)?;
                    registry::serialized(TodoList { items: runtime.store.inspect_todos(context.agent(), job)? })
                }),
            }
        },
    )?;
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
    items: Option<Vec<TodoItem<String>>>,
    /// Inspect a descendant's list using its agent job ID. Omit to read your own list.
    job: Option<crate::identity::JobId>,
}

enum TodoRequest {
    Replace(Vec<TodoItem<String>>),
    Inspect(Option<crate::identity::JobId>),
}

#[derive(Serialize, JsonSchema)]
struct TodoList {
    items: Vec<TodoItem>,
}

/// Admit todo items given as `argument`, rejecting blank text at its path.
fn admit_todos(argument: &str, items: Vec<TodoItem<String>>) -> Result<Vec<TodoItem>, ToolError> {
    let admit = |(index, TodoItem { text, status }): (usize, TodoItem<String>)| {
        let path = [argument, &index.to_string(), "text"];
        let text = text
            .parse()
            .map_err(|blank| invalid_argument(path, blank))?;
        Ok(TodoItem { text, status })
    };
    items.into_iter().enumerate().map(admit).collect()
}

fn register_child_agent(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
    models: &indexmap::IndexMap<ModelRef, ModelEntry>,
    modes: &indexmap::IndexMap<ModeName, Mode>,
) -> Result<(), RegistryError> {
    let hinted = |(name, entry): (&ModelRef, &ModelEntry)| {
        let hint = entry.profile.hint.as_ref()?;
        Some((name.to_string(), format!(": {hint}")))
    };
    let models: Vec<_> = models.iter().filter_map(hinted).collect();
    let modes = modes.clone();
    builder.register::<AgentArgs, String, _, _>(
        crate::tool::builtins::names::AGENT,
        "Start a child agent. Names are unique among your own installed children, including terminal children; other callers may reuse the same names. Send follow-ups or answers with tool.job(id).send({value: ...}). Questions pause the child; follow-ups arrive automatically at its next model-request boundary. Earlier replies arrive as events; the final reply is the job's result. Sending input to a completed child resumes its retained history under the same job ID.",
        ToolOptions::default().job_role(crate::job::JobRole::Agent)
            .complete_result::<String>()
            .named()
            .requires(Capability::Agents)
            .conditional_input(TARGET, Capability::Targets, crate::target::TargetRef::schema())
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
                        let hint = mode.hint.as_ref().map_or("", crate::Prose::as_str);
                        (name.as_str(), format!(" [{granted}]: {hint}"))
                    })
                    .collect();
                choice_input("Mode limiting what the child can do; omitted inherits what you can.", &choices)
            })
            .background()
            .input()
            .job_location(|input: &AgentArgs| crate::tool::JobLocation {
                target: input.target.clone(),
                workspace: input.workspace.clone(),
            }),
        move |context, input| {
            let runtime = runtime_slot.get().and_then(Weak::upgrade);
            async move {
                let runtime = runtime.ok_or_else(runtime_unavailable)?;
                let owner = context.agent();
                let unstarted = || ToolError::failed("parent agent never started")
                    .operation(Operation::Lookup, Subject::Label(format!("parent agent {owner}"))).effects(Effects::NotStarted);
                let delegation = runtime.held(owner, |contract, depth, _| (contract.profile.name.clone(), depth));
                let (model, depth) = delegation.ok_or_else(unstarted)?;
                let caller = (context.capabilities(), model, depth);
                let location = context.job_location().clone();
                let (launch, prompt) = child_launch(&runtime, input, caller, location)?;
                let child = runtime.next_child(owner).await.ok_or_else(unstarted)?;
                let job = context.job();
                let call = super::dispatch::Call { job, prompt };
                let launched = runtime.launch_child(child.clone(), launch, call);
                let answer = launched.await.map_err(|error| launch_error(error, &child))?;
                run_child_request(&runtime, &context, &child, answer).await
            }
        },
    )?;
    Ok(())
}

/// Why a child agent could not start, or start again.
fn launch_error(error: HarnessError, child: &crate::identity::AgentId) -> ToolError {
    let subject = Subject::Label(format!("child agent {child}"));
    let error = match error {
        error if stopped_starting(&error) => (ToolError::interrupted()
            .operation(Operation::Prepare, subject))
        .effects(Effects::NotStarted),
        // A reused name is a follow-up aimed at the wrong tool.
        HarnessError::Job(error @ crate::job::JobError::NameTaken { job, .. }) => {
            (ToolError::invalid_arguments(error).operation(Operation::Validate, Subject::Job(job)))
                .effects(Effects::NotStarted)
        }
        error => {
            (harness_error(error).operation(Operation::Create, subject)).effects(Effects::Unknown)
        }
    };
    error.at(FailureSite::Host)
}

/// Check `input` as the launch its call makes, with the prompt that begins it,
/// against what its caller now holds, which model it runs and how deep it may
/// delegate.
fn child_launch(
    runtime: &SessionRuntime,
    input: AgentArgs,
    (caller, model, available_depth): (&CapabilitySet, ModelRef, usize),
    location: crate::execution::ExecutionLocation,
) -> Result<(AgentLaunch, String), ToolError> {
    let todos = input.todos.map(|items| admit_todos("todos", items));
    let todos = todos.transpose()?;
    // Only hinted models were offered; anything else is unknown to the caller.
    let model = match input.model {
        Some(name) => {
            let hinted = |model: &ModelRef| {
                (runtime.harness.models.get(model))
                    .is_some_and(|entry| entry.profile.hint.is_some())
            };
            let model = name.parse::<ModelRef>().ok().filter(hinted);
            model.ok_or_else(|| invalid_argument(["model"], format!("unknown model `{name}`")))?
        }
        None => model,
    };
    let (mode, capabilities) = match &input.mode {
        None => (None, caller.clone()),
        Some(name) => {
            let offered = |(_, mode): &(&ModeName, &Mode)| offers_mode(mode, caller);
            let Some((name, mode)) = runtime.modes.get_key_value(name.as_str()).filter(offered)
            else {
                return Err(invalid_argument(["mode"], format!("unknown mode `{name}`")));
            };
            // Interaction follows the caller, as the root's follows the host.
            let applied = super::AppliedMode::configured(name, mode);
            (Some(applied), &runtime.granted_by(mode) & caller)
        }
    };
    if input.depth >= available_depth {
        return Err(invalid_argument(
            ["depth"],
            format!("depth must be less than the caller's available depth of {available_depth}"),
        ));
    }
    let launch = AgentLaunch::New {
        model,
        todos,
        available_depth: input.depth,
        location,
        mode,
        capabilities,
    };
    Ok((launch, input.prompt))
}

/// Run an agent call's installed child again with owner input, its loop started
/// again under its journaled contract if it parked or a restart left none, for
/// its owner as its journaled contract now lets it act. The runtime reference is
/// weak: jobs must not retain their own manager.
pub(super) fn child_resume_handler(
    runtime: Weak<SessionRuntime>,
    restarting: crate::job::Restarting,
) -> crate::job::ResumeHandler {
    Arc::new(move |value, input| {
        let (runtime, restarting) = (runtime.upgrade(), restarting.clone());
        Box::pin(async move {
            let runtime = runtime.ok_or_else(runtime_unavailable)?;
            let crate::job::Restarting {
                job,
                owner,
                parent,
                location,
                cancellation,
                restart: _,
            } = restarting;
            let cancelled = cancellation.clone();
            let resumed = async {
                let (journaled, capabilities) =
                    (runtime.contract(&owner).await).map_err(|error| not_restarted(job, error))?;
                let authorization = crate::tool::authorization::AuthorizationSubject {
                    agent: owner,
                    job,
                    parent,
                    capabilities,
                    cancellation,
                };
                let context = crate::tool::ToolContext::new(
                    authorization,
                    location,
                    journaled,
                    input,
                    runtime.jobs.clone(),
                );
                let child = runtime.jobs.child_agent(job).await.ok_or_else(|| {
                    ToolError::failed("the call has no child to resume").at(FailureSite::Host)
                })?;
                let content = value.iter().map(owner_input).collect();
                let answer = restart_child(&runtime, job, &child, content).await?;
                let text = run_child_request(&runtime, &context, &child, answer).await?;
                Ok(crate::tool::ToolOutput::new(json!(text)))
            };
            // As dispatch ran the call: a cancellation requested before the restart
            // took its child stops the child starting, and is the outcome.
            let result = resumed.await;
            crate::tool::executor::cancellation_result(result, cancelled.is_cancelled())
        })
    })
}

fn owner_input(value: &serde_json::Value) -> UserPart {
    let text = format!("Owner input: {value}");
    UserPart::ParentInput { text }
}

/// Start child agent `child`'s next invocation with owner input `content`,
/// releasing the answer the last one held as a reply. The receiver resolves with
/// the new invocation's answer.
async fn restart_child(
    runtime: &Arc<SessionRuntime>,
    job: JobId,
    child: &AgentId,
    content: Vec<UserPart>,
) -> Result<ChildAnswer, ToolError> {
    let started = child.clone();
    let restart =
        move |state: &CoreState| restart(state, job, started, content, std::convert::identity);
    let restarted = runtime.with_loops(child, restart).await;
    restarted.map_err(|error| not_restarted(job, error))
}

/// Give child agent `child` owner input: it joins the open invocation at its
/// next request boundary, or, once that resolved, starts the next as
/// [`restart_child`] does.
async fn owner_update(
    runtime: &Arc<SessionRuntime>,
    job: JobId,
    child: &AgentId,
    content: Vec<UserPart>,
) -> Result<Option<ChildAnswer>, ToolError> {
    let updated = child.clone();
    // A parked child has no facts until the change that starts its loop.
    let input = move |state: &CoreState| {
        let child = updated;
        let facts = state.agents.get(&child);
        if facts.is_none_or(|facts| facts.invocation == Invocation::Resolved) {
            return restart(state, job, child, content, Some);
        }
        let input = QueuedInput {
            options: Default::default(),
            content,
            attachments: Vec::new(),
            cancellation: Default::default(),
            committed: oneshot::channel().0,
        };
        let queued = AgentCommand::QueuedInputs(vec![input]);
        Ok(Plan::new(Vec::new(), move |state, _| {
            state.agents.send(&child, queued);
            None
        }))
    };
    let updated = runtime.with_loops(child, input).await;
    updated.map_err(|error| not_restarted(job, error))
}

/// What [`restart_child`] commits once `child`'s loop runs, its answer's
/// receiver passed `into` its result.
fn restart<T: Send + 'static>(
    state: &CoreState,
    job: JobId,
    child: AgentId,
    content: Vec<UserPart>,
    into: fn(ChildAnswer) -> T,
) -> Result<Plan<T>, HarnessError> {
    let released = crate::job::Jobs::release_answer(state, job)?;
    Ok(Plan::new(released, move |state, _| {
        crate::job::woken(state, job);
        let (done, received) = oneshot::channel();
        let input = AgentCommand::Input {
            options: Default::default(),
            content,
            done: Some(super::RequestCompletion::Child(done)),
        };
        let facts = (state.agents.get_mut(&child)).expect("the change runs its loop");
        facts.invocation = Invocation::Open;
        state.agents.send(&child, input);
        into(received)
    }))
}

/// Wait for `child`'s answer to the invocation whose answer arrives on `done_rx`,
/// passing on what its owner sends meanwhile.
async fn run_child_request(
    runtime: &Arc<SessionRuntime>,
    context: &crate::tool::ToolContext,
    child: &crate::identity::AgentId,
    mut done_rx: super::dispatch::ChildAnswer,
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
                // the answer above, held for its result, is released as a reply.
                let content = inputs.iter().map(owner_input).collect();
                done_rx = restart_child(runtime, job, child, content).await
                    .map_err(|error| error.with_result(crate::tool::ToolOutput::new(json!(text))))?;
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
                    // Owner updates join the open turn at its next request boundary, as
                    // queued root prompts do; once it resolved, they restart it.
                    let sent = owner_update(runtime, job, child, vec![owner_input(&value)]).await;
                    if let Some(restarted) = sent? {
                        done_rx = restarted;
                    }
                }
            }
        }
    }
}

fn invalid_argument<S: AsRef<str>>(
    path: impl IntoIterator<Item = S>,
    message: impl std::fmt::Display,
) -> ToolError {
    ToolError::invalid_arguments(message)
        .operation(Operation::Validate, Subject::argument(path))
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

/// How job `job` reports that what runs it again could not start: an agent call
/// whose child or owner could not be started, as either restart kind finds it.
pub(super) fn not_restarted(job: JobId, error: crate::agent::HarnessError) -> ToolError {
    let error = match error {
        error if stopped_starting(&error) => ToolError::interrupted(),
        error => harness_error(error),
    };
    error
        .operation(Operation::Send, Subject::Job(job))
        .at(FailureSite::Host)
        .effects(Effects::NotStarted)
}

/// A session that no longer starts agents refused it, which interrupts the call.
fn stopped_starting(error: &HarnessError) -> bool {
    matches!(
        error,
        HarnessError::Session(crate::session::SessionError::Refused(_))
    )
}

pub(super) fn harness_error(error: crate::agent::HarnessError) -> ToolError {
    use crate::agent::HarnessError;
    match error {
        HarnessError::Interrupted => ToolError::interrupted(),
        HarnessError::Io(error) => ToolError::io(error),
        HarnessError::Session(error) => error.into(),
        HarnessError::Job(error) => error.into(),
        HarnessError::Execution(error) => error,
        error => ToolError::failed(error),
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::runtime::tests::*;
    use crate::tool::executor::InvocationKind;
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
            "capabilities":capabilities
        }))
        .unwrap()
    }

    fn servers(config: McpServerConfig) -> BTreeMap<String, McpServerConfig> {
        BTreeMap::from([("fixture".into(), config)])
    }

    fn mcp_names(session: &SessionHandle) -> Vec<String> {
        let all = crate::tool::policy::Capability::ALL.into_iter().collect();
        let tools = session.runtime.executor.registry();
        let tools = tools.surface_for_agent(&all, &session.root).definitions();
        let names = tools.into_iter().map(|tool| tool.name);
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
                "capabilities":required
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
        assert!(
            disabled
                .surface_for_agent(&session.root)
                .get(&name)
                .is_none()
        );
        let denied = json!({"text":"denied"});
        let direct = disabled.execute(session.root.clone(), &name, denied.clone(), None);
        assert!(direct.await.is_err());
        let scripted = disabled.execute_as(
            InvocationKind::Script,
            session.root.clone(),
            &name,
            denied,
            None,
        );
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
            instructions: Some(format!("Holds {}.", capabilities.len()).parse().unwrap()),
            hint: hint.map(|hint| hint.parse().unwrap()),
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
            hint: Some("Cheap".parse().unwrap()),
            ..crate::tests::profile("cheap", false)
        };
        let provider = scripted_provider(&requests, (0..3).map(|_| answer("done")));
        let harness = serving(root.path(), provider, [("cheap", cheap)])
            .session_root(root.path().join("hinted"))
            .max_child_depth(2)
            .modes(
                modes
                    .map(|(name, mode)| (name.parse().unwrap(), mode))
                    .into(),
            )
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        session.prompt("root request").await.unwrap();
        // `work` holds no network, and `secret` has no hint.
        assert_eq!(agent_input(0, "model")["enum"], json!(["test/cheap", null]));
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
        let child =
            "return await tool.agent({prompt:'go', depth:1, mode:'scout', model:'test/cheap'});";
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
        assert_eq!(child.model.as_str(), "cheap");
        assert!(
            child.system[0]
                .text
                .contains("<mode name=\"scout\">\nHolds 2.")
        );
        let records = session.runtime.store.records();
        let started = events!(&records, SessionEvent::AgentStarted { mode, capabilities, .. } => (mode.clone().map(|mode| mode.name), capabilities.clone()));
        // Interaction follows the parent; the mode grants the rest.
        let held = vec![read, agents, Capability::Interactive];
        assert_eq!(started[1], (Some(mode_name("scout")), held));
        shutdown_session(session).await;
    }

    /// Replacing a list completes without a result, inspecting returns the
    /// items, and blank text is rejected at its argument path.
    #[tokio::test]
    async fn todo_replace_has_no_result_and_inspect_returns_items() {
        use crate::tool::diagnostic::{Cause, Subject};
        let root = tempfile::tempdir().unwrap();
        let harness = builder(root.path(), Requests::default())
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let executor = &session.runtime.executor;
        let todo = |arguments| bounded(executor.run_model(&session.root, "todo", arguments));
        let items = json!([{"text":"done","status":"completed"}]);
        let replaced = todo(json!({"items":items})).await.unwrap();
        assert_eq!(replaced.output.value, json!({}));
        let inspected = todo(json!({})).await.unwrap();
        assert_eq!(inspected.output.value["result"], json!({"items":items}));
        let blank = json!([{"text":"done","status":"completed"}, {"text":" ","status":"pending"}]);
        let error = executor.execute(session.root.clone(), "todo", json!({"items":blank}), None);
        let error = bounded(error).await.unwrap_err();
        let diagnostic = error.diagnostic();
        assert_eq!(
            diagnostic.context.subject,
            Subject::argument(["items", "1", "text"])
        );
        let blank = Cause::InvalidArguments("text must not be blank".into());
        assert_eq!(diagnostic.cause, blank);
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
    /// not hold is unknown to it, and a restart returns it to the mode it started in.
    #[tokio::test]
    async fn a_child_keeps_its_mode_through_depth_delegation_and_restart() {
        use crate::tool::policy::Mode;
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let mode = |capabilities: &[Capability], instructions: &str| Mode {
            capabilities: capabilities.to_vec(),
            instructions: Some(instructions.parse().unwrap()),
            hint: Some("Hinted".parse().unwrap()),
        };
        let (read, write, agents) = (Capability::Read, Capability::Write, Capability::Agents);
        let build = |scout: Mode, responses: Vec<Vec<ResponseEvent>>| {
            let provider = scripted_provider(&requests, responses);
            let modes = [
                ("work", mode(&[read, write, agents], "Work.")),
                ("scout", scout),
            ];
            // No turn is scripted for the completions: the held window keeps them.
            test_builder(root.path(), &sessions, provider, false)
                .coalescing(wait::Coalescing::held().0)
                .max_child_depth(2)
                .modes(
                    modes
                        .map(|(name, mode)| (name.parse().unwrap(), mode))
                        .into(),
                )
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
        let records = session.runtime.store.records();
        let started = events!(&records, SessionEvent::AgentStarted { mode: Some(mode), capabilities, .. } if mode.name.as_str() == "scout" => capabilities.clone());
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
                captured[2].messages().last()
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

        // The configuration now widens `scout`; the retained child keeps the edition
        // it started in.
        let widened = mode(&[read, write, agents], "Changed.");
        let responses = vec![answer("again"), answer("fresh")];
        let harness = build(widened.clone(), responses).await.unwrap();
        let resumed = harness.resume_session(id).await.unwrap();
        resumed.running().await.unwrap();
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
        // A child starting now runs the configured definition.
        let fresh = resumed.run_script(spawn(0)).await.unwrap();
        assert_eq!(fresh.value["value"]["result"], "fresh");
        let child = requests.lock().unwrap().last().unwrap().clone();
        assert!(child.system[0].text.contains("Changed."));
        shutdown_session(resumed).await;
        // As decoded from the journal: the first child recorded `scout`'s first
        // edition, with its hint, and the new child its second.
        let opened = crate::session::SessionStore::open(&sessions, id).await;
        let records = opened.unwrap().records();
        let recorded = events!(&records, SessionEvent::AgentStarted { mode: Some(mode), .. } if mode.name.as_str() == "scout" => mode.definition.clone());
        assert_eq!(recorded, [Some(scout), None, Some(widened)]);
    }

    /// Children starting at once in a mode the session has not used record its
    /// edition once.
    #[tokio::test(flavor = "multi_thread")]
    async fn children_starting_together_record_their_mode_once() {
        use crate::tool::policy::Mode;
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let mode = |capabilities: &[Capability]| Mode {
            capabilities: capabilities.to_vec(),
            instructions: None,
            hint: Some("Hinted".parse().unwrap()),
        };
        let modes = [
            ("work", mode(&[Capability::Read, Capability::Agents])),
            ("look", mode(&[Capability::Read])),
        ];
        let provider = scripted_provider(&requests, (0..8).map(|_| answer("done")));
        let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
            .max_child_depth(1)
            .modes(
                modes
                    .map(|(name, mode)| (name.parse().unwrap(), mode))
                    .into(),
            )
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let script = "const all = [...Array(8)].map(() => tool.agent({prompt:'go', mode:'look'})); \
                      return (await Promise.all(all)).length;";
        assert_eq!(session.run_script(script).await.unwrap().value["value"], 8);
        let records = session.runtime.store.records();
        let looks = events!(&records, SessionEvent::AgentStarted { mode: Some(mode), .. } if mode.name.as_str() == "look" => mode.definition.is_some());
        assert_eq!(
            (
                looks.len(),
                looks.iter().filter(|recorded| **recorded).count()
            ),
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
        let session = ephemeral_session(&harness).await;
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
            "/remote/workspace".into(),
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
                answer: "host-answer".into(),
                ..Default::default()
            };
            // No turn is scripted for the completions: the held window keeps them.
            let harness = builder(root.path(), requests.clone())
                .coalescing(wait::Coalescing::held().0)
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
                for kind in [
                    InvocationKind::Host,
                    InvocationKind::Model,
                    InvocationKind::Script,
                ] {
                    let args = json!({"id":"root", "prompt":"question", "bg":background});
                    let agent = session.root.clone();
                    let result = bounded(executor.execute_as(kind, agent, "ask", args, None)).await;
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
                let jobs = session.runtime.jobs.list(&session.root).await.unwrap();
                assert!(jobs.iter().all(|job| job.tool != "ask"));
            }
            shutdown_session(session).await;
        }
    }

    /// A background handle reports the child's workspace as written, not where
    /// the tool runs; the child runs where it resolves.
    #[tokio::test]
    async fn background_child_handle_reports_the_childs_workspace() {
        let root = tempfile::tempdir().unwrap();
        let provider = scripted_provider(&Requests::default(), [answer("child done")]);
        // No turn is scripted for the completion: the held window keeps it.
        let builder = test_builder(root.path(), &root.path().join("sessions"), provider, false);
        let harness = builder.coalescing(wait::Coalescing::held().0).build();
        let session = ephemeral_session(&harness.await.unwrap()).await;
        let workspace = &session.runtime.harness.workspace;
        std::fs::create_dir(workspace.join("sub")).unwrap();
        std::os::unix::fs::symlink("sub", workspace.join("alias")).unwrap();
        let arguments = json!({"prompt":"work", "workspace":"alias", "bg":true});
        let executor = &session.runtime.executor;
        let launched = executor.execute(session.root.clone(), "agent", arguments, None);
        let launched = launched.await.unwrap();
        let written = workspace.join("alias");
        let handle = launched.output.value;
        assert_eq!(
            handle["meta"]["workspace"],
            written.to_string_lossy().as_ref()
        );
        let job = terminal(&session, launched.job).await;
        assert_eq!(job.location.workspace, written);
        let records = session.runtime.store.records();
        let started = events!(&records, SessionEvent::AgentStarted { owner_job: Some(_), location, .. } => location.workspace.clone());
        let resolved = std::fs::canonicalize(workspace.join("sub")).unwrap();
        assert_eq!(started, [resolved]);
        shutdown_session(session).await;
    }
}
