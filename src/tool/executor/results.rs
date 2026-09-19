//! Collect job outcomes, import remote payloads, and present execution failures.

use super::*;
use thiserror::Error;

#[derive(Clone, Copy)]
enum CollectionPurpose {
    Native,
    Transfer,
}

impl ToolExecutor {
    pub(super) async fn collect_model_started(
        &self,
        started: StartedExecution,
    ) -> Result<ExecutionResult, ExecutionError> {
        let job = started.job;
        let background = started.background;
        if !background {
            self.shared.jobs.wait_foreground(job).await?;
        }
        let presented = self
            .shared
            .jobs
            .present_output_with(
                crate::job::output::OutputArgs::new(job),
                &self.capabilities,
                crate::job::output::OutputOptions::Model {
                    viewer: Some(&self.caller_location),
                    detailed: background,
                    presentation: crate::job::OutputPresentation::Automatic,
                },
            )
            .await?;
        let (state, output, images) = presented.into_parts();
        Ok(ExecutionResult {
            job,
            background,
            is_error: matches!(
                state,
                JobState::Failed | JobState::Cancelled | JobState::Interrupted
            ),
            output: ToolOutput::new(output).with_images(images),
        })
    }

    pub(crate) async fn collect_started(
        &self,
        started: StartedExecution,
    ) -> Result<ExecutionResult, ExecutionError> {
        self.collect_result(started, CollectionPurpose::Native)
            .await
    }

    pub(crate) async fn collect_for_transfer(
        &self,
        started: StartedExecution,
    ) -> Result<ExecutionResult, ExecutionError> {
        self.collect_result(started, CollectionPurpose::Transfer)
            .await
    }

    async fn collect_result(
        &self,
        started: StartedExecution,
        purpose: CollectionPurpose,
    ) -> Result<ExecutionResult, ExecutionError> {
        let StartedExecution { job, background } = started;
        if background {
            let envelope = self.shared.jobs.metadata(job).await?;
            let value =
                envelope.presented_for(&self.capabilities, Some(&self.caller_location), true)?;
            return Ok(ExecutionResult {
                job,
                background: true,
                is_error: false,
                output: ToolOutput::new(value),
            });
        }
        let mut envelope = match purpose {
            CollectionPurpose::Native => self.shared.jobs.wait_foreground(job).await?,
            CollectionPurpose::Transfer => {
                self.shared.jobs.wait_foreground_for_transfer(job).await?
            }
        };
        match purpose {
            CollectionPurpose::Native => self.shared.jobs.hydrate_envelope(&mut envelope).await?,
            CollectionPurpose::Transfer => {
                self.shared
                    .jobs
                    .hydrate_envelope_up_to(&mut envelope, crate::job::output::PAGE_BYTES as u64)
                    .await?
            }
        }
        if !envelope.state.is_terminal() {
            return Ok(ExecutionResult {
                job,
                background: true,
                is_error: false,
                output: ToolOutput::new(envelope.presented_for(
                    &self.capabilities,
                    Some(&self.caller_location),
                    true,
                )?),
            });
        }
        if matches!(purpose, CollectionPurpose::Native) {
            self.shared.jobs.claim(job).await?;
        }
        let images = self.shared.jobs.images(job).await?;
        if envelope.state == JobState::Completed {
            Ok(ExecutionResult {
                job,
                background: false,
                is_error: false,
                output: ToolOutput::new(envelope.output.unwrap_or(Value::Null)).with_images(images),
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
                    .map(|value| ToolOutput::new(value).with_images(images))
                    .map(Box::new),
            })
        }
    }
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
    pub(crate) is_error: bool,
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
    use crate::{
        job::{JobRole, JobSpec},
        provider::protocol::{AssistantContent, Message},
        tool::ToolRegistryBuilder,
    };

    async fn created(runtime: &crate::tests::TestRuntime, spec: JobSpec) -> JobId {
        runtime.jobs.create(spec).await.unwrap().into_test_id()
    }

    async fn complete(runtime: &crate::tests::TestRuntime, job: JobId, value: serde_json::Value) {
        let outcome = JobOutcome::Completed(ToolOutput::new(value));
        runtime.jobs.finish(job, outcome).await.unwrap();
    }

    #[tokio::test]
    async fn transfer_collection_does_not_consume_delivery() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = runtime.executor(ToolRegistryBuilder::default());
        // Both paths collect the same small completed payload. Only native
        // consumption acknowledges delivery; transfer is not consumption.
        let spec = JobSpec {
            background: true,
            ..JobSpec::test(runtime.agent.clone(), "transfer")
        };
        let job = created(&runtime, spec).await;
        complete(&runtime, job, serde_json::json!(42)).await;
        let started = StartedExecution {
            job,
            background: false,
        };
        let transfer = executor.collect_for_transfer(started).await.unwrap();
        assert_eq!(transfer.output.value, 42);
        assert!(runtime.jobs.has_pending(&runtime.agent).await);
        let native = executor
            .collect_started(StartedExecution {
                job,
                background: false,
            })
            .await
            .unwrap();
        assert_eq!(native.output.value, 42);
        assert!(!runtime.jobs.has_pending(&runtime.agent).await);
    }

    #[tokio::test]
    async fn automatic_completed_child_results_reference_independent_messages() {
        // Cover foreground presentation and a background child that has already
        // finished before its launch response is collected, without a timing race.
        for background in [false, true] {
            let runtime = crate::tests::TestRuntime::new().await;
            let executor = runtime.executor(ToolRegistryBuilder::default());
            let child = runtime.agent.child(1);
            let spec = JobSpec {
                background,
                role: JobRole::Agent,
                ..JobSpec::test(runtime.agent.clone(), "delegate")
            };
            let job = created(&runtime, spec).await;
            let started = crate::session::fixture::child_started(
                Some(runtime.agent.clone()),
                Some(job),
                ExecutionLocation::root(runtime.root.path().to_owned()),
            );
            runtime.store.append(child.clone(), started).await.unwrap();
            runtime
                .jobs
                .transition(job, JobState::Running)
                .await
                .unwrap();
            let text: String = (0..500)
                .map(|line| format!("child answer line {line}\n"))
                .collect();
            let message =
                Message::Assistant(vec![AssistantContent::text("answer", 0, text.clone())]);
            let sequence = runtime
                .jobs
                .commit_child_message(&child, job, message, text.clone(), true, |_| Vec::new())
                .await
                .unwrap();
            complete(&runtime, job, serde_json::json!(text)).await;

            let result = executor
                .collect_model_started(StartedExecution { job, background })
                .await
                .unwrap();
            assert_eq!(result.output.value["state"], "completed");
            let pending = runtime.jobs.pending_delivery(&runtime.agent).await.unwrap();
            if background {
                // The reply is an independently delivered event; the result
                // references it, and claiming the result leaves it pending.
                assert_eq!(result.output.value["last_message"], sequence);
                for field in ["result", "preview", "truncated"] {
                    assert!(result.output.value.get(field).is_none(), "{field}");
                }
                assert_eq!(pending.messages().len(), 1);
                assert_eq!(pending.messages()[0].text, text);
            } else {
                // A foreground call returns its answer; nothing is delivered later.
                assert!(result.output.value.get("last_message").is_none());
                let presented = serde_json::to_string(&result.output.value).unwrap();
                assert!(presented.contains("child answer line 0"), "{presented}");
                assert!(pending.messages().is_empty());
            }
            drop(pending);
            // The saved final answer survives either way.
            let saved = runtime.jobs.snapshot(job).await.unwrap();
            assert_eq!(saved.output, Some(serde_json::json!(text)));
        }
    }
}
