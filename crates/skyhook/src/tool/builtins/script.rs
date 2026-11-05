use std::sync::{OnceLock, Weak};

use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::{
    RegistryError, ToolError, ToolOptions, ToolRegistryBuilder,
    diagnostic::{Effects, Operation, Subject},
    executor::ToolExecutor,
    registry::AdmittedInvocation,
};

pub(crate) fn install_script_tool(
    builder: &mut ToolRegistryBuilder,
    executor: Weak<OnceLock<ToolExecutor>>,
) -> Result<(), RegistryError> {
    builder.register_checked(
        super::names::SCRIPT,
        SCRIPT_DESCRIPTION,
        ToolOptions::default()
            .job_role(crate::job::JobRole::Script)
            .background()
            .input()
            .script_unavailable()
            .result::<crate::tool::javascript::ScriptResult>(),
        Ok,
        move |args: ScriptArgs| {
            let executor = executor.clone();
            AdmittedInvocation::new(move |context| async move {
                let executor = executor
                    .upgrade()
                    .and_then(|slot| slot.get().cloned())
                    .ok_or_else(|| {
                        ToolError::failed("script executor is not initialized")
                            .operation(Operation::Prepare, Subject::Label("script executor".into()))
                            .effects(Effects::NotStarted)
                    })?
                    .with_location(context.caller_location().clone())
                    .with_capabilities(context.capabilities().clone());
                crate::tool::javascript::evaluate_captured(args.source, executor, context).await
            })
        },
    )?;
    Ok(())
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScriptArgs {
    /// Async JavaScript function body.
    source: String,
}

const SCRIPT_DESCRIPTION: &str = r#"Run an async JavaScript function body in QuickJS-ng with modern ECMAScript syntax, selected standard built-ins, and top-level await/return. The model receives `JobView<{value,console?,failure?}>`: value is the returned JSON (null without return), console the captured console.log text/JSON, failure a thrown error's other properties (such as cause), when it has any; the envelope's state/error report failure. No Node.js APIs or global `fetch` (use `tool.fetch(...)`), `URL`, `TextEncoder`/`TextDecoder`, or `setTimeout`/`setInterval`. No recursive script execution. Built-ins include Date, RegExp, Map/Set, Proxy/Reflect, BigInt, ArrayBuffer, DataView and typed arrays; Uint8Array supports fromBase64/fromHex/toBase64/toHex. Returns must be JSON: nested promises/builders are awaited and Errors become objects. Conversion runs after the body returns, beyond its try/catch: as in JSON.stringify, undefined members are omitted and undefined array elements become null; BigInts, dates, typed arrays, functions, cycles and throwing or rejecting members become null and fail the script, listed by path. Arguments omit undefined properties.

Awaiting a tool call yields `JobView<T>`, an envelope with native result `T` at `.result`; operational failures are `failed` envelopes, not throws. Where data is:
- a call: `r.result`, or `r.unwrap()`: the native payload itself (`r.unwrap().stdout`; `null` if none) when completed; otherwise it throws, with `.response`, `.output` (possibly `null`), and `.job` and `.code` when present
- `tool.jobs()`: `r.unwrap().jobs`
- `tool.jobs({job})`: the target's own view, `v.state` and `v.result`
- `tool.jobs({job, field})`: a page at `v.presentation.preview`
- `tool.job(id).cancel()`: `r.unwrap()` is the target's view
Absent optional tool-response fields (`x?`) read as `null`. Example: `const file = (await tool.read({path:"Cargo.toml"})).unwrap();`. Builders also chain: `tool.read().path("Cargo.toml")`, or `.set(key, value)` for any argument name, including dynamic keys; omitted arguments keep schema defaults. Builders execute once when awaited/returned. Builders nested in a returned value run concurrently: `return {a:tool.read({path:"a"}),b:tool.read({path:"b"})};`. In the body, `await {a:builder}` does not await members: await each builder or use Promise.all.

`await sleep(ms)`: 0 to 2147483647 milliseconds; cancellation interrupts it. performance.now() measures elapsed milliseconds. Unawaited promises/sleeps do not keep scripts alive; use background jobs for lasting work.

`new WorkPool(n).map(items,worker)` or `.run([fn1,fn2,...])` returns an async iterable of {index,value} in completion order, at most n workers. run takes exactly one array of functions called with no arguments; invalid arguments throw before any task starts. index identifies the original input; value is what the worker returned; a worker that throws yields {index,error} instead. Breaking stops scheduling and awaits running workers. `.feed(next,worker,{wake?})` runs work that becomes ready over time: it calls `await next(running,free)` (running: a Map of key to item; free: open slots) whenever a slot frees, starts the returned items whose `key` is not running (extras wait their turn before next is called again), and yields {key,item,value} or {key,item,error} per completion; it ends when nothing runs and next starts nothing. `wake` (e.g. `receive`) is raced while work runs: each value yields {wake}; a wake pending at the end is released to the next receive(). Awaiting `tool.agent({...})` resolves with the child's final JobView, so a `WorkPool` of `tool.agent` builders runs a bounded pool of children without `wait` or `jobs` polling.
```js
const paths = ["Cargo.toml", "justfile"], out = [];
for await (const {index, value: r} of new WorkPool(2).map(paths, path => tool.read({path}))) {
  out.push({index, content: r.unwrap().content});
}
return out;
```

For bg:true, await the launch for its job ID. Inspection returns at once, so do independent work, then wait only while the job is unfinished: `const {id}=await tool.exec({command:"make test",bg:true}); let v=await tool.jobs({job:id}); while(["queued","running"].includes(v.state)){await tool.wait({timeout:300}); v=await tool.jobs({job:id});} return v;`. `wait({timeout?:seconds})` yields until an agent event or the timeout (positive integer seconds, capped at 1800; omitted/null waits 1800); its `.result` is `{reason, waited_s}`, reason "event", "timeout" or "cancelled"; an event need not mean the job completed.
`await receive()` waits for the next value sent to this script's own job ID with `tool.job(scriptJobId).send({value})` and resolves to that value itself, not a JobView (the sender gets a JobView with no result); requires `script` with `bg:true`. Child-agent input is delivered automatically; do not use a script to receive it."#;
