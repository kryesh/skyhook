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

use crate::{
    media::ImageReference,
    tool::executor::ToolExecutor,
    tool::{ToolContext, ToolOutput},
};

const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const STACK_LIMIT: usize = 1024 * 1024;
const MAX_SOURCE_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HostRequest {
    Call { name: String, arguments: Value },
    Receive,
    Notify { value: Value },
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
    #[error("JavaScript returned invalid JSON: {0}")]
    InvalidOutput(String),
}

pub async fn evaluate(
    source: String,
    executor: ToolExecutor,
    context: ToolContext,
) -> Result<ToolOutput, JsError> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(JsError::SourceTooLarge);
    }
    let runtime =
        AsyncRuntime::new().map_err(|error| JsError::Initialization(error.to_string()))?;
    runtime.set_memory_limit(MEMORY_LIMIT).await;
    runtime.set_max_stack_size(STACK_LIMIT).await;
    let cancellation = context.clone();
    runtime
        .set_interrupt_handler(Some(Box::new(move || cancellation.is_cancelled())))
        .await;
    let js_context = AsyncContext::builder()
        .with::<intrinsic::Eval>()
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
                                Err(error) => Err(error.concise_message()),
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
                        HostRequest::Notify { value } => host_context
                            .progress("notification", value)
                            .await
                            .map(|()| Value::Null)
                            .map_err(|error| error.to_string()),
                    };
                    let response = match result {
                        Ok(value) => serde_json::json!({"ok": true, "value": value}),
                        Err(error) => serde_json::json!({"ok": false, "error": error}),
                    };
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
    let value = serde_json::from_str(&encoded)
        .map_err(|error| JsError::InvalidOutput(error.to_string()))?;
    let mut images = returned_images.lock().await.clone();
    images.sort();
    images.dedup();
    Ok(ToolOutput { value, images })
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

#[allow(clippy::too_many_lines)]
fn wrapper_script(source: &str, builders: &str) -> String {
    format!(
        r#"
const __builders = {builders};
const __callKind = Symbol("skyhook.call");
const __executions = new WeakMap();
const __callData = new WeakMap();
const __parse = JSON.parse;
const __stringify = JSON.stringify;

function __plainObject(name, input) {{
  if (input === null || typeof input !== "object" || Array.isArray(input)) {{
    throw new TypeError(`tool.${{name}} argument must be a plain object`);
  }}
  const prototype = Object.getPrototypeOf(input);
  if (prototype !== Object.prototype && prototype !== null) {{
    throw new TypeError(`tool.${{name}} argument must be a plain object`);
  }}
  return Object.assign(Object.create(null), input);
}}

function __operation(name, input) {{
  const operation = {{}};
  __callData.set(operation, {{ name, arguments: Object.freeze(Object.assign(Object.create(null), input)) }});
  Object.defineProperty(operation, __callKind, {{ value: true }});
  Object.defineProperty(operation, "then", {{
    enumerable: false,
    value(onFulfilled, onRejected) {{
      return __execute(operation).then(onFulfilled, onRejected);
    }},
  }});
  Object.defineProperty(operation, "catch", {{
    enumerable: false,
    value(onRejected) {{ return __execute(operation).catch(onRejected); }},
  }});
  Object.defineProperty(operation, "finally", {{
    enumerable: false,
    value(onFinally) {{ return __execute(operation).finally(onFinally); }},
  }});
  return operation;
}}

const __reserved = new Set(["then", "catch", "finally", "set"]);
function __validIdentifier(value) {{
  if (typeof value !== "string" || value.length === 0) return false;
  const first = value.charCodeAt(0);
  const firstOk = first === 36 || first === 95 || (first >= 65 && first <= 90) || (first >= 97 && first <= 122);
  if (!firstOk) return false;
  for (let index = 1; index < value.length; index++) {{
    const code = value.charCodeAt(index);
    if (!(code === 36 || code === 95 || (code >= 48 && code <= 57) || (code >= 65 && code <= 90) || (code >= 97 && code <= 122))) return false;
  }}
  return true;
}}

function __chainBuilder(manifest, input = Object.create(null)) {{
  const operation = __operation(manifest.name, input);
  Object.defineProperty(operation, "set", {{
    enumerable: false,
    value(key, value) {{
      if (!manifest.properties.includes(key)) throw new TypeError(`unknown argument ${{key}} for tool.${{manifest.name}}`);
      return __chainBuilder(manifest, Object.assign(Object.create(null), input, {{[key]: value}}));
    }},
  }});
  for (const property of manifest.properties) {{
    if (!__validIdentifier(property) || __reserved.has(property)) continue;
    Object.defineProperty(operation, property, {{
      enumerable: false,
      value(value) {{ return __chainBuilder(manifest, Object.assign(Object.create(null), input, {{[property]: value}})); }},
    }});
  }}
  return Object.freeze(operation);
}}

function __builder(manifest, values) {{
  if (values.length === 1) return __operation(manifest.name, __plainObject(manifest.name, values[0]));
  if (values.length !== 0) throw new TypeError(`tool.${{manifest.name}} accepts zero arguments or one argument object`);
  if (manifest.properties.length === 0) return __operation(manifest.name, Object.create(null));
  return __chainBuilder(manifest);
}}

function __jobBuilder(manifest, job, values) {{
  const input = Object.assign(Object.create(null), {{[manifest.job_argument]: job}});
  if (values.length === 1) return __operation(manifest.name, Object.assign(input, __plainObject(`job.${{manifest.method}}`, values[0])));
  if (values.length !== 0) throw new TypeError(`tool.job(job).${{manifest.method}} accepts zero arguments or one argument object`);
  if (manifest.required.length === 0) return __operation(manifest.name, input);
  return __chainBuilder(manifest, input);
}}

const tool = Object.create(null);
for (const manifest of __builders) {{
  if (manifest.binding === "top_level") tool[manifest.name] = (...values) => __builder(manifest, values);
}}

tool.job = job => {{
  if (job && job[__callKind] === true) {{
    throw new TypeError("tool.job received an unresolved tool builder; await the background call and pass result.id");
  }}
  if (!Number.isInteger(job) || job < 1) {{
    throw new TypeError("tool.job requires a positive integer job ID");
  }}
  const methods = Object.create(null);
  for (const manifest of __builders) {{
    if (manifest.binding === "job_method") methods[manifest.method] = (...values) => __jobBuilder(manifest, job, values);
  }}
  return Object.freeze(methods);
}};

async function __request(request) {{
  const response = __parse(await __skyhookHostCall(__stringify(request)));
  if (!response.ok) throw new Error(response.error);
  return response.value;
}}

function __execute(operation) {{
  if (!operation || operation[__callKind] !== true) throw new TypeError("expected a tool builder");
  const data = __callData.get(operation);
  let execution = __executions.get(operation);
  if (!execution) {{
    execution = __request({{ type: "call", name: data.name, arguments: data.arguments }})
      .catch(error => {{ throw new Error(`tool "${{data.name}}" failed: ${{error?.message ?? error}}`, {{ cause: error }}); }});
    __executions.set(operation, execution);
  }}
  return execution;
}}

class Queue {{
  constructor() {{ this.values = []; this.readers = []; this.closed = false; }}
  push(value) {{
    if (this.closed) throw new Error("queue is closed");
    if (this.readers.length) {{ this.readers.shift()({{value, done:false}}); return; }}
    this.values.push(value);
  }}
  async next() {{
    if (this.values.length) return {{value:this.values.shift(), done:false}};
    if (this.closed) return {{value:undefined, done:true}};
    return new Promise(resolve => this.readers.push(resolve));
  }}
  close() {{
    if (this.closed) return;
    this.closed = true;
    for (const reader of this.readers.splice(0)) reader({{value:undefined, done:true}});
  }}
  [Symbol.asyncIterator]() {{ return this; }}
}}

class WorkPool {{
  constructor(concurrency, options = {{}}) {{
    if (!Number.isInteger(concurrency) || concurrency < 1) throw new RangeError("concurrency must be positive");
    this.concurrency = concurrency; this.failFast = options.failFast !== false;
  }}
  async map(items, worker) {{
    const values = Array.from(items), results = new Array(values.length);
    let next = 0, failure;
    const run = async () => {{
      while (next < values.length && !failure) {{
        const index = next++;
        try {{
          const value = await worker(values[index], index);
          results[index] = this.failFast ? value : {{ok:true, value}};
        }} catch (error) {{
          if (this.failFast) {{ failure = error; return; }}
          results[index] = {{ok:false, error:String(error?.message ?? error)}};
        }}
      }}
    }};
    await Promise.all(Array.from({{length:Math.min(this.concurrency, values.length)}}, run));
    if (failure) throw failure;
    return results;
  }}
  async run(tasks) {{ return this.map(tasks, task => task()); }}
}}

const receive = async () => __request({{type:"receive"}});
const notify = async value => __request({{type:"notify", value}});

async function __resolve(value, path, ancestors) {{
  if (value && value[__callKind] === true) {{
    return __execute(value).catch(error => {{
      throw new Error(`deferred tool call at ${{path}} failed: ${{error?.message ?? error}}`, {{cause:error}});
    }});
  }}
  if (value === undefined) {{
    if (path === "$") return null;
    throw new TypeError(`undefined at ${{path}} is not JSON-compatible`);
  }}
  if (value === null || typeof value === "string" || typeof value === "boolean") return value;
  if (typeof value === "number") {{
    if (!Number.isFinite(value)) throw new TypeError(`non-finite number at ${{path}}`);
    return value;
  }}
  if (typeof value !== "object") throw new TypeError(`${{typeof value}} at ${{path}} is not JSON-compatible`);
  if (ancestors.has(value)) throw new TypeError(`circular value at ${{path}}`);
  const nested = new Set(ancestors); nested.add(value);
  if (Array.isArray(value)) return Promise.all(value.map((item, index) => __resolve(item, `${{path}}[${{index}}]`, nested)));
  const output = {{}};
  await Promise.all(Object.keys(value).map(async key => {{ output[key] = await __resolve(value[key], `${{path}}.${{key}}`, nested); }}));
  return output;
}}

(async () => {{
  "use strict";
  const value = await (async () => {{
{USER_SOURCE_MARKER}{source}
  }})();
  return __stringify(await __resolve(value, "$", new Set()));
}})()
"#
    )
}

#[cfg(test)]
mod tests {
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::*;
    use crate::{
        identity::AgentId,
        job::JobManager,
        session::SessionStore,
        tool::policy::AllowAll,
        tool::{ToolOptions, ToolRegistryBuilder, executor::ToolExecutor},
    };

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
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store);
        let lease = jobs
            .create(crate::job::JobSpec::test(agent.clone(), "script"))
            .await
            .unwrap();
        let executor = ToolExecutor::new(
            builder.build(),
            Arc::new(AllowAll),
            jobs.clone(),
            root.path().to_path_buf(),
        );
        let context = ToolContext::new(
            crate::tool::authorization::AuthorizationSubject {
                agent,
                job: lease.id,
                parent: None,
                scope: None,
                capabilities: crate::tool::policy::CapabilitySet::default(),
                cancellation: lease.cancellation.clone(),
            },
            crate::execution::ExecutionLocation::root(root.path().to_path_buf()),
            crate::execution::ExecutionLocation::root(root.path().to_path_buf()),
            lease.input,
            jobs.progress_sink(lease.id),
        );
        (root, executor, context)
    }

    #[tokio::test]
    async fn lazy_builders_are_memoized_and_returned_builders_are_concurrent() {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Echo, String, _, _>(
                "echo",
                "echo",
                ToolOptions::default(),
                |_context, input| async move { Ok(input.value) },
            )
            .unwrap();
        let (_root, executor, context) = test_runtime(builder).await;
        let output = evaluate(
            "const x=tool.echo({value:'a'}); return [x,x,tool.echo({value:'b'})];".to_owned(),
            executor,
            context,
        )
        .await
        .unwrap();
        assert_eq!(output.value, serde_json::json!(["a", "a", "b"]));
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
    async fn queue_and_work_pool_are_available_to_scripts() {
        let (_root, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        let output = evaluate(
            r#"
const queue = new Queue();
queue.push(2); queue.push(3); queue.close();
const values = [];
for await (const value of queue) values.push(value);
const pooled = await new WorkPool(2).map(values, async value => value * 2);
const settled = await new WorkPool(2, {failFast:false}).map(values, async value => {
  if (value === 3) throw new Error("three");
  return value * 3;
});
return {values, pooled, settled};
"#
            .to_owned(),
            executor,
            context,
        )
        .await
        .unwrap();
        assert_eq!(output.value["values"], serde_json::json!([2, 3]));
        assert_eq!(output.value["pooled"], serde_json::json!([4, 6]));
        assert_eq!(
            output.value["settled"],
            serde_json::json!([{"ok":true,"value":6}, {"ok":false,"error":"three"}])
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
