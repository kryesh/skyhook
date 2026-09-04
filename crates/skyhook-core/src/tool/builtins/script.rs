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
                    .map_err(|error| ToolError::Failed(error.to_string()))
            }
        },
    )?;
    Ok(())
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScriptArgs {
    /// An async JavaScript function body. Top-level `await` and `return` are supported.
    source: String,
}

const SCRIPT_DESCRIPTION: &str = r#"Run an async JavaScript function body with top-level `await` and `return`. Registered tools are available through lazy builders that memoize execution per builder instance: awaiting or returning the same builder twice executes it once, while separately constructed builders execute separately. `await tool.read({path:"src/lib.rs"})` and `await tool.read().path("src/lib.rs")` are equivalent, and omitted fields retain schema defaults. Returning builders from nested arrays or objects executes independent calls concurrently; `Promise.all` is also available.

Examples:
`return {lib: tool.read().path("src/lib.rs"), manifest: tool.read({path:"Cargo.toml"})};`
`const {paths} = await tool.glob().pattern("src/**/*.rs"); const pool = new WorkPool(4); return pool.map(paths, path => tool.read({path}));`
`const queue = new Queue(); queue.push("work"); queue.close(); const seen=[]; for await (const value of queue) seen.push(value); return seen;`

`new WorkPool(n)` preserves input order and fails fast, returning direct values. Pass `{failFast:false}` to receive `{ok:true,value}` or `{ok:false,error}` per item instead. `Queue` is an async FIFO with `push`, `next`, `close`, and async iteration. Await a background builder before using its envelope: `const job = await tool.shell({command:"make test", bg:true}); return tool.job(job.id).wait({timeout:300});`. Job-control and skill APIs are generated from their registered schemas and documented below. `receive()` waits for input and is only valid when this script invocation has `bg:true`; always use `await notify(value)` to ensure durable progress is written before the script returns. The `script` tool is intentionally unavailable inside scripts, so recursive script invocation is blocked. The runtime does not provide Node.js APIs or `console`."#;
