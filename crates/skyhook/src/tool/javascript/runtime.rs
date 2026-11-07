//! Script execution and the host side of its tool calls.

use std::{io::Write as _, sync::Arc};

use futures_util::FutureExt;
use serde_json::Value;
use tokio::sync::Mutex;

use super::{
    bridge::{Config, Finished, HostRequest, HostResponse, Presented, Returned},
    engine::MemoryLimits,
    errors::{JsError, javascript_error},
    isolate::{self, Host, Settlement},
    source::{FILENAME, MAX_SOURCE_BYTES, SubmittedSource},
};

use serde::{Deserialize, de::IgnoredAny};

use crate::{
    identity::JobId,
    job::{
        FieldPointer, Issuing, JOB_VIEW_SCHEMAS, JobManager,
        output::{CompletedCapture, HostOutput, fill_absent, presented_fields},
    },
    media::ImageRef,
    tool::diagnostic::{Effects, FailureSite, Operation, Subject},
    tool::executor::{InvocationKind, ToolExecutor, failure_response},
    tool::output::{OutputWriter, ProducedOutput, TextCaptureField},
    tool::{ToolContext, ToolError, ToolOutput, ToolResultPolicy},
};

type Console = Arc<std::sync::Mutex<OutputWriter>>;

/// The console capture, which buffers lines until the script waits or it closes.
struct ScriptConsole(Console);

impl isolate::Console for ScriptConsole {
    fn log(&mut self, line: &str) -> Result<(), String> {
        let mut console = self.0.lock().expect("console lock poisoned");
        // Buffered: a full chunk is written out at once, the rest when the script waits.
        let written = writeln!(console, "{line}");
        written.map_err(|failure| {
            let failure = failure.into();
            console_failure(failure, Operation::WriteCapture, Effects::OutputIncomplete).to_string()
        })
    }

    fn flush(&mut self) {
        let _ = self.0.lock().expect("console lock poisoned").flush();
    }
}

/// Execute into the owning job's capture; native callers hydrate only when collecting the job.
pub(crate) async fn evaluate_captured(
    source: String,
    executor: ToolExecutor,
    context: ToolContext,
    limits: MemoryLimits,
) -> Result<ToolOutput, ToolError> {
    let output = HostOutput::new(context.jobs(), context.job()).await;
    let capture = (output.context().text_capture(TextCaptureField::Console)).await;
    let capture = capture.map_err(|error| {
        console_failure(error.into(), Operation::CreateCapture, Effects::NotStarted)
            .with_result(super::result::script_output(Value::Null, None, None))
    })?;
    let console = Arc::new(std::sync::Mutex::new(capture.open()));
    let result = evaluate_inner(source, executor, context, console.clone(), limits).await;
    let finished = close(console, &output).await;
    finish_evaluation(result, finished)
}

/// Closes the console, a capture only when written to, once the script lets go of it.
async fn close(console: Console, output: &HostOutput) -> std::io::Result<Option<CompletedCapture>> {
    let finished = Arc::into_inner(console)
        .expect("the script released its console")
        .into_inner()
        .expect("console lock poisoned")
        .close_nonempty()?;
    output.context().settle().await?;
    let produced = ProducedOutput::default().with_captures(finished.into_iter().collect());
    Ok(output.finish(produced)?.captures.pop())
}

fn finish_evaluation(
    result: Result<ToolOutput, ToolError>,
    console: std::io::Result<Option<CompletedCapture>>,
) -> Result<ToolOutput, ToolError> {
    match (result, console) {
        (Ok(mut output), Ok(console)) => {
            let result = super::result::script_output(output.value, None, console);
            output.value = result.value;
            output.captures.extend(result.captures);
            Ok(output)
        }
        (Err(error), Ok(console)) => {
            let (diagnostic, mut output) = error.into_facts();
            if let Some(output) = &mut output {
                output.captures.extend(console);
            }
            Err(ToolError::from_facts(diagnostic, output))
        }
        (Ok(output), Err(error)) => Err(console_failure(
            error.into(),
            Operation::FinishCapture,
            Effects::OutputIncomplete,
        )
        .with_result(
            super::result::script_output(output.value, None, None)
                .with_images(output.images)
                .with_presented(output.presented),
        )),
        (Err(error), Err(_)) => {
            // Keep the primary outcome and summary, recording the incomplete output.
            let (mut diagnostic, output) = error.into_facts();
            diagnostic.context = diagnostic.context.effects(Effects::OutputIncomplete);
            Err(ToolError::from_facts(diagnostic, output))
        }
    }
}

/// Rendered into script-visible text as well as returned, so the site is explicit.
fn console_failure(error: ToolError, operation: Operation, effects: Effects) -> ToolError {
    error
        .operation(operation, Subject::Label("script console".into()))
        .at(FailureSite::Host)
        .effects(effects)
}

async fn evaluate_inner(
    source: String,
    executor: ToolExecutor,
    context: ToolContext,
    console: Console,
    limits: MemoryLimits,
) -> Result<ToolOutput, ToolError> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(javascript_error(JsError::SourceTooLarge));
    }
    let surface = executor.surface_for_agent(context.agent());
    let source = SubmittedSource::new(&source);
    let config = Config {
        builders: surface.script_manifests(),
        terminal: (crate::job::JobState::ALL.into_iter())
            .filter(|state| state.is_terminal())
            .collect(),
        filename: FILENAME,
    };
    let config = serde_json::to_string(&config)
        .map_err(|error| javascript_error(JsError::Initialization(error.to_string())))?;
    let images = Arc::new(Mutex::new(Vec::<ImageRef>::new()));
    let request = {
        let (executor, context, images) = (executor.clone(), context.clone(), images.clone());
        move |request| {
            // Counted at the JS call, so calls issued together are all published
            // before any of them can see its siblings.
            let issuing = Issuing::new(executor.jobs(), context.job());
            let (executor, context, images) = (executor.clone(), context.clone(), images.clone());
            self::request(request, issuing, executor, context, images).boxed()
        }
    };
    let stop = context.cancellation_token().child_token();
    let host = Host::new(request, ScriptConsole(console), limits, stop);
    let (result, mut calls) = isolate::run(source, config, host).await;
    if context.is_cancelled() {
        // Cancellation terminates JavaScript and ends the calls in flight. The
        // script may settle first, or await what can no longer settle; either way
        // its calls are driven to their end, so none outlives it.
        while calls.join_next().await.is_some() {}
        return Err(javascript_error(JsError::Cancelled));
    }
    // Calls the script left unawaited end with it.
    drop(calls);
    let Finished { returned, failure } = result.map_err(javascript_error)?;
    if let Some(failure) = failure {
        let returned = Box::new(returned);
        return Err(javascript_error(JsError::Failure { failure, returned }));
    }
    let Returned { value, presented } = returned;
    let mut images = std::mem::take(&mut *images.lock().await);
    images.sort();
    images.dedup();
    Ok(ToolOutput::new(value)
        .with_images(images)
        .with_presented(presented.into_iter().collect()))
}

/// A tool call or receive the script requested, answered as the JSON it reads.
async fn request(
    request: String,
    issuing: Issuing,
    executor: ToolExecutor,
    context: ToolContext,
    images: Arc<Mutex<Vec<ImageRef>>>,
) -> Settlement {
    let request: HostRequest = match serde_json::from_str(&request) {
        Ok(request) => request,
        Err(error) => return Settlement::Failed(error.to_string()),
    };
    let response = match request {
        HostRequest::Call { name, arguments } => {
            let parent = Some(context.job());
            // Failures before and after the job exists are ordinary
            // tool responses rather than JS exceptions.
            let failed = |error| failure_response(error, context.diagnostic_viewer());
            // Native schemas describe only envelope.result.
            let (mut policy, mut job) = (ToolResultPolicy::Value, None);
            let created = executor
                .create(
                    InvocationKind::Script,
                    context.agent().clone(),
                    &name,
                    Value::Object(arguments),
                    parent,
                )
                .await;
            drop(issuing);
            let mut output = match created {
                Ok(created) => {
                    (policy, job) = (created.result_policy(), Some(created.job()));
                    let reported = executor.run_reported(created, parent).await;
                    context.read(reported.acknowledges).await;
                    match reported.outcome {
                        Ok(result) => result.output,
                        Err(error) => failed(error),
                    }
                }
                Err(error) => failed(error),
            };
            images
                .lock()
                .await
                .extend(std::mem::take(&mut output.images));
            let jobs = executor.jobs();
            let result = match (policy, job) {
                (ToolResultPolicy::JobView, _) => inspected_schema(jobs, &output.value).await,
                (_, Some(job)) => jobs.output_schema(job).await.ok().flatten(),
                (_, None) => None,
            };
            script_view(output, result.as_deref())
        }
        HostRequest::Receive => {
            drop(issuing);
            let result = match executor.jobs().is_background(context.job()).await {
                Ok(true) => context.receive().await.map_err(|error| error.to_string()),
                Ok(false) => {
                    Err("receive() requires the script tool to be invoked with bg: true".to_owned())
                }
                Err(error) => Err(error.to_string()),
            };
            match result {
                Ok(value) => HostResponse::Ok {
                    value,
                    presented: Presented::new(),
                },
                Err(error) => HostResponse::Failed { error },
            }
        }
    };
    match serde_json::to_string(&response) {
        Ok(response) => Settlement::Response(response),
        Err(error) => Settlement::Failed(error.to_string()),
    }
}

/// A response as scripts see it: its own fields presented as its schema declares,
/// the rest as its output presents them, and each member its schemas declare but
/// it lacks as `null`. `result` is the schema its result follows, when known.
fn script_view(output: ToolOutput, result: Option<&Value>) -> HostResponse {
    let view = &JOB_VIEW_SCHEMAS.view;
    let (mut value, mut presented) = (output.value, output.presented);
    presented.extend(presented_fields(&value, &FieldPointer::root(), view));
    if let (Some(schema), Some(result)) = (result, value.get_mut("result")) {
        fill_absent(result, schema);
    }
    fill_absent(&mut value, view);
    let presented = presented
        .fields()
        .map(|(field, shown)| (field.clone(), shown));
    HostResponse::Ok {
        value,
        presented: presented.collect(),
    }
}

/// The schema an inspected job's result follows, when the view shows it whole: a
/// preview or page may stub members, so their absence there is unknown.
async fn inspected_schema(jobs: &JobManager, view: &Value) -> Option<Arc<Value>> {
    #[derive(Deserialize)]
    struct Inspected {
        id: JobId,
        presentation: Option<Shown>,
    }
    #[derive(Deserialize)]
    struct Shown {
        preview: Option<IgnoredAny>,
        truncated: Option<IgnoredAny>,
    }
    let view = Inspected::deserialize(view).ok()?;
    let whole = view
        .presentation
        .is_none_or(|shown| shown.preview.is_none() && shown.truncated.is_none());
    jobs.output_schema(view.id)
        .await
        .ok()
        .flatten()
        .filter(|_| whole)
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use crate::tests::TestRuntime;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::{Value, json};

    use super::*;
    use crate::job::JobState;
    use crate::tests::{assert_loaded, bounded, image_runtime, script};
    use crate::tool::javascript::bridge::Failure;
    use crate::tool::javascript::tests::{
        Echo, LIMITS, echo_runtime, evaluate, evaluate_wrapper, evaluate_wrapper_envelope, plain,
        test_runtime,
    };
    use crate::tool::{
        ToolOptions, ToolRegistryBuilder,
        diagnostic::{Cause, IoKind},
    };

    #[tokio::test]
    async fn local_source_and_sleep_limits_fail_before_user_effects() {
        let oversized = " ".repeat(MAX_SOURCE_BYTES + 1);
        let diagnostic = plain(&oversized).await.unwrap_err().diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Validate);
        assert_eq!(diagnostic.context.effects, Effects::NotStarted);
        let output = plain(
            r#"
            const errors = [];
            for (const milliseconds of [-1, NaN, Infinity, 1800001, "10"]) {
                try { await sleep(milliseconds); throw new Error("invalid sleep admitted"); }
                catch (error) { errors.push(error instanceof TypeError && error.message.startsWith("sleep(ms)")); }
            }
            await sleep(0);
            return errors;
        "#,
        )
        .await
        .unwrap();
        assert_eq!(output.value["value"], json!([true, true, true, true, true]));
    }

    #[tokio::test]
    async fn lazy_builders_execute_once_and_independent_calls_overlap() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Barrier::new(2));
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Echo, String, _, _>("echo", "echo", ToolOptions::default(), {
                let calls = calls.clone();
                move |_context, input| {
                    let gate = gate.clone();
                    calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        gate.wait().await;
                        Ok(input.value)
                    }
                }
            })
            .unwrap();
        let (_scope, executor, context) = test_runtime(builder).await;
        // Two equivalent builders must execute independently; reusing one must not execute again.
        let source = "const x=tool.echo({value:'a'}); return [x,x,tool.echo({value:'a'})];";
        // Each call waits for the other, so only overlapping calls finish.
        let output = bounded(evaluate(source, executor, context)).await.unwrap();
        let responses = output.value["value"].as_array().unwrap();
        assert_eq!(responses.len(), 3);
        for response in responses {
            assert_eq!(response["result"], "a");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// A returned response, or the result taken from one, keeps the presentations
    /// its tool's result schema declares.
    #[tokio::test]
    async fn returned_tool_results_keep_their_declared_presentations() {
        #[derive(serde::Serialize, JsonSchema)]
        struct Streams {
            #[schemars(transform = crate::tool::output::ends)]
            stdout: String,
            #[schemars(transform = crate::tool::output::complete)]
            note: String,
        }
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Echo, Streams, _, _>(
                "streams",
                "streams",
                ToolOptions::default(),
                |_, input| async move {
                    Ok(Streams {
                        stdout: input.value,
                        note: String::new(),
                    })
                },
            )
            .unwrap();
        let (_scope, executor, context) = test_runtime(builder).await;
        let source = "const full = await tool.streams({value:'x'});
const extracted = (await tool.streams({value:'y'})).result;
return {full, extracted};";
        let presented = evaluate(source, executor, context).await.unwrap().presented;
        let fields = [
            "/result/value/extracted/stdout",
            "/result/value/full/result/stdout",
        ];
        assert!(
            presented
                .ends()
                .iter()
                .map(|field| field.as_str())
                .eq(fields)
        );
        for field in [
            "/result/value/extracted/note",
            "/result/value/full/result/note",
        ] {
            let field: crate::job::FieldPointer = field.parse().unwrap();
            assert!(presented.complete().contains(&field), "{field:?}");
        }
    }

    /// The script tool is hidden and rejected inside scripts, and denied
    /// operations keep their metadata in exceptions and uncaught details.
    #[tokio::test]
    async fn hidden_script_tool_and_denials_surface_structured_failures() {
        #[derive(Deserialize, JsonSchema)]
        struct NestedScript {
            source: String,
        }
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<NestedScript, Value, _, _>(
                "script",
                "script",
                ToolOptions::default().script_unavailable(),
                |_context, input| async move { Ok(json!({"source": input.source})) },
            )
            .unwrap()
            .register::<Echo, String, _, _>(
                "deny",
                "test denial",
                ToolOptions::default(),
                |_, _| async { Err(crate::tool::ToolError::denied("user reason")) },
            )
            .unwrap();
        let (_scope, executor, context) = test_runtime(builder).await;
        let unavailable = executor
            .create(
                InvocationKind::Script,
                context.agent().clone(),
                "script",
                json!({"source": "return null;"}),
                Some(context.job()),
            )
            .await
            .err()
            .expect("script is unavailable even to direct bridge calls");
        let unavailable = failure_response(unavailable, context.diagnostic_viewer()).value;
        assert_eq!(unavailable["state"], "failed");
        assert!(unavailable.get("id").is_none());
        assert!(unavailable.get("meta").is_none());
        assert!(
            unavailable["error"]
                .as_str()
                .unwrap()
                .contains("not available in scripts")
        );
        let source = r#"
const deniedResponse = await tool.deny({value:"x"});
const imported = await import("skyhook:tools").then(() => true, () => false);
return {visible: typeof tool.script, imported, deniedResponse};
"#;
        let output = evaluate(source, executor.clone(), context.clone())
            .await
            .unwrap();
        let value = &output.value["value"];
        assert_eq!(value["visible"], "undefined");
        assert_eq!(value["imported"], false);
        assert_eq!(value["deniedResponse"]["state"], "failed");
        // A returned response keeps its error whole in the script's presentation.
        let error: crate::job::FieldPointer = "/result/value/deniedResponse/error".parse().unwrap();
        assert!(output.presented.complete().contains(&error));
        assert!(
            value["deniedResponse"]["error"]
                .as_str()
                .unwrap()
                .contains("user reason")
        );
        let error = evaluate(
            "(await tool.deny({value:'x'})).unwrap();",
            executor,
            context,
        )
        .await
        .unwrap_err();
        let (diagnostic, Some(output)) = error.into_parts() else {
            panic!("expected nested failure details")
        };
        assert_matches!(diagnostic.cause, Cause::Message(_));
        // The failed response is referenced by its job, not embedded again.
        let failure = &output.value["failure"];
        assert_eq!(failure["code"], "permission_denied");
        assert_eq!(
            failure.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["job", "code"]
        );
    }

    #[tokio::test]
    async fn script_failures_report_submitted_source_positions_only() {
        for (source, expected) in [
            (
                "throw new TypeError('boom');",
                "TypeError: boom at line 1:7",
            ),
            (
                "\n\nfunction fail() {\n  throw new Error('boom');\n}\nfail();",
                "boom at fail (line 4:9) at line 6:1",
            ),
            // A failing getter does not replace the error that was thrown.
            (
                "const error = new Error('primary'); Object.defineProperty(error, 'hostile', \
                 {enumerable:true, get() {throw new Error('secondary');}}); throw error;",
                "primary at line 1:15",
            ),
            // Continuations after an await keep their callers' frames.
            (
                "async function inner() {\n  await sleep(0);\n  throw new Error('deep');\n}\n\
                 async function outer() {\n  await inner();\n}\nawait outer();",
                "deep at inner (line 3:9) at async outer (line 6:3) \
                 at async line 8:1",
            ),
            // Runtime frames converting a nested argument do not crowd out the call's.
            (
                "const deep = {a: {b: {c: {d: {e: {f: Symbol()}}}}}};\ntool.echo({value: deep});",
                "TypeError: tool.echo argument has a symbol at $.value.a.b.c.d.e.f at line 2:6",
            ),
        ] {
            let (_scope, executor, context) = echo_runtime().await;
            let failed = evaluate(source, executor, context).await;
            let (diagnostic, _) = failed.unwrap_err().into_parts();
            assert_eq!(diagnostic.context.effects, Effects::Unknown, "{source}");
            assert_eq!(
                diagnostic.cause,
                Cause::Message(expected.into()),
                "{source}"
            );
        }
        // Only the thrown error's other properties remain as failure details, and
        // only an exception's own stack is filtered.
        let source = "const error = new Error('outer', {cause:new Error('inner')}); \
                      error.output = {stack:'data'}; throw error;";
        let (_, Some(output)) = plain(source).await.unwrap_err().into_parts() else {
            panic!("expected JavaScript failure output");
        };
        assert_eq!(
            output.value["failure"],
            json!({"output": {"stack":"data"}, "cause": {"name":"Error", "message":"inner",
                   "stack":"    at line 1:41"}})
        );
        // A returned value that does not convert keeps what did.
        let (_, Some(output)) = plain("return {kept: 1, nested: {stdout: () => 0}};")
            .await
            .unwrap_err()
            .into_parts()
        else {
            panic!("expected JavaScript failure output");
        };
        assert_eq!(
            output.value["value"],
            json!({"kept":1, "nested":{"stdout":null}})
        );
        // A host script's failure is its execution's, at the host.
        let runtime = TestRuntime::new().await;
        let (executor, _slot) =
            crate::tests::tool_executor(runtime.jobs.clone(), runtime.root.path());
        let source = json!({"source": "return {nested: () => 0};"});
        let failed = executor.run_host(&runtime.agent, "script", source).await;
        let diagnostic = failed.unwrap_err().diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Execute);
        assert_eq!(diagnostic.context.site, FailureSite::Host);
        assert_matches!(&diagnostic.cause, Cause::Message(message) if message.contains("a function at $.nested")
        );
    }

    #[tokio::test]
    async fn console_replaces_unpaired_surrogates() {
        let output = plain(r"console.log('\uD800', new Error('\uDC00')); return 1;")
            .await
            .unwrap();
        assert_eq!(
            output.value["console"],
            "\u{fffd} \u{fffd}\n    at line 1:23\n"
        );
    }

    #[tokio::test]
    async fn script_capture_initialization_failure_is_host_io_before_javascript() {
        let (_scope, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let reserved = HostOutput::new(context.jobs(), context.job()).await;
        let _held = (reserved.context().text_capture(TextCaptureField::Console))
            .await
            .unwrap();
        let error = evaluate_captured(
            "throw new Error('must not execute');".into(),
            executor,
            context,
            LIMITS,
        )
        .await
        .unwrap_err();
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::CreateCapture);
        assert_eq!(diagnostic.context.effects, Effects::NotStarted);
        assert_matches!(diagnostic.cause, Cause::Io { .. });
    }

    #[test]
    fn script_console_finalization_preserves_primary_failure_and_known_output() {
        // One primary with failure details and one without any output.
        for error in [
            javascript_error(JsError::Failure {
                failure: Failure {
                    message: "primary failure".into(),
                    details: Some(serde_json::Map::from_iter([("job".into(), json!(3))])),
                },
                returned: Box::default(),
            }),
            javascript_error(JsError::Cancelled),
        ] {
            let (expected, expected_output) = error.into_parts();
            let mut incomplete = expected.clone();
            incomplete.context.effects = Effects::OutputIncomplete;
            // Check both failed and successful console finalization against the same primary.
            for (console, diagnostic) in [
                (Err(std::io::ErrorKind::BrokenPipe.into()), &incomplete),
                (Ok(None), &expected),
            ] {
                let primary = ToolError::from_diagnostic(expected.clone(), expected_output.clone());
                let (actual, output) = finish_evaluation(Err(primary), console)
                    .unwrap_err()
                    .into_parts();
                assert_eq!(&actual, diagnostic);
                assert_eq!(
                    output.map(|output| output.value),
                    expected_output.as_ref().map(|output| output.value.clone())
                );
            }
        }
        let (diagnostic, output) = finish_evaluation(
            Ok(ToolOutput::new(json!({"computed": 42}))),
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
        )
        .unwrap_err()
        .into_parts();
        assert_eq!(diagnostic.context.operation, Operation::FinishCapture);
        assert_eq!(diagnostic.context.effects, Effects::OutputIncomplete);
        assert_matches!(
            diagnostic.cause,
            Cause::Io {
                kind: IoKind::BrokenPipe,
                ..
            }
        );
        assert_eq!(output.unwrap().value, json!({"value":{"computed":42}}));
    }

    #[test]
    fn work_pool_accepts_native_arrays_completion_order_and_early_close() {
        // A failure whatever is thrown still settles its task.
        let value = evaluate_wrapper(
            r#"
const values = [2, 3];
const pooled = [], settled = [];
for await (const result of new WorkPool(2).map(values, async value => value * 2)) pooled.push(result);
const tasks = values.map(value => async () => {
  if (value === 3) throw new Error("three");
  return value * 3;
});
for await (const result of new WorkPool(2).run(tasks)) settled.push(result);
const thrown = [{tag: "structured"}, Object.create(null), {get message() { throw 0; }}];
for await (const {index, error} of new WorkPool(1).run(thrown.map(value => () => { throw value; }))) {
  settled.push({index, same: error === thrown[index]});
}
let rejected = false;
try { new WorkPool(1).map(values, null); } catch (error) { rejected = error instanceof TypeError; }
return {values, pooled, settled, rejected};
"#,
        );
        assert_eq!(
            value,
            json!({
                "values": [2, 3],
                "pooled": [{ "index":0, "value":4 }, { "index":1, "value":6 }],
                "settled": [
                    {"index":0,"value":6}, {"index":1,"error":{"name":"Error","message":"three"}},
                    {"index":0,"same":true}, {"index":1,"same":true}, {"index":2,"same":true},
                ],
                "rejected": true,
            })
        );
        let value = evaluate_wrapper(
            r#"
const gates = Array.from({length:4}, () => {
  let resolve; const promise = new Promise(done => { resolve = done; });
  return {promise, resolve};
}), started = [];
const iterator = new WorkPool(2).map(gates, async (gate, index) => {
  started.push(index);
  const value = await gate.promise;
  if (value === "fail") throw new Error("late failure");
  return value;
});
const pending = iterator.next(); gates[1].resolve("second");
const first = await pending;
const next = iterator.next(); gates[2].resolve("third");
const second = await next;
const closing = iterator.return(); gates[0].resolve("fail");
await closing;
return {first, second, started};
"#,
        );
        assert_eq!(value["first"]["value"]["index"], 1);
        assert_eq!(value["second"]["value"]["index"], 2);
        assert_eq!(value["started"], json!([0, 1, 2]));
    }

    /// A feed refills from `next` as items complete, keeps the items beyond its free slots
    /// for later, never runs one key twice at once, returns failures and wake-ups as
    /// values, and releases a wake still pending when it ends.
    #[test]
    fn work_pool_feeds_ready_items_by_key() {
        let value = evaluate_wrapper(
            r#"
const queue = [{key: "a", n: 1}, {key: "b", n: 2}, {key: "a", n: 3}, {key: "c", n: 4}];
const asked = [], order = [], live = new Set();
let overlap = false, released = 0, wakes = 0;
const gates = new Map();
const feed = new WorkPool(2).feed(async (running, free) => {
  asked.push(free);
  return queue.splice(0, queue.length);
}, async item => {
  if (live.has(item.key)) overlap = true;
  live.add(item.key);
  await new Promise(resolve => gates.set(item.n, resolve));
  live.delete(item.key);
  if (item.n === 4) throw new Error("four");
  return item.n;
}, {wake: () => {
  wakes++;
  if (wakes === 1) return "ping";
  return Object.defineProperty(new Promise(() => {}), "release", {value: () => released++});
}});
const open = () => { for (const [n, resolve] of gates) { gates.delete(n); resolve(); return; } };
for await (const r of feed) {
  order.push("wake" in r ? {wake: r.wake} : r.error ? {key: r.key, error: r.error} : {key: r.key, value: r.value});
  await sleep(0);
  open();
}
return {order, asked, overlap, released, wakes};
"#,
        );
        assert_eq!(
            value,
            json!({
                "order": [
                    {"wake":"ping"}, {"key":"a","value":1}, {"key":"b","value":2},
                    {"key":"a","value":3}, {"key":"c","error":{"name":"Error","message":"four"}},
                ],
                "asked": [2, 1, 2],
                "overlap": false,
                "released": 1,
                "wakes": 2,
            })
        );
    }

    /// `JSON.parse` given a value a tool already decoded returns a copy of it, reviver applied.
    #[test]
    fn json_parse_takes_a_decoded_value_as_itself() {
        let source = "const decoded = {a: [1, {b: 2}]}; const copy = JSON.parse(decoded);
copy.a.push(3);
const revived = JSON.parse({n: 1}, (_, x) => typeof x === 'number' ? x + 1 : x);
return {copy, decoded, revived, text: JSON.parse('[1]')};";
        assert_eq!(
            evaluate_wrapper(source),
            json!({"copy": {"a": [1, {"b": 2}, 3]}, "decoded": {"a": [1, {"b": 2}]},
                   "revived": {"n": 2}, "text": [1]})
        );
    }

    #[test]
    fn response_methods_are_runtime_only_and_do_not_decorate_user_json() {
        let result = evaluate_wrapper(
            r#"
const payload = {id:7, state:"completed", result:null};
const response = await tool.echo({value:payload});
const encoded = JSON.stringify(response);
const copied = JSON.parse(encoded);
const received = await receive();
let logged;
__skyhookConsoleLog = message => { logged = message; };
console.log(response);
return {
  response, encoded, keys:Object.keys(response), logged,
  nonEnumerable:!Object.getOwnPropertyDescriptor(response, "unwrap").enumerable,
  samePayload:response.unwrap() === response.result,
  noGlobalHelper:typeof tool.unwrap === "undefined",
  plainPayload:typeof response.result.unwrap === "undefined"
    && typeof response.result.value.unwrap === "undefined",
  plainCopy:typeof copied.unwrap === "undefined",
  plainReceive:typeof received.unwrap === "undefined", received,
};
"#,
        );
        for key in [
            "nonEnumerable",
            "samePayload",
            "noGlobalHelper",
            "plainPayload",
            "plainCopy",
            "plainReceive",
        ] {
            assert_eq!(result[key], true, "{key}: {result}");
        }
        let response = &result["response"];
        assert!(response.get("unwrap").is_none());
        assert!(
            !result["keys"]
                .as_array()
                .unwrap()
                .contains(&json!("unwrap"))
        );
        for key in ["encoded", "logged"] {
            assert_eq!(
                serde_json::from_str::<Value>(result[key].as_str().unwrap()).unwrap(),
                *response
            );
        }
        assert_eq!(result["received"], json!({"id":99,"state":"completed"}));
    }

    /// `unwrap()` returns a completed view's result, where an absent `state`
    /// means completed, and throws with the view for any other state.
    #[test]
    fn unwrap_returns_completed_results_and_throws_otherwise() {
        let result = evaluate_wrapper(
            r#"
const results = [];
for (const value of [
  {result:{value:42}}, {}, {id:8, state:"completed", result:null},
  {id:9, state:"failed", error:"boom", result:{partial:true}, meta:{code:"permission_denied"}},
  {state:"failed", error:"unadmitted"},
  {id:10, state:"running"},
]) {
  __skyhookHostCall = () => JSON.stringify({outcome:"ok", value, presented:{}});
  const response = await tool.echo({value:null});
  try { results.push({value:response.unwrap()}); }
  catch (error) {
    results.push({message:error.message, sameResponse:error.response === response,
                  output:error.output ?? "absent", own:{...error}});
  }
}
return results;
"#,
        );
        assert_eq!(
            result,
            json!([
                {"value":{"value":42}},
                {"value":null},
                {"value":null},
                {"message":"boom", "sameResponse":true, "output":{"partial":true},
                 "own":{"job":9, "code":"permission_denied"}},
                {"message":"unadmitted", "sameResponse":true, "output":"absent", "own":{}},
                {"message":"job 10 is not completed (state: running)", "sameResponse":true,
                 "output":"absent", "own":{"job":10}}
            ])
        );
    }

    #[test]
    fn returns_await_thenables_convert_errors_and_report_each_unconvertible_member() {
        let value = evaluate_wrapper(
            r#"
const error = new TypeError("bad", {cause: "why"});
error.code = 7;
class Point { y = 2; [Symbol("hidden")] = 3; get inherited() { return 4; } }
class Again { kept = 1; toJSON() { return "again"; } }
class Itself { kept = 1; toJSON() { return this; } }
// A serializer naming the parent it meets again, which JSON.stringify accepts.
class TreeNode {
  constructor(id, parent) { this.id = id; this.parent = parent; this.children = []; parent?.children.push(this); }
  toJSON(key) { return key === "parent" ? this.id : {id: this.id, parent: this.parent ?? null, children: this.children}; }
}
const tree = new TreeNode(1);
new TreeNode(2, tree);
// A serializer that ends by remembering what it has given.
const seen = new WeakSet();
const node = {toJSON() { if (seen.has(this)) return "[seen]"; seen.add(this); return {node: this}; }};
return {
  awaited: [Promise.resolve(1), (async () => ({inner: tool.echo({value: "nested"})}))(),
            Object.assign(() => 0, {then: resolve => resolve(2)})],
  error, text: "\uD800", "a.b": [null], url: new URL("https://example.com/"),
  dates: [new Date(0), new Date(NaN)], big: 12345678901234567890n, numbers: [NaN, -Infinity],
  typed: [new Uint8Array([1, 2]), new Float64Array([0.5, NaN]), new BigInt64Array([-1n])],
  buffers: [new Uint16Array([0x0201]).buffer, new DataView(new Uint8Array([1, 2, 3]).buffer, 1)],
  set: new Set([1, "a", 1]), regexp: /x+/g, boxed: [Object(1), Object("s"), Object(false), Object(5n)],
  maps: [new Map([["a", 1], [2, new Set([3])]]), new Map([[{k: 1}, "object"]]), new Map([[1, "n"], ["1", "s"]])],
  point: Object.defineProperty(new Point(), "x", {enumerable: true, get: () => 1}),
  custom: {toJSON: key => ({key, at: new Date(0)})},
  absent: {toJSON: () => undefined}, absences: [{toJSON: () => undefined}, Promise.resolve()],
  replaced: [{toJSON: () => new Again()}, {toJSON: async () => new Again()}],
  shared: new SharedArrayBuffer(2),
  itself: new Itself(), proto: JSON.parse('{"__proto__": {"kept": 1}}'), tree, stateful: {node},
};
"#,
        );
        assert_eq!(
            value,
            json!({
                "awaited": [1, {"inner": {"id":1, "state":"completed", "result":{"value":"nested"}}}, 2],
                "error": {"code":7, "name":"TypeError", "message":"bad", "cause":"why"},
                "text": "\u{fffd}",
                "a.b": [null],
                "url": "https://example.com/",
                "dates": ["1970-01-01T00:00:00.000Z", null],
                "big": "12345678901234567890",
                "numbers": [null, null],
                "typed": [[1, 2], [0.5, null], ["-1"]],
                "buffers": [[1, 2], [2, 3]],
                "set": [1, "a"],
                "regexp": "/x+/g",
                "boxed": [1, "s", false, "5"],
                "maps": [{"a": 1, "2": [3]}, [[{"k": 1}, "object"]], [[1, "n"], ["1", "s"]]],
                "point": {"y": 2, "x": 1},
                "custom": {"key": "custom", "at": "1970-01-01T00:00:00.000Z"},
                "absences": [null, null],
                "replaced": [{"kept": 1}, {"kept": 1}],
                "shared": [0, 0],
                "itself": {"kept": 1},
                "proto": {"__proto__": {"kept": 1}},
                "tree": {"id": 1, "parent": null, "children": [{"id": 2, "parent": 1, "children": []}]},
                "stateful": {"node": {"node": "[seen]"}},
            })
        );
        // Each member that does not convert is reported and kept as null.
        let failed = evaluate_wrapper_envelope(
            r#"
const keys = new Error("keys");
keys["\uD800"] = 1;
let deep = [];
for (let i = 0; i < 100; i++) deep = [deep];
const loop = {toJSON: () => ({loop})};
return {kept: 1, list: [1, , undefined], a: {b: undefined}, "b.c": () => 0, keys, deep,
        get thrower() { throw new TypeError("boom"); },
        symbol: Symbol("s"), weak: new WeakMap(),
        odd: Object.defineProperty(new WeakSet(), "constructor", {value: {name: "\uD800"}}),
        json: {toJSON() { throw new Error("late"); }}, loop,
        rejected: Promise.reject(new Error("no"))};
"#,
        );
        let message = failed.failure.unwrap().message;
        assert!(
            message.starts_with(
                r#"the returned value has a function at $["b.c"]; an unpaired surrogate in a key at $.keys; nesting deeper than 100 at $.deep[0][0][0][0][0][0][0][0][0][0][0][0][0][0]…; a throwing property"#
            ),
            "{message}"
        );
        for problem in [
            "; a throwing property at $.thrower: TypeError: boom; a symbol at $.symbol; \
             a WeakMap at $.weak; a \u{fffd} at $.odd; a throwing toJSON at $.json: late; \
             nesting deeper than 100 at $.loop.loop.loop.loop",
            "; a rejected promise at $.rejected: no",
        ] {
            assert!(message.contains(problem), "{problem}: {message}");
        }
        let value = &failed.returned.value;
        for member in [
            "b.c", "thrower", "symbol", "weak", "odd", "json", "rejected",
        ] {
            assert_eq!(value.get(member), Some(&Value::Null), "{member}");
        }
        // Undefined converts as in JSON.stringify, as no problem.
        assert_eq!(value["list"], json!([1, null, null]));
        assert_eq!(value["a"], json!({}));
        assert_eq!(value["keys"], json!({"name":"Error", "message":"keys"}));
        assert_eq!(value["kept"], 1);
    }

    /// A feed offered only keys already running waits for a completion or a wake rather
    /// than asking again.
    #[test]
    fn a_feed_offered_only_running_keys_waits_for_completion() {
        let result = evaluate_wrapper(
            r#"
let open, done = false, wakes = 0;
const gate = new Promise(resolve => { open = resolve; });
const results = [];
const feed = new WorkPool(2).feed(async () => done ? [] : [{key: "a"}], async () => {
  await gate;
  done = true;
  return 1;
}, {wake: () => wakes++ ? new Promise(() => {}) : "ping"});
for await (const r of feed) {
  results.push("wake" in r ? r.wake : r.value);
  open();
}
return results;
"#,
        );
        assert_eq!(result, json!(["ping", 1]));
    }

    /// A released receive returns its value to the oldest receive still waiting, and
    /// values released by feeds that end with their wakes taken but unread reach later
    /// receives in the order they arrived. A wake still waiting when its feed ends keeps
    /// nothing: the value its receive was pulling reaches the next `receive()`.
    #[test]
    fn released_receives_keep_their_arrival_order() {
        let result = evaluate_wrapper(
            r#"
const answers = [];
__skyhookHostCall = () => new Promise(resolve => answers.push(value => {
  resolve(JSON.stringify({outcome: "ok", value: {value}, presented: {}}));
}));
const first = receive();
answers.shift()("one");
await first;
const second = receive();
first.release();
const third = receive();
answers.shift()("two");
return [await second, await third].map(received => received.value);
"#,
        );
        assert_eq!(result, json!(["one", "two"]));
        let result = evaluate_wrapper(
            r#"
const answers = [], wakes = [];
__skyhookHostCall = () => new Promise(resolve => answers.push(value => {
  resolve(JSON.stringify({outcome: "ok", value: {value}, presented: {}}));
}));
const fed = () => {
  let offered = false;
  const next = async () => offered ? [] : (offered = true, [{key: 1}]);
  const wake = () => { const received = receive(); wakes.push(received); return received; };
  return new WorkPool(1).feed(next, async () => "done", {wake});
};
const feeds = [fed(), fed()];
for (const feed of feeds) await feed.next();
for (const [index, value] of ["one", "two"].entries()) {
  answers.shift()(value);
  await wakes[index];
}
for (const feed of feeds) await feed.return();
return [await receive(), await receive()].map(received => received.value);
"#,
        );
        assert_eq!(result, json!(["one", "two"]));
        let result = evaluate_wrapper(
            r#"
const answers = [];
__skyhookHostCall = () => new Promise(resolve => answers.push(value => {
  resolve(JSON.stringify({outcome: "ok", value: {value}, presented: {}}));
}));
for await (const _ of new WorkPool(1).feed(async () => [], async () => 0, {wake: receive})) {}
const once = [{key: 1}];
for await (const _ of new WorkPool(1).feed(async () => once.splice(0), async () => sleep(0), {wake: receive})) {}
const first = receive(), second = receive();
answers.shift()("one");
await first;
answers.shift()("two");
return [await first, await second].map(received => received.value);
"#,
        );
        assert_eq!(result, json!(["one", "two"]));
    }

    #[test]
    fn arguments_convert_when_a_builder_is_made() {
        let result = evaluate_wrapper(
            r#"
const rejected = [];
for (const make of [
  () => tool.echo({value: [undefined]}),
  () => tool.echo({value: tool.echo({value: 1})}),
  () => tool.echo({value: "\uD800"}),
  () => tool.echo([]),
  () => tool.echo(Promise.resolve({value: 1})),
  () => tool.echo({value: [() => 0]}),
  () => tool.echo({value: {symbol: Symbol()}}),
  () => tool.echo({value: new WeakMap()}),
  () => tool.echo({value: {toJSON() { throw new Error("no"); }}}),
  () => tool.echo({value: [{toJSON() {}}]}),
  () => {
    // Its length is read once, so the elements a getter removes are still slots.
    const shrinking = [0, 1, 2];
    Object.defineProperty(shrinking, 0, {get() { shrinking.length = 1; return 0; }});
    return tool.echo({value: shrinking});
  },
]) {
  try { make(); rejected.push(null); } catch (error) { rejected.push(error.message); }
}
await tool.echo({value: {kept: 1, omitted: undefined}});
await tool.echo({value: [new URL("https://example.com/a b"), new Date(0), 1n, new Map([[1, new Set([2])]])]});
await tool.echo(new Map([["value", NaN]]));
await tool.echo({value: {absent: {toJSON() {}}}});
return {rejected, calls: __testCalls};
"#,
        );
        assert_eq!(
            result,
            json!({
                "rejected": [
                    "tool.echo argument has undefined at $.value[0]",
                    "tool.echo argument has an unawaited promise or tool call at $.value",
                    "tool.echo argument has an unpaired surrogate at $.value",
                    "tool.echo argument must be a plain object",
                    "tool.echo argument has an unawaited promise or tool call at $",
                    "tool.echo argument has a function at $.value[0]",
                    "tool.echo argument has a symbol at $.value.symbol",
                    "tool.echo argument has a WeakMap at $.value",
                    "tool.echo argument has a throwing toJSON at $.value: no",
                    "tool.echo argument has undefined at $.value[0]",
                    "tool.echo argument has undefined at $.value[1]",
                ],
                "calls": [
                    {"type":"call", "name":"echo", "arguments":{"value":{"kept":1}}},
                    {"type":"call", "name":"echo", "arguments":{"value":
                        ["https://example.com/a%20b", "1970-01-01T00:00:00.000Z", "1", {"1": [2]}]}},
                    {"type":"call", "name":"echo", "arguments":{"value":null}},
                    {"type":"call", "name":"echo", "arguments":{"value":{}}},
                ],
            })
        );
    }

    /// Each returned thenable continues as soon as it settles, so a branch may wait
    /// on a call nested in another's resolved value; members keep their order.
    #[test]
    fn returned_branches_continue_independently_in_source_order() {
        let envelope = evaluate_wrapper_envelope(
            r#"
const reply = __skyhookHostCall;
let release;
const gate = new Promise(resolve => { release = resolve; });
__skyhookHostCall = async encoded => {
  const {value} = JSON.parse(encoded).arguments;
  if (value === "waits") await gate;
  if (value === "releases") release();
  return reply(encoded);
};
return {waits: tool.echo({value: "waits"}), nested: (async () => ({releases: tool.echo({value: "releases"})}))()};
"#,
        );
        let value = &envelope.returned.value;
        assert_eq!(
            value.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["waits", "nested"]
        );
        assert_eq!(value["waits"]["result"]["value"], "waits");
        assert_eq!(value["nested"]["releases"]["result"]["value"], "releases");
    }

    /// Scripts read a field a response's schemas declare but it lacks as `null`,
    /// alike in a direct and an inspected result, while the model's view stays sparse.
    #[tokio::test]
    async fn scripts_read_declared_absent_fields_as_null() {
        let runtime = TestRuntime::new().await;
        tokio::fs::create_dir(runtime.root.path().join("listed"))
            .await
            .unwrap();
        let (executor, _slot) =
            crate::tests::tool_executor(runtime.jobs.clone(), runtime.root.path());
        let listed = executor
            .run_model(&runtime.agent, "read", json!({"path":"listed"}))
            .await
            .unwrap();
        let entries = &listed.output.value["result"]["entries"];
        assert_eq!(entries.get("symlinks"), None, "{entries}");
        let source = format!(
            r#"
const direct = await tool.read({{path:"listed"}});
const inspected = await tool.jobs({{job:{}}});
return {{direct, inspected, missing:direct.result.entries.symlinks === null}};
"#,
            listed.job
        );
        let output = script(&executor, &runtime.agent, source, false)
            .await
            .output;
        let value = &output.value["result"]["value"];
        assert_eq!(value["missing"], true);
        let groups = json!({"files":[], "directories":[], "symlinks":null, "other":null});
        assert_eq!(value["direct"]["result"]["entries"], groups);
        assert_eq!(value["direct"].get("id"), Some(&Value::Null));
        assert_eq!(value["inspected"]["result"]["entries"], groups);
        assert_eq!(value["inspected"].get("error"), Some(&Value::Null));
        // An inspected result fills from its own invocation's schema, here a JobView
        // rather than the listing the `jobs` tool declares, and only when shown whole.
        let inspection = executor
            .run_model(&runtime.agent, "jobs", json!({"job":listed.job}))
            .await
            .unwrap();
        let source = format!(
            "return (await tool.jobs({{job:{}}})).result;",
            inspection.job
        );
        let output = script(&executor, &runtime.agent, source, false)
            .await
            .output;
        let view = &output.value["result"]["value"];
        assert_eq!(view.get("error"), Some(&Value::Null), "{view}");
        assert_eq!(view.get("jobs"), None, "{view}");
        let shown = |presentation: Value| json!({"id":listed.job, "presentation":presentation});
        assert!(
            inspected_schema(&runtime.jobs, &shown(json!({})))
                .await
                .is_some()
        );
        let truncated = shown(json!({"truncated":[{"field":"/result/entries"}]}));
        assert!(inspected_schema(&runtime.jobs, &truncated).await.is_none());
    }

    /// A script holds a saved job's whole result, but returning its view presents
    /// that result to the model as a direct read would.
    #[tokio::test]
    async fn returned_job_views_present_their_results() {
        let runtime = TestRuntime::new().await;
        let (executor, _slot) =
            crate::tests::tool_executor(runtime.jobs.clone(), runtime.root.path());
        let agent = &runtime.agent;
        let source = "return Array.from({length: 5000}, (_, index) => index);";
        let saved = script(&executor, agent, source.into(), false).await;
        let source = format!(
            "const view = await tool.jobs({{job:{}}});
if (view.result.value.length !== 5000) throw new Error('partial');
return view;",
            saved.job
        );
        let output = script(&executor, agent, source, false).await.output;
        let shown = &output.value["result"]["value"]["result"]["value"];
        assert!(shown.as_array().unwrap().len() < 5000, "{:#}", output.value);
        assert!(output.value["presentation"]["truncated"].is_array());
    }

    #[test]
    fn builder_chains_are_immutable_and_execute_once() {
        let result = evaluate_wrapper(
            r#"
const base = tool.echo();
const first = base.value("first");
const second = first.set("value", "second");
await first;
await first;
await second;
return __testCalls;
"#,
        );
        assert_eq!(
            result,
            json!([
                {"type":"call", "name":"echo", "arguments":{"value":"first"}},
                {"type":"call", "name":"echo", "arguments":{"value":"second"}},
            ])
        );
    }

    /// What a script's calls report is acknowledged with the script's own
    /// finish, however it ends: one the journal refuses leaves it unread, and
    /// a cancelled one, which has no output, acknowledges it.
    #[tokio::test]
    async fn a_script_acknowledges_what_its_calls_report_with_its_finish() {
        use crate::session::SessionEvent::{JobClaimed, JobCreated};
        let (runtime, executor, _slot) = image_runtime().await;
        let created = |records: &[Arc<crate::session::EventRecord>], name| -> Vec<_> {
            let jobs = records.iter().filter_map(|record| match &record.event {
                JobCreated { job, tool, .. } if tool == name => Some(*job),
                _ => None,
            });
            jobs.collect()
        };
        let claimed = |records: &[Arc<crate::session::EventRecord>], read| {
            (records.iter()).any(|record| matches!(record.event, JobClaimed { job } if job == read))
        };
        let source = "return (await tool.read({path: 'image.png'})).state;";
        let arguments = json!({"source": source});
        let refused = "BEFORE INSERT ON job_finish WHEN NEW.job = 1";
        let refused = crate::session::tests::refuse(&runtime.store, refused);
        let failed = executor.run_host(&runtime.agent, "script", arguments).await;
        drop(refused);
        assert!(failed.is_err());
        let records = runtime.store.records();
        assert!(!claimed(&records, created(&records, "read")[0]));

        // A refused finish stops the session: the cancelled script runs in another.
        let (runtime, executor, _slot) = image_runtime().await;
        // The second read starts once the script took the first one's report.
        let read = "await tool.read({path: 'image.png'});";
        let source = format!("{read} {read} await sleep(1800000);");
        let arguments = json!({"source": source});
        let script = tokio::spawn({
            let (executor, agent) = (executor.clone(), runtime.agent.clone());
            async move { executor.run_host(&agent, "script", arguments).await }
        });
        let started = move |records: &[Arc<crate::session::EventRecord>]| {
            let reads = created(records, "read");
            (reads.len() == 2).then(|| (created(records, "script")[0], reads[0]))
        };
        let started = crate::session::tests::on_records(&runtime.store, &runtime.agent, started);
        let (script_job, read) = bounded(runtime.store.until(started)).await.unwrap();
        runtime.jobs.cancel(script_job).await.unwrap();
        let cancelled = bounded(script).await.unwrap().unwrap_err();
        assert_eq!(cancelled.diagnostic().cause, Cause::Cancelled);
        assert!(claimed(&runtime.store.records(), read));
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
        assert!(
            runtime
                .jobs
                .snapshot(failed.job)
                .await
                .unwrap()
                .images
                .is_empty()
        );
        let jobs = runtime.jobs.list(&runtime.agent).await.unwrap();
        let source = jobs.into_iter().find(|job| job.tool == "read");
        let source = source.expect("owning read result survives script failure");
        assert_eq!(source.state, JobState::Completed);
        assert_loaded(
            &runtime.store,
            runtime.jobs.snapshot(source.id).await.unwrap().images,
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
        runtime.jobs.wait(launched.job, true).await.unwrap();
        let output = executor
            .run_model(agent, "jobs", json!({"job":launched.job}))
            .await;
        let output = output.unwrap().output;
        assert_eq!(output.value["state"], "completed");
        // Returning a tool response preserves its canonical read JobView.
        assert_eq!(output.value["result"]["value"]["result"]["kind"], "image");
        assert_eq!(output.value["result"].get("console"), None);
        assert_loaded(&runtime.store, output.images).await;
        let source = format!(
            "console.log('kept'); return await tool.jobs({{job:{}}});",
            launched.job
        );
        let output = script(&executor, agent, source.clone(), false).await.output;
        assert_eq!(output.value["result"]["value"]["id"], launched.job.get());
        assert_eq!(output.value["result"]["console"], "kept\n");
        assert_loaded(&runtime.store, output.images).await;
        // A background script that retrieves an existing image must itself retain
        // the attachment, not merely the nested job's JSON metadata.
        let background = script(&executor, agent, source, true).await;
        runtime.jobs.wait(background.job, true).await.unwrap();
        let output = executor
            .run_model(agent, "jobs", json!({"job":background.job}))
            .await;
        let output = output.unwrap().output;
        assert_eq!(output.value["state"], "completed");
        assert_loaded(&runtime.store, output.images).await;
    }
}
