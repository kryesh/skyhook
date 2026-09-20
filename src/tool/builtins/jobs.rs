use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    identity::JobId,
    job::{JobManager, presented_job_schema},
    tool::{RegistryError, ToolError, ToolOptions, ToolRegistryBuilder},
};

pub(crate) fn register(
    builder: &mut ToolRegistryBuilder,
    jobs: JobManager,
) -> Result<(), RegistryError> {
    let list = jobs.clone();
    builder.register::<JobsArgs, Value, _, _>(
        "jobs",
        "List this agent's active jobs, excluding this call and its containing script. Set `all` to include completed history.",
        ToolOptions::default().generated_output_schema(|_| presented_job_schema(true)),
        move |context, args| {
            let jobs = list.clone();
            async move {
                let current = jobs
                    .snapshot(context.job())
                    .await
                    .map_err(ToolError::failed)?;
                let containing_script = if let Some(parent) = current.parent {
                    let parent = jobs
                        .snapshot(parent)
                        .await
                        .map_err(ToolError::failed)?;
                    (parent.role == crate::job::JobRole::Script).then_some(parent.id)
                } else {
                    None
                };
                let envelopes = jobs
                    .list(context.agent())
                    .await
                    .into_iter()
                    .filter(|job| job.id != context.job() && Some(job.id) != containing_script)
                    .filter(|job| args.all || !job.state.is_terminal())
                    .collect::<Vec<_>>();
                Ok(Value::Array(
                    envelopes
                        .iter()
                        .map(|job| job.metadata_view(context.capabilities()).into_value())
                        .collect(),
                ))
            }
        },
    )?;
    let output = jobs.clone();
    builder.register_presented::<crate::job::output::OutputArgs, _, _>(
        "job_output",
        "Read or search saved job output and status on job completion. Whole-output reads attach saved images; filtered or paginated reads return text in preview.lines without images.",
        ToolOptions::default().job_method("output", "job"),
        move |context, mut args| {
            let jobs = output.clone();
            async move {
                args.cancellation = Some(context.cancellation_token());
                jobs.present_output_with(
                    args,
                    context.capabilities(),
                    crate::job::output::OutputOptions::Model { presentation: crate::job::OutputPresentation::Full },
                )
                .await
            }
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
                    .map_err(ToolError::failed)?;
                Ok(serde_json::json!({"accepted": true}))
            }
        },
    )?;
    let cancel = jobs.clone();
    builder.register::<JobArgs, Value, _, _>(
        "job_cancel",
        "Request cancellation of job/descendants; confirm terminal state with job_output.",
        ToolOptions::default()
            .generated_output_schema(|_| presented_job_schema(false))
            .script_only()
            .job_method("cancel", "job"),
        move |context, args| {
            let jobs = cancel.clone();
            async move {
                jobs.cancel(args.job)
                    .await
                    .map_err(ToolError::failed)
                    .map(|job| job.metadata_view(context.capabilities()).into_value())
            }
        },
    )?;
    Ok(())
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobsArgs {
    /// Include terminal job history as well as active jobs.
    #[serde(default)]
    all: bool,
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
        session::SessionStore,
        tests::TestRuntime,
        tool::{
            ToolOutput, ToolRegistryBuilder,
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
        let pending = pending_lease.id();
        jobs.transition(pending, JobState::AwaitingApproval)
            .await
            .unwrap();
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
            .run_host(agent, "job_output", json!({"job":pending}))
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
        store: SessionStore,
        jobs: JobManager,
        root: &std::path::Path,
    ) -> (ToolExecutor, Arc<OnceLock<ToolExecutor>>) {
        let mut builder = ToolRegistryBuilder::default();
        super::super::filesystem::register(&mut builder, store).unwrap();
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
        let (executor, slot) = executor(
            runtime.store.clone(),
            runtime.jobs.clone(),
            runtime.root.path(),
        );
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
            model: "image-test".into(),
            system: vec![],
            tools: vec![],
            response_schema: None,
            reasoning: None,
            max_output_tokens: None,
            correlation: None,
            blobs: Default::default(),
            tail: Vec::new(),
            history_lifetime: Default::default(),
            history: vec![Message::Tool(vec![ToolResult {
                call_id: "output".into(),
                name: "job_output".into(),
                result: json!({}),
                images,
                is_error: false,
            }])],
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
                .run_model(agent, "job_output", json!({"job":launched.job}))
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
        let source = format!("return await tool.job({}).output();", launched.job);
        let output = script(&executor, agent, source.clone(), false).await.output;
        assert_eq!(output.value["result"]["console"], "");
        assert_eq!(output.value["result"]["value"]["id"], launched.job.get());
        assert_loaded(&runtime.store, output.images).await;
        // A background script that retrieves an existing image must itself retain
        // the attachment, not merely the nested job's JSON metadata.
        let background = script(&executor, agent, source, true).await;
        runtime.jobs.wait(background.job, None, true).await.unwrap();
        let output = executor
            .run_model(agent, "job_output", json!({"job":background.job}))
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
        let options = json!({"field":"/result"});
        let output = executor
            .run_model(
                agent,
                "job_output",
                json!({"job":read.job, "field":"/result"}),
            )
            .await
            .unwrap();
        assert!(output.output.images.is_empty());
        // The job binding supplies the ID; the object form accepts only options.
        let source = format!("return await tool.job({}).output({options});", read.job);
        let output = script(&executor, agent, source, false).await;
        assert!(output.output.images.is_empty());
        for query in [
            json!({"job":read.job,"limit":0}),
            json!({"job":read.job,"pattern":"["}),
            json!({"job":999999}),
        ] {
            let response = executor
                .run_model(agent, "job_output", query)
                .await
                .unwrap();
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
            .fail(JobOutcome::Failed {
                message: "failed after producing an image".into(),
                output: Some(output),
                denial: None,
            })
            .await;
        let id = runtime.store.id();
        drop((runtime.jobs, runtime.store));
        let sessions = runtime.root.path().join("sessions");
        let (store, records) = SessionStore::open(&sessions, id).await.unwrap();
        let jobs = JobManager::restore(store.clone(), &records).await.unwrap();
        let agent = AgentId::root(id);
        let (executor, _slot) = executor(store.clone(), jobs, runtime.root.path());
        let output = executor.run_model(&agent, "job_output", json!({"job":job}));
        let output = output.await.unwrap().output;
        assert_eq!(output.value["state"], "failed");
        assert_loaded(&store, output.images).await;
        let source = format!("return await tool.job({job}).output();");
        let output = script(&executor, &agent, source, false).await;
        assert_loaded(&store, output.output.images).await;
    }
}
