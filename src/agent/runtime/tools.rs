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
    agent::{Question, QuestionError, TodoItem},
    provider::protocol::UserContent,
    session::SessionEvent,
    tool::{RegistryError, ToolError, ToolRegistryBuilder, policy::ToolEffect},
};

use super::{AgentCommand, SessionRuntime};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AskArgs {
    /// One or more structured questions to present as a single answer batch.
    questions: Vec<Question>,
}

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
    builder.register::<AskArgs, Value, _, _>(
        "ask",
        "Ask the host (root agent) or owning parent agent (child agent) structured questions.",
        vec![ToolEffect::Interaction],
        false,
        true,
        move |context, input| {
            let runtime = runtime_slot.get().and_then(Weak::upgrade);
            async move {
                let runtime = runtime.ok_or_else(runtime_unavailable)?;
                let question_id = format!("q-{}", context.job);
                runtime
                    .store
                    .append(
                        context.agent.clone(),
                        SessionEvent::QuestionOpened {
                            job: context.job,
                            question_id: question_id.clone(),
                            questions: serde_json::to_value(&input.questions)?,
                        },
                    )
                    .await
                    .map_err(|error| tool_error(&error))?;
                let answers = if context.agent.parent().is_some() {
                    let owner_job = runtime
                        .open_child_question(
                            context.job,
                            json!({
                                "kind": "questions",
                                "question_id": question_id,
                                "questions": input.questions,
                            }),
                        )
                        .await
                        .map_err(|error| tool_error(&error))?;
                    let received = context.receive().await;
                    if received.is_err() {
                        let _ = runtime.resolve_child_question(owner_job).await;
                    }
                    let answers = received?;
                    runtime
                        .store
                        .append(
                            context.agent.clone(),
                            SessionEvent::QuestionResolved {
                                job: context.job,
                                question_id: question_id.clone(),
                                answers: answers.clone(),
                            },
                        )
                        .await
                        .map_err(|error| tool_error(&error))?;
                    runtime
                        .resolve_child_question(owner_job)
                        .await
                        .map_err(|error| tool_error(&error))?;
                    return Ok(answers);
                } else {
                    let handler =
                        runtime.harness.questions.clone().ok_or_else(|| {
                            ToolError::Failed(QuestionError::Unavailable.to_string())
                        })?;
                    handler
                        .ask(context.agent.clone(), input.questions)
                        .await
                        .map_err(|error| tool_error(&error))?
                };
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
        vec![ToolEffect::SessionState],
        false,
        false,
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
    builder.register_effectful::<AgentArgs, Value, _, _, _>(
        "agent",
        "Run a one-shot child agent locally or on a named SSH target.",
        vec![ToolEffect::SessionState],
        |input| {
            let mut effects = vec![ToolEffect::SessionState];
            if input.target.as_deref().is_some_and(|target| target != crate::target::ROOT_TARGET) {
                effects.extend([ToolEffect::RemoteAccess, ToolEffect::Network]);
            }
            effects
        },
        true,
        true,
        move |context, input| {
            let runtime = runtime_slot.get().and_then(Weak::upgrade);
            async move {
                let runtime = runtime.ok_or_else(runtime_unavailable)?;
                let child = runtime.next_child(&context.agent).await;
                let model = input.model
                    .unwrap_or_else(|| runtime.harness.default_model_profile.clone());
                let agent_profile = input.profile
                    .or_else(|| runtime.harness.default_agent_profile.clone());
                if let Some(target) = input.target.as_deref().filter(|target| *target != crate::target::ROOT_TARGET) {
                    let remote = runtime.run_remote_child(
                        &context,
                        child,
                        target.to_owned(),
                        input.workspace,
                        model,
                        agent_profile,
                        input.prompt,
                        input.todo,
                    );
                    tokio::pin!(remote);
                    loop {
                        tokio::select! {
                            result = &mut remote => {
                                return result.map_err(|error| tool_error(&error));
                            }
                            value = context.receive() => {
                                let value = match value {
                                    Ok(value) => value,
                                    Err(error) => {
                                        runtime.cancel_child_question(context.job).await;
                                        return Err(error);
                                    }
                                };
                                if !runtime.answer_child_question(context.job, value.clone()).await
                                    .map_err(|error| tool_error(&error))?
                                {
                                    return Err(ToolError::Failed(
                                        "agent is not waiting for owner input".to_owned(),
                                    ));
                                }
                            }
                        }
                    }
                }
                let sender = runtime.spawn_agent(
                    child.clone(), Some(context.agent.clone()), Some(context.job), model,
                    agent_profile, Vec::new(), true,
                ).await.map_err(|error| tool_error(&error))?;
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
                            return Ok(json!({"agent": child, "result": text}));
                        }
                        value = context.receive() => {
                            let value = match value {
                                Ok(value) => value,
                                Err(error) => {
                                    runtime.cancel_child_question(context.job).await;
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
