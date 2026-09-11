//! Collect job outcomes, import remote payloads, and present execution failures.

use super::*;
use base64::Engine as _;
use thiserror::Error;

impl ToolExecutor {
    pub(super) async fn collect_model_started(
        &self,
        name: &str,
        started: StartedExecution,
    ) -> Result<ExecutionResult, ExecutionError> {
        let job = started.job;
        let background = started.background;
        if !background {
            self.shared.jobs.wait_foreground(job).await?;
        }
        let mut output = self
            .shared
            .jobs
            .present_output_for(
                crate::job::output::OutputArgs::new(job),
                &self.capabilities,
                &self.caller_location,
                background,
            )
            .await?;
        // Automatic model responses reference independently published child
        // replies instead of returning their text again. This also handles
        // a background child that finishes before its launch is presented.
        // Explicit job_output and native script/host results are unchanged.
        if name == "agent"
            && output["state"] == "completed"
            && let Some(sequence) = self.shared.jobs.last_agent_message(job).await?
            && let Some(view) = output.as_object_mut()
        {
            view.remove("result");
            view.remove("preview");
            view.remove("truncated");
            view.insert("last_message".into(), serde_json::json!(sequence));
        }
        Ok(ExecutionResult {
            job,
            background,
            output: ToolOutput::new(output).with_images(self.shared.jobs.images(job).await?),
        })
    }

    pub(crate) async fn collect_started(
        &self,
        started: StartedExecution,
    ) -> Result<ExecutionResult, ExecutionError> {
        self.collect_up_to(started, u64::MAX).await
    }

    pub(crate) async fn collect_for_transfer(
        &self,
        started: StartedExecution,
    ) -> Result<ExecutionResult, ExecutionError> {
        self.collect_up_to(started, crate::job::output::PAGE_BYTES as u64)
            .await
    }

    async fn collect_up_to(
        &self,
        started: StartedExecution,
        maximum: u64,
    ) -> Result<ExecutionResult, ExecutionError> {
        let StartedExecution { job, background } = started;
        if background {
            let envelope = self.shared.jobs.metadata(job).await?;
            let value =
                envelope.presented_for(&self.capabilities, Some(&self.caller_location), true)?;
            return Ok(ExecutionResult {
                job,
                background: true,
                output: ToolOutput::new(value),
            });
        }
        let mut envelope = self.shared.jobs.wait_foreground(job).await?;
        self.shared
            .jobs
            .hydrate_envelope_up_to(&mut envelope, maximum)
            .await?;
        if !envelope.state.is_terminal() {
            return Ok(ExecutionResult {
                job,
                background: true,
                output: ToolOutput::new(envelope.presented_for(
                    &self.capabilities,
                    Some(&self.caller_location),
                    true,
                )?),
            });
        }
        if maximum == u64::MAX {
            self.shared.jobs.claim(job).await?;
        }
        let images = self.shared.jobs.images(job).await?;
        if envelope.state == JobState::Completed {
            Ok(ExecutionResult {
                job,
                background: false,
                output: ToolOutput {
                    value: envelope.output.unwrap_or(Value::Null),
                    images,
                },
            })
        } else if envelope.denial.is_some() {
            Err(ExecutionError::Denied(
                envelope
                    .error
                    .unwrap_or_else(|| "operation denied".to_owned()),
            ))
        } else {
            Err(ExecutionError::Failed {
                message: envelope
                    .error
                    .unwrap_or_else(|| format!("job ended as {:?}", envelope.state)),
                output: envelope
                    .output
                    .map(|value| ToolOutput { value, images })
                    .map(Box::new),
            })
        }
    }
}

pub(super) async fn import_remote_result(
    store: &crate::session::SessionStore,
    result: Result<ToolOutput, RemoteError>,
) -> Result<ToolOutput, ToolError> {
    match result {
        Ok(output) => import_remote_output(store, output).await,
        Err(RemoteError::Remote {
            message,
            output: Some(output),
        }) => Err(ToolError::with_output(
            message,
            import_remote_output(store, *output).await?,
        )),
        Err(error) => Err(error.into_tool_error()),
    }
}

async fn import_remote_output(
    store: &crate::session::SessionStore,
    mut output: ToolOutput,
) -> Result<ToolOutput, ToolError> {
    let mut imported = Vec::with_capacity(output.images.len());
    for image in output.images {
        let encoded = image
            .data_base64
            .as_deref()
            .ok_or_else(|| ToolError::Failed("remote image payload is missing".to_owned()))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        let reference = store
            .import_blob(&bytes, image.name, image.media_type)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        if reference.sha256 != image.sha256 {
            return Err(ToolError::Failed(
                "remote image hash did not match its payload".to_owned(),
            ));
        }
        imported.push(reference);
    }
    output.images = imported;
    Ok(output)
}

pub(crate) struct StartedExecution {
    pub job: JobId,
    pub(super) background: bool,
}

pub(crate) async fn persist_completion(jobs: &JobManager, job: JobId, completion: JobOutcome) {
    let result = jobs.finish(job, completion).await;
    if let Err(error) = result
        && !matches!(error, JobError::AlreadyTerminal(_))
    {
        jobs.fail_volatile(
            job,
            format!("job finalization could not be persisted: {error}"),
        )
        .await;
    }
}

#[derive(Debug)]
pub struct ExecutionResult {
    pub job: JobId,
    pub background: bool,
    pub output: ToolOutput,
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
    #[error("tool `{0}` is unavailable in this context")]
    UnavailableTool(String),
    #[error("tool `{0}` is not exposed for direct model calls")]
    ModelHidden(String),
    #[error("tool `{0}` is not available in scripts")]
    ScriptUnavailable(String),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Job(#[from] JobError),
    #[error("tool authorization denied: {0}")]
    Denied(String),
    #[error("tool execution failed: {message}")]
    Failed {
        message: String,
        output: Option<Box<ToolOutput>>,
    },
    #[error("could not serialize tool result: {0}")]
    Json(#[from] serde_json::Error),
}

impl ExecutionError {
    pub(crate) fn into_failure(self) -> super::ExecutionFailure {
        let message = self.concise_message();
        let denial = matches!(&self, Self::Denied(_) | Self::Tool(ToolError::Denied(_)))
            .then(crate::tool::Denial::permission_denied);
        let output = match self {
            Self::Failed { output, .. } => output.map(|output| *output),
            Self::Tool(ToolError::FailedWithOutput { output, .. }) => Some(*output),
            _ => None,
        };
        ExecutionFailure {
            message,
            output,
            denial,
        }
    }

    pub(crate) fn concise_message(&self) -> String {
        match self {
            Self::Tool(error) => error.concise_message(),
            Self::Failed { message, .. } => message.clone(),
            _ => self.to_string(),
        }
    }
}

pub(crate) struct ExecutionFailure {
    pub message: String,
    pub output: Option<ToolOutput>,
    pub denial: Option<crate::tool::Denial>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolRegistryBuilder;

    #[tokio::test]
    async fn automatic_completed_child_results_reference_independent_messages() {
        use crate::{
            job::JobSpec,
            provider::protocol::{AssistantContent, Message},
            session::SessionEvent,
        };

        // Cover foreground presentation and a background child that has already
        // finished before its launch response is collected, without a timing race.
        for background in [false, true] {
            let runtime = crate::tests::TestRuntime::new().await;
            let executor = runtime.executor(ToolRegistryBuilder::default());
            let child = runtime.agent.child(1);
            let job = runtime
                .jobs
                .create(JobSpec {
                    background,
                    ..JobSpec::test(runtime.agent.clone(), "agent")
                })
                .await
                .unwrap()
                .id;
            runtime
                .store
                .append(
                    child.clone(),
                    SessionEvent::AgentStarted {
                        parent: Some(runtime.agent.clone()),
                        owner_job: Some(job),
                        model_profile: "test".into(),
                        max_context: None,
                        location: ExecutionLocation::root(runtime.root.path().to_owned()),
                    },
                )
                .await
                .unwrap();
            runtime
                .jobs
                .transition(job, JobState::Running)
                .await
                .unwrap();
            let text = (0..500)
                .map(|line| format!("child answer line {line}\n"))
                .collect::<String>();
            let sequence = runtime
                .jobs
                .commit_child_message(
                    &child,
                    job,
                    Message::Assistant(vec![AssistantContent::text("answer", 0, text.clone())]),
                    text.clone(),
                )
                .await
                .unwrap();
            runtime
                .jobs
                .finish(
                    job,
                    JobOutcome::Completed(ToolOutput::new(serde_json::json!(text))),
                )
                .await
                .unwrap();

            let result = executor
                .collect_model_started("agent", StartedExecution { job, background })
                .await
                .unwrap();
            assert_eq!(result.output.value["state"], "completed");
            assert_eq!(result.output.value["last_message"], sequence);
            for field in ["result", "preview", "truncated"] {
                assert!(result.output.value.get(field).is_none(), "{field}");
            }
            // Claiming the model result must not claim the independently
            // deliverable reply, nor destroy the explicit saved final answer.
            let pending = runtime.jobs.pending_delivery(&runtime.agent).await.unwrap();
            assert_eq!(pending.messages().len(), 1);
            assert_eq!(pending.messages()[0].text, text);
            drop(pending);
            let saved = runtime.jobs.snapshot(job).await.unwrap();
            assert_eq!(saved.output, Some(serde_json::json!(text)));
        }
    }
}
