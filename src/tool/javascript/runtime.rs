//! `QuickJS` runtime implementation and host bridge.

use std::sync::Arc;

use rquickjs::{CatchResultExt, Function, Promise, function::Async};
use serde_json::Value;
use tokio::sync::Mutex;

use super::{
    bridge::{Config, Failure, Finished, HostRequest, HostResponse, Presented, Returned},
    engine,
    errors::{JsError, bridge_error, javascript_error},
    source::{MAX_SOURCE_BYTES, SubmittedSource},
};

use serde::{Deserialize, de::IgnoredAny};

use crate::{
    identity::JobId,
    job::{
        FieldPointer, JOB_VIEW_SCHEMAS, JobManager,
        output::{fill_absent, presented_fields},
    },
    media::ImageRef,
    tool::diagnostic::{Effects, FailureSite, Operation, Subject},
    tool::executor::{InvocationKind, ToolExecutor, failure_response},
    tool::output::FieldPresentation,
    tool::registry::ResultSchema,
    tool::{ToolContext, ToolError, ToolOutput, ToolResultPolicy},
};

/// Execute into the owning job's capture; native callers hydrate only when collecting the job.
pub(crate) async fn evaluate_captured(
    source: String,
    executor: ToolExecutor,
    context: ToolContext,
) -> Result<ToolOutput, ToolError> {
    let capture = context
        .text_capture(crate::job::output::TextCaptureField::Console)
        .await
        .map_err(|error| {
            console_failure(error, Operation::CreateCapture, Effects::NotStarted)
                .with_result(super::result::script_output(Value::Null, None, None))
        })?;
    let console = Arc::new(std::sync::Mutex::new(capture.open()));
    let result = evaluate_inner(source, executor, context, console.clone()).await;
    let finished = console
        .lock()
        .expect("console lock poisoned")
        .close_nonempty();
    finish_evaluation(result, finished)
}

fn finish_evaluation(
    result: Result<ToolOutput, ToolError>,
    console: std::io::Result<Option<crate::job::output::CompletedCapture>>,
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
    console: Arc<std::sync::Mutex<crate::job::output::CaptureWriter>>,
) -> Result<ToolOutput, ToolError> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(javascript_error(JsError::SourceTooLarge));
    }
    let (_runtime, js_context) = engine::create(&context)
        .await
        .map_err(|error| javascript_error(JsError::Initialization(error)))?;
    let surface = executor.surface_for_agent(context.agent());
    let source = SubmittedSource::new(&source);
    let config = Config {
        builders: surface.script_manifests(),
        source: source.layout(),
    };
    let config = serde_json::to_string(&config)
        .map_err(|error| javascript_error(JsError::Initialization(error.to_string())))?;
    let images = Arc::new(Mutex::new(Vec::<ImageRef>::new()));
    let returned_images = images.clone();
    let cancelled = context.clone();
    let calls = tokio_util::task::TaskTracker::new();
    let host_calls = calls.clone();
    let submitted = &source;
    let execution = js_context.async_with(async move |js| {
        let sleep_context = context.clone();
        // The runtime's `sleep` admits only delays a JavaScript timer accepts.
        let sleep = Function::new(js.clone(), Async(move |milliseconds: f64| {
            let context = sleep_context.clone();
            async move {
                let duration = std::time::Duration::try_from_secs_f64(milliseconds / 1000.0)
                    .map_err(|error| bridge_error(&error))?;
                // Whether the delay elapsed, rather than cancellation ending it.
                Ok::<_, rquickjs::Error>(tokio::select! {
                    biased;
                    () = context.cancelled() => false,
                    () = tokio::time::sleep(duration) => true,
                })
            }
        }))
        .map_err(|error| JsError::Initialization(error.to_string()))?;
        let log = Function::new(js.clone(), move |text: String| {
            console.lock().expect("console lock poisoned").write_text(&format!("{text}\n")).map_err(|error| {
                bridge_error(&console_failure(error.into(), Operation::WriteCapture, Effects::OutputIncomplete))
            })
        })
        .map_err(|error| JsError::Initialization(error.to_string()))?;
        let host_context = context.clone();
        let host_executor = executor.clone();
        let host_images = images.clone();
        let host_call = Function::new(
            js.clone(),
            Async(move |request: String| {
                // Counted at the JS call, so calls issued together are all published
                // before any of them can see its siblings.
                let issuing = host_executor.jobs().issue(host_context.job());
                let host_context = host_context.clone();
                let host_executor = host_executor.clone();
                let host_images = host_images.clone();
                let surface = surface.clone();
                host_calls.track_future(async move {
                    let request: HostRequest =
                        serde_json::from_str(&request).map_err(|error| bridge_error(&error))?;
                    let response = match request {
                        HostRequest::Call { name, arguments } => {
                            let parent = Some(host_context.job());
                            // Failures before and after the job exists are ordinary
                            // tool responses rather than JS exceptions.
                            let failed =
                                |error| failure_response(error, host_context.diagnostic_viewer());
                            // Native schemas describe only envelope.result.
                            let mut policy = ToolResultPolicy::Value;
                            let created = host_executor
                                .create(InvocationKind::Script, host_context.agent().clone(),
                                    &name,
                                    Value::Object(arguments),
                                    parent,
                                )
                                .await;
                            drop(issuing);
                            let output = match created {
                                Ok(created) => {
                                    policy = created.result_policy();
                                    let reported = host_executor.run_reported(created).await;
                                    host_context.read(reported.acknowledges).await;
                                    match reported.outcome {
                                        Ok(result) => result.output,
                                        Err(error) => failed(error),
                                    }
                                }
                                Err(error) => failed(error),
                            };
                            host_images.lock().await.extend(output.images);
                            let inspected = match policy {
                                ToolResultPolicy::JobView => inspected_schema(host_executor.jobs(), &output.value).await,
                                ToolResultPolicy::Value | ToolResultPolicy::Nothing => None,
                            };
                            let declared = surface.get(&name).and_then(|tool| tool.result_schema.as_ref());
                            let result = match policy {
                                ToolResultPolicy::Value => declared.map(ResultSchema::schema),
                                ToolResultPolicy::JobView => inspected.as_deref(),
                                ToolResultPolicy::Nothing => None,
                            };
                            script_view(output.value, policy, result)
                        }
                        HostRequest::Receive => {
                            drop(issuing);
                            let result = match host_executor.jobs().is_background(host_context.job()).await {
                                Ok(true) => host_context.receive().await.map_err(|error| error.to_string()),
                                Ok(false) => Err("receive() requires the script tool to be invoked with bg: true".to_owned()),
                                Err(error) => Err(error.to_string()),
                            };
                            match result {
                                Ok(value) => HostResponse::Ok { value, presented: Presented::new() },
                                Err(error) => HostResponse::Failed { error },
                            }
                        }
                    };
                    serde_json::to_string(&response).map_err(|error| bridge_error(&error))
                })
            }),
        )
        .map_err(|error| JsError::Initialization(error.to_string()))?;
        let entry = engine::load(&js).map_err(JsError::Initialization)?;
        let export = |name| entry.get::<_, Function>(name)
            .catch(&js)
            .map_err(|error| JsError::Initialization(error.to_string()));
        let (install, run, fail) = (export("install")?, export("run")?, export("fail")?);
        let config = js.json_parse(config)
            .catch(&js)
            .map_err(|error| JsError::Initialization(error.to_string()))?;
        install.call::<_, ()>((config, host_call, sleep, log))
            .catch(&js)
            .map_err(|error| JsError::Initialization(error.to_string()))?;
        let failed = |error: rquickjs::CaughtError<'_>| JsError::Execution(error.to_string());
        let thrown = |error| match error {
            rquickjs::Error::Exception => {
                let encoded: String = fail.call((js.catch(),)).catch(&js).map_err(failed)?;
                serde_json::from_str::<Failure>(&encoded).map_err(|error| JsError::InvalidOutput(error.to_string()))
            }
            error => Err(JsError::Execution(error.to_string())),
        };
        let module = match submitted.declare(&js) {
            Ok(module) => module,
            Err(error) => return Err(JsError::Syntax(thrown(error)?.message)),
        };
        let body = match SubmittedSource::instantiate(module).await {
            Ok(body) => body,
            // Only input that closes the wrapper early runs anything before the body.
            Err(error) => {
                let failure = Some(thrown(error)?);
                return Ok(Finished { returned: Returned::default(), failure });
            }
        };
        let promise: Promise = run.call((body,)).catch(&js).map_err(failed)?;
        let encoded: String = promise.into_future().await.catch(&js).map_err(failed)?;
        serde_json::from_str(&encoded).map_err(|error| JsError::InvalidOutput(error.to_string()))
    });
    let result = tokio::select! {
        biased;
        result = execution => result,
        () = cancelled.cancelled() => Err(JsError::Cancelled),
    };
    if cancelled.is_cancelled() {
        // Cancellation interrupts JavaScript and ends the calls in flight. The
        // script may settle first, or await what can no longer settle; either way
        // its calls are driven to their end, so none outlives it.
        calls.close();
        js_context.async_with(async |_| calls.wait().await).await;
        return Err(javascript_error(JsError::Cancelled));
    }
    let Finished { returned, failure } = result.map_err(javascript_error)?;
    if let Some(failure) = failure {
        let returned = Box::new(returned);
        return Err(javascript_error(JsError::Failure { failure, returned }));
    }
    let Returned { value, presented } = returned;
    let mut images = std::mem::take(&mut *returned_images.lock().await);
    images.sort();
    images.dedup();
    Ok(ToolOutput::new(value)
        .with_images(images)
        .with_presented(presented.into_iter().collect()))
}

/// A response as scripts see it: with the presentations its schemas declare, and
/// each member they declare but it lacks as `null`. `result` is the schema its
/// result follows, when known.
fn script_view(mut value: Value, policy: ToolResultPolicy, result: Option<&Value>) -> HostResponse {
    let schemas = &JOB_VIEW_SCHEMAS;
    let mut presented = Presented::new();
    let view = match policy {
        // The response keeps its error and questions whole, and its result what
        // the tool's schema declares.
        ToolResultPolicy::Value => {
            presented.extend(presented_fields(
                &value,
                &FieldPointer::root(),
                &schemas.response,
            ));
            if let (Some(schema), Some(result)) = (result, value.get("result")) {
                presented.extend(presented_fields(result, &FieldPointer::result(), schema));
            }
            &schemas.response
        }
        // Job views and bare envelopes are presentations already.
        ToolResultPolicy::Nothing => {
            presented.insert(FieldPointer::root(), FieldPresentation::Complete);
            &schemas.response
        }
        ToolResultPolicy::JobView => {
            presented.insert(FieldPointer::root(), FieldPresentation::Complete);
            &schemas.one
        }
    };
    if let (Some(schema), Some(result)) = (result, value.get_mut("result")) {
        fill_absent(result, schema);
    }
    fill_absent(&mut value, view);
    HostResponse::Ok { value, presented }
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
    use std::time::{Duration, Instant};

    use crate::tests::TestRuntime;
    use rquickjs::{CatchResultExt, Context, Promise, Runtime};
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::{Value, json};

    use super::*;
    use crate::job::JobState;
    use crate::tests::{assert_loaded, bounded, image_runtime, script};
    use crate::tool::{
        ToolOptions, ToolRegistryBuilder,
        diagnostic::{Cause, IoKind},
        executor::ToolExecutor,
    };

    // Tests may eagerly inspect the console; production hydrates job captures on collection.
    async fn evaluate(
        source: impl Into<String>,
        executor: ToolExecutor,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let saved = executor.jobs().output(context.job()).await;
        let captured = evaluate_captured(source.into(), executor, context).await;
        let mut output = captured?;
        let console = saved
            .test_bytes("/result/console")
            .map(|bytes| String::from_utf8(bytes).expect("console is UTF-8"))
            .unwrap_or_default();
        output.value["console"] = Value::String(console);
        Ok(output)
    }

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Echo {
        value: String,
    }

    struct TestScope {
        // Keep the job alive while the test drives evaluation directly.
        _worker: crate::job::JobWorker,
        _root: tempfile::TempDir,
    }

    async fn test_runtime(builder: ToolRegistryBuilder) -> (TestScope, ToolExecutor, ToolContext) {
        let runtime = TestRuntime::new().await;
        let spec = crate::job::JobSpec::test(runtime.agent.clone(), "script");
        let lease = runtime.jobs.create(spec).await.unwrap().test_run().await;
        let executor = runtime.executor(builder);
        let (context, worker) = runtime.tool_context(lease);
        (
            TestScope {
                _worker: worker,
                _root: runtime.root,
            },
            executor,
            context,
        )
    }

    /// Evaluates `source` against an executor with no registered tools.
    async fn plain(source: &str) -> Result<ToolOutput, ToolError> {
        let (_scope, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        evaluate(source, executor, context).await
    }

    #[tokio::test]
    async fn local_source_and_sleep_limits_fail_before_user_effects() {
        let oversized = " ".repeat(MAX_SOURCE_BYTES + 1);
        let diagnostic = plain(&oversized).await.unwrap_err().diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Validate);
        assert_eq!(diagnostic.context.effects, Effects::NotStarted);
        let output = plain(
            r#"
            const errors = [];
            for (const milliseconds of [-1, NaN, Infinity, 2 ** 31, "10"]) {
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
    async fn cancellation_stops_sleep_and_pending_promises_without_keeping_unawaited_work_alive() {
        // Cancellation ends the script only once a call in flight has ended, both
        // when the script settles first and when it awaits what never settles.
        for source in [
            "await Promise.all([sleep(60000), tool.entered({})]); return 'finished';",
            "await new Promise(() => { tool.entered({}).then(() => {}); });",
        ] {
            let entered = Arc::new(tokio::sync::Notify::new());
            let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut builder = ToolRegistryBuilder::default();
            let (notify, end) = (entered.clone(), ended.clone());
            builder
                .register_product::<crate::tool::registry::Arguments, Value, _, _>(
                    "entered",
                    "",
                    ToolOptions::default(),
                    move |context, _| {
                        notify.notify_one();
                        let end = end.clone();
                        async move {
                            context.cancelled().await;
                            tokio::task::yield_now().await;
                            end.store(true, std::sync::atomic::Ordering::SeqCst);
                            Ok(ToolOutput::default())
                        }
                    },
                )
                .unwrap();
            let (_scope, executor, context) = test_runtime(builder).await;
            let cancellation = context.cancellation_token();
            let (result, ()) = bounded(async {
                tokio::join!(evaluate(source, executor, context), async {
                    bounded(entered.notified()).await;
                    cancellation.cancel();
                })
            })
            .await;
            assert_eq!(result.unwrap_err().diagnostic().cause, Cause::Cancelled);
            assert!(ended.load(std::sync::atomic::Ordering::SeqCst), "{source}");
        }
        let unawaited = bounded(plain("sleep(60000); return 'finished';"));
        assert_eq!(unawaited.await.unwrap().value["value"], "finished");
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
let denied;
try { deniedResponse.unwrap(); } catch (error) {
  denied = {code:error.code, job:error.job, message:error.message,
            output:error.output ?? null, sameResponse:error.response === deniedResponse};
}
const imported = await import("skyhook:tools").then(() => true, () => false);
return {visible: typeof tool.script, imported, deniedResponse, denied};
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
        let job = &value["deniedResponse"]["id"];
        assert_eq!(
            (&value["denied"]["code"], &value["denied"]["job"]),
            (&json!("permission_denied"), job)
        );
        assert!(value["denied"]["output"].is_null());
        assert_eq!(value["denied"]["sameResponse"], true);
        assert!(
            value["denied"]["message"]
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
        assert!(matches!(diagnostic.cause, Cause::Message(_)));
        // The failed response is referenced by its job, not embedded again.
        let failure = &output.value["failure"];
        assert_eq!(failure["code"], "permission_denied");
        assert_eq!(
            failure.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["job", "code"]
        );
    }

    #[tokio::test]
    async fn work_pool_accepts_native_arrays_completion_order_and_early_close() {
        let output = plain(
            r#"
const values = [2, 3];
const pooled = [], settled = [];
for await (const result of new WorkPool(2).map(values, async value => value * 2)) pooled.push(result);
const tasks = values.map(value => async () => {
  if (value === 3) throw new Error("three");
  return value * 3;
});
for await (const result of new WorkPool(2).run(tasks)) settled.push(result);
let rejected = false;
try { new WorkPool(1).map(values, null); } catch (error) { rejected = error instanceof TypeError; }
return {values, pooled, settled, rejected};
"#,
        )
        .await
        .unwrap();
        assert_eq!(
            output.value["value"],
            json!({
                "values": [2, 3],
                "pooled": [{ "index":0, "value":4 }, { "index":1, "value":6 }],
                "settled": [{"index":0,"value":6}],
                "rejected": true,
            })
        );
        let output = plain(
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
        )
        .await
        .unwrap();
        let value = &output.value["value"];
        assert_eq!(value["first"]["value"]["index"], 1);
        assert_eq!(value["second"]["value"]["index"], 2);
        assert_eq!(value["started"], json!([0, 1, 2]));
        assert_eq!(
            output.value["console"],
            "WorkPool item 0 failed: late failure\n"
        );
    }

    #[tokio::test]
    async fn script_failures_report_submitted_source_positions_only() {
        for (source, expected) in [
            (
                "throw new TypeError('boom');",
                "TypeError: boom at <anonymous> (skyhook-script:1:11)",
            ),
            (
                "\n\nfunction fail() {\n  throw new Error('boom');\n}\nfail();",
                "boom at fail (skyhook-script:4:13) at <anonymous> (skyhook-script:6:1)",
            ),
            (
                "return {nested: {stdout: undefined}};",
                "the returned value has undefined at $.nested.stdout",
            ),
            // A failing getter does not replace the error that was thrown.
            (
                "const error = new Error('primary'); Object.defineProperty(error, 'hostile', \
                 {enumerable:true, get() {throw new Error('secondary');}}); throw error;",
                "primary at <anonymous> (skyhook-script:1:19)",
            ),
        ] {
            let (diagnostic, _) = plain(source).await.unwrap_err().into_parts();
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
                   "stack":"    at <anonymous> (skyhook-script:1:45)"}})
        );
        // A returned value that does not convert keeps what did.
        let (_, Some(output)) = plain("return {kept: 1, nested: {stdout: undefined}};")
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
        // Input that closes the wrapper runs before the body, not at compilation.
        let diagnostic = plain("}); throw new Error('escaped'); (0, async () => {")
            .await
            .unwrap_err()
            .diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Execute);
        assert_eq!(diagnostic.context.effects, Effects::Unknown);
        for (source, location) in [
            ("\n\nconst value = ;", "skyhook-script:3:15"),
            ("if (true) {", "end of source"),
        ] {
            let diagnostic = plain(source).await.unwrap_err().diagnostic();
            assert_eq!(diagnostic.context.operation, Operation::Validate);
            assert_eq!(diagnostic.context.effects, Effects::NotStarted);
            assert!(
                matches!(&diagnostic.cause, Cause::Message(message)
                    if message.starts_with("SyntaxError: ") && message.ends_with(&format!("at {location}"))),
                "{diagnostic:?}"
            );
        }
        // A host script's failure is its execution's, at the host.
        let runtime = TestRuntime::new().await;
        let (executor, _slot) =
            crate::tests::tool_executor(runtime.jobs.clone(), runtime.root.path());
        let source = json!({"source": "return {nested:NaN};"});
        let failed = executor.run_host(&runtime.agent, "script", source).await;
        let diagnostic = failed.unwrap_err().diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Execute);
        assert_eq!(diagnostic.context.site, FailureSite::Host);
        assert!(
            matches!(&diagnostic.cause, Cause::Message(message) if message.contains("a non-finite number at $.nested"))
        );
    }

    #[tokio::test]
    async fn console_replaces_unpaired_surrogates() {
        let output = plain(r"console.log('\uD800', new Error('\uDC00')); return 1;")
            .await
            .unwrap();
        assert_eq!(
            output.value["console"],
            "\u{fffd} \u{fffd}\n    at <anonymous> (skyhook-script:1:27)\n"
        );
    }

    #[tokio::test]
    async fn script_capture_initialization_failure_is_host_io_before_javascript() {
        let (_scope, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let _reserved = context
            .text_capture(crate::job::output::TextCaptureField::Console)
            .await
            .unwrap();
        let error = evaluate_captured(
            "throw new Error('must not execute');".into(),
            executor,
            context,
        )
        .await
        .unwrap_err();
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::CreateCapture);
        assert_eq!(diagnostic.context.effects, Effects::NotStarted);
        assert!(matches!(diagnostic.cause, Cause::Io { .. }));
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
        assert!(matches!(
            diagnostic.cause,
            Cause::Io {
                kind: IoKind::BrokenPipe,
                ..
            }
        ));
        assert_eq!(output.unwrap().value, json!({"value":{"computed":42}}));
    }

    // Standalone wrapper regressions: no executor, jobs, or external processes are needed.
    const BUILDERS: &str = r#"[
    {"binding":"top_level","name":"echo","properties":["value"]},
    {"binding":"job_method","name":"job_send","method":"send","job_argument":"job","properties":["value"]},
    {"binding":"job_method","name":"job_cancel","method":"cancel","job_argument":"job","properties":[]}
]"#;

    const MOCK_HOST: &str = r#"
const __testCalls = [];
function __skyhookHostCall(encoded) {
  const request = JSON.parse(encoded);
  __testCalls.push(request);
  if (request.type === "receive") {
    return JSON.stringify({outcome: "ok", value: {id: 99, state: "completed"}, presented: {}});
  }
  const presented = {"/result/value": "complete"};
  if (Array.isArray(request.arguments.value)) presented["/result/value/0"] = "ends";
  return JSON.stringify({outcome: "ok", value: {
    id: __testCalls.length, state: "completed", result: request.arguments,
  }, presented});
}
function __skyhookConsoleLog(message) {
  throw new Error(`unexpected console output: ${message}`);
}
"#;

    fn evaluate_wrapper_envelope(source: &str) -> Value {
        let runtime = Runtime::new().expect("standalone QuickJS runtime");
        // A Tokio timeout cannot interrupt a synchronous QuickJS scheduling loop. In
        // particular, a mutable zero/NaN pool limit used to spin without yielding.
        let deadline = Instant::now() + Duration::from_secs(5);
        runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
        let context = Context::full(&runtime).expect("standalone QuickJS context");
        context.with(|ctx| {
            ctx.eval::<(), _>(MOCK_HOST)
                .catch(&ctx)
                .expect("install mock host");
            let entry = engine::load(&ctx).expect("load runtime modules");
            let install: Function = entry.get("install").unwrap();
            let run: Function = entry.get("run").unwrap();
            let source = SubmittedSource::new(source);
            let layout = serde_json::to_string(source.layout()).unwrap();
            let config = ctx
                .json_parse(format!(r#"{{"builders":{BUILDERS},"source":{layout}}}"#))
                .unwrap();
            let host: Function = ctx.eval("request => __skyhookHostCall(request)").unwrap();
            let sleep: Function = ctx.eval("() => Promise.resolve(true)").unwrap();
            let log: Function = ctx.eval("text => __skyhookConsoleLog(text)").unwrap();
            install
                .call::<_, ()>((config, host, sleep, log))
                .catch(&ctx)
                .unwrap();
            let (module, evaluated) = source
                .declare(&ctx)
                .and_then(|module| module.eval())
                .catch(&ctx)
                .unwrap();
            evaluated.finish::<()>().catch(&ctx).unwrap();
            let body: Function = module.get("default").unwrap();
            let promise: Promise = run
                .call((body,))
                .catch(&ctx)
                .expect("evaluate body before watchdog deadline");
            let encoded: String = promise
                .finish()
                .catch(&ctx)
                .expect("finish wrapper before watchdog deadline without pending host work");
            serde_json::from_str(&encoded).expect("wrapper JSON result")
        })
    }

    fn evaluate_wrapper(source: &str) -> Value {
        let result = evaluate_wrapper_envelope(source);
        assert!(
            result.get("failure").is_none(),
            "wrapper failed: {result:#}"
        );
        result["value"].clone()
    }

    #[test]
    fn presented_fields_follow_full_envelopes_and_extracted_native_payloads() {
        let result = evaluate_wrapper_envelope(
            r#"
const full = await tool.echo({value:["full"]});
const extracted = (await tool.echo({value:["extracted"]})).result.value;
return {full, extracted};
"#,
        );
        assert_eq!(
            result["presented"],
            json!({
                "/result/value/full/result/value": "complete",
                "/result/value/full/result/value/0": "ends",
                "/result/value/extracted": "complete",
                "/result/value/extracted/0": "ends",
            })
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
return {
  awaited: [Promise.resolve(1), (async () => ({inner: tool.echo({value: "nested"})}))(),
            Object.assign(() => 0, {then: resolve => resolve(2)})],
  error, text: "\uD800", "a.b": [null],
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
            })
        );
        // Each member that does not convert is reported and kept as null.
        let failed = evaluate_wrapper_envelope(
            r#"
const keys = new Error("keys");
keys["\uD800"] = 1;
let deep = [];
for (let i = 0; i < 100; i++) deep = [deep];
return {kept: 1, list: [1, , 2], a: {b: undefined}, "b.c": new Date(0), keys, deep,
        get thrower() { throw new TypeError("boom"); },
        odd: Object.create({constructor: {name: "\uD800"}}),
        rejected: Promise.reject(new Error("no"))};
"#,
        );
        let message = failed["failure"]["message"].as_str().unwrap();
        assert!(
            message.starts_with(
                r#"the returned value has undefined at $.list[1]; undefined at $.a.b; a Date at $["b.c"]; an unpaired surrogate in a key at $.keys; nesting deeper than 100 at $.deep[0][0][0][0][0][0][0][0][0][0][0][0][0][0]…; a throwing property"#
            ),
            "{message}"
        );
        for problem in [
            "; a throwing property at $.thrower: TypeError: boom;",
            "; a \u{fffd} at $.odd;",
            "; a rejected promise at $.rejected: no",
        ] {
            assert!(message.contains(problem), "{problem}: {message}");
        }
        let value = &failed["value"];
        for member in ["thrower", "odd", "rejected"] {
            assert_eq!(value.get(member), Some(&Value::Null), "{member}");
        }
        assert_eq!(value["list"], json!([1, null, 2]));
        assert_eq!(value["a"], json!({"b": null}));
        assert_eq!(value["keys"], json!({"name":"Error", "message":"keys"}));
        assert_eq!(value["kept"], 1);
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
  () => tool.job(7).send({job: 8, value: 1}),
  () => tool.job(7).send(Promise.resolve({value: 1})),
  () => tool.job(7).cancel(new Date()),
]) {
  try { make(); rejected.push(null); } catch (error) { rejected.push(error.message); }
}
await tool.echo({value: {kept: 1, omitted: undefined}});
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
                    "tool.job(id).send cannot override its bound job ID",
                    "tool.job(id).send argument has an unawaited promise or tool call at $",
                    "tool.job(id).cancel argument has a Date at $",
                ],
                "calls": [{"type":"call", "name":"echo", "arguments":{"value":{"kept":1}}}],
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
        let value = &envelope["value"];
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
    }

    /// An inspected result fills from its own invocation's schema, here a JobView
    /// rather than the listing the `jobs` tool declares, and only when shown whole.
    #[tokio::test]
    async fn inspected_results_fill_from_their_invocations_schema() {
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

    #[test]
    fn bound_job_normal_calls_and_immutable_chains_keep_receiver() {
        let result = evaluate_wrapper(
            r#"
const base = tool.job(7).send();
const first = base.value("first");
const second = first.set("value", "second");
await first;
await first; // The same operation executes only once.
await second;
await tool.job(8).send({value: "object"});
await tool.job(10).cancel();
await tool.job(1).cancel();
await tool.job(Number.MAX_SAFE_INTEGER).cancel();
return __testCalls;
"#,
        );
        assert_eq!(
            result,
            json!([
                {"type":"call", "name":"job_send", "arguments":{"job":7,"value":"first"}},
                {"type":"call", "name":"job_send", "arguments":{"job":7,"value":"second"}},
                {"type":"call", "name":"job_send", "arguments":{"job":8,"value":"object"}},
                {"type":"call", "name":"job_cancel", "arguments":{"job":10}},
                {"type":"call", "name":"job_cancel", "arguments":{"job":1}},
                {"type":"call", "name":"job_cancel", "arguments":{"job":9_007_199_254_740_991_u64}}
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
        let source = format!("{read} {read} await sleep(3600000);");
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
        assert!(bounded(script).await.unwrap().is_err());
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
        for _ in 0..2 {
            let output = executor
                .run_model(agent, "jobs", json!({"job":launched.job}))
                .await;
            let output = output.unwrap().output;
            assert_eq!(output.value["state"], "completed");
            // Returning a tool response preserves its canonical read JobView.
            assert_eq!(output.value["result"]["value"]["result"]["kind"], "image");
            assert_eq!(output.value["result"].get("console"), None);
            assert_loaded(&runtime.store, output.images).await;
        }
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
