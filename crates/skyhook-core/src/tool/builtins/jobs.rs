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
    builder.register::<crate::job::output::OutputArgs, Value, _, _>(
        "job_output",
        "Read or search saved job output and status. Reads are repeatable. Use wait to await output, a question, or completion; timeout does not stop work. Select a field with start/limit; total_lines reports its size. Continue using next_start/next_offset as start/offset, repeating field and any pattern/context. Answer questions with tool.job(id).send({value:answer}).",
        ToolOptions::default().generated_output_schema(crate::job::output::view_schema).job_method("output", "job"),
        move |context, mut args| {
            let jobs = output.clone();
            args.cancellation = Some(context.cancellation_token());
            async move { jobs.present_output_for(args, &context.capabilities, &context.caller_location, false).await }
        },
    )?;
    let send = jobs.clone();
    builder.register::<JobSendArgs, Value, _, _>(
        "job_send",
        "Send JSON input; agent answers use its stable agent job ID.",
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
    /// Stable job identifier. Use the agent job, not its internal ask job.
    job: JobId,
    /// JSON input. For merged child questions, pass an object keyed directly by question ID.
    value: Value,
}

#[cfg(test)]
mod tests {
    use crate::job::JobState;
    use crate::test_support::TestRuntime;
    use std::sync::Arc;

    use super::*;
    use crate::{
        identity::AgentId,
        job::JobSpec,
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
            ("job_output", serde_json::json!({"job":pending,"wait":1})),
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
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store);
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
        let executor = ToolExecutor::new(
            builder.build(),
            Arc::new(AllowAll),
            jobs.clone(),
            root.path().to_path_buf(),
        )
        .with_capabilities({
            let mut capabilities = crate::tool::policy::CapabilitySet::default();
            capabilities.insert(crate::tool::policy::Capability::Targets);
            capabilities
        });

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
}
