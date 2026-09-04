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
    agent::Question,
    provider::protocol::UserContent,
    session::SessionEvent,
    tool::{RegistryError, ToolError, ToolOptions, ToolRegistryBuilder, policy::Capability},
};

use super::{AgentCommand, AgentLaunch, SessionRuntime};

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
    /// Named SSH target. Omit or use `root` to run locally.
    #[schemars(skip)]
    pub(super) target: Option<String>,
    /// Initial workspace override for the child.
    pub(super) workspace: Option<PathBuf>,
}

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
) -> Result<(), RegistryError> {
    register_ask(builder, runtime_slot.clone())?;
    register_child_agent(builder, runtime_slot)
}

fn register_ask(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
) -> Result<(), RegistryError> {
    builder.register::<Question, Value, _, _>(
        "ask",
        "Ask one structured question. Issue independent questions concurrently; the runtime merges calls that become ready together.",
        ToolOptions::default().input(),
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
    builder.register::<AgentArgs, Value, _, _>(
        "agent",
        "Run a one-shot child agent.",
        ToolOptions::default()
            .requires(Capability::Agents)
            .conditional_input(
                "target",
                Capability::Targets,
                json!({
                    "type": ["string", "null"],
                    "description": "Named SSH target. Omit or use `root` to run locally."
                }),
            )
            .background()
            .input(),
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
                let target = input
                    .target
                    .unwrap_or_else(|| context.caller_location.target.clone());
                let workspace = input.workspace.or_else(|| {
                    (target == context.caller_location.target)
                        .then(|| context.caller_location.workspace.clone())
                });
                let location = runtime
                    .resolve_location(&target, workspace)
                    .await
                    .map_err(|error| tool_error(&error))?;
                let result_target = context
                    .capabilities
                    .contains(Capability::Targets)
                    .then_some(location.target.clone())
                    .filter(|target| target != crate::target::ROOT_TARGET);
                let sender = runtime.spawn_agent(AgentLaunch {
                    id: child.clone(),
                    parent: Some(context.agent.clone()),
                    owner_job: Some(context.job),
                    model_profile: model,
                    agent_profile,
                    history: Vec::new(),
                    one_shot: true,
                    available_depth: input.depth,
                    location,
                }).await.map_err(|error| tool_error(&error))?;
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
                                    runtime.questions.cancel_child_question(context.job).await;
                                    runtime.interrupt_tree(&child).await;
                                    return Err(error);
                                }
                            };
                            if !runtime.questions.answer_child_question(context.job, value.clone()).await
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
