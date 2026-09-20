//! Collect job outcomes, import remote payloads, and present execution failures.

use super::*;
use thiserror::Error;

#[derive(Clone, Copy)]
enum CollectionPurpose {
    Native,
    Public(crate::tool::ToolResultPolicy),
}

impl ToolExecutor {
    pub(super) async fn collect_model_started(
        &self,
        started: StartedExecution,
    ) -> Result<ExecutionResult, ExecutionError> {
        // A launch always returns metadata, even if the worker won the race to
        // completion. Reading its result here would also consume its notification.
        if started.background {
            return self
                .collect_full_view_started(started, crate::tool::ToolResultPolicy::Value)
                .await;
        }
        let job = started.job;
        self.shared.jobs.wait_foreground(job).await?;
        let presented = self
            .shared
            .jobs
            .present_output_with(
                crate::job::output::OutputArgs::new(job),
                &self.capabilities,
                crate::job::output::OutputOptions::Model {
                    presentation: crate::job::OutputPresentation::Automatic,
                },
            )
            .await?;
        let (state, output, images) = presented.into_parts();
        Ok(ExecutionResult {
            job,
            background: false,
            is_error: matches!(
                state,
                JobState::Failed | JobState::Cancelled | JobState::Interrupted
            ),
            output: ToolOutput::new(output).with_images(images),
        })
    }

    /// The public script contract is the same envelope as model calls, but with
    /// complete captured data rather than an automatic preview. Host
    /// collectors intentionally retain the handler's native payload.
    pub(super) async fn collect_full_view_started(
        &self,
        started: StartedExecution,
        policy: crate::tool::ToolResultPolicy,
    ) -> Result<ExecutionResult, ExecutionError> {
        self.collect_result(started, CollectionPurpose::Public(policy))
            .await
    }

    pub(crate) async fn collect_started(
        &self,
        started: StartedExecution,
    ) -> Result<ExecutionResult, ExecutionError> {
        self.collect_result(started, CollectionPurpose::Native)
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
            let value = envelope.metadata_view(&self.capabilities).into_value();
            return Ok(ExecutionResult {
                job,
                background: true,
                is_error: false,
                output: ToolOutput::new(value),
            });
        }
        let mut envelope = self.shared.jobs.wait_foreground(job).await?;
        self.shared.jobs.hydrate_envelope(&mut envelope).await?;
        if !envelope.state.is_terminal() {
            return Ok(ExecutionResult {
                job,
                background: true,
                is_error: false,
                output: ToolOutput::new(
                    match purpose {
                        CollectionPurpose::Public(_) => envelope.response_view(&self.capabilities),
                        CollectionPurpose::Native => envelope.metadata_view(&self.capabilities),
                    }
                    .into_value(),
                ),
            });
        }
        self.shared.jobs.claim(job).await?;
        let images = self.shared.jobs.images(job).await?;
        if let CollectionPurpose::Public(policy) = purpose {
            // A successful output query already returns the target view. The
            // invocation's state, not the target's, determines is_error.
            let is_error = envelope.state != JobState::Completed;
            let value = if !is_error && policy == crate::tool::ToolResultPolicy::JobView {
                envelope
                    .output
                    .take()
                    .ok_or_else(|| ExecutionError::Failed {
                        message: "job-output query completed without a response".into(),
                        output: None,
                    })?
            } else {
                envelope.response_view(&self.capabilities).into_value()
            };
            return Ok(ExecutionResult {
                job,
                background: false,
                is_error,
                output: ToolOutput::new(value).with_images(images),
            });
        }
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

impl From<crate::tool::AdmissionError> for ExecutionError {
    fn from(error: crate::tool::AdmissionError) -> Self {
        Self::Tool(error.into())
    }
}

impl ExecutionError {
    /// Errors before a job can provide its own view still use the public response
    /// contract. A null ID explicitly means there is no inspectable job handle.
    pub(crate) fn into_response(
        self,
        tool: &str,
        parent: Option<JobId>,
        name: Option<&str>,
    ) -> ToolOutput {
        let failure = self.into_failure();
        let (value, images) = failure.output.map_or((None, Vec::new()), |output| {
            (Some(output.value), output.images)
        });
        let metadata = crate::job::JobMetadata {
            tool: Some(tool.to_owned()),
            parent,
            name: name.map(str::to_owned),
            ..Default::default()
        };
        let view = crate::job::JobView::failure(failure.message, value, failure.denial, metadata);
        ToolOutput::new(view.into_value()).with_images(images)
    }

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
    use serde_json::json;

    async fn created(runtime: &crate::tests::TestRuntime, spec: JobSpec) -> JobId {
        runtime.jobs.create(spec).await.unwrap().into_test_id()
    }

    async fn complete(runtime: &crate::tests::TestRuntime, job: JobId, value: serde_json::Value) {
        let outcome = JobOutcome::Completed(ToolOutput::new(value));
        runtime.jobs.finish(job, outcome).await.unwrap();
    }

    #[tokio::test]
    async fn public_responses_preserve_payloads_and_distinguish_invocation_from_target_failures() {
        const PAYLOAD_BYTES: usize = crate::job::output::CONTENT_BYTES + 1;
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        struct Args {
            #[serde(default)]
            fail: bool,
        }
        #[derive(serde::Serialize, schemars::JsonSchema)]
        struct Payload {
            #[schemars(extend("x-skyhook-truncatable" = true))]
            text: String,
            nullable: Option<String>,
        }
        let runtime = crate::tests::TestRuntime::new().await;
        let partial = json!({"partial": null});
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Args, Payload, _, _>(
                "payload",
                "test payload",
                crate::tool::ToolOptions::default(),
                |_, args| async move {
                    if args.fail {
                        Err(ToolError::FailedWithOutput {
                            message: "expected failure".into(),
                            output: Box::new(ToolOutput::new(json!({"partial": null}))),
                        })
                    } else {
                        Ok(Payload {
                            text: "x".repeat(PAYLOAD_BYTES),
                            nullable: None,
                        })
                    }
                },
            )
            .unwrap();
        crate::tool::builtins::jobs::register(&mut builder, runtime.jobs.clone()).unwrap();
        let executor = runtime.executor(builder);
        let call = |kind, tool: &'static str, args| {
            executor.execute_as(kind, runtime.agent.clone(), tool, args, None)
        };
        for kind in [InvocationKind::Script, InvocationKind::Model] {
            let response = call(kind, "payload", json!({})).await.unwrap();
            assert!(!response.is_error);
            let view = response.output.value;
            assert_eq!(view.as_object().unwrap().len(), 7);
            assert!(view["id"].as_u64().unwrap() > 0);
            assert_eq!(view["state"], "completed");
            assert_eq!(view["has_result"], true);
            assert_eq!(view.get("error"), Some(&Value::Null));
            assert_eq!(view.get("meta"), Some(&Value::Null));
            assert_eq!(view["result"].get("nullable"), Some(&Value::Null));
            let length = view["result"]["text"].as_str().unwrap().len();
            if matches!(kind, InvocationKind::Script) {
                assert_eq!(length, PAYLOAD_BYTES);
                assert_eq!(view.get("presentation"), Some(&Value::Null));
            } else {
                assert!(length < PAYLOAD_BYTES);
                assert_eq!(
                    view["presentation"]["truncated"][0]["field"],
                    "/result/text"
                );
                assert!(view["presentation"]["preview"].is_null());
            }

            let failure = call(kind, "payload", json!({"fail":true})).await.unwrap();
            assert!(failure.is_error);
            let target = failure.job;
            let inspection = call(kind, "job_output", json!({"job":target}))
                .await
                .unwrap();
            assert!(!inspection.is_error, "the inspection itself succeeded");
            for response in [failure, inspection] {
                let view = response.output.value;
                assert_eq!(view["id"], target.get());
                assert_eq!(view["state"], "failed");
                assert_eq!(view["error"], "expected failure");
                assert_eq!(view["result"], partial);
            }
            let invalid = call(kind, "job_output", json!({"job":999999}))
                .await
                .unwrap();
            assert!(
                invalid.is_error,
                "an invalid inspection is an invocation failure"
            );
        }
        let response = ExecutionError::UnknownTool("missing".into())
            .into_response(
                "missing",
                Some(JobId::new(7).unwrap()),
                Some("requested-name"),
            )
            .value;
        assert_eq!(response.get("id"), Some(&Value::Null));
        assert_eq!(response.get("result"), Some(&Value::Null));
        assert_eq!(response["state"], "failed");
        assert_eq!(response["error"], "unknown tool `missing`");
        assert_eq!(response["meta"]["tool"], "missing");
        assert_eq!(response["meta"]["parent"], 7);
        assert_eq!(response["meta"]["name"], "requested-name");
    }

    #[tokio::test]
    async fn completed_background_launches_return_the_same_unclaimed_metadata() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = runtime.executor(ToolRegistryBuilder::default());
        let job = created(
            &runtime,
            JobSpec {
                background: true,
                name: Some("fast-job".into()),
                ..JobSpec::test(runtime.agent.clone(), "fast")
            },
        )
        .await;
        let payload = json!({"data": null});
        complete(&runtime, job, payload.clone()).await;
        let started = || StartedExecution {
            job,
            background: true,
        };
        let model = executor.collect_model_started(started()).await.unwrap();
        let script = executor
            .collect_full_view_started(started(), crate::tool::ToolResultPolicy::Value)
            .await
            .unwrap();
        assert_eq!(model.output.value, script.output.value);
        assert!(model.background && script.background);
        assert!(model.output.images.is_empty() && script.output.images.is_empty());
        let view = model.output.value;
        assert_eq!(view["state"], "completed");
        assert_eq!(view["id"], job.get());
        assert_eq!(view["meta"]["tool"], "fast");
        assert_eq!(view["meta"]["name"], "fast-job");
        assert_eq!(view["has_result"], false);
        assert_eq!(view.get("result"), Some(&Value::Null));
        assert!(runtime.jobs.has_pending(&runtime.agent).await);
        assert_eq!(
            runtime.jobs.snapshot(job).await.unwrap().output,
            Some(payload)
        );
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
            complete(&runtime, job, json!(text)).await;

            let result = executor
                .collect_model_started(StartedExecution { job, background })
                .await
                .unwrap();
            assert_eq!(result.output.value["state"], "completed");
            if background {
                let notification = runtime
                    .jobs
                    .present_output_with(
                        crate::job::output::OutputArgs::new(job),
                        &CapabilitySet::default(),
                        crate::job::output::OutputOptions::Model {
                            presentation: crate::job::OutputPresentation::Automatic,
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(notification.view["meta"]["last_message"], sequence);
            }
            let pending = runtime.jobs.pending_delivery(&runtime.agent).await.unwrap();
            if background {
                // Launch handles do not read output or claim delivery. Automatic
                // notifications still reference the independently delivered reply.
                assert_eq!(result.output.value["has_result"], false);
                assert!(result.output.value["meta"]["last_message"].is_null());
                for field in ["result", "presentation"] {
                    assert_eq!(
                        result.output.value.get(field),
                        Some(&Value::Null),
                        "{field}"
                    );
                }
                assert_eq!(pending.messages().len(), 1);
                assert_eq!(pending.messages()[0].text, text);
            } else {
                // A foreground call returns its answer; nothing is delivered later.
                assert!(result.output.value["meta"]["last_message"].is_null());
                let presented = serde_json::to_string(&result.output.value).unwrap();
                assert!(presented.contains("child answer line 0"), "{presented}");
                assert!(pending.messages().is_empty());
            }
            drop(pending);
            // The saved final answer survives either way.
            let saved = runtime.jobs.snapshot(job).await.unwrap();
            assert_eq!(saved.output, Some(json!(text)));
        }
    }
}
