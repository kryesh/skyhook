use std::sync::{Arc, OnceLock, Weak};

use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde_json::Value;

use crate::tool::{
    RegistryError, ToolError, ToolOptions, ToolRegistryBuilder, executor::ToolExecutor,
};

pub fn install_script_tool(
    builder: &mut ToolRegistryBuilder,
    executor: Arc<OnceLock<ToolExecutor>>,
) -> Result<(), RegistryError> {
    install_script_tool_with(builder, move || executor.get().cloned())
}

pub(crate) fn install_script_tool_weak(
    builder: &mut ToolRegistryBuilder,
    executor: Weak<OnceLock<ToolExecutor>>,
) -> Result<(), RegistryError> {
    install_script_tool_with(builder, move || {
        executor.upgrade().and_then(|slot| slot.get().cloned())
    })
}

fn install_script_tool_with<F>(
    builder: &mut ToolRegistryBuilder,
    executor: F,
) -> Result<(), RegistryError>
where
    F: Fn() -> Option<ToolExecutor> + Send + Sync + 'static,
{
    let executor = Arc::new(executor);
    let schema = serde_json::to_value(schema_for!(ScriptArgs))
        .map_err(|error| RegistryError::Schema(error.to_string()))?;
    let output_schema = serde_json::to_value(schema_for!(Value))
        .map_err(|error| RegistryError::Schema(error.to_string()))?;
    builder.register_dynamic(
        "script",
        SCRIPT_DESCRIPTION,
        schema,
        ToolOptions::default()
            .output_schema(output_schema)
            .background()
            .input()
            .script_unavailable(),
        move |context, arguments| {
            let executor = executor.clone();
            async move {
                let args: ScriptArgs = serde_json::from_value(arguments)
                    .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
                let executor = executor()
                    .ok_or_else(|| {
                        ToolError::Failed("script executor is not initialized".to_owned())
                    })?
                    .with_location(context.caller_location.clone())
                    .with_capabilities(context.capabilities.clone());
                crate::tool::javascript::evaluate(args.source, executor, context)
                    .await
                    .map_err(script_error)
            }
        },
    )?;
    Ok(())
}

fn script_error(error: crate::tool::javascript::JsError) -> ToolError {
    use crate::tool::{ToolOutput, javascript::JsError};
    match error {
        JsError::Cancelled => ToolError::Cancelled,
        JsError::Failure { message, details } => ToolError::with_output(
            message,
            ToolOutput::new(serde_json::json!({"failure": details})),
        ),
        JsError::WithConsole {
            error,
            console_output,
        } => {
            let error = script_error(*error);
            match error {
                ToolError::Cancelled => ToolError::Cancelled,
                ToolError::FailedWithOutput {
                    message,
                    mut output,
                } => {
                    output.console_output = console_output;
                    ToolError::with_output(message, output)
                }
                error => {
                    let mut output = ToolOutput::new(serde_json::Value::Null);
                    output.console_output = console_output;
                    ToolError::with_output(error.concise_message(), output)
                }
            }
        }
        error => ToolError::Failed(error.to_string()),
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScriptArgs {
    /// An async JavaScript function body. Top-level `await` and `return` are supported.
    source: String,
}

const SCRIPT_DESCRIPTION: &str = r#"Run an async JavaScript function body for sequencing, transformation, or bounded concurrency. Prefer direct tool calls for simple operations.

Environment and results: Top-level await and return are supported. No Node.js APIs or recursive script calls. Return data to inspect it. console.log(...values) captures space-separated text (objects as JSON). Foreground execution returns the script's value; no return produces null. Return JSON-compatible data: nested undefined, non-finite numbers, functions, BigInts, and circular references fail serialization.

Built-ins include Date, RegExp, Map/Set, Proxy/Reflect, BigInt, ArrayBuffer, DataView, and typed arrays. Uint8Array supports fromBase64/fromHex and toBase64/toHex. Use performance.now() for elapsed milliseconds. Convert BigInts to strings, dates to ISO strings, and typed arrays to ordinary arrays or encoded strings before returning them. No fetch, URL, TextEncoder/TextDecoder, or setTimeout/setInterval.

`await sleep(ms)` asynchronously waits for finite nonnegative milliseconds (including fractional values) and resolves to undefined. Invalid types, negative/non-finite values, or delays outside the host timer range fail. Sleep stops on script cancellation; an unawaited sleep does not keep the script alive. For example: `const start = performance.now(); await sleep(100); return {elapsed_ms: performance.now() - start};`.

Tool calls use their separately documented arguments and results. `tool.read({path:"src/lib.rs"})` and `tool.read().path("src/lib.rs")` are equivalent; omitted arguments keep schema defaults. Builders are lazy: await or return them to execute. Reusing one builder executes it once; separate builders execute separately. Returning builders inside objects or arrays runs independent calls concurrently:
```js
return {
  lib: tool.read({path: "src/lib.rs"}),
  manifest: tool.read({path: "Cargo.toml"})
};
```
Promise.all is also supported. Await ordinary promises before placing their results inside returned objects or arrays. Tool failures throw; command results with nonzero exit_code do not.

Bounded concurrency: `new WorkPool(n).map(items, worker)` and `.run(tasks)` return async iterables; tasks are zero-argument worker functions. Each iteration yields {index, value}: index identifies the input, value is the worker's result. Successful results arrive in completion order, with at most n workers running. Failed items are logged and skipped; remaining items continue. Breaking iteration stops scheduling and waits for running workers.
```js
const {paths} = await tool.glob({pattern: "src/**/*.rs"});
const results = [];
for await (const {index, value} of new WorkPool(4).map(
  paths, path => tool.read({path})
)) {
  results.push({index, path: value.path, content: value.content, truncated: value.truncated});
}
return results;
```

Background execution: bg:true returns a JobEnvelope. Await the launch before accessing its ID. tool.job(id) requires the positive integer ID, not the envelope:
```js
const job = await tool.shell({command: "make test", bg: true});
const result = await tool.job(job.id).wait({timeout: 300});
return {state: result.state, output: result.output};
```
Use background jobs for work that must outlive the script; do not rely on unawaited JavaScript promises. Agent ownership and cancellation follow the shared lifecycle rules.

Messaging: Inside a background script, await receive() returns the next JSON input sent to that job with tool.job(id).send({value}). Read command output events with tool.job(commandJobId).events()."#;
