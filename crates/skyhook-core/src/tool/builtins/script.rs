use std::sync::{OnceLock, Weak};

use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::{
    RegistryError, ToolError, ToolOptions, ToolRegistryBuilder, executor::ToolExecutor,
};

pub(crate) fn install_script_tool(
    builder: &mut ToolRegistryBuilder,
    executor: Weak<OnceLock<ToolExecutor>>,
) -> Result<(), RegistryError> {
    builder.register_product::<ScriptArgs, crate::tool::javascript::ScriptResult, _, _>(
        "script",
        SCRIPT_DESCRIPTION,
        ToolOptions::default()
            .job_role(crate::job::JobRole::Script)
            .background()
            .input()
            .script_unavailable(),
        move |context, args| {
            let executor = executor.clone();
            async move {
                let executor = executor
                    .upgrade()
                    .and_then(|slot| slot.get().cloned())
                    .ok_or_else(|| {
                        ToolError::Failed("script executor is not initialized".to_owned())
                    })?
                    .with_location(context.caller_location().clone())
                    .with_capabilities(context.capabilities().clone());
                crate::tool::javascript::evaluate_captured(args.source, executor, context)
                    .await
                    .map_err(script_error)
            }
        },
    )?;
    Ok(())
}

fn script_error(captured: crate::tool::javascript::CapturedJsError) -> ToolError {
    use crate::tool::javascript::{JsError, script_output};
    let crate::tool::javascript::CapturedJsError { error, console } = captured;
    let (message, details) = match error {
        JsError::Cancelled => return ToolError::Cancelled,
        JsError::Failure { message, details } => (message, Some(details)),
        error => (error.to_string(), None),
    };
    ToolError::with_output(
        message,
        script_output(serde_json::Value::Null, details, console.map(|c| *c)),
    )
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScriptArgs {
    /// Async JavaScript body.
    source: String,
}

const SCRIPT_DESCRIPTION: &str = r#"Run an async JavaScript body in QuickJS-ng with modern ECMAScript syntax, selected standard built-ins, and top-level await/return. Native result is {value,console}: value is the returned JSON (null without return), and console is captured console.log text/JSON. The model receives a JobView with this script result. Read saved console at /result/console and returned-value fields under /result/value. No Node.js APIs or global `fetch` (use `tool.fetch(...)` for HTTP requests), `URL`, `TextEncoder`/`TextDecoder`, or `setTimeout`/`setInterval`. No recursive script execution. Built-ins include Date, RegExp, Map/Set, Proxy/Reflect, BigInt, ArrayBuffer, DataView and typed arrays; Uint8Array supports fromBase64/fromHex/toBase64/toHex. Return JSON-compatible values: convert BigInts, dates and typed arrays; undefined, non-finite numbers, functions and cycles fail serialization.

Use direct tools' arguments/results: `tool.read({path:"Cargo.toml"})` or `tool.read().path("Cargo.toml")`; omitted arguments keep schema defaults. Builders execute once when awaited/returned. Returning nested builders runs independent calls concurrently: `return {a:tool.read({path:"a"}),b:tool.read({path:"b"})};`. Promise.all works; await ordinary promises before nesting results.

`await sleep(ms)`: finite nonnegative milliseconds within the host timer range; cancellation interrupts it. performance.now() measures elapsed milliseconds. Unawaited promises/sleeps do not keep scripts alive; use background jobs for lasting work.

`new WorkPool(n).map(items,worker)` or `.run([fn1,fn2,...])` returns an async iterable of {index,value} in completion order, at most n workers. run takes exactly one array of functions called with no arguments (not variadic); invalid arguments throw before any task starts. index identifies the original input; value is its result. Task failures are logged/skipped; breaking stops scheduling and awaits running workers.
```js
const {paths} = await tool.glob({pattern: "src/**/*.rs"});
const results = [];
for await (const {index, value} of new WorkPool(4).map(
  paths, path => tool.read({path})
)) {
  results.push({index, path: value.path, content: value.content});
}
return results;
```

For bg:true, await launch before using the positive integer job ID: `const j=await tool.shell({command:"make test",bg:true}); return await tool.job(j.id).output();`. Output inspection returns immediately without waiting. Do independent work first; use `await tool.wait({timeout:300})` to yield for an agent event or timeout, then inspect again. `wait({timeout?:seconds})` waits indefinitely if timeout is omitted/null; otherwise it accepts only positive integer seconds. It returns `{reason:"event"}` or `{reason:"timeout"}`; an event need not mean the job completed.
`await receive()` waits for the next JSON value sent to this script's own job ID with `tool.job(scriptJobId).send({value})`; requires `script` with `bg:true`. Child-agent input is delivered automatically; do not use a script to receive it. Read or search saved output with tool.job(id).output({field:"/result/stdout"})."#;
