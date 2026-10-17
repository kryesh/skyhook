//! Collect job outcomes, import remote payloads, and present execution failures.

use super::*;

#[derive(Clone, Copy)]
pub(super) enum CollectionPurpose {
    /// Host collectors retain the handler's native payload.
    Native,
    /// The public script contract is the same envelope as model calls, but
    /// with complete captured data rather than an automatic preview.
    Public(crate::tool::ToolResultPolicy),
}

impl ToolExecutor {
    pub(super) async fn collect_model_started(&self, started: StartedExecution) -> Reported {
        let StartedExecution::Foreground(job) = started else {
            let purpose = CollectionPurpose::Public(crate::tool::ToolResultPolicy::Value);
            return self.collect_result(started, purpose).await;
        };
        let jobs = &self.shared.jobs;
        let presented = async {
            jobs.wait_foreground(job).await.map_err(job_error)?;
            jobs.present_output_with(
                crate::job::output::OutputArgs::new(job),
                crate::job::CancellationToken::new(),
                self.diagnostic_viewer(),
                crate::job::output::OutputOptions::Model {
                    kind: crate::job::ViewKind::Response,
                },
            )
            .await
        };
        let presented = match presented.await {
            Ok(presented) => presented,
            Err(error) => return Err(error).into(),
        };
        let acknowledges = presented.acknowledges;
        let (state, output, images) = presented.into_parts();
        let result = ExecutionResult {
            job,
            background: false,
            is_error: matches!(
                state,
                JobState::Failed | JobState::Cancelled | JobState::Interrupted
            ),
            output: ToolOutput::new(output).with_images(images),
        };
        Reported {
            outcome: Ok(result),
            acknowledges,
        }
    }

    pub(super) async fn collect_result(
        &self,
        started: StartedExecution,
        purpose: CollectionPurpose,
    ) -> Reported {
        let job = match started {
            StartedExecution::Foreground(job) => job,
            StartedExecution::Background(launched) => {
                let value = launched
                    .metadata_view(self.diagnostic_viewer())
                    .into_value();
                return Ok(ExecutionResult {
                    job: launched.id,
                    background: true,
                    is_error: false,
                    output: ToolOutput::new(value),
                })
                .into();
            }
        };
        let jobs = &self.shared.jobs;
        let (envelope, delivery) = match jobs.wait_foreground(job).await {
            Ok(waited) => waited,
            Err(error) => return Err(job_error(error)).into(),
        };
        if !envelope.state.is_terminal() {
            return Ok(ExecutionResult {
                job,
                background: true,
                is_error: false,
                output: ToolOutput::new(
                    match purpose {
                        CollectionPurpose::Public(_) => {
                            envelope.response_view(self.diagnostic_viewer(), None)
                        }
                        CollectionPurpose::Native => {
                            envelope.metadata_view(self.diagnostic_viewer())
                        }
                    }
                    .into_value(),
                ),
            })
            .into();
        }
        let result = match jobs.saved_result(&envelope).await {
            Ok(result) => result,
            Err(error) => return Err(job_error(error)).into(),
        };
        Reported {
            outcome: self.finished_result(job, envelope, result, purpose),
            acknowledges: delivery,
        }
    }

    /// The result reporting a finished foreground job.
    fn finished_result(
        &self,
        job: JobId,
        envelope: crate::job::JobEnvelope,
        result: Option<Value>,
        purpose: CollectionPurpose,
    ) -> Result<ExecutionResult, ToolError> {
        let images = envelope.images.clone();
        if let CollectionPurpose::Public(policy) = purpose {
            // A successful output query already returns the target view. The
            // invocation's state, not the target's, determines is_error.
            let is_error = envelope.state != JobState::Completed;
            let value = if !is_error && policy == crate::tool::ToolResultPolicy::JobView {
                result.ok_or_else(|| {
                    ToolError::failed("job-output query completed without a response")
                })?
            } else {
                envelope
                    .response_view(self.diagnostic_viewer(), result)
                    .into_value()
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
                output: ToolOutput::new(result.unwrap_or(Value::Null)).with_images(images),
            })
        } else {
            let mut failure = envelope.diagnostic.map_or_else(
                || ToolError::failed(format!("job ended as {:?}", envelope.state)),
                |diagnostic| ToolError::from_diagnostic(diagnostic, None),
            );
            if let Some(value) = result {
                failure = failure.with_result(ToolOutput::new(value).with_images(images));
            }
            Err(failure)
        }
    }
}

/// A call's outcome with the delivery it reports to the job's owner. Whoever
/// records the outcome acknowledges the delivery with that record, so a crash
/// before it commits leaves the delivery pending.
pub(crate) struct Reported {
    pub(crate) outcome: Result<ExecutionResult, ToolError>,
    pub(crate) acknowledges: Option<crate::job::Delivery>,
}

impl From<Result<ExecutionResult, ToolError>> for Reported {
    fn from(outcome: Result<ExecutionResult, ToolError>) -> Self {
        Self {
            outcome,
            acknowledges: None,
        }
    }
}

/// A started invocation. A background launch is answered with the job as it was
/// at launch, never its later state: its outcome reaches the owner as a
/// notification, which reading it here would consume.
pub(crate) enum StartedExecution {
    Foreground(JobId),
    Background(Box<crate::job::JobEnvelope>),
}

#[derive(Debug)]
pub struct ExecutionResult {
    pub job: JobId,
    pub background: bool,
    pub output: ToolOutput,
    pub(crate) is_error: bool,
}

/// Errors before a job can provide its own view still use the public response
/// contract. A missing ID means there is no inspectable job handle.
pub(crate) fn failure_response(
    error: ToolError,
    viewer: crate::tool::diagnostic::DiagnosticViewer<'_>,
) -> ToolOutput {
    let (diagnostic, output) = error.into_parts();
    let message = diagnostic.render_for(viewer);
    let (value, images) = output.map_or((None, Vec::new()), |output| {
        (Some(output.value), output.images)
    });
    let view = crate::job::JobView::failure(message, value, diagnostic.is_denial());
    ToolOutput::new(view.into_value()).with_images(images)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{job::JobSpec, tool::ToolRegistryBuilder};
    use serde_json::json;

    async fn complete(runtime: &crate::tests::TestRuntime, job: JobId, value: serde_json::Value) {
        let outcome = crate::job::JobOutcome::Completed(ToolOutput::new(value));
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
                        Err(ToolError::with_output(
                            "expected failure",
                            ToolOutput::new(json!({"partial": null})),
                        ))
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
        for location in [
            ExecutionLocation::root(runtime.root.path().to_owned()),
            ExecutionLocation::named("worker".parse().unwrap(), "/remote".into()),
        ] {
            let executor = executor.clone().with_location(location.clone());
            let call = |kind, tool: &'static str, args| {
                executor.execute_as(kind, runtime.agent.clone(), tool, args, None)
            };
            for kind in [InvocationKind::Script, InvocationKind::Model] {
                let response = call(kind, "payload", json!({})).await.unwrap();
                assert!(!response.is_error);
                let view = response.output.value;
                assert_eq!(view["result"].get("nullable"), Some(&Value::Null));
                let length = view["result"]["text"].as_str().unwrap().len();
                if matches!(kind, InvocationKind::Script) {
                    // Nothing more to read: the completed response needs no handle.
                    assert_eq!(length, PAYLOAD_BYTES);
                    assert_eq!(view.as_object().unwrap().len(), 1);
                } else {
                    assert!(view["id"].as_u64().unwrap() > 0);
                    assert_eq!(view["state"], "completed");
                    assert!(length < PAYLOAD_BYTES);
                    assert_eq!(
                        view["presentation"]["truncated"][0]["field"],
                        "/result/text"
                    );
                    assert_eq!(view["presentation"].get("preview"), None);
                }

                let failure = call(kind, "payload", json!({"fail":true})).await.unwrap();
                assert!(failure.is_error);
                let target = failure.job;
                let inspection = call(kind, "jobs", json!({"job":target})).await.unwrap();
                assert!(!inspection.is_error, "the inspection itself succeeded");
                let expected_error = runtime
                    .jobs
                    .metadata(target)
                    .unwrap()
                    .rendered_error(executor.diagnostic_viewer())
                    .unwrap();
                assert_eq!(
                    expected_error.contains("on session host"),
                    !location.is_root()
                );
                for response in [failure, inspection] {
                    let view = response.output.value;
                    assert_eq!(view["id"], target.get());
                    assert_eq!(view["state"], "failed");
                    assert_eq!(view["error"], expected_error);
                    assert_eq!(view["result"], partial);
                }
                let invalid = call(kind, "jobs", json!({"job":999999})).await.unwrap();
                assert!(
                    invalid.is_error,
                    "an invalid inspection is an invocation failure"
                );
            }
        }
        let missing = executor.execute_as(
            InvocationKind::Script,
            runtime.agent.clone(),
            "missing",
            json!({}),
            JobId::new(7).ok(),
        );
        let error = missing.await.unwrap_err();
        assert_eq!(
            error.diagnostic().cause,
            Cause::UnknownTool {
                tool: "missing".into()
            }
        );
        let response = failure_response(error, executor.diagnostic_viewer()).value;
        assert_eq!(
            response,
            json!({"state": "failed",
                   "error": "validate tool `missing` failed: unknown tool `missing`"})
        );
    }

    /// A background handle shows the job as launched, even when the job completed
    /// before the handle was collected: never a completed state without its result.
    #[tokio::test]
    async fn background_launches_report_the_job_as_launched() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = runtime.executor(ToolRegistryBuilder::default());
        let spec = JobSpec {
            background: true,
            name: Some("fast-job".parse().unwrap()),
            ..JobSpec::test(runtime.agent.clone(), "fast")
        };
        let job = runtime.jobs.test_running(spec).await.into_test_id();
        let launched = Box::new(runtime.jobs.metadata(job).unwrap());
        let payload = json!({"data": null});
        complete(&runtime, job, payload.clone()).await;
        let started = || StartedExecution::Background(launched.clone());
        let model = executor.collect_model_started(started()).await;
        let purpose = CollectionPurpose::Public(crate::tool::ToolResultPolicy::Value);
        let script = executor.collect_result(started(), purpose).await;
        let (model, script) = (model.outcome.unwrap(), script.outcome.unwrap());
        assert_eq!(model.output.value, script.output.value);
        assert!(model.background && script.background);
        assert!(model.output.images.is_empty() && script.output.images.is_empty());
        let view = model.output.value;
        assert_eq!(view["state"], "running");
        assert_eq!(view["id"], job.get());
        assert_eq!(view["meta"]["tool"], "fast");
        assert_eq!(view["meta"]["name"], "fast-job");
        assert_eq!(view.get("result"), None);
        assert!(runtime.jobs.has_pending(&runtime.agent));
        assert_eq!(
            runtime.jobs.test_snapshot(job).await.unwrap().output,
            Some(payload)
        );
    }
}
