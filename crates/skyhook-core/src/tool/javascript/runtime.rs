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

use super::console::ConsoleOutput;

use crate::{
    media::ImageReference,
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
    #[error("{error}")]
    WithConsole {
        error: Box<Self>,
        console_output: String,
    },
}

pub async fn evaluate(
    source: String,
    executor: ToolExecutor,
    context: ToolContext,
) -> Result<ToolOutput, JsError> {
    let console = Arc::new(std::sync::Mutex::new(ConsoleOutput::default()));
    let result = evaluate_inner(source, executor, context, console.clone()).await;
    let console_output = console.lock().expect("console lock poisoned").take();
    match result {
        Ok(mut output) => {
            output.console_output = console_output;
            Ok(output)
        }
        Err(error) if console_output.is_empty() => Err(error),
        Err(error) => Err(JsError::WithConsole {
            error: Box::new(error),
            console_output,
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
    let builders = executor.surface().script_manifests();
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
    let images = Arc::new(Mutex::new(Vec::<ImageReference>::new()));
    let returned_images = images.clone();
    let cancelled = context.clone();
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
            console.lock().expect("console lock poisoned").log(&text);
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
                async move {
                    let request: HostRequest =
                        serde_json::from_str(&request).map_err(|error| bridge_error(&error))?;
                    let mut failure_output = None;
                    let mut denial = None;
                    let result = match request {
                        HostRequest::Call { name, arguments } => {
                            match host_executor
                                .execute_script(
                                    host_context.agent.clone(),
                                    &name,
                                    arguments,
                                    Some(host_context.job),
                                )
                                .await
                            {
                                Ok(result) => {
                                    host_images
                                        .lock()
                                        .await
                                        .extend(result.output.images.iter().cloned());
                                    Ok(result.output.value)
                                }
                                Err(error) => {
                                    let failure = error.into_failure();
                                    denial = failure.denial;
                                    if let Some(output) = failure.output {
                                        host_images.lock().await.extend(output.images);
                                        failure_output = Some(output.value);
                                    }
                                    Err(failure.message)
                                }
                            }
                        }
                        HostRequest::Receive => {
                            match host_executor.jobs().is_background(host_context.job).await {
                                Ok(true) => host_context
                                    .receive()
                                    .await
                                    .map_err(|error| error.to_string()),
                                Ok(false) => Err(
                                    "receive() requires the script tool to be invoked with bg: true"
                                        .to_owned(),
                                ),
                                Err(error) => Err(error.to_string()),
                            }
                        }
                    };
                    let mut response = match result {
                        Ok(value) => serde_json::json!({"ok": true, "value": value}),
                        Err(error) => serde_json::json!({"ok": false, "error": error}),
                    };
                    if let Some(denial) = denial {
                        response["code"] = serde_json::json!(denial.code);
                        response["executed"] = serde_json::json!(denial.executed);
                    }
                    if let Some(output) = failure_output {
                        response["output"] = output;
                    }
                    serde_json::to_string(&response).map_err(|error| bridge_error(&error))
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
    let result: Value = serde_json::from_str(&encoded)
        .map_err(|error| JsError::InvalidOutput(error.to_string()))?;
    if result["ok"] == false {
        let details = result["error"].clone();
        let message = details.get("message").and_then(Value::as_str).map_or_else(
            || details.to_string(),
            |message| format!("{message}\n{}", details["stack"].as_str().unwrap_or("")),
        );
        return Err(JsError::Failure {
            message: map_script_lines(&message, user_start_line, user_line_count),
            details,
        });
    }
    let value = result["value"].clone();
    let mut images = returned_images.lock().await.clone();
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

fn wrapper_script(source: &str, builders: &str) -> String {
    let runtime = include_str!("runtime.js");
    format!(
        "const __builders = {builders};\n{runtime}\n\
         (async () => {{\n\
         \"use strict\";\n\
         try {{\n\
         const value = await (async () => {{\n\
         {USER_SOURCE_MARKER}{source}\n\
         }})();\n\
         return __stringify({{ok:true, value:await __resolve(value, \"$\", new Set())}});\n\
         }} catch (error) {{ return __stringify({{ok:false, error:__describeError(error)}}); }}\n\
         }})()\n"
    )
}

#[cfg(test)]
mod tests {
    use crate::test_support::TestRuntime;
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::*;
    use crate::tool::{ToolOptions, ToolRegistryBuilder, executor::ToolExecutor};

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Echo {
        value: String,
    }

    #[derive(Deserialize, JsonSchema, serde::Serialize)]
    #[serde(deny_unknown_fields)]
    struct Defaults {
        required: String,
        #[serde(default = "default_limit")]
        limit: usize,
    }

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct SkillCall {
        name: String,
        path: Option<String>,
        to: Option<String>,
    }

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct NestedScript {
        source: String,
    }

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct TestJobArgs {
        job: u64,
    }

    const fn default_limit() -> usize {
        7
    }

    async fn test_runtime(
        builder: ToolRegistryBuilder,
    ) -> (tempfile::TempDir, ToolExecutor, ToolContext) {
        let runtime = TestRuntime::new().await;
        let agent = runtime.agent.clone();
        let jobs = runtime.jobs.clone();
        let lease = jobs
            .create(crate::job::JobSpec::test(agent.clone(), "script"))
            .await
            .unwrap();
        let executor = runtime.executor(builder);
        let context = ToolContext::new(
            crate::tool::authorization::AuthorizationSubject {
                agent,
                job: lease.id,
                parent: None,
                scope: None,
                capabilities: crate::tool::policy::CapabilitySet::default(),
                cancellation: lease.cancellation.clone(),
            },
            crate::execution::ExecutionLocation::root(runtime.root.path().to_path_buf()),
            crate::execution::ExecutionLocation::root(runtime.root.path().to_path_buf()),
            lease.input,
            jobs.clone(),
        );
        (runtime.root, executor, context)
    }

    #[tokio::test]
    async fn common_javascript_builtins_are_available() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let output = evaluate(
            r#"
const bytes = new Uint8Array(new ArrayBuffer(4));
const view = new DataView(bytes.buffer);
view.setUint32(0, 0x12345678);
const proxy = new Proxy({value: 21}, {
  get(target, key) { return Reflect.get(target, key) * 2; }
});
const started = performance.now();
return {
  date: new Date('2026-09-05T00:00:00Z').toISOString(),
  timestamp: Number.isFinite(Date.now()),
  matches: 'abc123 def456'.match(/\d+/g),
  constructedRegex: new RegExp('^abc', 'i').test('ABC123'),
  bytes: Array.from(bytes),
  word: view.getUint32(0),
  hex: bytes.toHex(),
  base64: Uint8Array.fromBase64(bytes.toBase64()).toHex(),
  fromHex: Array.from(Uint8Array.fromHex('00ff')),
  bigint: (9007199254740993n + BigInt(2)).toString(),
  proxy: proxy.value,
  monotonic: performance.now() >= started,
  timeOrigin: Number.isFinite(performance.timeOrigin)
};
"#
            .to_owned(),
            executor,
            context,
        )
        .await
        .unwrap();
        assert_eq!(
            output.value,
            serde_json::json!({
                "date": "2026-09-05T00:00:00.000Z",
                "timestamp": true,
                "matches": ["123", "456"],
                "constructedRegex": true,
                "bytes": [18, 52, 86, 120],
                "word": 0x1234_5678,
                "hex": "12345678",
                "base64": "12345678",
                "fromHex": [0, 255],
                "bigint": "9007199254740995",
                "proxy": 42,
                "monotonic": true,
                "timeOrigin": true
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sleep_yields_and_independent_sleeps_overlap() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let started = tokio::time::Instant::now();
        let output = evaluate(
            r#"
const order = [];
await Promise.all([
  (async () => { await sleep(30); order.push('long'); })(),
  (async () => { await sleep(10); order.push('short'); })()
]);
return {order, result: typeof await sleep(0), fractional: typeof await sleep(0.5)};
"#
            .to_owned(),
            executor,
            context,
        )
        .await
        .unwrap();
        assert_eq!(
            output.value,
            serde_json::json!({
                "order": ["short", "long"], "result": "undefined", "fractional": "undefined"
            })
        );
        assert!(started.elapsed() >= std::time::Duration::from_millis(30));
        assert!(started.elapsed() < std::time::Duration::from_millis(40));
    }

    #[tokio::test]
    async fn sleep_rejects_invalid_delays() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let output = evaluate(
            r#"
const rejected = [];
for (const ms of [-1, -Number.MIN_VALUE, NaN, Infinity, -Infinity, 1e300, '10', null, undefined]) {
  try { await sleep(ms); rejected.push(false); }
  catch (error) { rejected.push(true); }
}
return rejected;
"#
            .to_owned(),
            executor,
            context,
        )
        .await
        .unwrap();
        assert_eq!(output.value, serde_json::json!(vec![true; 9]));
    }

    #[tokio::test(start_paused = true)]
    async fn sleep_stops_on_script_cancellation() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let cancellation = context.authorization.cancellation.clone();
        let started = tokio::time::Instant::now();
        let (result, ()) = tokio::join!(
            evaluate(
                "await sleep(60000); return 'finished';".to_owned(),
                executor,
                context
            ),
            async {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                cancellation.cancel();
            }
        );
        assert!(matches!(result, Err(JsError::Cancelled)));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn unawaited_sleep_does_not_keep_a_script_alive() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let output = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            evaluate(
                "sleep(60000); return 'finished';".to_owned(),
                executor,
                context,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(output.value, "finished");
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
        let (_root, executor, context) = test_runtime(builder).await;
        // Two equivalent builders must execute independently; reusing one must not execute again.
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            evaluate(
                "const x=tool.echo({value:'a'}); return [x,x,tool.echo({value:'a'})];".to_owned(),
                executor,
                context,
            ),
        )
        .await
        .expect("independent builders did not run concurrently")
        .unwrap();
        assert_eq!(output.value, serde_json::json!(["a", "a", "a"]));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn schemas_generate_immutable_builders() {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Defaults, Defaults, _, _>(
                "defaults",
                "defaults",
                ToolOptions::default(),
                |_context, input| async move { Ok(input) },
            )
            .unwrap();
        builder
            .register::<SkillCall, Value, _, _>(
                "skill",
                "skill",
                ToolOptions::default(),
                |_context, input| async move {
                    Ok(serde_json::json!({
                        "name": input.name,
                        "path": input.path,
                        "to": input.to,
                    }))
                },
            )
            .unwrap();
        let (_root, executor, context) = test_runtime(builder).await;
        let output = evaluate(
            r#"
const base = tool.defaults();
const built = base.required("x");
return {
  direct: tool.defaults({required:"x"}),
  built,
  set: base.set("required", "y"),
  reused: [built, built],
  skillObject: tool.skill({name:"beta"}),
  skillFluent: tool.skill().name("gamma"),
  asset: tool.skill().name("alpha").path("template.txt"),
};
"#
            .to_owned(),
            executor,
            context,
        )
        .await
        .unwrap();
        assert_eq!(
            output.value["direct"],
            serde_json::json!({"required":"x", "limit":7})
        );
        assert_eq!(output.value["built"], output.value["direct"]);
        assert_eq!(
            output.value["set"],
            serde_json::json!({"required":"y", "limit":7})
        );
        assert_eq!(
            output.value["reused"],
            serde_json::json!([
                {"required":"x", "limit":7}, {"required":"x", "limit":7}
            ])
        );
        assert_eq!(output.value["skillObject"]["name"], "beta");
        assert_eq!(output.value["skillFluent"]["name"], "gamma");
        assert_eq!(output.value["asset"]["path"], "template.txt");
    }

    #[tokio::test]
    async fn script_tool_is_hidden_and_rejected_inside_scripts() {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<NestedScript, Value, _, _>(
                "script",
                "script",
                ToolOptions::default().script_unavailable(),
                |_context, input| async move { Ok(serde_json::json!({"source": input.source})) },
            )
            .unwrap();
        let (_root, executor, context) = test_runtime(builder).await;
        let output = evaluate(
            r#"
const direct = JSON.parse(await __skyhookHostCall(JSON.stringify({
  type: "call",
  name: "script",
  arguments: {source: "return null;"},
})));
return {visible: typeof tool.script, direct};
"#
            .to_owned(),
            executor,
            context,
        )
        .await
        .unwrap();
        assert_eq!(output.value["visible"], "undefined");
        assert_eq!(output.value["direct"]["ok"], false);
        assert!(
            output.value["direct"]["error"]
                .as_str()
                .unwrap()
                .contains("not available in scripts")
        );
    }

    #[tokio::test]
    async fn denied_operations_keep_metadata_in_exceptions_and_uncaught_details() {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Echo, String, _, _>(
                "deny",
                "test denial",
                ToolOptions::default(),
                |_context, _input| async {
                    Err(crate::tool::ToolError::Denied("user reason".to_owned()))
                },
            )
            .unwrap();
        let (_root, executor, context) = test_runtime(builder).await;
        let caught = evaluate(r#"try { await tool.deny({value:"x"}); } catch (error) { return {code:error.code, executed:error.executed, message:error.message}; }"#.to_owned(), executor.clone(), context.clone()).await.unwrap();
        assert_eq!(caught.value["code"], "permission_denied");
        assert_eq!(caught.value["executed"], false);
        assert!(
            caught.value["message"]
                .as_str()
                .unwrap()
                .contains("user reason")
        );
        let error = evaluate(
            "await tool.deny({value:'x'});".to_owned(),
            executor,
            context,
        )
        .await
        .unwrap_err();
        let JsError::Failure { details, .. } = error else {
            panic!("expected nested failure details")
        };
        assert_eq!(details["code"], "permission_denied");
        assert_eq!(details["executed"], false);
    }

    #[tokio::test]
    async fn work_pool_accepts_native_arrays() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let output = evaluate(
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
"#
            .to_owned(),
            executor,
            context,
        )
        .await
        .unwrap();
        assert_eq!(output.value["values"], serde_json::json!([2, 3]));
        assert_eq!(
            output.value["pooled"],
            serde_json::json!([{ "index":0, "value":4 }, { "index":1, "value":6 }])
        );
        assert_eq!(
            output.value["settled"],
            serde_json::json!([{"index":0,"value":6}])
        );
    }

    #[tokio::test]
    async fn work_pool_keeps_captured_state_alive_across_host_calls() {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Echo, String, _, _>(
                "echo",
                "Return the supplied payload after yielding to the host.",
                ToolOptions::default(),
                |_context, args| async move {
                    tokio::task::yield_now().await;
                    Ok(args.value)
                },
            )
            .unwrap();
        let (_root, executor, context) = test_runtime(builder).await;
        let output = evaluate(
            r#"
const payload = "x".repeat(16384);
const results = [];
for await (const {index, value} of new WorkPool(4).map(
  Array.from({length: 128}, (_, index) => index),
  () => tool.echo({value: payload})
)) results.push({index, length: value.length});
return results.sort((a, b) => a.index - b.index);
"#
            .to_owned(),
            executor,
            context,
        )
        .await
        .unwrap();
        let expected = (0..128)
            .map(|index| serde_json::json!({"index": index, "length": 16384}))
            .collect::<Vec<_>>();
        assert_eq!(output.value, serde_json::json!(expected));
    }

    #[tokio::test]
    async fn work_pool_continues_after_failures_and_logs_falsy_errors() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let output = evaluate(
            r#"
const results = [], started = [];
for await (const result of new WorkPool(1).map([0, null, undefined, 7], async (value, index) => {
  started.push(index);
  if (index < 3) throw value;
  return value;
})) results.push(result);
return {started, results};
"#
            .to_owned(),
            executor,
            context,
        )
        .await
        .unwrap();
        assert_eq!(output.value["started"], serde_json::json!([0, 1, 2, 3]));
        assert_eq!(
            output.value["results"],
            serde_json::json!([{"index":3,"value":7}])
        );
        assert_eq!(
            output.console_output,
            "WorkPool item 0 failed: 0\nWorkPool item 1 failed: null\nWorkPool item 2 failed: undefined\n"
        );
    }

    #[tokio::test]
    async fn work_pool_completion_order_and_early_close() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let output = evaluate(
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
"#
            .to_owned(),
            executor,
            context,
        )
        .await
        .unwrap();
        assert_eq!(output.value["first"]["value"]["index"], 1);
        assert_eq!(output.value["second"]["value"]["index"], 2);
        assert_eq!(output.value["started"], serde_json::json!([0, 1, 2]));
        assert_eq!(
            output.console_output,
            "WorkPool item 0 failed: late failure\n"
        );
    }

    #[tokio::test]
    async fn work_pool_logs_all_failures_and_returns_no_results() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let output = evaluate("const results=[]; for await (const result of new WorkPool(2).map([0,1,2,3], value => { throw new Error(`failure ${value}`); })) results.push(result); return results;".to_owned(), executor, context).await.unwrap();
        assert_eq!(output.value, serde_json::json!([]));
        for index in 0..4 {
            assert!(
                output
                    .console_output
                    .contains(&format!("WorkPool item {index} failed: failure {index}\n"))
            );
        }
        assert_eq!(output.console_output.lines().count(), 4);
    }

    #[tokio::test]
    async fn console_formats_values_and_caps_large_capture_without_changing_the_result() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let output = evaluate(
            r#"
const shared = {x: 1}, cycle = {}; cycle.self = cycle;
console.log("hello", undefined, null, 3n, cycle, [shared, shared]);
console.log();
"#
            .to_owned(),
            executor.clone(),
            context.clone(),
        )
        .await
        .unwrap();
        assert_eq!(output.value, Value::Null);
        assert_eq!(
            output.console_output,
            "hello undefined null \"3n\" {\"self\":\"[Circular]\"} [{\"x\":1},{\"x\":1}]\n\n"
        );
        let output = evaluate(
            "console.log('x'.repeat(17 * 1024 * 1024)); console.log('discarded'); return 42;"
                .to_owned(),
            executor,
            context,
        )
        .await
        .unwrap();
        assert_eq!(output.value, serde_json::json!(42));
        let (captured, marker) = output.console_output.split_at(16 * 1024 * 1024);
        assert!(captured.bytes().all(|byte| byte == b'x'));
        assert_eq!(marker, "\n[console output truncated at 16 MiB]\n");
    }

    #[tokio::test]
    async fn output_validation_and_job_errors_are_actionable() {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Echo, String, _, _>(
                "echo",
                "echo",
                ToolOptions::default(),
                |_context, input| async move { Ok(input.value) },
            )
            .unwrap();
        builder
            .register::<TestJobArgs, Value, _, _>(
                "inspect_test",
                "inspect",
                ToolOptions::default()
                    .script_only()
                    .job_method("inspect", "job"),
                |_context, input| async move { Ok(serde_json::json!({"id": input.job})) },
            )
            .unwrap();

        let (_root, executor, context) = test_runtime(builder).await;
        let null = evaluate(
            "return undefined;".to_owned(),
            executor.clone(),
            context.clone(),
        )
        .await
        .unwrap();
        assert_eq!(null.value, Value::Null);

        let nested = evaluate(
            "return {nested: undefined};".to_owned(),
            executor.clone(),
            context.clone(),
        )
        .await
        .unwrap_err();
        assert!(nested.to_string().contains("undefined at $.nested"));

        let unresolved = evaluate(
            "const call = tool.echo({value:'x'}); return tool.job(call).inspect();".to_owned(),
            executor.clone(),
            context.clone(),
        )
        .await
        .unwrap_err();
        assert!(
            unresolved
                .to_string()
                .contains("await the background call and pass result.id")
        );

        let thrown = evaluate("throw new Error('boom');".to_owned(), executor, context)
            .await
            .unwrap_err();
        assert!(thrown.to_string().contains("skyhook-script:1"));
    }
}
