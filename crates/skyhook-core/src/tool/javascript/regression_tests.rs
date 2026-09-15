//! Standalone wrapper regressions: no executor, jobs, or external processes are needed.

use std::time::{Duration, Instant};

use rquickjs::{CatchResultExt, Context, Promise, Runtime};
use serde_json::{Value, json};

use super::runtime::wrapper_script;

const BUILDERS: &str = r#"[
    {"binding":"top_level","name":"echo","properties":["value"],"required":["value"]},
    {"binding":"job_method","name":"job_send","method":"send","job_argument":"job","properties":["job","value"],"required":["value"]},
    {"binding":"job_method","name":"job_output","method":"output","job_argument":"job","properties":["job","field"],"required":[]},
    {"binding":"job_method","name":"job_cancel","method":"cancel","job_argument":"job","properties":["job"],"required":[]}
]"#;

const MOCK_HOST: &str = r#"
const __testCalls = [];
function __skyhookHostCall(encoded) {
  const request = JSON.parse(encoded);
  __testCalls.push(request);
  return JSON.stringify({ok: true, value: request.arguments});
}
function __skyhookConsoleLog(message) {
  throw new Error(`unexpected console output: ${message}`);
}
"#;

fn evaluate(source: &str) -> Value {
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
        assert_eq!(result["ok"], true, "wrapper failed: {result:#}");
        result["value"].clone()
    })
}

#[test]
fn bound_job_normal_calls_and_immutable_chains_keep_receiver() {
    let result = evaluate(
        r#"
const base = tool.job(7).send();
const first = base.value("first");
const second = first.set("value", "second");
await first;
await first; // The same operation executes only once.
await second;
await tool.job(8).send({value: "object"});
await tool.job(9).output({field: "/result/value"});
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
            {"type":"call", "name":"job_output", "arguments":{"job":9,"field":"/result/value"}},
            {"type":"call", "name":"job_cancel", "arguments":{"job":10}},
            {"type":"call", "name":"job_cancel", "arguments":{"job":1}},
            {"type":"call", "name":"job_cancel", "arguments":{"job":9_007_199_254_740_991_u64}}
        ])
    );
}
