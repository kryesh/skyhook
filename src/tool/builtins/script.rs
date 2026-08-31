use std::sync::{Arc, OnceLock};

use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::tool::{
    RegistryError, ToolError, ToolRegistryBuilder, executor::ToolExecutor, policy::ToolEffect,
};

pub fn install_script_tool(
    builder: &mut ToolRegistryBuilder,
    executor: Arc<OnceLock<ToolExecutor>>,
) -> Result<(), RegistryError> {
    let schema = serde_json::to_value(schema_for!(ScriptArgs))
        .map_err(|error| RegistryError::Schema(error.to_string()))?;
    builder.register_dynamic(
        "script",
        SCRIPT_DESCRIPTION,
        schema,
        vec![ToolEffect::SessionState],
        true,
        true,
        move |context, arguments| {
            let executor = executor.clone();
            async move {
                let args: ScriptArgs = serde_json::from_value(arguments)
                    .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
                let executor = executor.get().cloned().ok_or_else(|| {
                    ToolError::Failed("script executor is not initialized".to_owned())
                })?;
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

const SCRIPT_DESCRIPTION: &str = r#"Run an async JavaScript function body with top-level `await` and `return`. Registered tools are available through lazy, memoized builders: `await tool.read({path:"src/lib.rs"})` and `await tool.read().path("src/lib.rs")` are equivalent, and omitted fields retain schema defaults. Returning builders from nested arrays or objects executes independent calls concurrently; `Promise.all` is also available.

Examples:
`return {lib: tool.read().path("src/lib.rs"), manifest: tool.read({path:"Cargo.toml"})};`
`const files = await tool.glob().pattern("src/**/*.rs"); const pool = new WorkPool(4); return pool.map(files, path => tool.read({path}));`
`const queue = new Queue(); queue.push("work"); queue.close(); const seen=[]; for await (const value of queue) seen.push(value); return seen;`

`new WorkPool(n, {failFast:false})` preserves input order and returns `{ok:true,value}` or `{ok:false,error}` per item; with `failFast:true` it throws on failure. `Queue` is an async FIFO with `push`, `next`, `close`, and async iteration. Use `tool.job(id).inspect()/wait()/send({value})/cancel()/events()` for jobs. `tool.skill(name)` loads a skill; use `.asset({path})` to read an asset instead. `receive()` waits for input and is only valid when this script invocation has `bg:true`; `notify(value)` emits durable progress from foreground or background scripts."#;
