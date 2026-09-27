//! Collect job outcomes, import remote payloads, and present execution failures.

use super::*;

#[derive(Clone, Copy)]
enum CollectionPurpose {
    Native,
    Public(crate::tool::ToolResultPolicy),
}

impl ToolExecutor {
    pub(super) async fn collect_model_started(
        &self,
        started: StartedExecution,
    ) -> Result<ExecutionResult, ToolError> {
        let StartedExecution::Foreground(job) = started else {
            return self
                .collect_full_view_started(started, crate::tool::ToolResultPolicy::Value)
                .await;
        };
        self.shared
            .jobs
            .wait_foreground(job)
            .await
            .map_err(job_error)?;
        let presented = self
            .shared
            .jobs
            .present_output_with(
                crate::job::output::OutputArgs::new(job),
                crate::job::CancellationToken::new(),
                self.diagnostic_viewer(),
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
    ) -> Result<ExecutionResult, ToolError> {
        self.collect_result(started, CollectionPurpose::Public(policy))
            .await
    }

    pub(crate) async fn collect_started(
        &self,
        started: StartedExecution,
    ) -> Result<ExecutionResult, ToolError> {
        self.collect_result(started, CollectionPurpose::Native)
            .await
    }

    async fn collect_result(
        &self,
        started: StartedExecution,
        purpose: CollectionPurpose,
    ) -> Result<ExecutionResult, ToolError> {
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
                });
            }
        };
        let jobs = &self.shared.jobs;
        let mut envelope = jobs.wait_foreground(job).await.map_err(job_error)?;
        jobs.hydrate_envelope(&mut envelope)
            .await
            .map_err(job_error)?;
        envelope.render_output_diagnostic(self.diagnostic_viewer());
        if !envelope.state.is_terminal() {
            return Ok(ExecutionResult {
                job,
                background: true,
                is_error: false,
                output: ToolOutput::new(
                    match purpose {
                        CollectionPurpose::Public(_) => {
                            envelope.response_view(self.diagnostic_viewer())
                        }
                        CollectionPurpose::Native => {
                            envelope.metadata_view(self.diagnostic_viewer())
                        }
                    }
                    .into_value(),
                ),
            });
        }
        jobs.claim(job).await.map_err(job_error)?;
        let images = jobs.images(job).await.map_err(job_error)?;
        if let CollectionPurpose::Public(policy) = purpose {
            // A successful output query already returns the target view. The
            // invocation's state, not the target's, determines is_error.
            let is_error = envelope.state != JobState::Completed;
            let value = if !is_error && policy == crate::tool::ToolResultPolicy::JobView {
                envelope.output.take().ok_or_else(|| {
                    ToolError::failed("job-output query completed without a response")
                })?
            } else {
                envelope
                    .response_view(self.diagnostic_viewer())
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
                output: ToolOutput::new(envelope.output.unwrap_or(Value::Null)).with_images(images),
            })
        } else {
            let mut failure = envelope.diagnostic.map_or_else(
                || ToolError::failed(format!("job ended as {:?}", envelope.state)),
                |diagnostic| ToolError::from_diagnostic(diagnostic, None),
            );
            if let Some(value) = envelope.output {
                failure = failure.with_result(ToolOutput::new(value).with_images(images));
            }
            Err(failure)
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
    use crate::{
        job::{JobRole, JobSpec},
        provider::protocol::AssistantItem,
        session::Message,
        tool::ToolRegistryBuilder,
    };
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
                    .await
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
        let launched = Box::new(runtime.jobs.metadata(job).await.unwrap());
        let payload = json!({"data": null});
        complete(&runtime, job, payload.clone()).await;
        let started = || StartedExecution::Background(launched.clone());
        let model = executor.collect_model_started(started()).await.unwrap();
        let script = executor
            .collect_full_view_started(started(), crate::tool::ToolResultPolicy::Value)
            .await
            .unwrap();
        assert_eq!(model.output.value, script.output.value);
        assert!(model.background && script.background);
        assert!(model.output.images.is_empty() && script.output.images.is_empty());
        let view = model.output.value;
        assert_eq!(view["state"], "running");
        assert_eq!(view["id"], job.get());
        assert_eq!(view["meta"]["tool"], "fast");
        assert_eq!(view["meta"]["name"], "fast-job");
        assert_eq!(view.get("result"), None);
        assert!(runtime.jobs.has_pending(&runtime.agent).await);
        assert_eq!(
            runtime.jobs.snapshot(job).await.unwrap().output,
            Some(payload)
        );
    }

    /// A child's final reply is its job's result: a foreground call returns it and
    /// a background child's completion carries it, never as a separate reply.
    #[tokio::test]
    async fn completed_child_answers_are_their_results() {
        for background in [false, true] {
            let runtime = crate::tests::TestRuntime::new().await;
            let executor = runtime.executor(ToolRegistryBuilder::default());
            let child = runtime.agent.child(1);
            let spec = JobSpec {
                background,
                role: JobRole::Agent,
                ..JobSpec::test(runtime.agent.clone(), "delegate")
            };
            let job = runtime.jobs.test_running(spec).await.into_test_id();
            let started = crate::session::tests::child_started(
                Some(job),
                ExecutionLocation::root(runtime.root.path().to_owned()),
            );
            runtime.store.append(child.clone(), started).await.unwrap();
            let jobs = &runtime.jobs;
            jobs.set_child_agent(job, child.clone()).await.unwrap();
            let text = "child answer";
            let message = Message::Assistant(vec![AssistantItem::text("answer", 0, text)]);
            jobs.commit_child_message(&child, job, message, text.into(), |_| Vec::new())
                .await
                .unwrap();
            complete(&runtime, job, json!(text)).await;
            let pending = jobs.pending_delivery(&runtime.agent).await.unwrap();
            assert!(pending.messages().is_empty());
            let delivered: Vec<_> = pending.envelopes().iter().map(|job| job.id).collect();
            drop(pending);
            let view = if background {
                assert_eq!(delivered, [job]);
                jobs.present_output_with(
                    crate::job::output::OutputArgs::new(job),
                    crate::job::CancellationToken::new(),
                    &CapabilitySet::default(),
                    crate::job::output::OutputOptions::Host,
                )
                .await
                .unwrap()
                .into_view()
            } else {
                assert!(delivered.is_empty());
                let started = StartedExecution::Foreground(job);
                executor
                    .collect_model_started(started)
                    .await
                    .unwrap()
                    .output
                    .value
            };
            let view: crate::job::JobView = serde_json::from_value(view).unwrap();
            assert_eq!(view.state(), JobState::Completed);
            assert_eq!(view.result(), Some(&json!(text)));
        }
    }
}
