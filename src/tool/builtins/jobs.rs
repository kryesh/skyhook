use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    identity::JobId,
    job::{
        FieldPointer, JobError, JobManager, OutputPresentation,
        output::{OutputArgs, OutputOptions},
        presented_job_schema,
    },
    tool::{
        AdmissionError, RegistryError, ToolContext, ToolError, ToolOptions, ToolOutput,
        ToolRegistryBuilder,
        diagnostic::{Effects, Operation, PartialContext, Subject, deserialize_arguments},
        registry::{Invocation, input_schema},
    },
};

pub(crate) fn register(
    builder: &mut ToolRegistryBuilder,
    jobs: JobManager,
) -> Result<(), RegistryError> {
    let admitted = jobs.clone();
    builder.register_admission(
        "jobs",
        "Without `job`, list this agent's active jobs, excluding this call and its containing script; `all` adds finished jobs. With `job`, read or search that job's saved output and status immediately, returning its JobView in place of this call's. Whole-output reads attach saved images; filtered or paginated reads return text in preview.lines without images.",
        input_schema::<JobsArgs>()?,
        ToolOptions::default().generated_output_schema(|_| presented_job_schema(true)),
        move |arguments| {
            let jobs = admitted.clone();
            Ok(match JobsRequest::try_from(deserialize_arguments::<JobsArgs>(&arguments)?)? {
                JobsRequest::List { all } => {
                    Invocation::new(move |context| list(jobs, context, all))
                }
                JobsRequest::Output(args) => Invocation::job_view(move |context| async move {
                    let job = args.job;
                    jobs.present_output_with(
                        args,
                        context.cancellation_token(),
                        context.diagnostic_viewer(),
                        OutputOptions::Model {
                            presentation: OutputPresentation::Full,
                        },
                    )
                    .await
                    .map_err(|error| error.or(PartialContext::new(Operation::Read, Subject::Job(job))))
                }),
            })
        },
    )?;
    let send = jobs.clone();
    builder.register::<JobSendArgs, Value, _, _>(
        "job_send",
        "Send JSON input to a job. For child agents, answer pending questions or deliver follow-up instructions automatically at the next model-request boundary. Sending to a completed, failed, or interrupted child agent appends to its retained conversation and resumes it under the same job ID; cancelled jobs remain non-resumable. Background scripts read input with await receive().",
        ToolOptions::default()
            .script_only()
            .job_method("send", "job"),
        move |_context, args| {
            let jobs = send.clone();
            async move {
                jobs.send(args.job, args.value)
                    .await
                    .map_err(|error| job_failure(error, Operation::Send, args.job))?;
                Ok(serde_json::json!({"accepted": true}))
            }
        },
    )?;
    let cancel = jobs.clone();
    builder.register::<JobArgs, Value, _, _>(
        "job_cancel",
        "Request cancellation of job/descendants; confirm terminal state with jobs.",
        ToolOptions::default()
            .generated_output_schema(|_| presented_job_schema(false))
            .script_only()
            .job_method("cancel", "job"),
        move |context, args| {
            let jobs = cancel.clone();
            async move {
                jobs.cancel(args.job)
                    .await
                    .map_err(|error| job_failure(error, Operation::Terminate, args.job))
                    .map(|job| job.metadata_view(context.diagnostic_viewer()).into_value())
            }
        },
    )?;
    Ok(())
}

/// Convert at the host job boundary, before opaque persistence failures can
/// expose stored input or be mistaken for a rejected, unstarted mutation.
fn job_failure(error: JobError, operation: Operation, job: JobId) -> ToolError {
    let not_started = matches!(
        &error,
        JobError::Unknown(_)
            | JobError::InputUnavailable { .. }
            | JobError::InputUnsupported(_)
            | JobError::InputClosed(_)
    );
    let error = ToolError::from(error).operation(operation, Subject::Job(job));
    if not_started {
        error.effects(Effects::NotStarted)
    } else {
        error
    }
}

async fn list(jobs: JobManager, context: ToolContext, all: bool) -> Result<ToolOutput, ToolError> {
    let current = jobs.metadata(context.job()).await.map_err(|error| {
        job_failure(error, Operation::Inspect, context.job()).effects(Effects::Unchanged)
    })?;
    let containing_script = if let Some(parent) = current.parent {
        let parent = jobs.metadata(parent).await.map_err(|error| {
            job_failure(error, Operation::Inspect, parent).effects(Effects::Unchanged)
        })?;
        (parent.role == crate::job::JobRole::Script).then_some(parent.id)
    } else {
        None
    };
    let listing = jobs
        .list(context.agent())
        .await
        .into_iter()
        .filter(|job| job.id != context.job() && Some(job.id) != containing_script)
        .filter(|job| all || !job.state.is_terminal())
        .map(|job| job.metadata_view(context.diagnostic_viewer()).into_value())
        .collect();
    Ok(ToolOutput::new(Value::Array(listing)))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobsArgs {
    /// Job whose saved output to read; omit to list jobs.
    job: Option<JobId>,
    /// Listing only: include finished jobs as well as active ones.
    #[serde(default)]
    all: bool,
    /// JSON Pointer in saved content, e.g. /result/stdout, /result/content, /result/console.
    field: Option<FieldPointer>,
    /// One-based first source line; use returned next_start to continue.
    #[schemars(range(min = 1), extend("default" = 1))]
    start: Option<usize>,
    /// Maximum returned lines, including match context.
    #[schemars(range(min = 1, max = 1000), extend("default" = 100))]
    limit: Option<usize>,
    /// Case-sensitive line regex; use (?i) for case-insensitive matching.
    pattern: Option<String>,
    /// Surrounding lines per match.
    #[schemars(range(min = 0, max = 20), extend("default" = 0))]
    context: Option<usize>,
    /// Zero-based UTF-8 byte offset within the starting line; use returned next_offset to continue.
    #[schemars(range(min = 0), extend("default" = 0))]
    offset: Option<usize>,
}

enum JobsRequest {
    List { all: bool },
    Output(OutputArgs),
}

impl TryFrom<JobsArgs> for JobsRequest {
    type Error = AdmissionError;

    fn try_from(args: JobsArgs) -> Result<Self, Self::Error> {
        let JobsArgs {
            job,
            all,
            field,
            start,
            limit,
            pattern,
            context,
            offset,
        } = args;
        match job {
            Some(_) if all => Err(AdmissionError::invalid_arguments(
                "all lists jobs; omit it with job",
            )),
            Some(job) => Ok(Self::Output(OutputArgs {
                job,
                field,
                start,
                limit,
                pattern,
                context,
                offset,
            })),
            None if field.is_some()
                || start.is_some()
                || limit.is_some()
                || pattern.is_some()
                || context.is_some()
                || offset.is_some() =>
            {
                Err(AdmissionError::invalid_arguments(
                    "output selection requires job",
                ))
            }
            None => Ok(Self::List { all }),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobArgs {
    job: JobId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobSendArgs {
    /// Destination job ID. For child updates or answers, use the agent job; for receive(), use the background script job.
    job: JobId,
    /// JSON input. For merged child questions, pass an object keyed directly by question ID.
    value: Value,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, OnceLock};

    use serde_json::json;

    use super::*;
    use crate::{
        identity::AgentId,
        job::{JobOutcome, JobRole, JobSpec, JobState},
        media::ImageRef,
        provider::protocol::{Message, ModelRequest, ToolResult},
        session::{SessionError, SessionStore},
        tests::TestRuntime,
        tool::{
            ToolOutput, ToolRegistryBuilder,
            diagnostic::Cause,
            executor::{ExecutionResult, ToolExecutor},
            policy::{AllowAll, Capability, CapabilitySet},
        },
    };

    fn ids(listing: &Value) -> Vec<Value> {
        listing
            .as_array()
            .unwrap()
            .iter()
            .map(|job| job["id"].clone())
            .collect()
    }

    #[test]
    fn job_arguments_select_listing_or_output_but_never_mix_them() {
        let request = |value: Value| {
            serde_json::from_value::<JobsArgs>(value)
                .map_err(AdmissionError::invalid_arguments)
                .and_then(JobsRequest::try_from)
        };
        assert!(matches!(
            request(json!({})),
            Ok(JobsRequest::List { all: false })
        ));
        assert!(matches!(
            request(json!({"all":true})),
            Ok(JobsRequest::List { all: true })
        ));
        assert!(matches!(
            request(json!({"job":3, "pattern":"x"})),
            Ok(JobsRequest::Output(args)) if args.job.get() == 3 && args.pattern.as_deref() == Some("x")
        ));
        for value in [
            json!({"job":3, "all":true}),
            json!({"field":"/result"}),
            json!({"limit":5}),
        ] {
            assert!(
                matches!(request(value.clone()), Err(error)
                    if matches!(error.diagnostic().cause, Cause::InvalidArguments(_))),
                "{value}"
            );
        }
    }

    #[test]
    fn job_failure_retains_canonical_cause_and_boundary_mutation_evidence() {
        let job = JobId::new(42).unwrap();
        let session_error = || {
            SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "private persisted input",
            ))
        };
        let cases = [
            (
                JobError::Session(session_error()),
                ToolError::from(session_error()).diagnostic().cause,
                Effects::Unknown,
            ),
            (
                JobError::Output(Box::new(
                    ToolError::failed("capture write failed").effects(Effects::OutputIncomplete),
                )),
                Cause::Message("capture write failed".into()),
                Effects::OutputIncomplete,
            ),
            (
                JobError::Unknown(job),
                ToolError::failed(JobError::Unknown(job)).diagnostic().cause,
                Effects::NotStarted,
            ),
        ];
        for (error, cause, effects) in cases {
            let diagnostic = job_failure(error, Operation::Send, job).diagnostic();
            assert_eq!(diagnostic.context.operation, Operation::Send);
            assert_eq!(diagnostic.context.subject, Subject::Job(job));
            assert_eq!(diagnostic.context.effects, effects);
            assert_eq!(diagnostic.cause, cause);
        }
    }

    #[tokio::test]
    async fn job_output_reads_name_the_requested_job_on_the_host() {
        let runtime = TestRuntime::new().await;
        let (executor, _slot) = executor(runtime.jobs.clone(), runtime.root.path());
        let unknown = JobId::new(999_999).unwrap();
        for (query, operation, subject) in [
            (
                json!({"job": unknown}),
                Operation::Read,
                Subject::Job(unknown),
            ),
            (
                json!({"job": unknown, "pattern":"["}),
                Operation::Validate,
                Subject::argument(["pattern"]),
            ),
        ] {
            let error = executor
                .run_host(&runtime.agent, "jobs", query)
                .await
                .unwrap_err();
            let context = error.diagnostic().context;
            assert_eq!(context.operation, operation);
            assert_eq!(context.subject, subject);
            assert_eq!(context.site, crate::tool::diagnostic::FailureSite::Host);
        }
    }

    #[tokio::test]
    async fn job_listing_projects_approval_and_defaults_to_other_active_jobs() {
        let runtime = TestRuntime::new().await;
        let (agent, jobs) = (&runtime.agent, &runtime.jobs);
        // Leases stay alive: dropping startup ownership cancels a job.
        let spec = |tool: &str| JobSpec {
            background: true,
            ..JobSpec::test(agent.clone(), tool)
        };
        let pending_lease = jobs
            .create(JobSpec::test(agent.clone(), "pending"))
            .await
            .unwrap();
        let pending_lease = pending_lease.await_approval().await.unwrap();
        let pending = pending_lease.id();
        let completed_lease = jobs.create(spec("completed")).await.unwrap();
        let completed = completed_lease.id();
        let finished = JobOutcome::Completed(ToolOutput::new(Value::Null));
        jobs.finish(completed, finished).await.unwrap();
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, jobs.clone()).unwrap();
        let executor = runtime.executor(builder);
        // Every job API projects pending authorization as queued.
        let current = executor
            .run_host(agent, "jobs", json!({}))
            .await
            .unwrap()
            .output
            .value;
        let output = executor
            .run_host(agent, "jobs", json!({"job":pending}))
            .await;
        let output = output.unwrap().output.value;
        for (value, state) in [
            (&current, &current[0]["state"]),
            (&output, &output["state"]),
        ] {
            assert!(!value.to_string().contains("awaiting_approval"));
            assert_eq!(state, "queued");
        }
        assert_eq!(ids(&current), [json!(pending)]);

        let spec = JobSpec {
            accepts_input: true,
            background: true,
            role: JobRole::Script,
            ..JobSpec::test(agent.clone(), "container")
        };
        let script_lease = jobs.create(spec).await.unwrap();
        let script = script_lease.id();
        let nested = executor
            .execute(agent.clone(), "jobs", json!({}), Some(script))
            .await;
        let nested = ids(&nested.unwrap().output.value);
        assert!(nested.contains(&json!(pending)) && !nested.contains(&json!(script)));

        let all = executor
            .run_host(agent, "jobs", json!({"all": true}))
            .await
            .unwrap();
        let listed = ids(&all.output.value);
        assert!(listed.contains(&json!(pending)) && listed.contains(&json!(completed)));
        assert!(!listed.contains(&json!(all.job)));
    }

    // End-to-end attachments through the tool executor and JavaScript bridge.
    const IMAGE: &[u8] = b"\x89PNG\r\n\x1a\nattachment test";

    fn executor(
        jobs: JobManager,
        root: &std::path::Path,
    ) -> (ToolExecutor, Arc<OnceLock<ToolExecutor>>) {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_local(crate::tool::builtins::filesystem::register)
            .unwrap();
        super::register(&mut builder, jobs.clone()).unwrap();
        let slot = Arc::new(OnceLock::new());
        super::super::install_script_tool(&mut builder, Arc::downgrade(&slot)).unwrap();
        let executor = ToolExecutor::new(builder.build(), Arc::new(AllowAll), jobs, root.into());
        assert!(slot.set(executor.clone()).is_ok());
        (executor, slot)
    }

    async fn image_runtime() -> (TestRuntime, ToolExecutor, Arc<OnceLock<ToolExecutor>>) {
        let runtime = TestRuntime::new().await;
        tokio::fs::write(runtime.root.path().join("image.png"), IMAGE)
            .await
            .unwrap();
        let (executor, slot) = executor(runtime.jobs.clone(), runtime.root.path());
        (runtime, executor, slot)
    }

    async fn script(
        executor: &ToolExecutor,
        agent: &AgentId,
        source: String,
        bg: bool,
    ) -> ExecutionResult {
        let arguments = json!({"source":source, "bg":bg});
        executor
            .run_model(agent, "script", arguments)
            .await
            .unwrap()
    }

    async fn assert_loaded(store: &SessionStore, images: Vec<ImageRef>) {
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].format, crate::media::ImageFormat::Png);
        let blob = images[0].blob;
        let mut request = ModelRequest {
            history: vec![Message::Tool(vec![ToolResult {
                call_id: "output".into(),
                name: "jobs".into(),
                result: json!({}),
                images,
                is_error: false,
            }])],
            ..ModelRequest::test("image-test")
        };
        assert!(request.blobs.get(&blob).is_err());
        store.load_blobs(&mut request).await.unwrap();
        assert_eq!(request.blobs.get(&blob).unwrap(), IMAGE);
    }

    #[tokio::test]
    async fn failed_script_discards_collected_images_but_retains_console_and_source() {
        let (runtime, executor, _slot) = image_runtime().await;
        let source = r#"
            await tool.read({path:"image.png"});
            console.log("console survives the deliberate image-discard policy");
            throw new Error("failed after receiving an image");
        "#;
        let failed = script(&executor, &runtime.agent, source.into(), false).await;
        assert!(failed.is_error);
        let value = &failed.output.value;
        assert_eq!(
            (&value["state"], &value["result"]["value"]),
            (&json!("failed"), &Value::Null)
        );
        assert_eq!(
            value["result"]["console"],
            "console survives the deliberate image-discard policy\n"
        );
        assert!(failed.output.images.is_empty());
        assert!(runtime.jobs.images(failed.job).await.unwrap().is_empty());
        let jobs = runtime.jobs.list(&runtime.agent).await;
        let source = jobs.into_iter().find(|job| job.tool == "read");
        let source = source.expect("owning read result survives script failure");
        assert_eq!(source.state, JobState::Completed);
        assert_loaded(
            &runtime.store,
            runtime.jobs.images(source.id).await.unwrap(),
        )
        .await;
    }

    #[tokio::test]
    async fn background_image_output_attaches_directly_and_through_javascript() {
        let (runtime, executor, _slot) = image_runtime().await;
        let agent = &runtime.agent;
        // read itself is foreground-only; a background script is the supported way
        // to read an image asynchronously and retrieve its saved output later.
        let source = "return await tool.read({path:'image.png'});";
        let launched = script(&executor, agent, source.into(), true).await;
        assert!(launched.background);
        runtime.jobs.wait(launched.job, None, true).await.unwrap();
        for _ in 0..2 {
            let output = executor
                .run_model(agent, "jobs", json!({"job":launched.job}))
                .await;
            let output = output.unwrap().output;
            assert_eq!(output.value["state"], "completed");
            // Returning a tool response preserves its canonical read JobView.
            assert_eq!(output.value["result"]["value"]["state"], "completed");
            assert_eq!(output.value["result"]["value"]["result"]["kind"], "image");
            assert_eq!(output.value["result"]["console"], "");
            assert!(output.value.get("console").is_none());
            assert_loaded(&runtime.store, output.images).await;
        }
        let source = format!("return await tool.jobs({{job:{}}});", launched.job);
        let output = script(&executor, agent, source.clone(), false).await.output;
        assert_eq!(output.value["result"]["console"], "");
        assert_eq!(output.value["result"]["value"]["id"], launched.job.get());
        assert_loaded(&runtime.store, output.images).await;
        // A background script that retrieves an existing image must itself retain
        // the attachment, not merely the nested job's JSON metadata.
        let background = script(&executor, agent, source, true).await;
        runtime.jobs.wait(background.job, None, true).await.unwrap();
        let output = executor
            .run_model(agent, "jobs", json!({"job":background.job}))
            .await;
        let output = output.unwrap().output;
        assert_eq!(output.value["state"], "completed");
        assert_loaded(&runtime.store, output.images).await;
    }

    #[tokio::test]
    async fn image_output_text_selections_do_not_attach_and_invalid_queries_fail() {
        let (runtime, executor, _slot) = image_runtime().await;
        let agent = &runtime.agent;
        let read = executor
            .run_model(agent, "read", json!({"path":"image.png"}))
            .await
            .unwrap();
        assert_loaded(&runtime.store, read.output.images).await;
        // Selection semantics are tested with the output product; this proves
        // both entry points forward the selector.
        let query = json!({"job":read.job, "field":"/result"});
        let output = executor
            .run_model(agent, "jobs", query.clone())
            .await
            .unwrap();
        assert!(output.output.images.is_empty());
        let source = format!("return await tool.jobs({query});");
        let output = script(&executor, agent, source, false).await;
        assert!(output.output.images.is_empty());
        for query in [
            json!({"job":read.job,"limit":0}),
            json!({"job":read.job,"pattern":"["}),
            json!({"job":999999}),
        ] {
            let response = executor.run_model(agent, "jobs", query).await.unwrap();
            assert_eq!(response.output.value["state"], "failed");
        }
        // A denied image-producing call never creates a retrievable attachment.
        let mut capabilities = CapabilitySet::default();
        capabilities.remove(Capability::Read);
        let denied = executor.clone().with_capabilities(capabilities);
        let before = runtime.jobs.list(agent).await.len();
        let arguments = json!({"path":"image.png"});
        // This is a pre-admission error: public dispatch converts it to an
        // id:null failure envelope, while the internal executor returns Err.
        assert!(denied.run_model(agent, "read", arguments).await.is_err());
        for job in runtime.jobs.list(agent).await.into_iter().skip(before) {
            assert!(runtime.jobs.images(job.id).await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn retrieved_images_survive_resume_including_failed_tool_output() {
        let runtime = TestRuntime::on_disk().await;
        let image = crate::media::Image::new(IMAGE.to_vec()).unwrap();
        let image = runtime.store.store_image(Some("image.png".into()), &image);
        let image = image.await.unwrap();
        let spec = JobSpec::test(runtime.agent.clone(), "partial-image");
        let lease = runtime.jobs.create(spec).await.unwrap();
        let job = lease.id();
        // Consume the lease before reopening; it retains the manager and journal lock.
        let output = ToolOutput::new(json!({"image":image})).with_images(vec![image]);
        lease
            .fail(
                ToolError::failed("failed after producing an image")
                    .with_result(output)
                    .into(),
            )
            .await;
        let id = runtime.store.id();
        drop((runtime.jobs, runtime.store));
        let sessions = runtime.root.path().join("sessions");
        let (store, records) = SessionStore::open(&sessions, id).await.unwrap();
        let jobs = JobManager::restore(store.clone(), &records).await.unwrap();
        let agent = AgentId::root(id);
        let (executor, _slot) = executor(jobs, runtime.root.path());
        let output = executor.run_model(&agent, "jobs", json!({"job":job}));
        let output = output.await.unwrap().output;
        assert_eq!(output.value["state"], "failed");
        assert_loaded(&store, output.images).await;
        let source = format!("return await tool.jobs({{job:{job}}});");
        let output = script(&executor, &agent, source, false).await;
        assert_loaded(&store, output.output.images).await;
    }
}
