//! Sandboxed JavaScript orchestration over the shared tool registry.

mod bridge;
mod engine;
mod errors;
mod isolate;
mod result;
mod runtime;
mod source;
mod urls;
mod web;

pub(crate) use engine::MemoryLimits;
pub(crate) use result::ScriptResult;
pub(crate) use runtime::evaluate_captured;

/// The fixtures the runtime's modules test with.
#[cfg(test)]
mod tests {
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::Value;

    use super::{
        bridge::Finished,
        engine::MemoryLimits,
        isolate,
        runtime::evaluate_captured,
        source::{FILENAME, SubmittedSource},
    };
    use crate::tests::TestRuntime;
    use crate::tool::{
        ToolContext, ToolError, ToolOptions, ToolOutput, ToolRegistryBuilder,
        executor::ToolExecutor,
    };

    /// Small enough that the memory tests fill little; a script needs far less.
    pub(super) const LIMITS: MemoryLimits = MemoryLimits {
        heap: 32 * 1024 * 1024,
        buffers: 32 * 1024 * 1024,
    };

    // Tests may eagerly inspect the console; production hydrates job captures on collection.
    pub(super) async fn evaluate(
        source: impl Into<String>,
        executor: ToolExecutor,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let saved = executor.jobs().output(context.job()).await;
        let captured = evaluate_captured(source.into(), executor, context, LIMITS).await;
        let mut output = captured?;
        let console = saved
            .test_bytes("/result/console")
            .map(|bytes| String::from_utf8(bytes).expect("console is UTF-8"))
            .unwrap_or_default();
        output.value["console"] = Value::String(console);
        Ok(output)
    }

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Echo {
        pub(super) value: String,
    }

    pub(super) struct TestScope {
        // Keep the job alive while the test drives evaluation directly.
        _worker: crate::job::JobWorker,
        _root: tempfile::TempDir,
    }

    pub(super) async fn test_runtime(
        builder: ToolRegistryBuilder,
    ) -> (TestScope, ToolExecutor, ToolContext) {
        let runtime = TestRuntime::new().await;
        let spec = crate::job::JobSpec::test(runtime.agent.clone(), "script");
        let lease = runtime.jobs.create(spec).await.unwrap().test_run().await;
        let executor = runtime.executor(builder);
        let (context, worker) = runtime.tool_context(lease);
        (
            TestScope {
                _worker: worker,
                _root: runtime.root,
            },
            executor,
            context,
        )
    }

    /// A runtime whose `echo` tool returns its `value`.
    pub(super) async fn echo_runtime() -> (TestScope, ToolExecutor, ToolContext) {
        let mut builder = ToolRegistryBuilder::default();
        let echo = |_, input: Echo| async move { Ok(input.value) };
        builder
            .register::<Echo, String, _, _>("echo", "echo", ToolOptions::default(), echo)
            .unwrap();
        test_runtime(builder).await
    }

    /// Evaluates `source` against an executor with no registered tools.
    pub(super) async fn plain(source: &str) -> Result<ToolOutput, ToolError> {
        let (_scope, executor, context) = test_runtime(ToolRegistryBuilder::default()).await;
        evaluate(source, executor, context).await
    }

    // Standalone wrapper regressions: no executor, jobs, or external processes are needed.
    const BUILDERS: &str = r#"[
    {"name":"echo","properties":["value"]}
]"#;

    const MOCK_HOST: &str = r#"
const __testCalls = [];
function __skyhookHostCall(encoded) {
  const request = JSON.parse(encoded);
  __testCalls.push(request);
  if (request.type === "receive") {
    return JSON.stringify({outcome: "ok", value: {id: 99, state: "completed"}, presented: {}});
  }
  const presented = {"/result/value": "complete"};
  if (Array.isArray(request.arguments.value)) presented["/result/value/0"] = "ends";
  return JSON.stringify({outcome: "ok", value: {
    id: __testCalls.length, state: "completed", result: request.arguments,
  }, presented});
}
function __skyhookConsoleLog(message) {
  throw new Error(`unexpected console output: ${message}`);
}
function __skyhookHostSleep() {
  return [Promise.resolve(), 0];
}
function __skyhookHostWake() {}
"#;

    pub(super) fn evaluate_wrapper_envelope(source: &str) -> Finished {
        fn eval<'s>(scope: &v8::PinScope<'s, '_>, text: &str) -> v8::Local<'s, v8::Value> {
            let text = v8::String::new(scope, text).unwrap();
            let script = v8::Script::compile(scope, text, None).expect("mock host compiles");
            script.run(scope).expect("mock host runs")
        }
        let mut engine = super::engine::Engine::new(LIMITS).expect("script isolate");
        // Termination ends a synchronous scheduling loop that never yields; a
        // mutable zero/NaN pool limit used to spin.
        let (finished, watched) = std::sync::mpsc::channel::<()>();
        let isolate = engine.isolate.thread_safe_handle();
        let watchdog = std::thread::spawn(move || {
            let deadline = std::time::Duration::from_secs(5);
            if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = watched.recv_timeout(deadline)
            {
                isolate.terminate_execution();
            }
        });
        let outcome = {
            v8::scope!(let scope, &mut engine.isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            eval(scope, MOCK_HOST);
            let host = [
                "request => __skyhookHostCall(request)",
                "ms => __skyhookHostSleep(ms)",
                "sleep => __skyhookHostWake(sleep)",
                "text => __skyhookConsoleLog(text)",
                "() => 0",
            ]
            .map(|function| eval(scope, function));
            let source = SubmittedSource::new(source);
            let config = format!(
                r#"{{"builders":{BUILDERS},"terminal":["completed","failed","cancelled","interrupted"],"filename":"{FILENAME}"}}"#
            );
            isolate::script(scope, &config, &source, host, |_| {
                panic!("the wrapper awaits what no pending host work settles")
            })
        };
        drop(finished);
        watchdog.join().unwrap();
        outcome.expect("finish wrapper before the watchdog deadline")
    }

    pub(super) fn evaluate_wrapper(source: &str) -> Value {
        let Finished { returned, failure } = evaluate_wrapper_envelope(source);
        assert!(failure.is_none(), "wrapper failed: {failure:?}");
        returned.value
    }
}
