use std::sync::{OnceLock, Weak};

use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::catalog::{RegistryError, ToolOptions};
use crate::tool::{ToolError, ToolRegistryBuilder, catalog::Invocation, executor::ToolExecutor};
use skyhook_worker::diagnostic::{Effects, Operation, Subject};

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
            Invocation::new(move |context| async move {
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
                let limits = crate::tool::javascript::MemoryLimits::SCRIPT;
                crate::tool::javascript::evaluate_captured(args.source, executor, context, limits)
                    .await
            })
        },
    )?;
    Ok(())
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScriptArgs {
    source: String,
}

const SCRIPT_DESCRIPTION: &str = r#"Run an async JavaScript function body with top-level await/return to compute over tool results and files. `value` is the returned JSON (null without return), `console` the captured console.log text/JSON, `failure` a thrown error's other properties (such as cause), when it has any; the envelope's state/error report failure. The host is reached only through `tool`: no imports or host globals, and no global `fetch` (use `tool.fetch(...)`). Returned values convert to JSON: nested promises/builders are awaited and native values take their JSON form. Conversion runs after the body returns, beyond its try/catch: as in JSON.stringify, undefined members are omitted and undefined array elements become null; functions, symbols, cycles and throwing or rejecting members become null and fail the script, listed by path. Arguments omit undefined properties.

Awaiting a tool call yields its JobView; operational failures are `failed` envelopes, not throws. Read `r.result`, or `r.unwrap()`: the native payload itself (`r.unwrap().stdout`; `null` if none) when completed; otherwise it throws, with `.response`, `.output` (possibly `null`), and `.job` and `.code` when present. Absent optional tool-response fields (`x?`) read as `null`. Example: `const file = (await tool.read({path:"README.md"})).unwrap();`. Omitted arguments keep schema defaults. Builders execute once when awaited/returned. Promise.all runs independent calls together.

`await sleep(ms)` waits ms milliseconds, at most 1800000 (30 minutes). Unawaited promises, sleeps and timers do not keep scripts alive; use background jobs for lasting work.

`new WorkPool(n).map(items,worker)` or `.run([fn1,fn2,...])` returns an async iterable of {index,value} in completion order, at most n workers. run takes exactly one array of functions called with no arguments; invalid arguments throw before any task starts. index identifies the original input; value is what the worker returned; a worker that throws yields {index,error} instead. Breaking stops scheduling and awaits running workers. `.feed(next,worker,{wake?})` runs work that becomes ready over time: it calls `await next(running,free)` (running: a Map of key to item; free: open slots) whenever a slot frees, starts the returned items whose `key` is not running (extras wait their turn before next is called again), and yields {key,item,value} or {key,item,error} per completion; it ends when nothing runs and next starts nothing. `wake` (e.g. `receive`) is raced while work runs: each value yields {wake}; a wake pending at the end is released to the next receive(). Awaiting `tool.agent({...})` resolves with the child's final JobView, so a `WorkPool` of `tool.agent` builders runs a bounded pool of children without `wait` or `jobs` polling.
```js
const paths = ["README.md", "LICENSE"], out = [];
for await (const {index, value: r} of new WorkPool(2).map(paths, path => tool.read({path}))) {
  out.push({index, content: r.unwrap().content});
}
return out;
```

`await receive()` resolves to the next value sent to this script's job, itself, not a JobView; requires bg:true"#;
