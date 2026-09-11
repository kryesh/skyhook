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
