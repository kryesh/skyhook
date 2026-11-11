use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::tool::catalog::{Invocation, RegistryError, ToolOptions};
use crate::{
    job::{
        JobError, JobListing, JobManager, ViewKind,
        output::{
            DEFAULT_LIMIT, MAX_CONTEXT, OutputArgs, OutputOptions, PageLimit, PageUnit, Selectors,
        },
    },
    tool::{ToolContext, ToolError, ToolOutput, ToolRegistryBuilder},
};
use skyhook_tools::{diagnostic::PartialContext, error::AdmissionError, output::FieldPointer};
use skyhook_worker::diagnostic::{Effects, Operation, Subject};
use skyhook_worker::job::JobId;

pub(crate) fn register(
    builder: &mut ToolRegistryBuilder,
    jobs: JobManager,
) -> Result<(), RegistryError> {
    let admitted = jobs.clone();
    builder.register_checked(
        "jobs",
        "Without `job`, list this agent's active jobs, excluding this call and its containing script; `older` counts those `limit` leaves out. With `job`, read, page, search or query that job's saved output and status immediately, returning its JobView in place of this call's. Whole-output reads attach saved images; field reads return one page in preview, without images",
        ToolOptions::default()
            .output_schema(crate::job::JOB_VIEW_SCHEMAS.listing.clone())
            .internal_result(|request| {
                let schemas = &crate::job::JOB_VIEW_SCHEMAS;
                match request {
                    JobsRequest::List { .. } => schemas.listing.clone(),
                    JobsRequest::Output(_) => schemas.view.clone(),
                }
            }),
        |args: JobsArgs| JobsRequest::try_from(args),
        move |request| {
            let jobs = admitted.clone();
            match request {
                JobsRequest::List { all, limit } => {
                    Invocation::new(move |context| list(jobs, context, all, limit))
                }
                JobsRequest::Output(args) => crate::tool::catalog::Invocation::job_view(move |context| async move {
                    let job = args.job;
                    let read = PartialContext::new(Operation::Read, Subject::Job(job));
                    let presented = jobs.present_output_with(
                        args,
                        context.cancellation_token(),
                        context.diagnostic_viewer(),
                        OutputOptions::Model {
                            kind: ViewKind::Inspection,
                            reader: Some(context.job()),
                            fit: context.fit(),
                            reach: context.reach(),
                        },
                    );
                    let presented = presented.await.map_err(|error| error.or(read))?;
                    context.read(presented.acknowledges).await;
                    Ok(presented)
                }),
            }
        },
    )?;
    builder.register_checked(
        "control",
        "Send input to a job or cancel it",
        ToolOptions::default(),
        |args: ControlArgs| Control::try_from(args),
        move |action| {
            let jobs = jobs.clone();
            match action {
                Control::Send { job, value } => Invocation::unit(move |_| async move {
                    (jobs.send(job, value).await)
                        .map_err(|error| job_failure(error, Operation::Send, job))
                }),
                Control::Cancel { job } => Invocation::unit(move |_| async move {
                    (jobs.cancel(job).await)
                        .map(drop)
                        .map_err(|error| job_failure(error, Operation::Terminate, job))
                }),
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

async fn list(
    jobs: JobManager,
    context: ToolContext,
    all: bool,
    limit: PageLimit,
) -> Result<ToolOutput, ToolError> {
    let current = jobs
        .snapshot(context.job())
        .await
        .map_err(|error| job_failure(error, Operation::Inspect, context.job()))?;
    let containing_script = if let Some(parent) = current.parent {
        let parent = jobs
            .snapshot(parent)
            .await
            .map_err(|error| job_failure(error, Operation::Inspect, parent))?;
        (parent.role == crate::job::JobRole::Script).then_some(parent.id)
    } else {
        None
    };
    let except = [Some(context.job()), containing_script];
    let (listed, older) = if all {
        let newest = jobs.newest(
            context.agent(),
            except.into_iter().flatten().collect(),
            crate::job::output::count(limit),
        );
        newest
            .await
            .map_err(|error| job_failure(error, Operation::Inspect, context.job()))?
    } else {
        let mut active = jobs.active(context.agent());
        active.retain(|job| !except.contains(&Some(job.id)));
        let older = active
            .len()
            .saturating_sub(crate::job::output::count(limit));
        (active.split_off(older), older)
    };
    let listing = JobListing {
        jobs: (listed.into_iter())
            .map(|job| job.metadata_view(context.diagnostic_viewer()))
            .collect(),
        older: std::num::NonZeroUsize::new(older),
    };
    let listing = serde_json::to_value(listing).expect("a job listing serializes");
    // Bounded by its limit, a listing is a presentation already.
    let presented = skyhook_tools::output::Presented::presentation(FieldPointer::result());
    Ok(ToolOutput::new(listing).with_presented(presented))
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct JobsArgs {
    job: Option<JobId>,
    /// Listing only: include finished jobs
    #[serde(default)]
    all: bool,
    /// JSON Pointer in saved content, e.g. /result/stdout, /result/content, /result/console
    field: Option<FieldPointer>,
    /// One-based first line of text, or element, member or query match of JSON; use returned next_start to continue
    #[schemars(range(min = 1), extend("default" = 1))]
    start: Option<usize>,
    /// Required with start > 1 on a JSON field
    unit: Option<PageUnit>,
    /// Maximum returned lines (including match context), values or query matches; without job, the newest jobs listed
    #[schemars(extend("default" = DEFAULT_LIMIT))]
    limit: Option<PageLimit>,
    /// Case-sensitive regex keeping the lines of text, or the elements and members of JSON whose compact JSON matches; use (?i) for case-insensitive matching
    pattern: Option<String>,
    /// Text fields: surrounding lines per pattern match
    #[schemars(range(min = 0, max = MAX_CONTEXT), extend("default" = 0))]
    context: Option<usize>,
    /// Text fields: zero-based UTF-8 byte offset within the starting line; use returned next_offset to continue
    #[schemars(range(min = 0), extend("default" = 0))]
    offset: Option<usize>,
    /// JSONPath (RFC 9535) query over a JSON field, with `$` as the field, e.g. $[?search(@.name, '^api') && @.phase == 'Failed']; returns matches with their pointers. Numeric comparisons use double precision
    query: Option<String>,
}

enum JobsRequest {
    List { all: bool, limit: PageLimit },
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
            unit,
            limit,
            pattern,
            context,
            offset,
            query,
        } = args;
        match job {
            Some(_) if all => Err(AdmissionError::invalid_arguments(
                "all lists jobs; omit it with job",
            )),
            Some(job) => Ok(Self::Output(OutputArgs {
                job,
                field,
                start,
                unit,
                limit,
                pattern,
                context,
                offset,
                query,
            })),
            None if field.is_some()
                || unit.is_some()
                || pattern.is_some()
                || query.is_some()
                || (Selectors {
                    start,
                    offset,
                    context,
                })
                .is_set() =>
            {
                Err(AdmissionError::invalid_arguments(
                    "field, start, unit, offset, pattern, context and query read a job's output; pass job",
                ))
            }
            None => Ok(Self::List {
                all,
                limit: limit.unwrap_or(DEFAULT_LIMIT),
            }),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ControlArgs {
    job: JobId,
    /// JSON input
    send: Option<Value>,
    /// Cancel the job and its descendants
    #[serde(default)]
    cancel: bool,
}

enum Control {
    Send { job: JobId, value: Value },
    Cancel { job: JobId },
}

impl TryFrom<ControlArgs> for Control {
    type Error = AdmissionError;

    fn try_from(ControlArgs { job, send, cancel }: ControlArgs) -> Result<Self, Self::Error> {
        match (send, cancel) {
            (Some(value), false) => Ok(Self::Send { job, value }),
            (None, true) => Ok(Self::Cancel { job }),
            (Some(_), true) => Err(AdmissionError::invalid_arguments(
                "send and cancel are alternatives; pass one",
            )),
            (None, false) => Err(AdmissionError::invalid_arguments("pass send or cancel")),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::job::JobRole;
    use crate::{
        job::{JobOutcome, JobSpec},
        tests::{TestRuntime, tool_executor},
        tool::{ToolOutput, ToolRegistryBuilder},
    };
    use skyhook_tools::diagnostic::Cause;

    fn ids(listing: &Value) -> Vec<Value> {
        let jobs = listing["jobs"].as_array().unwrap();
        jobs.iter().map(|job| job["id"].clone()).collect()
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
            Ok(JobsRequest::List {
                all: false,
                limit: DEFAULT_LIMIT
            })
        ));
        // Selectors restating their defaults are a listing's, as schema-filling models send them.
        let listing = json!({"all":true, "limit":5, "start":1, "offset":0, "context":0});
        assert!(matches!(
            request(listing),
            Ok(JobsRequest::List { all: true, limit }) if u64::from(limit) == 5
        ));
        assert!(matches!(
            request(json!({"job":3, "pattern":"x"})),
            Ok(JobsRequest::Output(args)) if args.job.get() == 3 && args.pattern.as_deref() == Some("x")
        ));
        for value in [
            json!({"job":3, "all":true}),
            json!({"field":"/result"}),
            json!({"start":2}),
            json!({"limit":0}),
        ] {
            assert!(
                matches!(request(value.clone()), Err(error)
                    if matches!(error.diagnostic().cause, Cause::InvalidArguments(_))),
                "{value}"
            );
        }
    }

    #[test]
    fn control_takes_exactly_one_of_send_or_cancel() {
        let action = |value: Value| {
            serde_json::from_value::<ControlArgs>(value)
                .map_err(AdmissionError::invalid_arguments)
                .and_then(Control::try_from)
        };
        assert!(matches!(
            action(json!({"job":3, "send":{"a":[1]}})),
            Ok(Control::Send { job, value }) if job.get() == 3 && value == json!({"a":[1]})
        ));
        assert!(matches!(
            action(json!({"job":3, "cancel":true, "send":null})),
            Ok(Control::Cancel { job }) if job.get() == 3
        ));
        for value in [
            json!({"job":3}),
            json!({"job":3, "send":1, "cancel":true}),
            json!({"send":1}),
        ] {
            assert!(action(value.clone()).is_err(), "{value}");
        }
    }

    #[tokio::test]
    async fn job_output_reads_name_the_requested_job_on_the_host() {
        let runtime = TestRuntime::new().await;
        let (executor, _slot) = tool_executor(runtime.jobs.clone(), runtime.root.path());
        let unknown = JobId::new(999_999).unwrap();
        for (query, operation, subject) in [
            (
                json!({"job": unknown}),
                Operation::Read,
                Subject::Job(unknown),
            ),
            (
                json!({"job": unknown, "pattern":"private-pattern-[z-a]"}),
                Operation::Validate,
                Subject::argument(["pattern"]),
            ),
        ] {
            let error = executor
                .run_host(&runtime.agent, "jobs", query)
                .await
                .unwrap_err();
            // Upstream compile errors echo the pattern; ours must not.
            assert!(!error.to_string().contains("private-pattern"));
            let context = error.diagnostic().context;
            assert_eq!(context.operation, operation);
            assert_eq!(context.subject, subject);
            assert_eq!(context.site, skyhook_tools::diagnostic::FailureSite::Host);
        }
        // An unknown job was never touched.
        let cancel = executor.run_host(
            &runtime.agent,
            "control",
            json!({"job": unknown, "cancel": true}),
        );
        let context = cancel.await.unwrap_err().diagnostic().context;
        assert_eq!(context.effects, Effects::NotStarted);
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
        let finished = JobOutcome::Completed(ToolOutput::new(Value::Null).into());
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
        for state in [&current["jobs"][0]["state"], &output["state"]] {
            assert_eq!(state, "queued");
        }
        assert_eq!(ids(&current), [json!(pending)]);
        assert_eq!(current.get("older"), None);

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
        // The newest jobs, and how many older ones the limit leaves out.
        let newest = executor
            .run_host(agent, "jobs", json!({"all": true, "limit": 2}))
            .await
            .unwrap();
        let older = listed.len() + 1 - 2;
        assert_eq!(newest.output.value["older"], older);
        let expected = [listed.last().unwrap().clone(), json!(all.job)];
        assert_eq!(ids(&newest.output.value), expected);
        let active = executor.run_host(agent, "jobs", json!({"limit": 1})).await;
        let active = active.unwrap().output.value;
        assert_eq!(
            (ids(&active), &active["older"]),
            (vec![json!(script)], &json!(1))
        );
    }

    /// A script's whole read of a saved result holds all of it, while a model's read of the
    /// same result is a preview. Reading those reads again, the model's preview stays as it
    /// was, and the script's whole result is previewed as its saved fields are.
    #[tokio::test]
    async fn a_script_reads_a_saved_result_whole() {
        let runtime = TestRuntime::new().await;
        let (agent, jobs) = (&runtime.agent, &runtime.jobs);
        let mut spec = JobSpec {
            background: true,
            ..JobSpec::test(agent.clone(), "exec")
        };
        let ends = json!({"type": "string", skyhook_tools::output::PREVIEW: "ends"});
        spec.output_schema = Some(json!({"type": "object", "properties": {"stdout": ends}}));
        let target = jobs.test_create(spec).await;
        let stdout: String = (1..=5000).map(|line| format!("line {line}\n")).collect();
        let finished = JobOutcome::Completed(ToolOutput::new(json!({"stdout": stdout})).into());
        jobs.finish(target, finished).await.unwrap();
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, jobs.clone()).unwrap();
        let executor = runtime.executor(builder);
        let spec = JobSpec {
            background: true,
            role: JobRole::Script,
            ..JobSpec::test(agent.clone(), "script")
        };
        let script = jobs.create(spec).await.unwrap();
        let read = |job: JobId| json!({"job": job});
        let model = executor
            .run_model(agent, "jobs", read(target))
            .await
            .unwrap();
        let shown = &model.output.value;
        assert!(shown["presentation"]["truncated"].is_array(), "{shown}");
        let kind = crate::tool::catalog::InvocationKind::Script;
        let scripted = executor
            .execute_as(kind, agent.clone(), "jobs", read(target), Some(script.id()))
            .await
            .unwrap();
        assert_eq!(scripted.output.value["result"]["stdout"], stdout);
        assert_eq!(scripted.output.value.get("presentation"), None);
        let again = executor.run_model(agent, "jobs", read(model.job)).await;
        assert_eq!(&again.unwrap().output.value["result"], shown);
        let child = executor.run_model(agent, "jobs", read(scripted.job)).await;
        let child = child.unwrap().output.value;
        let cut = &child["presentation"]["truncated"][0];
        assert_eq!(cut["field"], "/result/result/stdout", "{child}");
    }

    /// An output read acknowledges the outcome it presents with its own finish:
    /// one the journal refuses leaves that outcome pending for its owner.
    #[tokio::test]
    async fn an_output_read_acknowledges_what_it_presents_with_its_finish() {
        let runtime = TestRuntime::new().await;
        let (agent, jobs) = (&runtime.agent, &runtime.jobs);
        let spec = JobSpec {
            background: true,
            ..JobSpec::test(agent.clone(), "done")
        };
        let done = jobs.test_create(spec).await;
        let finished = JobOutcome::Completed(ToolOutput::new(json!("result")).into());
        jobs.finish(done, finished).await.unwrap();
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, jobs.clone()).unwrap();
        let executor = runtime.executor(builder);
        let read = done.get() + 1;
        let rule = format!("BEFORE INSERT ON job_finish WHEN NEW.job = {read}");
        let refused = crate::session::tests::refuse(&runtime.store, &rule);
        let failed = executor
            .run_model(agent, "jobs", json!({"job": done}))
            .await;
        drop(refused);
        assert!(failed.is_err());
        assert!(jobs.has_pending(agent));
    }

    /// A script receives what is sent to it in order, across calls, and one cancelled
    /// while it waits on `receive()` ends cancelled.
    #[tokio::test]
    async fn script_messages_preserve_order_across_calls_and_cancel_waiting_receivers() {
        let runtime = TestRuntime::new().await;
        let (executor, _slot) = tool_executor(runtime.jobs.clone(), runtime.root.path());
        let run = async |source: String, bg: bool| {
            crate::tests::script(&executor, &runtime.agent, source, bg).await
        };
        let value =
            async |source: String| run(source, false).await.output.value["result"]["value"].clone();
        let source = "const messages=[]; for(let i=0;i<4;i++) messages.push(await receive()); return messages;";
        let running = run(source.into(), true).await.job;
        let id = running.get();
        let queued = value(format!(
            r#"
    const accepted = [];
    for (const value of [{{text:"hello", nested:[1,true]}}, 0, false]) {{
      accepted.push((await tool.control({{job:{id}, send:value}})).unwrap());
    }}
    const pending = await tool.jobs({{job:{id}}});
    return {{accepted, state:pending.state}};
    "#
        ))
        .await;
        assert_eq!(queued["accepted"], json!([null, null, null]));
        assert_eq!(queued["state"], "running");
        value(format!("return tool.control({{job:{id}, send:\"last\"}});")).await;
        runtime.jobs.wait(running, true).await.unwrap();
        let completed = value(format!("return tool.jobs({{job:{id}}});")).await;
        assert_eq!(completed["state"], "completed");
        assert_eq!(
            completed["result"]["value"],
            json!([{"text":"hello", "nested":[1,true]}, 0, false, "last"])
        );

        let waiting = run("return await receive();".into(), true).await.job;
        let id = waiting.get();
        let cancel = format!(
            "await tool.jobs({{job:{id}}}); return (await tool.control({{job:{id}, cancel:true}})).unwrap();"
        );
        assert_eq!(value(cancel).await, Value::Null);
        runtime.jobs.wait(waiting, true).await.unwrap();
        let cancelled = value(format!("return tool.jobs({{job:{id}}});")).await;
        assert_eq!(cancelled["state"], "cancelled");
    }
}
