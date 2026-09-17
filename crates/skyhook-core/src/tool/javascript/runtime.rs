//! `QuickJS` runtime implementation and host bridge.

use std::sync::Arc;

use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, Function, Promise, context::intrinsic,
    function::Async,
};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;

use super::{
    bridge::{HostResponse, SourceProvenance},
    console::ConsoleOutput,
};

use crate::{
    media::ImageRef,
    tool::executor::ToolExecutor,
    tool::{ToolContext, ToolOutput},
};

const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HostRequest {
    Call { name: String, arguments: Value },
    Receive,
}

#[derive(Debug, Error)]
pub enum JsError {
    #[error("JavaScript source exceeds the {MAX_SOURCE_BYTES}-byte limit")]
    SourceTooLarge,
    #[error("QuickJS initialization failed: {0}")]
    Initialization(String),
    #[error("QuickJS execution failed: {0}")]
    Execution(String),
    #[error("QuickJS execution was cancelled")]
    Cancelled,
    #[error("{message}")]
    Failure { message: String, details: Value },
    #[error("JavaScript returned invalid JSON: {0}")]
    InvalidOutput(String),
}

/// Evaluation errors retain finalized console evidence until the builtin projects
/// its failure result. This is private artifact ownership, not a public JS error shape.
#[derive(Debug, Error)]
#[error("{error}")]
pub(crate) struct CapturedJsError {
    pub(crate) error: JsError,
    pub(crate) console: Option<Box<crate::job::output::CompletedCapture>>,
}

/// Execute into the owning job's capture; native callers hydrate only when collecting the job.
pub(crate) async fn evaluate_captured(
    source: String,
    executor: ToolExecutor,
    context: ToolContext,
) -> Result<ToolOutput, CapturedJsError> {
    let uncaptured = |error| CapturedJsError {
        error,
        console: None,
    };
    let capture = context
        .text_capture(crate::job::output::TextCaptureField::Console)
        .await
        .map_err(|error| uncaptured(JsError::Execution(error.to_string())))?;
    let console = Arc::new(std::sync::Mutex::new(ConsoleOutput::new(capture.open())));
    let result = evaluate_inner(source, executor, context, console.clone()).await;
    let console = console
        .lock()
        .expect("console lock poisoned")
        .finish()
        .map_err(|error| uncaptured(JsError::Execution(error.to_string())))?;
    match result {
        Ok(mut output) => {
            let result = super::result::script_output(output.value, None, console);
            output.value = result.value;
            output.captures.extend(result.captures);
            Ok(output)
        }
        Err(error) => Err(CapturedJsError {
            error,
            console: console.map(Box::new),
        }),
    }
}

async fn evaluate_inner(
    source: String,
    executor: ToolExecutor,
    context: ToolContext,
    console: Arc<std::sync::Mutex<ConsoleOutput>>,
) -> Result<ToolOutput, JsError> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(JsError::SourceTooLarge);
    }
    let runtime =
        AsyncRuntime::new().map_err(|error| JsError::Initialization(error.to_string()))?;
    let cancellation = context.clone();
    runtime
        .set_interrupt_handler(Some(Box::new(move || cancellation.is_cancelled())))
        .await;
    let js_context = AsyncContext::builder()
        .with::<intrinsic::Eval>()
        .with::<intrinsic::Date>()
        .with::<intrinsic::RegExpCompiler>()
        .with::<intrinsic::RegExp>()
        .with::<intrinsic::TypedArrays>()
        .with::<intrinsic::Performance>()
        .with::<intrinsic::Proxy>()
        .with::<intrinsic::Promise>()
        .with::<intrinsic::Json>()
        .with::<intrinsic::MapSet>()
        .build_async(&runtime)
        .await
        .map_err(|error| JsError::Initialization(error.to_string()))?;
    let surface = Arc::new(executor.surface_for_agent(context.agent()));
    let builders = surface.script_manifests();
    let builders = serde_json::to_string(&builders)
        .map_err(|error| JsError::Initialization(error.to_string()))?;
    let source = source.trim();
    let script = wrapper_script(source, &builders);
    let marker_end = script
        .find(USER_SOURCE_MARKER)
        .expect("script wrapper contains its source marker")
        + USER_SOURCE_MARKER.len();
    let user_start_line = script[..marker_end]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1;
    let user_line_count = source.lines().count().max(1);
    let images = Arc::new(Mutex::new(Vec::<ImageRef>::new()));
    let returned_images = images.clone();
    let cancelled = context.clone();
    let presentation_jobs = executor.jobs().clone();
    let script_job = context.job();
    let execution = js_context.async_with(async move |js| {
        let sleep_context = context.clone();
        let sleep = Function::new(js.clone(), Async(move |milliseconds: f64| {
            let context = sleep_context.clone();
            async move {
                if !milliseconds.is_finite() || milliseconds < 0.0 {
                    return Err(bridge_error(&"sleep(ms) requires finite nonnegative milliseconds"));
                }
                let duration = std::time::Duration::try_from_secs_f64(milliseconds / 1000.0)
                    .map_err(|_| bridge_error(&"sleep(ms) requires finite nonnegative milliseconds within the supported timer range"))?;
                let deadline = tokio::time::Instant::now().checked_add(duration)
                    .ok_or_else(|| bridge_error(&"sleep(ms) exceeds the supported timer range"))?;
                tokio::select! {
                    biased;
                    () = context.cancelled() => Err(bridge_error(&"sleep was cancelled")),
                    () = tokio::time::sleep_until(deadline) => Ok(()),
                }
            }
        }))
        .map_err(|error| error.to_string())?;
        js.globals().set("sleep", sleep).map_err(|error| error.to_string())?;
        let log = Function::new(js.clone(), move |text: String| {
            console.lock().expect("console lock poisoned").log(&text).map_err(|e| bridge_error(&e))
        })
        .map_err(|error| error.to_string())?;
        js.globals()
            .set("__skyhookConsoleLog", log)
            .map_err(|error| error.to_string())?;
        let host_context = context.clone();
        let host_executor = executor.clone();
        let host_images = images.clone();
        let host_call = Function::new(
            js.clone(),
            Async(move |request: String| {
                let host_context = host_context.clone();
                let host_executor = host_executor.clone();
                let host_images = host_images.clone();
                let surface = surface.clone();
                async move {
                    let request: HostRequest =
                        serde_json::from_str(&request).map_err(|error| bridge_error(&error))?;
                    let response = match request {
                        HostRequest::Call { name, arguments } => {
                            let tool = surface.get(&name);
                            let schema = tool.and_then(|tool| tool.result_schema.as_ref());
                            match host_executor
                                .execute_script(
                                    host_context.agent().clone(),
                                    &name,
                                    arguments,
                                    Some(host_context.job()),
                                )
                                .await
                            {
                                Ok(result) => {
                                    // Job output queries already return views; background
                                    // calls return handles rather than completed tool data.
                                    let native = tool.is_none_or(|tool| tool.result_policy != crate::tool::ToolResultPolicy::JobView);
                                    let provenance = (!result.background && native).then(|| SourceProvenance {
                                        source_job: result.job,
                                        annotations: schema.map(|schema| crate::job::output::annotated_fields(&result.output.value, schema)).unwrap_or_default(),
                                    });
                                    host_images.lock().await.extend(result.output.images);
                                    HostResponse::Success { value: result.output.value, provenance }
                                }
                                Err(error) => {
                                    let failure = error.into_failure();
                                    let output = if let Some(output) = failure.output {
                                        host_images.lock().await.extend(output.images);
                                        Some(output.value)
                                    } else { None };
                                    HostResponse::Failure { message: failure.message, denial: failure.denial, output }
                                }
                            }
                        }
                        HostRequest::Receive => {
                            let result = match host_executor.jobs().is_background(host_context.job()).await {
                                Ok(true) => host_context.receive().await.map_err(|error| error.to_string()),
                                Ok(false) => Err("receive() requires the script tool to be invoked with bg: true".to_owned()),
                                Err(error) => Err(error.to_string()),
                            };
                            match result {
                                Ok(value) => HostResponse::Success { value, provenance: None },
                                Err(message) => HostResponse::failure(message),
                            }
                        }
                    };
                    response.encode().map_err(|error| bridge_error(&error))
                }
            }),
        )
        .map_err(|error| error.to_string())?;
        js.globals()
            .set("__skyhookHostCall", host_call)
            .map_err(|error| error.to_string())?;
        let promise = js
            .eval::<Promise<'_>, _>(script)
            .catch(&js)
            .map_err(|error| error.to_string())?;
        promise
            .into_future::<String>()
            .await
            .catch(&js)
            .map_err(|error| error.to_string())
    });
    let result = execution.await;
    if cancelled.is_cancelled() {
        return Err(JsError::Cancelled);
    }
    let encoded = result.map_err(|error| {
        JsError::Execution(map_script_lines(&error, user_start_line, user_line_count))
    })?;
    let envelope: super::outcome::Envelope = serde_json::from_str(&encoded)
        .map_err(|error| JsError::InvalidOutput(error.to_string()))?;
    if !envelope.ok {
        let details = envelope.error;
        let message = details.get("message").and_then(Value::as_str).map_or_else(
            || details.to_string(),
            |message| {
                format!(
                    "{message}\n{}",
                    details.get("stack").and_then(Value::as_str).unwrap_or("")
                )
            },
        );
        return Err(JsError::Failure {
            message: map_script_lines(&message, user_start_line, user_line_count),
            details,
        });
    }
    let (value, presentation) = (envelope.value, envelope.presentation);
    presentation_jobs
        .save_script_presentation(script_job, presentation)
        .await
        .map_err(|error| JsError::Execution(error.to_string()))?;
    let mut images = std::mem::take(&mut *returned_images.lock().await);
    images.sort();
    images.dedup();
    Ok(ToolOutput::new(value).with_images(images))
}

fn bridge_error(error: &impl ToString) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("host request", "JavaScript promise", error.to_string())
}

const USER_SOURCE_MARKER: &str = "// __skyhook_user_source__\n";

fn map_script_lines(error: &str, user_start: usize, user_lines: usize) -> String {
    let user_end = user_start.saturating_add(user_lines.saturating_sub(1));
    let mut output = String::with_capacity(error.len());
    let mut remaining = error;
    while let Some((prefix, after)) = remaining.split_once("eval_script:") {
        output.push_str(prefix);
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        let line = after[..digits].parse::<usize>().ok();
        if let Some(line) = line.filter(|line| (*line >= user_start) && (*line <= user_end)) {
            output.push_str("skyhook-script:");
            output.push_str(&(line - user_start + 1).to_string());
        } else {
            output.push_str("eval_script:");
            output.push_str(&after[..digits]);
        }
        remaining = &after[digits..];
    }
    output.push_str(remaining);
    output
}

pub(super) fn wrapper_script(source: &str, builders: &str) -> String {
    let runtime = include_str!("runtime.js");
    format!(
        "const __builders = {builders};\n{runtime}\n\
         (async () => {{\n\
         \"use strict\";\n\
         try {{\n\
         const value = await (async () => {{\n\
         {USER_SOURCE_MARKER}{source}\n\
         }})();\n\
         const presentation = {{jobs:Object.create(null), fields:[]}};\n\
         const resolved = await __resolve(value, \"$\", new Set(), \"/result/value\", presentation);\n\
         return __stringify({{ok:true, value:resolved, presentation}});\n\
         }} catch (error) {{ return __stringify({{ok:false, error:__describeError(error)}}); }}\n\
         }})()\n"
    )
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
    use crate::tool::{ToolOptions, ToolRegistryBuilder, executor::ToolExecutor};

    // Tests may eagerly inspect the console; production hydrates job captures on collection.
    async fn evaluate(
        source: impl Into<String>,
        executor: ToolExecutor,
        context: ToolContext,
    ) -> Result<ToolOutput, JsError> {
        let saved = executor.jobs().output(context.job());
        let captured = evaluate_captured(source.into(), executor, context).await;
        let mut output = captured.map_err(|captured| captured.error)?;
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
        // Keep startup alive while the test drives evaluation directly.
        _lease: crate::job::JobLease,
        _root: tempfile::TempDir,
    }

    async fn test_runtime(builder: ToolRegistryBuilder) -> (TestScope, ToolExecutor, ToolContext) {
        let runtime = TestRuntime::new().await;
        let spec = crate::job::JobSpec::test(runtime.agent.clone(), "script");
        let mut lease = runtime.jobs.create(spec).await.unwrap();
        let executor = runtime.executor(builder);
        let context = runtime.tool_context(&mut lease);
        (
            TestScope {
                _lease: lease,
                _root: runtime.root,
            },
            executor,
            context,
        )
    }

    /// Evaluates `source` against an executor with no registered tools.
    async fn plain(source: &str) -> Result<ToolOutput, JsError> {
        let (_scope, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        evaluate(source, executor, context).await
    }

    #[tokio::test]
    async fn local_source_and_sleep_limits_fail_before_user_effects() {
        let oversized = " ".repeat(MAX_SOURCE_BYTES + 1);
        assert!(matches!(
            plain(&oversized).await,
            Err(JsError::SourceTooLarge)
        ));
        let output = plain(
            r#"
            const errors = [];
            for (const milliseconds of [-1, NaN, Infinity, Number.MAX_VALUE]) {
                try { await sleep(milliseconds); throw new Error("invalid sleep admitted"); }
                catch (error) { errors.push(String(error).includes("sleep(ms)")); }
            }
            await sleep(0);
            return errors;
        "#,
        )
        .await
        .unwrap();
        assert_eq!(output.value["value"], json!([true, true, true, true]));
    }

    #[tokio::test(start_paused = true)]
    async fn sleep_stops_on_cancellation_and_unawaited_sleep_does_not_keep_a_script_alive() {
        let (_scope, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let cancellation = context.cancellation_token();
        let started = tokio::time::Instant::now();
        let source = "await sleep(60000); return 'finished';";
        let (result, ()) = tokio::join!(evaluate(source, executor, context), async {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            cancellation.cancel();
        });
        assert!(matches!(result, Err(JsError::Cancelled)));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let unawaited = plain("sleep(60000); return 'finished';");
        let output = tokio::time::timeout(std::time::Duration::from_millis(100), unawaited);
        assert_eq!(output.await.unwrap().unwrap().value["value"], "finished");
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
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            evaluate(source, executor, context),
        );
        let output = output
            .await
            .expect("independent builders did not run concurrently")
            .unwrap();
        assert_eq!(output.value["value"], json!(["a", "a", "a"]));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
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
                |_, _| async { Err(crate::tool::ToolError::Denied("user reason".to_owned())) },
            )
            .unwrap();
        let (_scope, executor, context) = test_runtime(builder).await;
        let source = r#"
const direct = JSON.parse(await __skyhookHostCall(JSON.stringify({
  type: "call",
  name: "script",
  arguments: {source: "return null;"},
})));
let denied;
try { await tool.deny({value:"x"}); } catch (error) { denied = {code:error.code, executed:error.executed, message:error.message}; }
return {visible: typeof tool.script, direct, denied};
"#;
        let output = evaluate(source, executor.clone(), context.clone())
            .await
            .unwrap();
        let value = &output.value["value"];
        assert_eq!(value["visible"], "undefined");
        assert_eq!(value["direct"]["ok"], false);
        assert!(
            value["direct"]["error"]
                .as_str()
                .unwrap()
                .contains("not available in scripts")
        );
        assert_eq!(
            (&value["denied"]["code"], &value["denied"]["executed"]),
            (&json!("permission_denied"), &json!(false))
        );
        assert!(
            value["denied"]["message"]
                .as_str()
                .unwrap()
                .contains("user reason")
        );
        let error = evaluate("await tool.deny({value:'x'});", executor, context)
            .await
            .unwrap_err();
        let JsError::Failure { details, .. } = error else {
            panic!("expected nested failure details")
        };
        assert_eq!(
            (&details["code"], &details["executed"]),
            (&json!("permission_denied"), &json!(false))
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
return {values, pooled, settled};
"#,
        )
        .await
        .unwrap();
        assert_eq!(
            output.value["value"],
            json!({
                "values": [2, 3],
                "pooled": [{ "index":0, "value":4 }, { "index":1, "value":6 }],
                "settled": [{"index":0,"value":6}]
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
    async fn script_errors_preserve_captured_console() {
        let runtime = TestRuntime::new().await;
        let mut builder = ToolRegistryBuilder::default();
        let slot = Arc::new(std::sync::OnceLock::new());
        crate::tool::builtins::install_script_tool(&mut builder, Arc::downgrade(&slot)).unwrap();
        let executor = runtime.executor(builder);
        slot.set(executor.clone()).ok().unwrap();
        for (source, expected) in [
            (
                "console.log('before error'); throw new Error('boom');",
                "boom",
            ),
            (
                "console.log('before error'); return {nested:undefined};",
                "undefined at $.nested",
            ),
        ] {
            let error = executor.run_host(&runtime.agent, "script", json!({"source": source}));
            let crate::tool::executor::ExecutionError::Failed {
                message,
                output: Some(output),
            } = error.await.unwrap_err()
            else {
                panic!("expected script failure with captured output");
            };
            assert_eq!(output.value["console"], "before error\n");
            assert!(message.contains(expected));
        }
    }

    // Standalone wrapper regressions: no executor, jobs, or external processes are needed.
    const BUILDERS: &str = r#"[
    {"binding":"top_level","name":"echo","properties":["value"],"required":["value"]},
    {"binding":"job_method","name":"job_send","method":"send","job_argument":"job","properties":["job","value"],"required":["value"]},
    {"binding":"job_method","name":"job_output","method":"output","job_argument":"job","properties":["job","field"],"required":[]},
    {"binding":"job_method","name":"job_cancel","method":"cancel","job_argument":"job","properties":["job"],"required":[]}
]"#;

    const MOCK_HOST: &str = r#"
const __testCalls = [];
function __skyhookHostCall(encoded) {
  const request = JSON.parse(encoded);
  __testCalls.push(request);
  return JSON.stringify({ok: true, value: request.arguments});
}
function __skyhookConsoleLog(message) {
  throw new Error(`unexpected console output: ${message}`);
}
"#;

    fn evaluate_wrapper(source: &str) -> Value {
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
            let promise: Promise = ctx
                .eval(wrapper_script(source, BUILDERS))
                .catch(&ctx)
                .expect("evaluate wrapper before watchdog deadline");
            let encoded: String = promise
                .finish()
                .catch(&ctx)
                .expect("finish wrapper before watchdog deadline without pending host work");
            let result: Value = serde_json::from_str(&encoded).expect("wrapper JSON result");
            assert_eq!(result["ok"], true, "wrapper failed: {result:#}");
            result["value"].clone()
        })
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
await tool.job(9).output({field: "/result/value"});
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
                {"type":"call", "name":"job_output", "arguments":{"job":9,"field":"/result/value"}},
                {"type":"call", "name":"job_cancel", "arguments":{"job":10}},
                {"type":"call", "name":"job_cancel", "arguments":{"job":1}},
                {"type":"call", "name":"job_cancel", "arguments":{"job":9_007_199_254_740_991_u64}}
            ])
        );
    }
}
