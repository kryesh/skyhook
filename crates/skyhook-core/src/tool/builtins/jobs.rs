use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    identity::JobId,
    job::{JobEnvelope, JobManager, JobProgressRecord, JobState},
    tool::{RegistryError, ToolError, ToolOptions, ToolRegistryBuilder, policy::ToolEffect},
};

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    jobs: JobManager,
) -> Result<(), RegistryError> {
    let list = jobs.clone();
    builder.register::<JobsArgs, Vec<JobEnvelope>, _, _>(
        "jobs",
        "List active jobs owned by this agent. Set `all` to include completed history.",
        ToolOptions::new(vec![ToolEffect::SessionState]),
        move |context, args| {
            let jobs = list.clone();
            async move {
                Ok(jobs
                    .list(&context.agent)
                    .await
                    .into_iter()
                    .filter(|job| job.id != context.job)
                    .filter(|job| args.all || !job.state.is_terminal())
                    .collect())
            }
        },
    )?;
    let inspect = jobs.clone();
    builder.register::<JobArgs, JobEnvelope, _, _>(
        "job_inspect",
        "Inspect one job without claiming its result.",
        ToolOptions::new(vec![ToolEffect::SessionState])
            .script_only()
            .job_method("inspect", "job"),
        move |_context, args| {
            let jobs = inspect.clone();
            async move {
                jobs.snapshot(args.job)
                    .await
                    .map_err(|error| job_error(&error))
            }
        },
    )?;
    let wait = jobs.clone();
    builder.register::<JobWaitArgs, JobEnvelope, _, _>(
        "wait",
        "Wait for and claim the next question or terminal result from a job. A waiting_input result can be answered with job_send.",
        ToolOptions::new(vec![ToolEffect::SessionState]).job_method("wait", "job"),
        move |_context, args| {
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
            }
        },
    )?;
    let send = jobs.clone();
    builder.register::<JobSendArgs, Value, _, _>(
        "job_send",
        "Send JSON input to a running job. For an agent in waiting_input, send answers to its stable agent job ID.",
        ToolOptions::new(vec![ToolEffect::SessionState])
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
    builder.register::<JobArgs, JobEnvelope, _, _>(
        "job_cancel",
        "Request cancellation of a job.",
        ToolOptions::new(vec![ToolEffect::SessionState])
            .script_only()
            .job_method("cancel", "job"),
        move |_context, args| {
            let jobs = cancel.clone();
            async move {
                jobs.cancel(args.job)
                    .await
                    .map_err(|error| job_error(&error))
            }
        },
    )?;
    builder.register::<JobEventsArgs, JobEventsOutput, _, _>(
        "job_events",
        "Read typed progress events after a durable cursor.",
        ToolOptions::new(vec![ToolEffect::SessionState])
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
                    .state;
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
    /// Stable job identifier returned by a background or suspended tool call.
    job: JobId,
    /// Maximum seconds to wait (1-3600). Omit to wait indefinitely.
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
    /// Return events strictly after this durable sequence cursor.
    #[serde(default)]
    after: u64,
    /// Maximum number of events to return (1-1000).
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
        session::SessionStore,
        tool::{ToolOutput, ToolRegistryBuilder, executor::ToolExecutor, policy::AllowAll},
    };

    #[tokio::test]
    async fn jobs_defaults_to_other_active_jobs_and_all_includes_history() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store);
        let active = jobs
            .create(
                agent.clone(),
                None,
                "active".to_owned(),
                serde_json::json!({}),
                false,
                true,
                None,
            )
            .await
            .unwrap()
            .id;
        let completed = jobs
            .create(
                agent.clone(),
                None,
                "completed".to_owned(),
                serde_json::json!({}),
                false,
                true,
                None,
            )
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
            jobs,
            root.path().to_path_buf(),
        );

        let current = executor
            .execute(agent.clone(), "jobs", serde_json::json!({}), None)
            .await
            .unwrap();
        let current: Vec<JobEnvelope> = serde_json::from_value(current.output.value).unwrap();
        assert_eq!(
            current.iter().map(|job| job.id).collect::<Vec<_>>(),
            [active]
        );

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
