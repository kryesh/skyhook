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
    /// Execution target. Omit to inherit the caller; root selects the local host and root workspace.
    #[schemars(skip)]
    pub(super) target: Option<String>,
    /// Child workspace override: absolute, or relative to the workspace selected by target.
    pub(super) workspace: Option<PathBuf>,
}

#[derive(Serialize, JsonSchema)]
struct AgentOutput {
    agent: crate::identity::AgentId,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    result: String,
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
    builder.register::<AgentArgs, AgentOutput, _, _>(
        "agent",
        "Run a one-shot child agent with fresh conversation history. The job completes only after owned work finishes or is cancelled; questions suspend it and return a job envelope.",
        ToolOptions::default()
            .generated_output_schema(|capabilities| {
                let mut schema = serde_json::to_value(schemars::schema_for!(AgentOutput)).expect("agent output schema serializes");
                if !capabilities.contains(Capability::Targets) {
                    schema["properties"].as_object_mut().unwrap().remove("target");
                }
                schema
            })
            .requires(Capability::Agents)
            .conditional_input(
                "target",
                Capability::Targets,
                json!({
                    "type": ["string", "null"],
                    "description": "Execution target. Omit to inherit the caller; root selects the local host and root workspace."
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
                let explicit_root = input.target.as_deref() == Some(crate::target::ROOT_TARGET);
                let target = input.target.unwrap_or_else(|| context.caller_location.target.clone());
                let inherited = (target == context.caller_location.target && !explicit_root)
                    .then(|| context.caller_location.workspace.clone());
                let mut location = runtime.resolve_location(&target, inherited).await
                    .map_err(|error| tool_error(&error))?;
                if let Some(workspace) = input.workspace {
                    if workspace.as_os_str().is_empty() {
                        return Err(ToolError::InvalidArguments("workspace cannot be empty".to_owned()));
                    }
                    location.workspace = location.workspace.join(workspace);
                }
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
                            return Ok(AgentOutput { agent: child, target: result_target, result: text });
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
