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
}

/// Execute into the owning job's capture; native callers hydrate only when collecting the job.
pub(crate) async fn evaluate_captured(
    source: String,
    executor: ToolExecutor,
    context: ToolContext,
) -> Result<ToolOutput, JsError> {
    let path = context
        .capture_path("/result/console")
        .await
        .map_err(|e| JsError::Execution(e.to_string()))?;
    let console = Arc::new(std::sync::Mutex::new(
        ConsoleOutput::new(&path).map_err(|e| JsError::Execution(e.to_string()))?,
    ));
    let result = evaluate_inner(source, executor, context, console.clone()).await;
    console
        .lock()
        .expect("console lock poisoned")
        .finish()
        .map_err(|e| JsError::Execution(e.to_string()))?;
    result.map(|mut output| {
        output.value = serde_json::json!({"value": output.value, "console": ""});
        output
    })
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
    let surface = Arc::new(executor.surface());
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
    let images = Arc::new(Mutex::new(Vec::<ImageReference>::new()));
    let returned_images = images.clone();
    let cancelled = context.clone();
    let presentation_jobs = executor.jobs().clone();
    let script_job = context.job;
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
                    let mut failure_output = None;
                    let mut denial = None;
                    let mut source_job = None;
                    let mut annotations = Default::default();
                    let result = match request {
                        HostRequest::Call { name, arguments } => {
                            let schema = surface.get(&name).and_then(|tool| tool.result_schema.as_ref());
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
                                    // Job output queries already return views; background
                                    // calls return handles rather than completed tool data.
                                    if !result.background && name != "job_output" {
                                        source_job = Some(result.job);
                                        if let Some(schema) = schema {
                                            annotations = crate::job::output::annotated_fields(&result.output.value, schema);
                                        }
                                    }
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
                    if let Some(job) = source_job {
                        response["source_job"] = serde_json::json!(job);
                        response["annotations"] = serde_json::json!(annotations);
                    }
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
    let presentation = serde_json::from_value(result["presentation"].clone())
        .map_err(|error| JsError::InvalidOutput(error.to_string()))?;
    presentation_jobs
        .save_script_presentation(script_job, presentation)
        .await
        .map_err(|error| JsError::Execution(error.to_string()))?;
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
         const presentation = {{jobs:Object.create(null), fields:[]}};\n\
         const resolved = await __resolve(value, \"$\", new Set(), \"/result/value\", presentation);\n\
         return __stringify({{ok:true, value:resolved, presentation}});\n\
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

    // Tests may eagerly inspect the console; production hydrates job captures on collection.
    async fn evaluate(
        source: String,
        executor: ToolExecutor,
        context: ToolContext,
    ) -> Result<ToolOutput, JsError> {
        let path = context.capture_path("/result/console").await.unwrap();
        let mut output = evaluate_captured(source, executor, context).await?;
        output.value["console"] = Value::String(tokio::fs::read_to_string(path).await.unwrap());
        Ok(output)
    }

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Echo {
        value: String,
    }

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct NestedScript {
        source: String,
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
        assert_eq!(output.value["value"], "finished");
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
        assert_eq!(output.value["value"], serde_json::json!(["a", "a", "a"]));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
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
        assert_eq!(output.value["value"]["visible"], "undefined");
        assert_eq!(output.value["value"]["direct"]["ok"], false);
        assert!(
            output.value["value"]["direct"]["error"]
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
        assert_eq!(caught.value["value"]["code"], "permission_denied");
        assert_eq!(caught.value["value"]["executed"], false);
        assert!(
            caught.value["value"]["message"]
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
        assert_eq!(output.value["value"]["values"], serde_json::json!([2, 3]));
        assert_eq!(
            output.value["value"]["pooled"],
            serde_json::json!([{ "index":0, "value":4 }, { "index":1, "value":6 }])
        );
        assert_eq!(
            output.value["value"]["settled"],
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
        assert_eq!(output.value["value"], serde_json::json!(expected));
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
        assert_eq!(output.value["value"]["first"]["value"]["index"], 1);
        assert_eq!(output.value["value"]["second"]["value"]["index"], 2);
        assert_eq!(
            output.value["value"]["started"],
            serde_json::json!([0, 1, 2])
        );
        assert_eq!(
            output.value["console"].as_str().unwrap(),
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
        for source in [
            "console.log('before error'); throw new Error('boom');",
            "console.log('before error'); return {nested:undefined};",
        ] {
            let error = executor
                .execute(
                    runtime.agent.clone(),
                    "script",
                    serde_json::json!({"source": source}),
                    None,
                )
                .await
                .unwrap_err();
            let crate::tool::executor::ExecutionError::Failed {
                message,
                output: Some(output),
            } = error
            else {
                panic!("expected script failure with captured output");
            };
            assert_eq!(output.value["console"], "before error\n");
            assert!(message.contains(if source.contains("boom") {
                "boom"
            } else {
                "undefined at $.nested"
            }));
        }
    }
}
