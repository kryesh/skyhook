use std::{future::Future, panic::AssertUnwindSafe, path::PathBuf, sync::Arc};

use futures_util::FutureExt;
use serde_json::Value;
use thiserror::Error;

use crate::{
    identity::{AgentId, JobId},
    job::{JobError, JobManager, JobState},
    tool::policy::{AuthorizationRequest, Policy, PolicyDecision},
};

use super::{ToolContext, ToolError, ToolOutput, ToolRegistry};

#[derive(Clone)]
pub struct ToolExecutor {
    registry: ToolRegistry,
    policy: Arc<dyn Policy>,
    jobs: JobManager,
    workspace: PathBuf,
}

impl ToolExecutor {
    #[must_use]
    pub fn new(
        registry: ToolRegistry,
        policy: Arc<dyn Policy>,
        jobs: JobManager,
        workspace: PathBuf,
    ) -> Self {
        Self {
            registry,
            policy,
            jobs,
            workspace,
        }
    }

    #[must_use]
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    #[must_use]
    pub fn jobs(&self) -> &JobManager {
        &self.jobs
    }

    pub async fn execute(
        &self,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> Result<ExecutionResult, ExecutionError> {
        let tool = self
            .registry
            .get(name)
            .ok_or_else(|| ExecutionError::UnknownTool(name.to_owned()))?;
        let original = arguments.clone();
        let (arguments, background) = self.registry.split_execution(&tool, arguments)?;
        let effects = tool.effects_for(&arguments)?;
        let lease = self
            .jobs
            .create(
                agent.clone(),
                parent,
                tool.name.clone(),
                original.clone(),
                tool.accepts_input,
                background,
            )
            .await?;
        self.jobs
            .transition(lease.id, JobState::AwaitingApproval)
            .await?;
        let decision = self
            .policy
            .authorize(AuthorizationRequest {
                agent: agent.clone(),
                job: lease.id,
                tool: tool.name.clone(),
                effects,
                arguments: original,
            })
            .await;
        if let PolicyDecision::Deny { reason } = decision {
            self.jobs
                .finish(lease.id, Err(reason.clone()), None)
                .await?;
            return Err(ExecutionError::Denied(reason));
        }
        self.jobs.transition(lease.id, JobState::Running).await?;
        let context = ToolContext::new(
            agent,
            lease.id,
            self.workspace.clone(),
            lease.cancellation.clone(),
            lease.input,
            self.jobs.progress_sink(lease.id),
        );
        let jobs = self.jobs.clone();
        let job = lease.id;
        tokio::spawn(async move {
            let call = AssertUnwindSafe(tool.call(context, arguments))
                .catch_unwind()
                .await;
            let (result, terminal) = match call {
                Ok(Ok(output)) => (Ok(output), None),
                Ok(Err(ToolError::Cancelled)) => (
                    Err("tool was cancelled".to_owned()),
                    Some(JobState::Cancelled),
                ),
                Ok(Err(ToolError::FailedWithOutput { message, output })) => {
                    let _ = jobs.finish_failed(job, message, Some(output), None).await;
                    return;
                }
                Ok(Err(error)) => (Err(error.to_string()), None),
                Err(_) => (Err("tool handler panicked".to_owned()), None),
            };
            let _ = jobs.finish(job, result, terminal).await;
        });
        if background {
            let envelope = self.jobs.snapshot(job).await?;
            Ok(ExecutionResult {
                job,
                background: true,
                output: ToolOutput::new(serde_json::to_value(envelope)?),
            })
        } else {
            let envelope = self.jobs.wait_foreground(job).await?;
            if !envelope.state.is_terminal() {
                return Ok(ExecutionResult {
                    job,
                    background: true,
                    output: ToolOutput::new(serde_json::to_value(envelope)?),
                });
            }
            self.jobs.claim(job).await?;
            if envelope.state == JobState::Completed {
                Ok(ExecutionResult {
                    job,
                    background: false,
                    output: ToolOutput {
                        value: envelope.output.unwrap_or(Value::Null),
                        images: self.jobs.images(job).await?,
                    },
                })
            } else {
                Err(ExecutionError::Failed(envelope.error.unwrap_or_else(
                    || format!("job ended as {:?}", envelope.state),
                )))
            }
        }
    }

    /// Supervise a tool invocation whose implementation executes in another runtime.
    pub async fn execute_external<F, Fut>(
        &self,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
        extra_effects: Vec<crate::tool::policy::ToolEffect>,
        run: F,
    ) -> Result<ExecutionResult, ExecutionError>
    where
        F: FnOnce(Value) -> Fut + Send + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        let tool = self
            .registry
            .get(name)
            .ok_or_else(|| ExecutionError::UnknownTool(name.to_owned()))?;
        let original = arguments.clone();
        let (arguments, background) = self.registry.split_execution(&tool, arguments)?;
        let mut effects = tool.effects_for(&arguments)?;
        effects.extend(extra_effects);
        let lease = self
            .jobs
            .create(
                agent.clone(),
                parent,
                tool.name.clone(),
                original.clone(),
                tool.accepts_input,
                background,
            )
            .await?;
        self.jobs
            .transition(lease.id, JobState::AwaitingApproval)
            .await?;
        let decision = self
            .policy
            .authorize(AuthorizationRequest {
                agent,
                job: lease.id,
                tool: tool.name.clone(),
                effects,
                arguments: original,
            })
            .await;
        if let PolicyDecision::Deny { reason } = decision {
            self.jobs
                .finish(lease.id, Err(reason.clone()), None)
                .await?;
            return Err(ExecutionError::Denied(reason));
        }
        self.jobs.transition(lease.id, JobState::Running).await?;
        let jobs = self.jobs.clone();
        let job = lease.id;
        tokio::spawn(async move {
            let call = AssertUnwindSafe(run(arguments)).catch_unwind().await;
            match call {
                Ok(Ok(output)) => {
                    let _ = jobs.finish(job, Ok(output), None).await;
                }
                Ok(Err(ToolError::Cancelled)) => {
                    let _ = jobs
                        .finish(
                            job,
                            Err("tool was cancelled".to_owned()),
                            Some(JobState::Cancelled),
                        )
                        .await;
                }
                Ok(Err(ToolError::FailedWithOutput { message, output })) => {
                    let _ = jobs.finish_failed(job, message, Some(output), None).await;
                }
                Ok(Err(error)) => {
                    let _ = jobs.finish(job, Err(error.to_string()), None).await;
                }
                Err(_) => {
                    let _ = jobs
                        .finish(job, Err("tool handler panicked".to_owned()), None)
                        .await;
                }
            }
        });
        if background {
            let envelope = self.jobs.snapshot(job).await?;
            return Ok(ExecutionResult {
                job,
                background: true,
                output: ToolOutput::new(serde_json::to_value(envelope)?),
            });
        }
        let envelope = self.jobs.wait_foreground(job).await?;
        if !envelope.state.is_terminal() {
            return Ok(ExecutionResult {
                job,
                background: true,
                output: ToolOutput::new(serde_json::to_value(envelope)?),
            });
        }
        self.jobs.claim(job).await?;
        if envelope.state == JobState::Completed {
            Ok(ExecutionResult {
                job,
                background: false,
                output: ToolOutput {
                    value: envelope.output.unwrap_or(Value::Null),
                    images: self.jobs.images(job).await?,
                },
            })
        } else {
            Err(ExecutionError::Failed(envelope.error.unwrap_or_else(
                || format!("job ended as {:?}", envelope.state),
            )))
        }
    }
}

pub struct ExecutionResult {
    pub job: JobId,
    pub background: bool,
    pub output: ToolOutput,
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Job(#[from] JobError),
    #[error("tool authorization denied: {0}")]
    Denied(String),
    #[error("tool execution failed: {0}")]
    Failed(String),
    #[error("could not serialize tool result: {0}")]
    Json(#[from] serde_json::Error),
}
