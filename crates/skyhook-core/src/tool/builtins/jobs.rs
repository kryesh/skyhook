use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    identity::JobId,
    job::{JobManager, presented_job_schema},
    tool::{RegistryError, ToolError, ToolOptions, ToolOutput, ToolRegistryBuilder},
};

pub(crate) fn register(
    builder: &mut ToolRegistryBuilder,
    jobs: JobManager,
) -> Result<(), RegistryError> {
    let list = jobs.clone();
    builder.register::<JobsArgs, Value, _, _>(
        "jobs",
        "List this agent's active jobs, excluding this call and its containing script. Set `all` to include completed history.",
        ToolOptions::default().generated_output_schema(|capabilities| {
            presented_job_schema(capabilities, true)
        }),
        move |context, args| {
            let jobs = list.clone();
            async move {
                let current = jobs
                    .snapshot(context.job)
                    .await
                    .map_err(|error| job_error(&error))?;
                let containing_script = if let Some(parent) = current.parent {
                    let parent = jobs
                        .snapshot(parent)
                        .await
                        .map_err(|error| job_error(&error))?;
                    (parent.tool == "script").then_some(parent.id)
                } else {
                    None
                };
                let envelopes = jobs
                    .list(&context.agent)
                    .await
                    .into_iter()
                    .filter(|job| job.id != context.job && Some(job.id) != containing_script)
                    .filter(|job| args.all || !job.state.is_terminal())
                    .collect::<Vec<_>>();
                Ok(Value::Array(
                    envelopes
                        .iter()
                        .map(|job| job.presented_for(&context.capabilities, Some(&context.caller_location), true))
                        .collect::<Result<_, _>>()?,
                ))
            }
        },
    )?;
    let output = jobs.clone();
    builder.register_dynamic(
        "job_output",
        "Read or search saved job output and status on job completion. Whole-output reads attach saved images; filtered or paginated reads return text in preview.lines without images.",
        serde_json::to_value(schemars::schema_for!(crate::job::output::OutputArgs))
            .map_err(|error| RegistryError::Schema(error.to_string()))?,
        ToolOptions::default().generated_output_schema(crate::job::output::view_schema).job_method("output", "job"),
        move |context, arguments| {
            let jobs = output.clone();
            async move {
                let mut args: crate::job::output::OutputArgs = serde_json::from_value(arguments)
                    .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
                // Any explicit text selection, even its default value, opts out
                // of potentially large image payloads.
                let attach_images = args.field.is_none()
                    && args.start.is_none()
                    && args.limit.is_none()
                    && args.pattern.is_none()
                    && args.context.is_none()
                    && args.offset.is_none();
                let job = args.job;
                args.cancellation = Some(context.cancellation_token());
                // Preserve validation, cancellation, capability projection, and
                // acknowledgment before retrieving any attachment side channel.
                let value = jobs.present_output_for(
                    args, &context.capabilities, &context.caller_location, false,
                ).await?;
                let images = if attach_images {
                    jobs.images(job).await.map_err(|error| job_error(&error))?
                } else {
                    Vec::new()
                };
                Ok(ToolOutput::new(value).with_images(images))
            }
        },
    )?;
    let send = jobs.clone();
    builder.register::<JobSendArgs, Value, _, _>(
        "job_send",
        "Send JSON input to a job. For child agents, answer pending questions or deliver follow-up instructions automatically at the next model-request boundary. Sending to a completed agent appends to its retained conversation and resumes it under the same job ID. Background scripts read input with await receive().",
        ToolOptions::default()
            .script_only()
            .job_method("send", "job"),
        move |_context, args| {
            let jobs = send.clone();
            async move {
                jobs.send(args.job, args.value)
                    .await
                    .map_err(|error| job_error(&error))?;
                Ok(serde_json::json!({"accepted": true}))
            }
        },
    )?;
    let cancel = jobs.clone();
    builder.register::<JobArgs, Value, _, _>(
        "job_cancel",
        "Request cancellation of job/descendants; confirm terminal state with job_output.",
        ToolOptions::default()
            .generated_output_schema(|capabilities| presented_job_schema(capabilities, false))
            .script_only()
            .job_method("cancel", "job"),
        move |context, args| {
            let jobs = cancel.clone();
            async move {
                jobs.cancel(args.job)
                    .await
                    .map_err(|error| job_error(&error))
                    .and_then(|job| {
                        job.presented_for(
                            &context.capabilities,
                            Some(&context.caller_location),
                            false,
                        )
                        .map_err(ToolError::from)
                    })
            }
        },
    )?;
    Ok(())
}

fn job_error(error: &impl ToString) -> ToolError {
    ToolError::Failed(error.to_string())
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
    use crate::job::{JobOutcome, JobState};
    use crate::tests::TestRuntime;
    use crate::tool::policy::{Capability, CapabilitySet};
    use std::sync::{Arc, OnceLock};

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;

    use super::*;
    use crate::{
        identity::AgentId,
        job::JobSpec,
        media::ImageReference,
        provider::protocol::{Message, ModelRequest, ToolResult},
        session::SessionStore,
        tool::{ToolOutput, ToolRegistryBuilder, executor::ToolExecutor, policy::AllowAll},
    };

    #[tokio::test]
    async fn every_job_api_projects_pending_authorization_as_queued() {
        let runtime = TestRuntime::new().await;
        let agent = runtime.agent.clone();
        let jobs = runtime.jobs.clone();
        let pending = jobs
            .create(JobSpec::test(agent.clone(), "pending"))
            .await
            .unwrap()
            .id;
        jobs.transition(pending, JobState::AwaitingApproval)
            .await
            .unwrap();
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, jobs.clone()).unwrap();
        let executor = runtime.executor(builder);
        for (tool, args) in [
            ("jobs", serde_json::json!({})),
            ("job_output", serde_json::json!({"job":pending})),
        ] {
            let result = executor
                .execute(agent.clone(), tool, args, None)
                .await
                .unwrap()
                .output
                .value;
            assert!(!result.to_string().contains("awaiting_approval"));
            assert_eq!(
                if tool == "jobs" {
                    &result[0]["state"]
                } else {
                    &result["state"]
                },
                "queued"
            );
        }
    }

    #[tokio::test]
    async fn jobs_defaults_to_other_active_jobs_and_all_includes_history() {
        let runtime = TestRuntime::new().await;
        let agent = runtime.agent.clone();
        let jobs = &runtime.jobs;
        let active = jobs
            .create(JobSpec {
                background: true,
                ..JobSpec::test(agent.clone(), "active")
            })
            .await
            .unwrap()
            .id;
        let completed = jobs
            .create(JobSpec {
                background: true,
                ..JobSpec::test(agent.clone(), "completed")
            })
            .await
            .unwrap()
            .id;
        jobs.finish(
            completed,
            crate::job::JobOutcome::Completed(ToolOutput::new(Value::Null)),
        )
        .await
        .unwrap();
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, jobs.clone()).unwrap();
        let executor = runtime.executor(builder);

        let current = executor
            .execute(agent.clone(), "jobs", serde_json::json!({}), None)
            .await
            .unwrap();
        let current = current.output.value.as_array().unwrap();
        assert_eq!(
            current
                .iter()
                .map(
                    |job| serde_json::from_value::<crate::identity::JobId>(job["id"].clone())
                        .unwrap()
                )
                .collect::<Vec<_>>(),
            [active]
        );

        let script = jobs
            .create(JobSpec {
                accepts_input: true,
                background: true,
                ..JobSpec::test(agent.clone(), "script")
            })
            .await
            .unwrap()
            .id;
        let nested = executor
            .execute(agent.clone(), "jobs", serde_json::json!({}), Some(script))
            .await
            .unwrap();
        let nested = nested.output.value.as_array().unwrap();
        assert!(
            nested
                .iter()
                .any(|job| job["id"] == serde_json::json!(active))
        );
        assert!(
            nested
                .iter()
                .all(|job| job["id"] != serde_json::json!(script))
        );

        let all = executor
            .execute(agent, "jobs", serde_json::json!({"all": true}), None)
            .await
            .unwrap();
        let listing_job = all.job;
        let all = all.output.value.as_array().unwrap();
        assert!(all.iter().any(|job| job["id"] == serde_json::json!(active)));
        assert!(
            all.iter()
                .any(|job| job["id"] == serde_json::json!(completed))
        );
        assert!(
            all.iter()
                .all(|job| job["id"] != serde_json::json!(listing_job))
        );
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

    async fn assert_hydrated(store: &SessionStore, images: Vec<ImageReference>) {
        assert_eq!(images.len(), 1);
        assert!(
            images[0].data_base64.is_none(),
            "journal references stay payload-free"
        );
        let mut request = ModelRequest {
            model: "image-test".into(),
            system: vec![],
            tools: vec![],
            response_schema: None,
            reasoning: None,
            max_output_tokens: None,
            correlation: None,
            messages: vec![Message::Tool(vec![ToolResult {
                call_id: "output".into(),
                name: "job_output".into(),
                result: json!({}),
                images,
                is_error: false,
            }])],
        };
        store.hydrate_model_request(&mut request).await.unwrap();
        let Message::Tool(results) = &request.messages[0] else {
            panic!("tool results")
        };
        assert_eq!(
            results[0].images[0].data_base64.as_deref(),
            Some(STANDARD.encode(IMAGE).as_str())
        );
    }

    #[tokio::test]
    async fn background_image_output_attaches_directly_and_through_javascript() {
        let runtime = TestRuntime::new().await;
        tokio::fs::write(runtime.root.path().join("image.png"), IMAGE)
            .await
            .unwrap();
        let (executor, _slot) = executor(
            runtime.store.clone(),
            runtime.jobs.clone(),
            runtime.root.path(),
        );
        // read itself is foreground-only; a background script is the supported way
        // to read an image asynchronously and retrieve its saved output later.
        let launched = executor
            .execute_model(
                runtime.agent.clone(),
                "script",
                json!({"source":"return await tool.read({path:'image.png'});","bg":true}),
                None,
            )
            .await
            .unwrap();
        assert!(launched.background);
        runtime.jobs.wait(launched.job, None, true).await.unwrap();
        for _ in 0..2 {
            let output = executor
                .execute_model(
                    runtime.agent.clone(),
                    "job_output",
                    json!({"job":launched.job}),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(output.output.value["state"], "completed");
            // Returning a tool result preserves its originating read job view.
            assert_eq!(output.output.value["result"]["value"]["tool"], "read");
            assert_eq!(output.output.value["result"]["console"], "");
            assert!(output.output.value.get("console").is_none());
            assert_hydrated(&runtime.store, output.output.images).await;
        }
        let source = format!("return await tool.job({}).output();", launched.job);
        let output = executor
            .execute_model(
                runtime.agent.clone(),
                "script",
                json!({"source":source}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(output.output.value["result"]["console"], "");
        assert_eq!(
            output.output.value["result"]["value"]["id"],
            launched.job.get()
        );
        assert_hydrated(&runtime.store, output.output.images).await;

        // A background script that retrieves an existing image must itself retain
        // the attachment, not merely the nested job's JSON metadata.
        let script = executor
            .execute_model(
                runtime.agent.clone(),
                "script",
                json!({"source":format!("return await tool.job({}).output();", launched.job), "bg":true}),
                None,
            )
            .await
            .unwrap();
        runtime.jobs.wait(script.job, None, true).await.unwrap();
        let output = executor
            .execute_model(
                runtime.agent.clone(),
                "job_output",
                json!({"job":script.job}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(output.output.value["state"], "completed");
        assert_hydrated(&runtime.store, output.output.images).await;
    }

    #[tokio::test]
    async fn image_output_text_selections_do_not_attach_and_invalid_queries_fail() {
        let runtime = TestRuntime::new().await;
        tokio::fs::write(runtime.root.path().join("image.png"), IMAGE)
            .await
            .unwrap();
        let (executor, _slot) = executor(
            runtime.store.clone(),
            runtime.jobs.clone(),
            runtime.root.path(),
        );
        let read = executor
            .execute_model(
                runtime.agent.clone(),
                "read",
                json!({"path":"image.png"}),
                None,
            )
            .await
            .unwrap();
        assert_hydrated(&runtime.store, read.output.images).await;
        for selection in [
            json!({"field":"/result"}),
            json!({"field":""}),
            json!({"start":1}),
            json!({"limit":100}),
            json!({"offset":0}),
            json!({"context":0}),
            json!({"pattern":"image"}),
            json!({"pattern":"image", "context":1}),
        ] {
            let mut query = selection;
            query["job"] = json!(read.job);
            let output = executor
                .execute_model(runtime.agent.clone(), "job_output", query.clone(), None)
                .await
                .unwrap();
            assert!(output.output.images.is_empty(), "{query}");
            // The job binding supplies the ID; the object form accepts only options.
            let mut options = query.clone();
            options.as_object_mut().unwrap().remove("job");
            let source = format!("return await tool.job({}).output({options});", read.job);
            let output = executor
                .execute_model(
                    runtime.agent.clone(),
                    "script",
                    json!({"source":source}),
                    None,
                )
                .await
                .unwrap();
            assert!(output.output.images.is_empty(), "{query}");
        }
        for query in [
            json!({"job":read.job,"limit":0}),
            json!({"job":read.job,"pattern":"["}),
            json!({"job":999999}),
        ] {
            assert!(
                executor
                    .execute_model(runtime.agent.clone(), "job_output", query, None)
                    .await
                    .is_err()
            );
        }
        // A denied image-producing call never creates a retrievable attachment.
        let mut capabilities = CapabilitySet::default();
        capabilities.remove(Capability::Read);
        assert!(
            executor
                .clone()
                .with_capabilities(capabilities)
                .execute_model(
                    runtime.agent.clone(),
                    "read",
                    json!({"path":"image.png"}),
                    None
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn retrieved_images_survive_resume_including_failed_tool_output() {
        let runtime = TestRuntime::new().await;
        let image = runtime
            .store
            .import_blob(IMAGE, "image.png".into(), "image/png".into())
            .await
            .unwrap();
        let job = runtime
            .jobs
            .create(JobSpec::test(runtime.agent.clone(), "partial-image"))
            .await
            .unwrap()
            .id;
        runtime
            .jobs
            .finish(
                job,
                JobOutcome::Failed {
                    message: "failed after producing an image".into(),
                    output: Some(ToolOutput::new(json!({"image":image})).with_images(vec![image])),
                    denial: None,
                },
            )
            .await
            .unwrap();
        let id = runtime.store.id();
        drop(runtime.jobs);
        drop(runtime.store);
        let (store, records) = SessionStore::open(&runtime.root.path().join("sessions"), id)
            .await
            .unwrap();
        let jobs = JobManager::restore(store.clone(), &records).await.unwrap();
        let agent = AgentId::root(id);
        let (executor, _slot) = executor(store.clone(), jobs, runtime.root.path());
        let output = executor
            .execute_model(agent.clone(), "job_output", json!({"job":job}), None)
            .await
            .unwrap();
        assert_eq!(output.output.value["state"], "failed");
        assert_hydrated(&store, output.output.images).await;
        let source = format!("return await tool.job({job}).output();");
        let output = executor
            .execute_model(agent, "script", json!({"source":source}), None)
            .await
            .unwrap();
        assert_hydrated(&store, output.output.images).await;
    }
}
