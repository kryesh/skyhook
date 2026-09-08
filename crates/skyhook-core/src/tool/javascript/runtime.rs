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
    let path = context
        .capture_path("/result/console")
        .await
        .map_err(|e| JsError::Execution(e.to_string()))?;
    let result = evaluate_captured(source, executor, context).await;
    let console_output = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| JsError::Execution(e.to_string()))?;
    match result {
        Ok(mut output) => {
            output.value["console"] = Value::String(console_output);
            Ok(output)
        }
        Err(error) if console_output.is_empty() => Err(error),
        Err(error) => Err(JsError::WithConsole {
            error: Box::new(error),
            console_output,
        }),
    }
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

    async fn presentation_runtime() -> (
        TestRuntime,
        ToolExecutor,
        Arc<std::sync::OnceLock<ToolExecutor>>,
    ) {
        let runtime = TestRuntime::new().await;
        let mut builder = ToolRegistryBuilder::default();
        crate::tool::builtins::register_worker_tools(&mut builder, runtime.store.clone()).unwrap();
        crate::tool::builtins::jobs::register(&mut builder, runtime.jobs.clone()).unwrap();
        let slot = Arc::new(std::sync::OnceLock::new());
        crate::tool::builtins::install_script_tool(&mut builder, Arc::downgrade(&slot)).unwrap();
        let executor = runtime.executor(builder);
        slot.set(executor.clone()).ok().unwrap();
        (runtime, executor, slot)
    }

    #[tokio::test]
    async fn returned_tool_views_keep_full_script_values_and_child_ranges_after_resume() {
        let (runtime, executor, _slot) = presentation_runtime().await;
        std::fs::create_dir(runtime.root.path().join("files")).unwrap();
        for index in 0..150 {
            std::fs::write(
                runtime
                    .root
                    .path()
                    .join(format!("files/file-{index:03}.rs")),
                "needle\n",
            )
            .unwrap();
        }
        let call = executor
            .execute_model(
                runtime.agent.clone(),
                "script",
                serde_json::json!({"source":r#"
const files = await tool.glob({pattern:"*.rs", path:"files"});
if (files.paths.length !== 150) throw new Error("tool data was truncated inside script");
console.log("logged\n".repeat(150));
return {
  "files/~":files,
  count:files.paths.length,
  nested:[files, tool.search({pattern:"needle", path:"files"})],
  custom:{text:"x".repeat(5000)}
};
"#}),
                None,
            )
            .await
            .unwrap();
        let view = call.output.value;
        let child = &view["result"]["value"]["files/~"];
        assert_eq!(child["tool"], "glob");
        assert!(child["result"]["paths"].as_array().unwrap().len() < 150);
        assert!(child.get("paths").is_none());
        assert_eq!(view["result"]["value"]["nested"][0], *child);
        assert_eq!(view["result"]["value"]["nested"][1]["tool"], "search");
        assert_eq!(view["result"]["value"]["count"], 150);
        assert_eq!(
            view["result"]["value"]["custom"]["text"]
                .as_str()
                .unwrap()
                .len(),
            5000
        );
        assert_eq!(
            view["result"]["console"].as_str().unwrap().lines().count(),
            100
        );
        assert_eq!(view["truncated"].as_array().unwrap().len(), 1);
        assert_eq!(view["truncated"][0]["field"], "/result/console");
        let raw = runtime
            .jobs
            .snapshot(call.job)
            .await
            .unwrap()
            .output
            .unwrap();
        assert_eq!(
            raw["value"]["files/~"]["paths"].as_array().unwrap().len(),
            150
        );
        assert!(
            runtime
                .jobs
                .take_pending(&runtime.agent)
                .await
                .unwrap()
                .is_empty()
        );

        let child_id = serde_json::from_value(child["id"].clone()).unwrap();
        let mut query = crate::job::output::OutputArgs::new(child_id);
        query.field = Some(child["truncated"][0]["field"].as_str().unwrap().into());
        query.start = Some(child["truncated"][0]["next_start"].as_u64().unwrap() as usize);
        query.offset = Some(child["truncated"][0]["next_offset"].as_u64().unwrap_or(0) as usize);
        let page = runtime
            .jobs
            .present_output(query.clone(), &Default::default())
            .await
            .unwrap();
        assert!(!page["preview"]["lines"].as_array().unwrap().is_empty());
        let mut wrong_job = query.clone();
        wrong_job.job = call.job;
        assert!(
            runtime
                .jobs
                .present_output(wrong_job, &Default::default())
                .await
                .is_err()
        );

        let session = runtime.store.id();
        runtime.store.close().await.unwrap();
        let (store, records) =
            crate::session::SessionStore::open(&runtime.root.path().join("sessions"), session)
                .await
                .unwrap();
        let restored = crate::job::JobManager::restore(store, &records)
            .await
            .unwrap();
        assert_eq!(
            restored
                .present_output_for(
                    crate::job::output::OutputArgs::new(call.job),
                    &Default::default(),
                    &crate::execution::ExecutionLocation::root(runtime.root.path().to_path_buf()),
                    false,
                )
                .await
                .unwrap(),
            view
        );
        assert_eq!(
            restored
                .present_output(query, &Default::default())
                .await
                .unwrap(),
            page
        );
    }

    #[tokio::test]
    async fn edited_results_and_extracted_arrays_use_script_annotations() {
        let (runtime, executor, _slot) = presentation_runtime().await;
        let text = "original\n".repeat(200);
        std::fs::write(runtime.root.path().join("file.txt"), &text).unwrap();
        let call = executor.execute_model(runtime.agent.clone(), "script", serde_json::json!({"source":r#"
const original = await tool.read({path:"file.txt"});
const edited = await tool.read({path:"file.txt"});
edited.content = "edited\n".repeat(200);
const files = await tool.glob({pattern:"file.txt"});
files.paths.push(...Array.from({length:200}, (_, i) => "path-"+i));
return {original, edited, extracted:original.content, "array/~":files.paths, mapped:files.paths.map(p=>p)};
"#}), None).await.unwrap();
        let view = call.output.value;
        assert_eq!(view["result"]["value"]["original"]["tool"], "read");
        assert!(view["result"]["value"]["edited"].get("tool").is_none());
        assert_eq!(
            view["result"]["value"]["edited"]["content"],
            "edited\n".repeat(100)
        );
        assert_eq!(view["result"]["value"]["extracted"], text);
        assert!(view["result"]["value"]["array/~"].as_array().unwrap().len() < 201);
        assert_eq!(
            view["result"]["value"]["mapped"].as_array().unwrap().len(),
            201
        );
        let truncated = view["truncated"].as_array().unwrap();
        assert_eq!(truncated.len(), 2);
        assert!(
            truncated
                .iter()
                .any(|t| t["field"] == "/result/value/array~1~0")
        );
        let entry = truncated
            .iter()
            .find(|t| t["field"] == "/result/value/edited/content")
            .unwrap();
        let mut query = crate::job::output::OutputArgs::new(call.job);
        query.field = Some(entry["field"].as_str().unwrap().into());
        query.start = Some(entry["next_start"].as_u64().unwrap() as usize);
        query.offset = Some(entry["next_offset"].as_u64().unwrap_or(0) as usize);
        let page = runtime
            .jobs
            .present_output(query, &Default::default())
            .await
            .unwrap();
        assert_eq!(page["preview"]["lines"][0], "edited");
        let saved = runtime
            .jobs
            .snapshot(call.job)
            .await
            .unwrap()
            .output
            .unwrap();
        assert_eq!(saved["value"]["edited"]["content"], "edited\n".repeat(200));
    }

    #[tokio::test]
    async fn direct_returns_and_existing_job_views_are_not_double_wrapped() {
        let (runtime, executor, _slot) = presentation_runtime().await;
        std::fs::write(runtime.root.path().join("file.txt"), "hello").unwrap();
        let direct = executor
            .execute_model(
                runtime.agent.clone(),
                "script",
                serde_json::json!({"source":"return tool.read({path:'file.txt'});"}),
                None,
            )
            .await
            .unwrap();
        let child = &direct.output.value["result"]["value"];
        assert_eq!(child["tool"], "read");
        assert_eq!(child["result"]["content"], "hello");
        let query = executor
            .execute_model(
                runtime.agent.clone(),
                "script",
                serde_json::json!({"source":format!("return tool.job({}).output();", child["id"])}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(query.output.value["result"]["value"]["id"], child["id"]);
        assert_eq!(
            query.output.value["result"]["value"]["result"],
            child["result"]
        );
        assert!(query.output.value["result"]["value"].get("tool").is_none());
        let background = executor
            .execute_model(
                runtime.agent.clone(),
                "script",
                serde_json::json!({
                    "source":"return {handle:await tool.shell({command:'printf hello',bg:true})};"
                }),
                None,
            )
            .await
            .unwrap();
        let handle = &background.output.value["result"]["value"]["handle"];
        assert_eq!(handle["tool"], "shell");
        assert!(handle.get("result").is_none());
        let query = crate::job::output::OutputArgs::new(
            serde_json::from_value(handle["id"].clone()).unwrap(),
        );
        runtime.jobs.wait(query.job, None, true).await.unwrap();
        let completed = runtime
            .jobs
            .present_output(query, &Default::default())
            .await
            .unwrap();
        assert_eq!(completed["result"]["stdout"], "hello");
    }

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
            output.value["value"],
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
            output.value["value"],
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
        assert_eq!(output.value["value"], serde_json::json!(vec![true; 9]));
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
            output.value["value"]["direct"],
            serde_json::json!({"required":"x", "limit":7})
        );
        assert_eq!(
            output.value["value"]["built"],
            output.value["value"]["direct"]
        );
        assert_eq!(
            output.value["value"]["set"],
            serde_json::json!({"required":"y", "limit":7})
        );
        assert_eq!(
            output.value["value"]["reused"],
            serde_json::json!([
                {"required":"x", "limit":7}, {"required":"x", "limit":7}
            ])
        );
        assert_eq!(output.value["value"]["skillObject"]["name"], "beta");
        assert_eq!(output.value["value"]["skillFluent"]["name"], "gamma");
        assert_eq!(output.value["value"]["asset"]["path"], "template.txt");
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
    async fn work_pool_run_rejects_invalid_calls_before_starting_any_tasks() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let output = evaluate(
            r#"
const pool = new WorkPool(2);
let started = 0;
const task = () => { started++; return 1; };
const invalid = [
  () => pool.run(),
  () => pool.run(task),
  () => pool.run(task, task, task),
  () => pool.run([task], [task]),
  () => pool.run(null),
  () => pool.run(undefined),
  () => pool.run({}),
  () => pool.run({0: task, length: 1}),
  () => pool.run(new Set([task])),
  () => pool.run("tasks"),
  () => pool.run(3),
  () => pool.run([task, 42]),
  () => pool.run([task, undefined]),
  () => pool.run([task, , task]),
];
const errors = invalid.map(invoke => {
  try { invoke(); return null; }
  catch (error) { return {name: error.name, message: error.message}; }
});
await Promise.resolve();
return {errors, started};
"#
            .to_owned(),
            executor,
            context,
        )
        .await
        .unwrap();
        assert_eq!(output.value["value"]["started"], 0);
        let errors = output.value["value"]["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 14);
        for (index, error) in errors.iter().enumerate() {
            assert_eq!(error["name"], "TypeError", "invalid call {index}: {error}");
            let message = error["message"].as_str().unwrap();
            if index < 11 {
                assert!(message.contains("run([f1, f2])"), "{message}");
            } else {
                assert!(message.contains("index 1 must be a function"), "{message}");
            }
        }
        assert!(output.value["console"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn work_pool_run_array_example_and_empty_array() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let output = evaluate(
            r#"
const results = [];
for await (const {index, value} of new WorkPool(2).run([
  async () => 1,
  async () => 2,
  async () => 3,
])) {
  results.push({index, value});
}
results.sort((a, b) => a.index - b.index);
const empty = [];
for await (const result of new WorkPool(2).run([])) empty.push(result);
return {results, empty};
"#
            .to_owned(),
            executor,
            context,
        )
        .await
        .unwrap();
        assert_eq!(
            output.value["value"],
            serde_json::json!({
                "results": [{"index":0,"value":1}, {"index":1,"value":2}, {"index":2,"value":3}],
                "empty": []
            })
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
        assert_eq!(
            output.value["value"]["started"],
            serde_json::json!([0, 1, 2, 3])
        );
        assert_eq!(
            output.value["value"]["results"],
            serde_json::json!([{"index":3,"value":7}])
        );
        assert_eq!(
            output.value["console"].as_str().unwrap(),
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
    async fn work_pool_logs_all_failures_and_returns_no_results() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let output = evaluate("const results=[]; for await (const result of new WorkPool(2).map([0,1,2,3], value => { throw new Error(`failure ${value}`); })) results.push(result); return results;".to_owned(), executor, context).await.unwrap();
        assert_eq!(output.value["value"], serde_json::json!([]));
        for index in 0..4 {
            assert!(
                output.value["console"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("WorkPool item {index} failed: failure {index}\n"))
            );
        }
        assert_eq!(output.value["console"].as_str().unwrap().lines().count(), 4);
    }

    #[tokio::test]
    async fn script_result_wrapper_preserves_user_fields_and_primitive_values() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        for (source, value) in [
            (
                "console.log('captured'); return {console:'user',value:42};",
                serde_json::json!({"console":"user", "value":42}),
            ),
            ("console.log('captured'); return 42;", serde_json::json!(42)),
            ("console.log('captured'); return null;", Value::Null),
            ("console.log('captured');", Value::Null),
        ] {
            let output = evaluate(source.to_owned(), executor.clone(), context.clone())
                .await
                .unwrap();
            assert_eq!(
                output.value,
                serde_json::json!({"value":value, "console":"captured\n"}),
                "{source}"
            );
        }
    }

    #[tokio::test]
    async fn model_script_results_keep_console_inside_the_result_wrapper() {
        let (runtime, executor, _slot) = presentation_runtime().await;
        for (expression, value) in [
            (
                "{console:'user',value:42}",
                serde_json::json!({"console":"user", "value":42}),
            ),
            ("42", serde_json::json!(42)),
            ("null", Value::Null),
        ] {
            let call = executor
                .execute_model(
                    runtime.agent.clone(),
                    "script",
                    serde_json::json!({"source":format!(
                        "console.log('captured'); return {expression};"
                    )}),
                    None,
                )
                .await
                .unwrap();
            let view = call.output.value;
            assert_eq!(
                view["result"],
                serde_json::json!({"value":value, "console":"captured\n"})
            );
            assert!(view.get("console").is_none());
        }
    }

    #[tokio::test]
    async fn script_errors_preserve_captured_console() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        for source in [
            "console.log('before error'); throw new Error('boom');",
            "console.log('before error'); return {nested:undefined};",
        ] {
            let error = evaluate(source.to_owned(), executor.clone(), context.clone())
                .await
                .unwrap_err();
            let JsError::WithConsole {
                error,
                console_output,
            } = error
            else {
                panic!("expected captured console with error")
            };
            assert_eq!(console_output, "before error\n");
            assert!(error.to_string().contains(if source.contains("boom") {
                "boom"
            } else {
                "undefined at $.nested"
            }));
        }
    }

    #[tokio::test]
    async fn console_formats_values_and_retains_large_capture_without_changing_the_result() {
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
        assert_eq!(output.value["value"], Value::Null);
        assert_eq!(
            output.value["console"].as_str().unwrap(),
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
        assert_eq!(output.value["value"], serde_json::json!(42));
        assert_eq!(
            output.value["console"].as_str().unwrap().len(),
            17 * 1024 * 1024 + "\ndiscarded\n".len()
        );
        assert!(
            output.value["console"]
                .as_str()
                .unwrap()
                .ends_with("\ndiscarded\n")
        );
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
        assert_eq!(null.value["value"], Value::Null);

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
