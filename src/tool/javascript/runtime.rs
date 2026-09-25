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

use super::{bridge::HostResponse, console::ConsoleOutput};

use crate::{
    media::ImageRef,
    tool::diagnostic::{Effects, FailureSite, Operation, Subject},
    tool::executor::{ExecutionError, ToolExecutor},
    tool::registry::JobName,
    tool::{ToolContext, ToolError, ToolOutput},
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
) -> Result<ToolOutput, ToolError> {
    let capture = context
        .text_capture(crate::job::output::TextCaptureField::Console)
        .await
        .map_err(|error| {
            console_failure(error, Operation::CreateCapture, Effects::NotStarted)
                .with_result(super::result::script_output(Value::Null, None, None))
        })?;
    let console = Arc::new(std::sync::Mutex::new(ConsoleOutput::new(capture.open())));
    let result = evaluate_inner(source, executor, context, console.clone()).await;
    let console = console.lock().expect("console lock poisoned").finish();
    finish_evaluation(result, console)
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
            if let (Some(output), Some(console)) = (&mut output, console) {
                output
                    .value
                    .as_object_mut()
                    .expect("script result is an object")
                    .remove("console");
                output.captures.push(console);
            }
            Err(ToolError::from_facts(diagnostic, output))
        }
        (Ok(output), Err(error)) => Err(console_failure(
            error.into(),
            Operation::FinishCapture,
            Effects::OutputIncomplete,
        )
        .with_result(
            super::result::script_output(output.value, None, None).with_images(output.images),
        )),
        (Err(error), Err(secondary)) => {
            // Keep the primary outcome and summary. A JS stack can retain secondary
            // storage evidence; outcomes without one still record incomplete output.
            let secondary = console_failure(
                secondary.into(),
                Operation::FinishCapture,
                Effects::OutputIncomplete,
            );
            let (mut diagnostic, mut output) = error.into_facts();
            diagnostic.context = diagnostic.context.effects(Effects::OutputIncomplete);
            if let Some(Value::String(stack)) = output
                .as_mut()
                .and_then(|output| output.value.pointer_mut("/failure/stack"))
            {
                stack.push('\n');
                stack.push_str(&secondary.to_string());
            }
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

fn javascript_error(error: JsError) -> ToolError {
    let (operation, subject, effects) = match &error {
        JsError::SourceTooLarge => (
            Operation::Validate,
            "JavaScript source",
            Effects::NotStarted,
        ),
        JsError::Initialization(_) => (
            Operation::Prepare,
            "JavaScript runtime",
            Effects::NotStarted,
        ),
        JsError::Execution(_) | JsError::Cancelled => {
            (Operation::Execute, "JavaScript source", Effects::Unknown)
        }
        JsError::Failure { .. } => (Operation::Execute, "JavaScript source", Effects::Started),
        JsError::InvalidOutput(_) => (
            Operation::Deserialize,
            "JavaScript result",
            Effects::OutputIncomplete,
        ),
    };
    let error = match error {
        JsError::Cancelled => ToolError::cancelled(),
        JsError::Failure { message, details } => ToolError::with_output(
            message,
            super::result::script_output(Value::Null, Some(details), None),
        ),
        error => ToolError::with_output(
            error.to_string(),
            super::result::script_output(Value::Null, None, None),
        ),
    };
    error
        .operation(operation, Subject::Label(subject.into()))
        .effects(effects)
}

async fn evaluate_inner(
    source: String,
    executor: ToolExecutor,
    context: ToolContext,
    console: Arc<std::sync::Mutex<ConsoleOutput>>,
) -> Result<ToolOutput, ToolError> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(javascript_error(JsError::SourceTooLarge));
    }
    let runtime = AsyncRuntime::new()
        .map_err(|error| javascript_error(JsError::Initialization(error.to_string())))?;
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
        .map_err(|error| javascript_error(JsError::Initialization(error.to_string())))?;
    let surface = Arc::new(executor.surface_for_agent(context.agent()));
    let builders = surface.script_manifests();
    let builders = serde_json::to_string(&builders)
        .map_err(|error| javascript_error(JsError::Initialization(error.to_string())))?;
    let script = wrapper_script(&source, &builders);
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
        .map_err(|error| JsError::Initialization(error.to_string()))?;
        js.globals().set("sleep", sleep).map_err(|error| JsError::Initialization(error.to_string()))?;
        let log = Function::new(js.clone(), move |text: String| {
            console.lock().expect("console lock poisoned").log(&text).map_err(|error| {
                bridge_error(&console_failure(error.into(), Operation::WriteCapture, Effects::OutputIncomplete))
            })
        })
        .map_err(|error| JsError::Initialization(error.to_string()))?;
        js.globals()
            .set("__skyhookConsoleLog", log)
            .map_err(|error| JsError::Initialization(error.to_string()))?;
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
                            let parent = Some(host_context.job());
                            let requested = arguments.as_object().and_then(JobName::requested);
                            // Failures before and after the job exists are ordinary
                            // tool responses rather than JS exceptions.
                            let failed = |error: ExecutionError, job_name| {
                                error.into_response(
                                    &name,
                                    parent,
                                    job_name,
                                    host_context.diagnostic_viewer(),
                                )
                            };
                            // A presented job view already carries its own annotations.
                            // Native schemas describe only envelope.result.
                            let mut native = true;
                            let output = match host_executor
                                .create_script(
                                    host_context.agent().clone(),
                                    &name,
                                    arguments,
                                    parent,
                                )
                                .await
                            {
                                Ok(created) => {
                                    let job_name = created.job_name().cloned();
                                    native = created.result_policy()
                                        == crate::tool::ToolResultPolicy::Value;
                                    match host_executor.run(created).await {
                                        Ok(result) => result.output,
                                        Err(error) => failed(error, job_name),
                                    }
                                }
                                Err(error) => failed(error, requested),
                            };
                            let schema = surface
                                .get(&name)
                                .filter(|_| native)
                                .and_then(|tool| tool.result_schema.as_ref());
                            let annotations = schema
                                .and_then(|schema| output.value.get("result").map(|value| (schema, value)))
                                .map(|(schema, value)| {
                                    crate::job::output::annotated_fields(value, schema)
                                        .into_iter()
                                        .map(|pointer| format!("/result{pointer}"))
                                        .collect::<std::collections::BTreeSet<_>>()
                                })
                                .unwrap_or_default();
                            let annotations = (!annotations.is_empty())
                                .then_some(annotations);
                            host_images.lock().await.extend(output.images);
                            HostResponse::Success { value: output.value, annotations }
                        }
                        HostRequest::Receive => {
                            let result = match host_executor.jobs().is_background(host_context.job()).await {
                                Ok(true) => host_context.receive().await.map_err(|error| error.to_string()),
                                Ok(false) => Err("receive() requires the script tool to be invoked with bg: true".to_owned()),
                                Err(error) => Err(error.to_string()),
                            };
                            match result {
                                Ok(value) => HostResponse::Success { value, annotations: None },
                                Err(message) => HostResponse::Failure(message),
                            }
                        }
                    };
                    response.encode().map_err(|error| bridge_error(&error))
                }
            }),
        )
        .map_err(|error| JsError::Initialization(error.to_string()))?;
        js.globals()
            .set("__skyhookHostCall", host_call)
            .map_err(|error| JsError::Initialization(error.to_string()))?;
        let promise = js
            .eval::<Promise<'_>, _>(script)
            .catch(&js)
            .map_err(|error| JsError::Execution(error.to_string()))?;
        promise
            .into_future::<String>()
            .await
            .catch(&js)
            .map_err(|error| JsError::Execution(error.to_string()))
    });
    let result = execution.await;
    if cancelled.is_cancelled() {
        return Err(javascript_error(JsError::Cancelled));
    }
    let encoded = result.map_err(|error| {
        javascript_error(match error {
            JsError::Execution(message) => {
                JsError::Execution(map_script_lines(&message, user_start_line, user_line_count))
            }
            error => error,
        })
    })?;
    let envelope: super::outcome::Envelope = serde_json::from_str(&encoded)
        .map_err(|error| javascript_error(JsError::InvalidOutput(error.to_string())))?;
    let (value, presentation) = match envelope {
        super::outcome::Envelope::Ok {
            value,
            presentation,
        } => (value, presentation),
        super::outcome::Envelope::Failed { error: mut details } => {
            map_failure_stack_lines(&mut details, user_start_line, user_line_count);
            let message = details.get("message").and_then(Value::as_str).map_or_else(
                || details.to_string(),
                |message| {
                    format!(
                        "{message}\n{}",
                        details.get("stack").and_then(Value::as_str).unwrap_or("")
                    )
                },
            );
            return Err(javascript_error(JsError::Failure {
                message: map_script_lines(&message, user_start_line, user_line_count),
                details,
            }));
        }
    };
    let mut images = std::mem::take(&mut *returned_images.lock().await);
    images.sort();
    images.dedup();
    presentation_jobs
        .save_script_presentation(script_job, presentation)
        .await
        .map_err(|error| {
            error
                .operation(
                    Operation::Save,
                    Subject::Label("script presentation".into()),
                )
                .effects(Effects::OutputIncomplete)
                .with_result(
                    super::result::script_output(value.clone(), None, None)
                        .with_images(images.clone()),
                )
        })?;
    Ok(ToolOutput::new(value).with_images(images))
}

fn bridge_error(error: &impl ToString) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("host request", "JavaScript promise", error.to_string())
}

const USER_SOURCE_MARKER: &str = "// __skyhook_user_source__\n";

// Only exception stacks belong to this wrapper. Nested JobViews and partial
// outputs remain untouched, even if they contain similar location text.
fn map_failure_stack_lines(details: &mut Value, user_start: usize, user_lines: usize) {
    match details {
        Value::Object(error) => {
            if let Some(Value::String(stack)) = error.get_mut("stack") {
                *stack = map_script_lines(stack, user_start, user_lines);
            }
            if let Some(cause) = error.get_mut("cause") {
                map_failure_stack_lines(cause, user_start, user_lines);
            }
        }
        Value::Array(errors) => {
            for error in errors {
                map_failure_stack_lines(error, user_start, user_lines);
            }
        }
        _ => {}
    }
}

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
         const presentation = {{fields:[]}};\n\
         const resolved = await __resolve(value, \"$\", new Set(), \"/result/value\", presentation);\n\
         return __stringify({{outcome:\"ok\", value:resolved, presentation}});\n\
         }} catch (error) {{ return __stringify({{outcome:\"failed\", error:__describeError(error)}}); }}\n\
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
        let saved = executor.jobs().output(context.job());
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

    #[tokio::test]
    async fn sleep_stops_on_cancellation_and_unawaited_sleep_does_not_keep_a_script_alive() {
        let (_scope, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let cancellation = context.cancellation_token();
        let source = "await sleep(60000); return 'finished';";
        let (result, ()) = tokio::join!(evaluate(source, executor, context), async {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            cancellation.cancel();
        });
        assert_eq!(result.unwrap_err().diagnostic().cause, Cause::Cancelled);
        // Far below the script's sleep, yet generous under load.
        let unawaited = plain("sleep(60000); return 'finished';");
        let output = tokio::time::timeout(std::time::Duration::from_secs(20), unawaited);
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
        let responses = output.value["value"].as_array().unwrap();
        assert_eq!(responses.len(), 3);
        for response in responses {
            assert_eq!(response["state"], "completed");
            assert_eq!(response["has_result"], true);
            assert_eq!(response["result"], "a");
            assert!(response["error"].is_null());
            assert!(response["presentation"].is_null());
        }
        // Reusing one builder keeps the exact response, including its job ID.
        assert_eq!(responses[0], responses[1]);
        assert_ne!(responses[0]["id"], responses[2]["id"]);
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
                |_, _| async { Err(crate::tool::ToolError::denied("user reason")) },
            )
            .unwrap();
        let (_scope, executor, context) = test_runtime(builder).await;
        let source = r#"
const direct = JSON.parse(await __skyhookHostCall(JSON.stringify({
  type: "call",
  name: "script",
  arguments: {source: "return null;", name: "nested"},
})));
const deniedResponse = await tool.deny({value:"x"});
let denied;
try { deniedResponse.unwrap(); } catch (error) {
  denied = {code:error.code, executed:error.executed, message:error.message,
            output:error.output, sameResponse:error.response === deniedResponse};
}
return {visible: typeof tool.script, direct, deniedResponse, denied};
"#;
        let output = evaluate(source, executor.clone(), context.clone())
            .await
            .unwrap();
        let value = &output.value["value"];
        assert_eq!(value["visible"], "undefined");
        assert_eq!(value["direct"]["ok"], true);
        assert_eq!(value["direct"]["value"]["state"], "failed");
        assert_eq!(value["direct"]["value"].get("id"), Some(&Value::Null));
        assert_eq!(value["direct"]["value"]["meta"]["tool"], "script");
        assert_eq!(value["direct"]["value"]["meta"]["name"], "nested");
        assert_eq!(
            value["direct"]["value"]["meta"]["parent"],
            json!(context.job())
        );
        assert!(
            value["direct"]["value"]["error"]
                .as_str()
                .unwrap()
                .contains("not available in scripts")
        );
        assert_eq!(value["deniedResponse"]["state"], "failed");
        assert_eq!(
            (&value["denied"]["code"], &value["denied"]["executed"]),
            (&json!("permission_denied"), &json!(false))
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
        let details = &output.value["failure"];
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
    async fn script_failure_stack_and_summary_use_submitted_source_lines() {
        for (source, line) in [
            ("\n\nawait Promise.resolve();\nthrow new Error('boom');", 4),
            (
                "\n\nfunction fail() {\n  throw new Error('boom');\n}\nfail();",
                4,
            ),
            (
                "\n\nthrow new Error('outer', {cause:new Error('inner')});",
                3,
            ),
        ] {
            let (diagnostic, Some(output)) = plain(source).await.unwrap_err().into_parts() else {
                panic!("expected JavaScript failure output");
            };
            let Cause::Message(message) = diagnostic.cause else {
                panic!("expected JavaScript failure");
            };
            let details = &output.value["failure"];
            let stack = details["stack"].as_str().unwrap();
            assert!(
                stack.contains(&format!("skyhook-script:{line}:")),
                "{stack}"
            );
            assert!(
                message.contains(&format!("skyhook-script:{line}:")),
                "{message}"
            );
            if let Some(cause) = details.get("cause") {
                assert!(
                    cause["stack"]
                        .as_str()
                        .unwrap()
                        .contains(&format!("skyhook-script:{line}:"))
                );
            }
        }
        let (diagnostic, Some(_)) = plain("\n\nconst value = ;").await.unwrap_err().into_parts()
        else {
            panic!("expected JavaScript syntax failure output");
        };
        let Cause::Message(message) = diagnostic.cause else {
            panic!("expected JavaScript syntax failure");
        };
        assert!(message.contains("skyhook-script:3:"), "{message}");
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
        let details = json!({"message": "primary failure", "stack": "skyhook-script:3:1"});
        // One primary with a stack to extend and one without any output.
        for error in [
            javascript_error(JsError::Failure {
                message: "primary failure".into(),
                details: details.clone(),
            }),
            javascript_error(JsError::Cancelled),
        ] {
            let (expected, expected_output) = error.into_parts();
            // Check both successful and failed console finalization against the same primary.
            let primary = ToolError::from_diagnostic(expected.clone(), expected_output.clone());
            let (diagnostic, mut output) = finish_evaluation(
                Err(primary),
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
            )
            .unwrap_err()
            .into_parts();
            let mut incomplete = expected.clone();
            incomplete.context.effects = Effects::OutputIncomplete;
            assert_eq!(diagnostic, incomplete);
            if let Some(stack) = output
                .as_mut()
                .and_then(|output| output.value.pointer_mut("/failure/stack"))
            {
                let text = stack.as_str().unwrap();
                assert!(
                    text.starts_with(details["stack"].as_str().unwrap()),
                    "{text}"
                );
                assert!(text.contains("broken pipe"), "{text}");
                *stack = details["stack"].clone();
            }
            assert_eq!(
                output.map(|output| output.value),
                expected_output.as_ref().map(|output| output.value.clone())
            );
            let primary = ToolError::from_diagnostic(expected.clone(), expected_output.clone());
            let (diagnostic, output) = finish_evaluation(Err(primary), Ok(None))
                .unwrap_err()
                .into_parts();
            assert_eq!(diagnostic, expected);
            assert_eq!(
                output.map(|output| output.value),
                expected_output.map(|output| output.value)
            );
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
        assert_eq!(
            output.unwrap().value,
            json!({"value":{"computed":42},"console":"","failure":null})
        );
    }

    #[test]
    fn script_line_mapping_preserves_nested_tool_responses_and_partial_output() {
        let response = json!({
            "state": "failed",
            "error": "eval_script:102: child failure",
            "result": {"stack": "eval_script:102:"},
            "meta": {"target": "remote", "code": "permission_denied", "executed": false},
        });
        let mut details = json!({
            "message": "failed",
            "stack": "at eval_script:102:4\nat eval_script:9:1",
            "cause": {"stack": "at eval_script:103:5"},
            "response": response,
            "output": response["result"],
        });
        map_failure_stack_lines(&mut details, 100, 5);
        assert_eq!(
            details["stack"],
            "at skyhook-script:3:4\nat eval_script:9:1"
        );
        assert_eq!(details["cause"]["stack"], "at skyhook-script:4:5");
        assert_eq!(details["response"], response);
        assert_eq!(details["output"], response["result"]);
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
            let error = executor
                .run_host(&runtime.agent, "script", json!({"source": source}))
                .await
                .unwrap_err();
            let diagnostic = error.diagnostic();
            assert_eq!(diagnostic.context.operation, Operation::Execute);
            assert_eq!(diagnostic.context.site, FailureSite::Host);
            assert!(
                matches!(&diagnostic.cause, Cause::Message(message) if message.contains(expected))
            );
            let output = error
                .into_tool_error()
                .into_parts()
                .1
                .expect("script failure retains captured output");
            assert_eq!(output.value["console"], "before error\n");
        }
    }

    // Standalone wrapper regressions: no executor, jobs, or external processes are needed.
    const BUILDERS: &str = r#"[
    {"binding":"top_level","name":"echo","properties":["value"],"required":["value"]},
    {"binding":"job_method","name":"job_send","method":"send","job_argument":"job","properties":["job","value"],"required":["value"]},
    {"binding":"job_method","name":"job_cancel","method":"cancel","job_argument":"job","properties":["job"],"required":[]}
]"#;

    const MOCK_HOST: &str = r#"
const __testCalls = [];
function __skyhookHostCall(encoded) {
  const request = JSON.parse(encoded);
  __testCalls.push(request);
  if (request.type === "receive") {
    return JSON.stringify({ok: true, value: {
      id: 99, state: "completed", has_result: true, result: null,
    }});
  }
  return JSON.stringify({ok: true, value: {
    id: __testCalls.length, state: "completed", has_result: true,
    result: request.arguments, error: null,
    meta: {parent:null, tool:null, name:null, target:null, workspace:null,
           last_message:null, code:null, executed:null},
    presentation: {preview:null, truncated:[], captures:[], question:null, notice:null},
  }, annotations: ["/result/value"]});
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
            let promise: Promise = ctx
                .eval(wrapper_script(source, BUILDERS))
                .catch(&ctx)
                .expect("evaluate wrapper before watchdog deadline");
            let encoded: String = promise
                .finish()
                .catch(&ctx)
                .expect("finish wrapper before watchdog deadline without pending host work");
            let result: Value = serde_json::from_str(&encoded).expect("wrapper JSON result");
            assert_eq!(result["outcome"], "ok", "wrapper failed: {result:#}");
            result
        })
    }

    fn evaluate_wrapper(source: &str) -> Value {
        evaluate_wrapper_envelope(source)["value"].clone()
    }

    #[test]
    fn annotations_follow_full_envelopes_and_extracted_native_payloads() {
        let result = evaluate_wrapper_envelope(
            r#"
const full = await tool.echo({value:["full"]});
const extracted = (await tool.echo({value:["extracted"]})).result.value;
return {full, extracted};
"#,
        );
        let presentation = result["presentation"].as_object().unwrap();
        assert_eq!(presentation.len(), 1);
        let fields = presentation["fields"].as_array().unwrap();
        assert!(fields.contains(&json!("/result/value/full/result/value")));
        assert!(fields.contains(&json!("/result/value/extracted")));
    }

    #[test]
    fn response_methods_are_runtime_only_and_do_not_decorate_user_json() {
        let result = evaluate_wrapper(
            r#"
const payload = {id:7, state:"completed", has_result:true, result:null};
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
  grouped:response.meta.code === null && response.presentation.preview === null,
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
            "grouped",
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
        assert_eq!(
            result["received"],
            json!({"id":99,"state":"completed","has_result":true,"result":null})
        );
    }

    #[test]
    fn non_object_job_responses_fail_at_the_bridge_boundary() {
        let result = evaluate_wrapper(
            r#"
const errors = [];
for (const value of [null, 17, [], "bad"]) {
  __skyhookHostCall = () => JSON.stringify({ok:true, value});
  try { await tool.echo({value:null}); }
  catch (error) { errors.push(error.message); }
}
return errors;
"#,
        );
        let errors = result.as_array().unwrap();
        assert_eq!(errors.len(), 4);
        for error in errors {
            assert!(
                error
                    .as_str()
                    .unwrap()
                    .contains("invalid JobView envelope: expected an object")
            );
        }
    }

    #[test]
    fn explicit_unwrap_returns_completed_results_and_preserves_failure_responses() {
        let result = evaluate_wrapper(
            r#"
const completed = await tool.echo({value:42});
const literalNull = await tool.echo({value:null});
literalNull.result = null;
const errors = [];
for (const patch of [
  {id:8, state:"failed", error:"boom", result:{partial:true}, meta:null},
  {id:9, state:"running", has_result:false, result:null},
  {id:10, has_result:false, result:null},
  {result:undefined},
]) {
  const response = Object.assign(await tool.echo({value:null}), patch);
  try { response.unwrap(); }
  catch (error) {
    errors.push({message:error.message, sameResponse:error.response === response,
                 ...(error.response ? {output:error.output, code:error.code, executed:error.executed} : {})});
  }
}
return {value:completed.unwrap(), literalNull:literalNull.unwrap(), errors};
"#,
        );
        assert_eq!(result["value"], json!({"value": 42}));
        assert_eq!(result.get("literalNull"), Some(&Value::Null));
        assert_eq!(
            result["errors"],
            json!([
                {"message":"boom", "sameResponse":true, "output":{"partial":true},
                 "code":null, "executed":null},
                {"message":"job 9 is not completed (state: running)", "sameResponse":true,
                 "output":null, "code":null, "executed":null},
                {"message":"job 10 has no loaded result; inspect it with tool.jobs({job: id})",
                 "sameResponse":true, "output":null, "code":null, "executed":null},
                {"message":"unwrap completed response requires a JSON result field (null is allowed)",
                 "sameResponse":false}
            ])
        );
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
}
