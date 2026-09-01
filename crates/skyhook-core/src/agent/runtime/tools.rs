//! Agent-specific tools layered on top of the general coding tool set.

use std::{
    path::PathBuf,
    sync::{Arc, OnceLock, Weak},
};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::{
    agent::{Question, TodoItem},
    provider::protocol::UserContent,
    session::SessionEvent,
    tool::{RegistryError, ToolError, ToolOptions, ToolRegistryBuilder, policy::ToolEffect},
};

use super::{AgentCommand, AgentLaunch, SessionRuntime};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TodoArgs {
    items: Vec<TodoItem>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AgentArgs {
    /// Complete task for the one-shot child agent.
    pub(super) prompt: String,
    /// Further agent generations available to the child. Defaults to zero.
    #[serde(default)]
    pub(super) depth: usize,
    /// Model profile override.
    pub(super) model: Option<String>,
    /// Agent profile override.
    pub(super) profile: Option<String>,
    /// Initial todo snapshot for the child.
    #[serde(default)]
    pub(super) todo: Vec<TodoItem>,
    /// Named SSH target. Omit or use `root` to run locally.
    pub(super) target: Option<String>,
    /// Workspace override on the selected target.
    pub(super) workspace: Option<PathBuf>,
}

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
) -> Result<(), RegistryError> {
    register_ask(builder, runtime_slot.clone())?;
    register_todo(builder, runtime_slot.clone())?;
    register_child_agent(builder, runtime_slot)
}

fn register_ask(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
) -> Result<(), RegistryError> {
    builder.register::<Question, Value, _, _>(
        "ask",
        "Ask one structured question. Issue independent questions concurrently; the runtime merges calls that become ready together.",
        ToolOptions::new(vec![ToolEffect::Interaction]).input(),
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
                let answers = runtime.coordinate_question(context.clone(), input).await?;
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

fn register_todo(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
) -> Result<(), RegistryError> {
    builder.register::<TodoArgs, Vec<TodoItem>, _, _>(
        "todo",
        "Replace the current agent's complete todo snapshot.",
        ToolOptions::new(vec![ToolEffect::SessionState]),
        move |context, input| {
            let runtime = runtime_slot.get().and_then(Weak::upgrade);
            async move {
                let runtime = runtime.ok_or_else(runtime_unavailable)?;
                runtime
                    .store
                    .append(
                        context.agent,
                        SessionEvent::TodoReplaced {
                            items: input.items.clone(),
                        },
                    )
                    .await
                    .map_err(|error| tool_error(&error))?;
                Ok(input.items)
            }
        },
    )?;
    Ok(())
}

fn register_child_agent(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
) -> Result<(), RegistryError> {
    let availability_slot = runtime_slot.clone();
    builder.register_effectful::<AgentArgs, Value, _, _, _>(
        "agent",
        "Run a one-shot child agent locally or on a named SSH target.",
        ToolOptions::new(vec![ToolEffect::SessionState])
            .background()
            .input()
            .target_path_argument("workspace", crate::tool::policy::PathAccess::Read, crate::tool::PathKind::Existing)
            .target_path_argument("workspace", crate::tool::policy::PathAccess::Write, crate::tool::PathKind::Existing)
            .available_when(move |context| {
                availability_slot
                    .get()
                    .and_then(Weak::upgrade)
                    .is_some_and(|runtime| runtime.available_depth(&context.agent) > 0)
            }),
        |input| {
            let mut effects = vec![ToolEffect::SessionState];
            if input.target.as_deref().is_some_and(|target| target != crate::target::ROOT_TARGET) {
                effects.extend([ToolEffect::RemoteAccess, ToolEffect::Network]);
            }
            effects
        },
        move |context, input| {
            let runtime = runtime_slot.get().and_then(Weak::upgrade);
            async move {
                let runtime = runtime.ok_or_else(runtime_unavailable)?;
                let available_depth = runtime.available_depth(&context.agent);
                if input.depth >= available_depth {
                    return Err(ToolError::InvalidArguments(format!(
                        "depth must be less than the caller's available depth of {available_depth}"
                    )));
                }
                let child = runtime.next_child(&context.agent).await;
                let model = input
                    .model
                    .unwrap_or_else(|| runtime.harness.default_model_profile.clone());
                let agent_profile = input
                    .profile
                    .or_else(|| runtime.harness.default_agent_profile.clone());
                let target = input.target;
                let result_target = target
                    .as_deref()
                    .filter(|target| *target != crate::target::ROOT_TARGET)
                    .map(str::to_owned);
                let sender = runtime.spawn_agent(AgentLaunch {
                    id: child.clone(),
                    parent: Some(context.agent.clone()),
                    owner_job: Some(context.job),
                    model_profile: model,
                    agent_profile,
                    history: Vec::new(),
                    one_shot: true,
                    available_depth: input.depth,
                    target,
                    workspace: input.workspace,
                }).await.map_err(|error| tool_error(&error))?;
                if !input.todo.is_empty() {
                    runtime.store.append(
                        child.clone(),
                        SessionEvent::TodoReplaced { items: input.todo },
                    ).await.map_err(|error| tool_error(&error))?;
                }
                let (done_tx, mut done_rx) = oneshot::channel();
                sender.send(AgentCommand::Input {
                    content: vec![UserContent::Text { text: input.prompt }],
                    done: Some(done_tx),
                }).await.map_err(|_| ToolError::Failed("child agent stopped".to_owned()))?;
                loop {
                    tokio::select! {
                        result = &mut done_rx => {
                            let text = result
                                .map_err(|_| ToolError::Failed("child agent stopped".to_owned()))?
                                .map_err(ToolError::Failed)?;
                            return Ok(match result_target {
                                Some(target) => json!({"agent": child, "target": target, "result": text}),
                                None => json!({"agent": child, "result": text}),
                            });
                        }
                        value = context.receive() => {
                            let value = match value {
                                Ok(value) => value,
                                Err(error) => {
                                    runtime.cancel_child_question(context.job).await;
                                    runtime.interrupt_tree(&child).await;
                                    return Err(error);
                                }
                            };
                            if !runtime.answer_child_question(context.job, value.clone()).await
                                .map_err(|error| tool_error(&error))?
                            {
                                sender.send(AgentCommand::Input {
                                    content: vec![UserContent::Runtime { text: format!("Owner input: {value}") }],
                                    done: None,
                                }).await.map_err(|_| ToolError::Failed("child agent stopped".to_owned()))?;
                            }
                        }
                    }
                }
            }
        },
    )?;
    Ok(())
}

fn runtime_unavailable() -> ToolError {
    ToolError::Failed("session runtime is unavailable".to_owned())
}

fn tool_error(error: &impl ToString) -> ToolError {
    ToolError::Failed(error.to_string())
}
