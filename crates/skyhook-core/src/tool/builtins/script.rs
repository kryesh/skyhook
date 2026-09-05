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
    /// Async JavaScript body.
    source: String,
}

const SCRIPT_DESCRIPTION: &str = r#"Run an async JavaScript body with top-level await/return. Foreground returns JSON (null without return). console.log captures text/JSON. No Node, fetch, URL, TextEncoder/TextDecoder, setTimeout/setInterval or recursive script. Built-ins include Date, RegExp, Map/Set, Proxy/Reflect, BigInt, ArrayBuffer, DataView and typed arrays; Uint8Array supports fromBase64/fromHex/toBase64/toHex. Return JSON-compatible values: convert BigInts, dates and typed arrays; undefined, non-finite numbers, functions and cycles fail serialization.

Use direct tools' arguments/results: `tool.read({path:"Cargo.toml"})` or `tool.read().path("Cargo.toml")`; omitted arguments keep schema defaults. Builders execute once when awaited/returned. Returning nested builders runs independent calls concurrently: `return {a:tool.read({path:"a"}),b:tool.read({path:"b"})};`. Promise.all works; await ordinary promises before nesting results.

`await sleep(ms)`: finite nonnegative milliseconds within the host timer range; cancellation interrupts it. performance.now() measures elapsed milliseconds. Unawaited promises/sleeps do not keep scripts alive; use background jobs for lasting work.

`new WorkPool(n).map(items,worker)` or `.run(zeroArgFunctions)` returns an async iterable of {index,value} in completion order, at most n workers. index identifies the original input; value is its result. Failures are logged/skipped; breaking stops scheduling and awaits running workers.
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

For bg:true, await launch before using the positive integer job ID: `const j=await tool.shell({command:"make test",bg:true}); return await tool.job(j.id).wait();`.
Inside background scripts, `await receive()` reads the next JSON input sent with tool.job(id).send({value}). Read command output events with tool.job(id).events()."#;
