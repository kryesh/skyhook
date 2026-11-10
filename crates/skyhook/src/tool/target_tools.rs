//! The target tools, what each call of them may do, and the machine each runs on:
//! a call's operation runs there and its output is imported into the call's job.
use crate::remote::{Flown, PreparedConnection, RemoteError};
use crate::tool::catalog::{Flight, Invocation, RegistryError, ToolOptions, ToolPlacement};
use crate::tool::policy::{Capability, PermissionUse, ResourceId};
use crate::tool::{ToolContext, ToolError, ToolOutput, ToolRegistryBuilder};
use skyhook_tools::{
    builtins::{Fetch, Glob, Read, RemoveEntry, ReplaceText, RunCommand, Search, WriteFile},
    error::AdmissionError,
    output::{OutputContext, OutputEvent, OutputSink},
    spec::{Call, Spills, TargetTool},
    target::ExecutionLocation,
};
use skyhook_worker::{
    local::Local,
    op::{self, Running},
    protocol::Wire,
    target::Target,
};

/// The machine a call runs on: the session's own, or one a connection reaches.
#[derive(Clone)]
pub(crate) enum ExecutionTarget {
    Local(Local),
    Remote(PreparedConnection),
}

impl ExecutionTarget {
    /// What the machine reports about itself.
    pub(crate) fn facts(&self) -> skyhook_worker::target::Facts {
        match self {
            Self::Local(local) => local.facts(),
            Self::Remote(remote) => remote.facts().clone(),
        }
    }

    /// Each target on the route the machine was reached along, at the revision its
    /// connection was admitted at: none for the session's own.
    pub(crate) fn route(&self) -> crate::target::Revisions {
        match self {
            Self::Local(_) => Vec::new(),
            Self::Remote(remote) => remote.admitted().clone(),
        }
    }
}

impl Target for ExecutionTarget {
    type Unreachable = RemoteError;

    async fn run<O: Wire>(&self, operation: O) -> Result<Running<O, RemoteError>, RemoteError> {
        match self {
            Self::Local(local) => {
                let Ok(running) = local.run(operation).await;
                Ok(running.widen())
            }
            Self::Remote(remote) => remote.run(operation),
        }
    }
}

/// The length of a file on `target`, at `location`, once it opened there, with where
/// in it `read` starts and the bytes it streams from there; the stream ends as the
/// read did.
pub(crate) async fn read_file(
    target: &ExecutionTarget,
    read: op::Read,
    location: &ExecutionLocation,
) -> Result<(u64, u64, impl tokio::io::AsyncRead + Send + Unpin + 'static), ToolError> {
    use skyhook_tools::diagnostic::{FailureSite, PartialContext};
    use skyhook_worker::diagnostic::{Failure, Fault};
    let mut running = target.run(read).await?;
    let Some(op::ReadEvent::Opened { length, offset, .. }) = running.event().await else {
        let site = FailureSite::Execution(location.clone());
        return Err(match running.end().await? {
            Err(failure) => ToolError::from(failure).or(PartialContext::default().at(site)),
            Ok(_) => ToolError::failed("the file's read ended before it opened"),
        });
    };
    let chunks = futures_util::stream::unfold(Some(running), |running| async move {
        let mut running = running?;
        loop {
            match running.event().await {
                Some(op::ReadEvent::Bytes(bytes)) => {
                    return Some((Ok(bytes::Bytes::from(bytes)), Some(running)));
                }
                Some(_) => {}
                None => {
                    let failure = match running.end().await {
                        Ok(Ok(_)) => return None,
                        // Only reading can fail once the file is open.
                        Ok(Err(Failure {
                            fault: Fault::Os(error),
                            ..
                        })) => std::io::Error::from(&error),
                        Ok(Err(_)) => std::io::ErrorKind::InvalidData.into(),
                        Err(error) => std::io::Error::other(error),
                    };
                    return Some((Err(failure), None));
                }
            }
        }
    });
    let bytes = tokio_util::io::StreamReader::new(Box::pin(chunks));
    Ok((length, offset, bytes))
}

/// Register every target tool.
pub(crate) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    filesystem(builder)?;
    search(builder)?;
    exec(builder)?;
    fetch(builder)
}

/// A tool that reads where its `target` selects.
fn reads<A>() -> ToolOptions<A> {
    ToolOptions::new(vec![Capability::Read]).placement(ToolPlacement::TargetedWorkspace)
}

/// A tool that writes where its caller works.
fn writes<A>() -> ToolOptions<A> {
    ToolOptions::new(vec![Capability::Write]).placement(ToolPlacement::InheritWorkspace)
}

pub(crate) fn filesystem(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    register_target::<Read>(builder, reads())?;
    let source = <<WriteFile as TargetTool>::Admitted>::source;
    register_target::<WriteFile>(builder, writes().argument_source(source))?;
    register_target::<ReplaceText>(builder, writes())?;
    register_target::<RemoveEntry>(builder, writes())
}

pub(crate) fn search(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    register_target::<Search>(builder, reads())?;
    register_target::<Glob>(builder, reads())
}

pub(crate) fn exec(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    let options = ToolOptions::new(vec![Capability::Exec])
        .placement(ToolPlacement::TargetedWorkspace)
        .managed_agent()
        .named()
        .background();
    register_target::<RunCommand>(builder, options)
}

pub(crate) fn fetch(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    let options = ToolOptions::new(vec![Capability::Network])
        .argument_permissions(
            |location, plan: &<Fetch as skyhook_tools::spec::TargetTool>::Admitted| {
                let origin = ResourceId::network(&location.target, &plan.url.origin());
                Ok(vec![PermissionUse::new(Capability::Network, origin)])
            },
        )
        .conditional_input("save_to", Capability::Write, Fetch::save_to_schema())
        .placement(ToolPlacement::TargetedWorkspace)
        .background()
        .named();
    register_target::<Fetch>(builder, options)
}

/// Register target tool `S`, admitted under `options`, its remote calls starting
/// their operation in the flight that resolves their paths. A tool without a result
/// schema completes without a result.
fn register_target<S: TargetTool>(
    builder: &mut ToolRegistryBuilder,
    options: ToolOptions<S::Admitted>,
) -> Result<(), RegistryError> {
    let options = options.argument_paths(S::paths);
    let (options, completes) = match S::result_schema() {
        Some(schema) => (options.output_schema(schema), true),
        None => (options, false),
    };
    let fly = move |admitted: &mut _, paths, connection, site| {
        fly::<S>(admitted, paths, connection, site, completes)
    };
    let invoke = move |admitted| invocation::<S>(completes, Prepared::Admitted(admitted));
    let options = options.flight(std::sync::Arc::new(fly));
    builder.register_checked(S::NAME, S::DESCRIPTION, options, S::admit, invoke)?;
    Ok(())
}

/// What a call runs: its admitted input, or its operation, already in flight, with
/// what rendering it keeps.
enum Prepared<S: TargetTool> {
    Admitted(S::Admitted),
    Flown(S::Rendering, Flown<S::Op>),
}

/// The call that runs `prepared`.
fn invocation<S: TargetTool>(completes: bool, prepared: Prepared<S>) -> Invocation {
    let run = move |context| run::<S>(context, prepared);
    match completes {
        true => Invocation::new(run),
        false => Invocation::unit(async move |context| run(context).await.map(drop)),
    }
}

/// Build `admitted`'s operation, once, and start it on `connection` in the flight
/// that resolves its `paths` there; the call then renders from `admitted` as its
/// paths resolved.
fn fly<S: TargetTool>(
    admitted: &mut S::Admitted,
    paths: op::Resolve,
    connection: PreparedConnection,
    (location, spills): (ExecutionLocation, Spills),
    completes: bool,
) -> Flight<S::Admitted> {
    // Off the session host a process runs in its target's own environment, and
    // building the operation records nothing.
    let output = OutputContext::new(std::sync::Arc::new(Unrecorded));
    let (environment, cancellation) = Default::default();
    let call = Call::new(location, environment, cancellation, output, spills);
    let op = (S::op(admitted, &call))
        .map_err(|error| AdmissionError::from_diagnostic(error.diagnostic(), None));
    Box::pin(async move {
        let (landed, flown) = connection.contain(paths, op?).await?;
        let invoke = move |admitted| {
            invocation::<S>(completes, Prepared::Flown(S::rendering(admitted), flown))
        };
        Ok((landed, Box::new(invoke) as _))
    })
}

/// Where building an operation would record output, which it never does.
struct Unrecorded;

impl OutputSink for Unrecorded {
    fn send(&self, _: OutputEvent) -> std::io::Result<()> {
        Ok(())
    }
}

async fn run<S: TargetTool>(
    context: ToolContext,
    prepared: Prepared<S>,
) -> Result<ToolOutput, ToolError> {
    let target = context.target()?.clone();
    let output = crate::job::output::HostOutput::new(context.jobs(), context.job()).await;
    let call = Call::new(
        context.execution_location().clone(),
        context.process_environment.clone(),
        context.cancellation_token(),
        output.context(),
        context.jobs().spills(context.job()),
    )
    .with_source(context.source().cloned());
    let result = async {
        let (rendering, running) = match prepared {
            Prepared::Admitted(mut admitted) => {
                let op = S::op(&mut admitted, &call)?;
                (S::rendering(admitted), target.run(op).await?)
            }
            Prepared::Flown(rendering, flown) => (rendering, flown.running()),
        };
        S::render(rendering, running, &call).await
    }
    .await;
    output.context().settle().await?;
    // What the call's target keeps of its output is on the machine the call ran on,
    // as its own connection reached it.
    let route = target.route();
    match result {
        Ok(value) => Ok(output.finish_on(&route, value).await?),
        Err(error) => {
            let (diagnostic, partial) = error.into_facts();
            let partial = match partial {
                Some(partial) => Some(output.finish_on(&route, partial).await?),
                None => None,
            };
            Err(ToolError::from_facts(diagnostic, partial))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    /// A read's content is imported as the job's own capture: paging it later shows
    /// the file as it was read, not as it is.
    #[tokio::test]
    async fn read_captures_complete_snapshot() {
        let runtime = crate::tests::TestRuntime::new().await;
        let (executor, _slot) =
            crate::tests::tool_executor(runtime.jobs.clone(), runtime.root.path());
        let path = runtime.root.path().join("lines.txt");
        tokio::fs::write(&path, "one\ntwo\nthree\n").await.unwrap();
        let result = executor
            .run_host(&runtime.agent, "read", json!({"path":"lines.txt"}))
            .await
            .unwrap();
        assert_eq!(result.output.value["content"], "one\ntwo\nthree\n");
        tokio::fs::write(&path, "changed").await.unwrap();
        let mut args = crate::job::output::OutputArgs::new(result.job);
        (args.field, args.start, args.limit) = (
            Some("/result/content".parse().unwrap()),
            Some(2),
            Some(crate::job::output::PageLimit::new::<1>()),
        );
        let page = runtime
            .jobs
            .present_output(args, &Default::default())
            .await
            .unwrap();
        assert_eq!(page["presentation"]["preview"]["lines"][0], "two");
    }

    /// The target tools with `jobs`, for a runtime that keeps saved fields past 4 KiB
    /// in files.
    fn tools(runtime: &crate::tests::TestRuntime) -> crate::tool::ToolRegistryBuilder {
        let mut builder = crate::tool::ToolRegistryBuilder::default();
        super::register(&mut builder).unwrap();
        crate::tool::builtins::jobs::register(&mut builder, runtime.jobs.clone()).unwrap();
        builder
    }

    /// An executor of [`tools`], approving with `policy`.
    fn spilling(
        runtime: &crate::tests::TestRuntime,
        policy: std::sync::Arc<dyn crate::tool::policy::Policy>,
    ) -> crate::tool::executor::ToolExecutor {
        runtime.executor_with_policy(tools(runtime), policy)
    }

    /// The one file holding a field of `job`, as host inspection tells of it.
    async fn file(
        runtime: &crate::tests::TestRuntime,
        job: skyhook_worker::job::JobId,
    ) -> crate::job::FieldFile {
        let args = crate::job::output::OutputArgs::new(job);
        let (cancellation, capabilities) = Default::default();
        let inspected =
            (runtime.jobs).inspect_output_with_captures(args, cancellation, &capabilities);
        let files = inspected.await.unwrap().files;
        let [file] = &files[..] else {
            panic!("one field is kept in a file: {files:?}")
        };
        file.clone()
    }

    /// What a model's `jobs` call with `arguments` shows: its preview, or its failure.
    async fn jobs(
        executor: &crate::tool::executor::ToolExecutor,
        agent: &crate::identity::AgentId,
        arguments: Value,
    ) -> Result<Value, String> {
        let read = executor.run_model(agent, "jobs", arguments).await;
        let view = read.map_err(|error| error.to_string())?.output.value;
        match view["error"].as_str() {
            Some(error) => Err(error.to_owned()),
            None => Ok(view["presentation"]["preview"].clone()),
        }
    }

    /// One page of `field` of `job`, from line `start`, as a model's `jobs` call reads it.
    async fn page(
        executor: &crate::tool::executor::ToolExecutor,
        agent: &crate::identity::AgentId,
        (job, field, start): (skyhook_worker::job::JobId, &str, usize),
    ) -> Result<Value, String> {
        let arguments = json!({"job": job, "field": field, "start": start, "limit": 2});
        jobs(executor, agent, arguments).await
    }

    /// `job`'s whole view, as a script's `tool.jobs({job})` holds it.
    async fn held(
        runtime: &crate::tests::TestRuntime,
        executor: &crate::tool::executor::ToolExecutor,
        job: skyhook_worker::job::JobId,
    ) -> Result<Value, String> {
        let agent = &runtime.agent;
        let spec = crate::job::JobSpec {
            background: true,
            role: crate::job::JobRole::Script,
            ..crate::job::JobSpec::test(agent.clone(), "script")
        };
        let script = runtime.jobs.create(spec).await.unwrap();
        let kind = crate::tool::catalog::InvocationKind::Script;
        let arguments = json!({"job": job});
        let read = executor.execute_as(kind, agent.clone(), "jobs", arguments, Some(script.id()));
        let view = read.await.map_err(|error| error.to_string())?.output.value;
        match view["error"].as_str() {
            Some(error) => Err(error.to_owned()),
            None => Ok(view),
        }
    }

    const UNAVAILABLE: &str = "saved output for /result/stdout is no longer available";

    /// A command's output past the limit stays on the target that ran it, and reads
    /// as any field does. The session keeps the first bytes it was sent and the last
    /// the target reported, so the preview shows both ends and the line count as
    /// ever; pages, patterns, queries and a script's whole view read the rest from
    /// the target's file, which host inspection tells the session's user of. Once
    /// that file no longer holds the stream, reading it fails as unavailable.
    #[tokio::test]
    async fn exec_output_past_the_limit_reads_as_any_field_from_its_target() {
        let runtime = crate::tests::TestRuntime::new().await.spilling_past(4096);
        let executor = spilling(&runtime, std::sync::Arc::new(crate::tool::policy::AllowAll));
        let agent = &runtime.agent;
        let command = json!({"command": "seq 1 2000; echo warning >&2"});
        let ran = executor.run_model(agent, "exec", command).await.unwrap();
        let view = &ran.output.value;
        let stdout = view["result"]["stdout"].as_str().unwrap();
        assert!(
            stdout.starts_with("1\n2\n") && stdout.ends_with("1999\n2000\n"),
            "{view}"
        );
        assert_eq!(view["result"]["stderr"], "warning\n");
        let cut = &view["presentation"]["truncated"][0];
        assert_eq!(
            (&cut["field"], &cut["total_lines"]),
            (&json!("/result/stdout"), &json!(2000))
        );
        assert!(stdout.contains(&format!("lines {}–", cut["next_start"])));
        // The session's user is told the file that holds the stream.
        let whole: String = (1..=2000).map(|line| format!("{line}\n")).collect();
        let kept = file(&runtime, ran.job).await;
        let spills = runtime.root.path().join("spills");
        assert!(kept.path.as_str().starts_with(spills.to_str().unwrap()));
        assert_eq!(std::fs::read_to_string(kept.path.as_str()).unwrap(), whole);
        assert_eq!(kept.bytes, whole.len() as u64);
        // The session holds the first bytes alone; the rest comes from the file.
        let output = runtime.jobs.output(ran.job).await;
        let head = output.test_bytes("/result/stdout").unwrap();
        assert_eq!(head, whole.as_bytes()[..4096]);
        let middle = page(&executor, agent, (ran.job, "/result/stdout", 1500))
            .await
            .unwrap();
        assert_eq!(middle["lines"], json!(["1500", "1501"]));
        assert_eq!(
            (&middle["total_lines"], &middle["next_start"]),
            (&json!(2000), &json!(1502))
        );
        let found = json!({"job": ran.job, "field": "/result/stdout", "pattern": "^1999$",
            "context": 1});
        let found = jobs(&executor, agent, found).await.unwrap();
        let numbered =
            |lines: [usize; 3]| lines.map(|line| json!({"line": line, "text": line.to_string()}));
        assert_eq!(found["lines"], json!(numbered([1998, 1999, 2000])));

        // Filters over the result read the field whole: a match past the first
        // bytes is found, and a selected value is all of it.
        let read = async |arguments: Value| jobs(&executor, agent, arguments).await.unwrap();
        let query = async |path: &str| read(json!({"job": ran.job, "query": path})).await;
        assert_eq!(query("$.exit.code").await["matches"][0]["value"], 0);
        let late = query("$[?search(@, '1999')]").await;
        assert_eq!(
            (&late["total_matches"], &late["matches"][0]["at"]),
            (&json!(1), &json!("/result/stdout"))
        );
        assert_eq!(query("$.stdout").await["matches"][0]["value"], whole);
        let matched = read(json!({"job": ran.job, "field": "/result", "pattern": "1999"})).await;
        assert_eq!(matched["matches"][0]["at"], "/result/stdout");
        // A page of the result's members samples the field from what the session holds.
        let members = json!({"job": ran.job, "field": "/result", "start": 2, "unit": "members"});
        let members = read(members).await;
        let sampled = members["members"]["stdout"].as_str().unwrap();
        assert!(sampled.starts_with("1\n2\n") && sampled.len() < whole.len());
        assert_eq!(members["truncated"][0]["total_lines"], 2000);
        let script = held(&runtime, &executor, ran.job).await.unwrap();
        assert_eq!(script["result"]["stdout"], whole);

        // The file is read as far as its target counted as it wrote it. Once it
        // holds less, or is gone, the preview still shows what the session holds and
        // every read of the rest fails alike.
        std::fs::write(kept.path.as_str(), format!("{whole}extra\n")).unwrap();
        let last = page(&executor, agent, (ran.job, "/result/stdout", 2000));
        let last = last.await.unwrap();
        assert_eq!(
            (&last["lines"], last.get("next_start")),
            (&json!(["2000"]), None)
        );
        for lose in [
            |file: &str| std::fs::write(file, "short\n").unwrap(),
            |file: &str| std::fs::remove_file(file).unwrap(),
        ] {
            lose(kept.path.as_str());
            let paged = page(&executor, agent, (ran.job, "/result/stdout", 1500)).await;
            let queried = jobs(
                &executor,
                agent,
                json!({"job": ran.job, "query": "$.stdout"}),
            );
            let scripted = held(&runtime, &executor, ran.job).await;
            for failed in [
                paged,
                queried.await,
                scripted.map(|view| view["result"].clone()),
            ] {
                let failed = failed.unwrap_err();
                assert!(failed.ends_with(UNAVAILABLE), "{failed}");
            }
            let shown = jobs(&executor, agent, json!({"job": ran.job})).await;
            assert_eq!(shown, Ok(Value::Null));
        }
    }

    /// A script that polls a background command until it ends holds its whole output,
    /// whichever of its reads finds the command ended and its stream in a file.
    #[tokio::test]
    async fn a_script_polling_a_background_command_holds_its_whole_output() {
        let runtime = crate::tests::TestRuntime::new().await.spilling_past(4096);
        let mut builder = tools(&runtime);
        let slot = std::sync::Arc::new(std::sync::OnceLock::new());
        let script = std::sync::Arc::downgrade(&slot);
        crate::tool::builtins::install_script_tool(&mut builder, script).unwrap();
        let executor = runtime.executor(builder);
        assert!(slot.set(executor.clone()).is_ok());
        let source = r#"
const command = "while [ ! -e release ]; do sleep 0.01; done; seq 1 2000";
const job = await tool.exec({command, bg: true});
let status = await tool.jobs({job: job.id});
const first = status.state;
(await tool.write({path: "release", content: ""})).unwrap();
while (["queued", "running", "waiting_input"].includes(status.state)) {
  status = await tool.jobs({job: job.id});
}
const lines = status.result.stdout.split("\n");
return {first, state: status.state, lines: lines.length - 1, last: lines[1999]};
"#;
        let polled = crate::tests::script(&executor, &runtime.agent, source.into(), false);
        let polled = crate::tests::bounded(polled).await.output.value;
        let expected = json!({"first": "running", "state": "completed", "lines": 2000,
            "last": "2000"});
        assert_eq!(polled["result"]["value"], expected, "{polled}");
    }

    /// A text file past the limit is not copied: the session records its path, what
    /// its target counted of it and the first bytes it showed, and each later page
    /// reads the file as it is then, changed or shorter. What was shown stays as it
    /// was shown, and a file that is gone is saved output no longer available.
    #[tokio::test]
    async fn a_file_past_the_limit_is_read_where_it_is_each_time_it_is_paged() {
        let runtime = crate::tests::TestRuntime::new().await.spilling_past(4096);
        let executor = spilling(&runtime, std::sync::Arc::new(crate::tool::policy::AllowAll));
        let agent = &runtime.agent;
        let path = runtime.root.path().join("big.log");
        let lines = |word: &str, count| -> String {
            (1..=count).map(|line| format!("{word} {line}\n")).collect()
        };
        let original = lines("line", 12_000);
        std::fs::write(&path, &original).unwrap();
        let read = executor.run_model(agent, "read", json!({"path": "big.log"}));
        let read = read.await.unwrap();
        let view = &read.output.value;
        let shown = view["result"]["content"].as_str().unwrap();
        assert!(shown.starts_with("line 1\nline 2\n"), "{view}");
        let cut = &view["presentation"]["truncated"][0];
        assert_eq!(
            (&cut["next_start"], &cut["total_lines"]),
            (&json!(201), &json!(12_000))
        );
        let kept = file(&runtime, read.job).await;
        assert_eq!(kept.path.as_str(), path.to_str().unwrap());
        assert_eq!(kept.bytes, original.len() as u64);
        let output = runtime.jobs.output(read.job).await;
        let head = output.test_bytes("/result/content").unwrap();
        assert!(head.len() < original.len() / 2 && original.as_bytes().starts_with(&head));

        let field = (read.job, "/result/content", 11_000);
        let later = page(&executor, agent, field).await.unwrap();
        assert_eq!(later["lines"], json!(["line 11000", "line 11001"]));
        assert_eq!(later["total_lines"], 12_000);
        std::fs::write(&path, lines("changed", 12_000)).unwrap();
        let changed = page(&executor, agent, field).await.unwrap();
        assert_eq!(changed["lines"], json!(["changed 11000", "changed 11001"]));
        std::fs::write(&path, lines("short", 10)).unwrap();
        let shorter = page(&executor, agent, field).await.unwrap();
        assert_eq!(
            (&shorter["lines"], shorter.get("next_start")),
            (&json!([]), None)
        );
        std::fs::remove_file(&path).unwrap();
        let gone = page(&executor, agent, field).await.unwrap_err();
        assert!(
            gone.ends_with("saved output for /result/content is no longer available"),
            "{gone}"
        );
        let whole = json!({"job": read.job});
        let again = executor.run_model(agent, "jobs", whole).await.unwrap();
        assert_eq!(again.output.value["result"]["content"], shown);
    }

    /// A session resumed with no grants reads back the output it had kept in a file,
    /// and reads a file a `read` referenced as a read of it: it asks for that path.
    /// A script that calls a tool holds that call's fields whole.
    #[tokio::test]
    async fn a_resumed_session_reads_its_saved_output_and_asks_for_a_referenced_file() {
        use crate::tests::RecordingPolicy;
        use crate::tool::policy::{PermissionUse, PolicyDecision, ResourceId};
        let runtime = crate::tests::TestRuntime::new().await.spilling_past(4096);
        let granting = || {
            RecordingPolicy::deciding(|request| PolicyDecision::Allow {
                grants: (request.permissions.iter())
                    .filter_map(PermissionUse::proposed_grant)
                    .collect(),
            })
        };
        let agent = &runtime.agent;
        let executor = spilling(&runtime, granting());
        let ran = executor.run_model(agent, "exec", json!({"command": "seq 1 2000"}));
        let ran = ran.await.unwrap();
        // A file outside the workspace is asked for by itself.
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("big.log");
        let text: String = (1..=12_000).map(|line| format!("line {line}\n")).collect();
        std::fs::write(&path, &text).unwrap();
        let arguments = json!({"path": path});
        let read = executor.run_model(agent, "read", arguments.clone());
        let read = read.await.unwrap();

        // Grants last as long as their coordinator: a resumed session has none.
        let policy = granting();
        let resumed = spilling(&runtime, policy.clone());
        let stream = page(&resumed, agent, (ran.job, "/result/stdout", 1500));
        assert_eq!(stream.await.unwrap()["lines"], json!(["1500", "1501"]));
        let field = (read.job, "/result/content", 11_000);
        let later = page(&resumed, agent, field).await.unwrap();
        assert_eq!(later["lines"], json!(["line 11000", "line 11001"]));
        let requests = std::mem::take(&mut *policy.requests.lock().unwrap());
        let [request] = &requests[..] else {
            panic!("the page of the file asked once: {requests:?}")
        };
        assert_eq!(request.tool, "jobs");
        let file = ResourceId::path(&"root".parse().unwrap(), &path);
        assert!(
            request
                .permissions
                .iter()
                .any(|asked| asked.resource == file)
        );
        let whole = held(&runtime, &resumed, read.job).await.unwrap();
        assert_eq!(whole["result"]["content"], text);

        let spec = crate::job::JobSpec {
            background: true,
            role: crate::job::JobRole::Script,
            ..crate::job::JobSpec::test(agent.clone(), "script")
        };
        let script = runtime.jobs.create(spec).await.unwrap();
        let kind = crate::tool::catalog::InvocationKind::Script;
        let scripted =
            resumed.execute_as(kind, agent.clone(), "read", arguments, Some(script.id()));
        let scripted = scripted.await.unwrap();
        assert_eq!(scripted.output.value["result"]["content"], text);
    }

    /// A response body kept in a file is named to its caller, and is the session's own
    /// to read from then on: with no grant held, as the session stays open and once
    /// it is resumed.
    #[tokio::test]
    async fn a_fetch_body_kept_in_a_file_is_the_sessions_own_to_read() {
        use crate::tool::policy::PolicyDecision;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut runtime = crate::tests::TestRuntime::on_disk().await;
        let outside = tempfile::tempdir().unwrap();
        let spill = crate::job::output::Spill {
            root: Some(outside.path().join("spills")),
            bytes: 4096,
        };
        runtime.jobs = crate::job::JobManager::spilling(runtime.store.clone(), spill.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let body = "body\n".repeat(2000);
        let reply = format!(
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let serving = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            while !request.windows(4).any(|end| end == b"\r\n\r\n") {
                assert_ne!(socket.read(&mut request).await.unwrap(), 0);
            }
            socket.write_all(reply.as_bytes()).await.unwrap();
        });
        // A read of a path outside the workspace that the policy is asked about is
        // refused.
        let policy = || {
            crate::tests::RecordingPolicy::deciding(|request| match &*request.tool {
                "read" => PolicyDecision::Deny {
                    reason: "asked".into(),
                },
                _ => PolicyDecision::allow(),
            })
        };
        let executor = spilling(&runtime, policy());
        let agent = runtime.agent.clone();
        let fetched = executor.run_model(&agent, "fetch", json!({"url": url}));
        let fetched = crate::tests::bounded(fetched).await.unwrap().output.value;
        crate::tests::bounded(serving).await.unwrap();
        let kept = &fetched["result"]["body"];
        assert_eq!(
            (&kept["kind"], &kept["bytes"]),
            (&json!("file"), &json!(body.len()))
        );
        let unasked = async |executor: &crate::tool::executor::ToolExecutor| {
            let read = executor.run_model(&agent, "read", json!({"path": kept["path"]}));
            let view = read.await.unwrap().output.value;
            let content = view["result"]["content"].as_str().unwrap();
            assert!(content.starts_with("body\n"), "{view}");
        };
        unasked(&executor).await;
        runtime.store.close().await.unwrap();
        let sessions = runtime.root.path().join("sessions");
        runtime.store = crate::session::tests::reopen(&sessions, runtime.store.id()).await;
        runtime.jobs = crate::job::JobManager::spilling(runtime.store.clone(), spill);
        unasked(&spilling(&runtime, policy())).await;
    }

    /// A download names its file and whether to replace it together, where the
    /// caller may write.
    #[test]
    fn fetch_saves_to_a_file_only_where_the_caller_may_write() {
        use crate::identity::{AgentId, SessionId};
        use crate::tool::policy::{Capability, CapabilitySet};
        let mut builder = crate::tool::ToolRegistryBuilder::default();
        super::fetch(&mut builder).unwrap();
        let registry = builder.build();
        let root = AgentId::root(SessionId::from_bytes([1; 16]));
        let admits = |capabilities: &CapabilitySet, save_to: Value| {
            let surface = registry.surface_for_agent(capabilities, &root);
            let arguments = json!({"url": "https://example.org", "save_to": save_to});
            let kind = crate::tool::catalog::InvocationKind::Host;
            surface.get("fetch").unwrap().admit(kind, arguments).is_ok()
        };
        let mut capabilities: CapabilitySet = Capability::ALL.into_iter().collect();
        assert!(admits(&capabilities, json!({"path": "out"})));
        assert!(admits(
            &capabilities,
            json!({"path": "out", "overwrite": true})
        ));
        capabilities.remove(&Capability::Write);
        assert!(!admits(&capabilities, json!({"path": "out"})));
    }

    /// Whether another description could take a write lock on SQLite's first-opener
    /// byte of the `-shm` behind `probe`, which every live connection holds shared. The
    /// probe stays open across checks: closing it would release those locks itself.
    fn shm_locked(probe: &std::fs::File) -> bool {
        use std::os::fd::AsRawFd as _;
        // SAFETY: an all-zero flock is a valid value of the C struct.
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = libc::F_WRLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        lock.l_start = 128;
        lock.l_len = 1;
        // SAFETY: the descriptor is open and `lock` outlives the call.
        assert_eq!(
            unsafe { libc::fcntl(probe.as_raw_fd(), libc::F_OFD_GETLK, &mut lock) },
            0
        );
        i32::from(lock.l_type) != libc::F_UNLCK
    }

    /// Opening and closing a live database's file would release SQLite's locks, which
    /// another process could then take as the database being unused: the tools' walks
    /// skip its files and a search of one is refused, both without opening them.
    #[tokio::test]
    async fn searches_skip_live_databases_and_leave_their_locks() {
        let runtime = crate::tests::TestRuntime::new().await;
        let root = runtime.root.path();
        std::fs::create_dir(root.join(".skyhook")).unwrap();
        std::fs::write(root.join(".skyhook/notes"), "needle\n").unwrap();
        let db =
            crate::db::Db::connect(&root.join(".skyhook/live.db"), crate::db::OpenMode::Create);
        let db = db.unwrap();
        db.query_row("PRAGMA journal_mode = WAL", Vec::new(), |_| Ok(()))
            .unwrap();
        db.batch("CREATE TABLE t(x TEXT); INSERT INTO t VALUES ('needle');")
            .unwrap();
        let shm = root.join(".skyhook/live.db-shm");
        let probe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&shm)
            .unwrap();
        assert!(shm_locked(&probe));
        let mut builder = crate::tool::ToolRegistryBuilder::default();
        super::search(&mut builder).unwrap();
        let executor = runtime.executor(builder);
        let search =
            async |arguments: Value| executor.run_host(&runtime.agent, "search", arguments).await;
        // A temporary directory inside a checkout inherits its ignores, which may cover `.skyhook/`.
        let arguments = json!({"pattern":"needle","hidden":true,"no_ignore":true});
        let found = search(arguments).await.unwrap().output.value;
        let found: Vec<_> = found["matches"].as_object().unwrap().keys().collect();
        assert_eq!(found, [".skyhook/notes"]);
        let refused = search(json!({"path": ".skyhook/live.db-shm", "pattern":"needle"})).await;
        assert!(refused.unwrap_err().to_string().contains("holds open"));
        assert!(shm_locked(&probe));
    }
}
