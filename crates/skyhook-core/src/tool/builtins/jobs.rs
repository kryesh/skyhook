use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    identity::JobId,
    job::{JobManager, JobProgressRecord, JobState, presented_job_schema},
    tool::{RegistryError, ToolError, ToolOptions, ToolRegistryBuilder},
};

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    jobs: JobManager,
) -> Result<(), RegistryError> {
    let list = jobs.clone();
    builder.register::<JobsArgs, Value, _, _>(
        "jobs",
        "List active jobs owned by this agent, excluding this call and its containing script. Set `all` to include completed history.",
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
                        .map(|job| job.presented(&context.capabilities))
                        .collect::<Result<_, _>>()?,
                ))
            }
        },
    )?;
    let inspect = jobs.clone();
    builder.register::<JobArgs, Value, _, _>(
        "job_inspect",
        "Read the current envelope, including delivered questions/results; never acknowledges delivery.",
        ToolOptions::default()
            .generated_output_schema(|capabilities| presented_job_schema(capabilities, false))
            .script_only()
            .job_method("inspect", "job"),
        move |context, args| {
            let jobs = inspect.clone();
            async move {
                jobs.snapshot(args.job)
                    .await
                    .map_err(|error| job_error(&error))
                    .and_then(|job| {
                        job.presented(&context.capabilities)
                            .map_err(ToolError::from)
                    })
            }
        },
    )?;
    let wait = jobs.clone();
    builder.register::<JobWaitArgs, Value, _, _>(
        "wait",
        "Wait for and acknowledge the next undelivered question or terminal result. Timeout returns a nonterminal envelope without stopping work. Answer via script: tool.job(id).send({value:answer}).",
        ToolOptions::default()
            .generated_output_schema(|capabilities| presented_job_schema(capabilities, false))
            .job_method("wait", "job"),
        move |context, args| {
            let jobs = wait.clone();
            async move {
                if args
                    .timeout
                    .is_some_and(|timeout| !(1..=3_600).contains(&timeout))
                {
                    return Err(ToolError::InvalidArguments(
                        "timeout must be 1 through 3600".to_owned(),
                    ));
                }
                jobs.wait(args.job, args.timeout.map(Duration::from_secs), true)
                    .await
                    .map_err(|error| job_error(&error))
                    .and_then(|job| job.presented(&context.capabilities).map_err(ToolError::from))
            }
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
        "Request cancellation of job/descendants; confirm terminal state with wait/inspect.",
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
                        job.presented(&context.capabilities)
                            .map_err(ToolError::from)
                    })
            }
        },
    )?;
    builder.register::<JobEventsArgs, JobEventsOutput, _, _>(
        "job_events",
        "Read typed progress events after a durable cursor.",
        ToolOptions::default()
            .script_only()
            .job_method("events", "job"),
        move |_context, args| {
            let jobs = jobs.clone();
            async move {
                if !(1..=1_000).contains(&args.limit) {
                    return Err(ToolError::InvalidArguments(
                        "limit must be 1 through 1000".to_owned(),
                    ));
                }
                let events = jobs
                    .events(args.job, args.after, args.limit)
                    .await
                    .map_err(|error| job_error(&error))?;
                let next = events.last().map_or(args.after, |event| event.sequence);
                let state = jobs
                    .snapshot(args.job)
                    .await
                    .map_err(|error| job_error(&error))?
                    .state
                    .presented();
                Ok(JobEventsOutput {
                    events,
                    next,
                    state,
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
struct JobWaitArgs {
    /// Background/suspended job ID.
    job: JobId,
    /// Maximum seconds to wait (1-3600). Omit to wait indefinitely.
    #[schemars(range(min = 1, max = 3600))]
    timeout: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobSendArgs {
    /// Stable job identifier. Use the agent job, not its internal ask job.
    job: JobId,
    /// JSON input. For merged child questions, pass an object keyed directly by question ID.
    value: Value,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobEventsArgs {
    job: JobId,
    /// Exclusive durable sequence cursor.
    #[serde(default)]
    after: u64,
    /// Maximum events (1-1000).
    #[serde(default = "default_job_events")]
    #[schemars(range(min = 1, max = 1000))]
    limit: usize,
}

#[derive(Serialize, JsonSchema)]
struct JobEventsOutput {
    events: Vec<JobProgressRecord>,
    next: u64,
    state: JobState,
}

const fn default_job_events() -> usize {
    100
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        identity::AgentId,
        job::JobEnvelope,
        job::JobSpec,
        session::SessionStore,
        tool::{ToolOutput, ToolRegistryBuilder, executor::ToolExecutor, policy::AllowAll},
    };

    #[tokio::test]
    async fn every_job_api_projects_pending_authorization_as_queued() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store);
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
        let executor = ToolExecutor::new(
            builder.build(),
            Arc::new(AllowAll),
            jobs,
            root.path().to_path_buf(),
        );
        for (tool, args) in [
            ("jobs", serde_json::json!({})),
            ("job_inspect", serde_json::json!({"job":pending})),
            ("job_events", serde_json::json!({"job":pending})),
            ("wait", serde_json::json!({"job":pending,"timeout":1})),
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
        jobs.finish(completed, Ok(ToolOutput::new(Value::Null)), None)
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
        let current: Vec<JobEnvelope> = serde_json::from_value(current.output.value).unwrap();
        assert_eq!(
            current.iter().map(|job| job.id).collect::<Vec<_>>(),
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
        let nested: Vec<JobEnvelope> = serde_json::from_value(nested.output.value).unwrap();
        assert!(nested.iter().any(|job| job.id == active));
        assert!(nested.iter().all(|job| job.id != script));

        let all = executor
            .execute(agent, "jobs", serde_json::json!({"all": true}), None)
            .await
            .unwrap();
        let listing_job = all.job;
        let all: Vec<JobEnvelope> = serde_json::from_value(all.output.value).unwrap();
        assert!(all.iter().any(|job| job.id == active));
        assert!(all.iter().any(|job| job.id == completed));
        assert!(all.iter().all(|job| job.id != listing_job));
    }
}
