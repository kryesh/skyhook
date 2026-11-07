//! A script's own thread: its isolate, the host functions it installs, and the
//! loop that settles their promises as host work on Tokio completes. Isolates
//! cannot move between threads, so a script never runs on a Tokio worker.

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    pin::Pin,
    time::{Duration, Instant},
};

use futures_util::future::BoxFuture;

use tokio::{runtime::Handle, task::JoinSet};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use super::{
    bridge::{Failure, Finished, Returned},
    engine::{self, Engine, MemoryLimits},
    errors::JsError,
    source::SubmittedSource,
};

/// The script thread's own stack, well above the stack V8 limits JavaScript to,
/// so deep recursion throws a RangeError whatever `RUST_MIN_STACK` says.
const STACK: usize = 8 * 1024 * 1024;

/// What settles a host call's promise.
pub(super) enum Settlement {
    Response(String),
    Failed(String),
}

/// Starts a tool call or receive the script requested, when JavaScript makes it,
/// for the JSON it reads.
type Request = Box<dyn FnMut(String) -> BoxFuture<'static, Settlement> + Send>;
/// Where the script's console lines go: kept as it logs them, and written out each
/// time it waits, so a loop that logs costs no write per line.
pub(super) trait Console: Send {
    /// Keeps a line, or says why the console cannot take it.
    fn log(&mut self, line: &str) -> Result<(), String>;
    /// Writes out what it kept; a failure is the console's own to report at its
    /// next line or its close.
    fn flush(&mut self);
}

/// The script's host state, kept in its isolate for the host functions.
pub(super) struct Host {
    request: Request,
    console: Box<dyn Console>,
    limits: MemoryLimits,
    started: Instant,
    /// Cancellation, or the caller letting go of the script.
    stop: CancellationToken,
    /// Work issued since the loop last waited: it starts only once JavaScript
    /// yields, so calls issued together are all published before any runs.
    issued: Vec<(
        v8::Global<v8::PromiseResolver>,
        BoxFuture<'static, Settlement>,
    )>,
    calls: JoinSet<Settlement>,
    pending: HashMap<tokio::task::Id, v8::Global<v8::PromiseResolver>>,
    /// Sleeps by the deadline taken at their call, then the order they were made,
    /// so sleeps that come due together settle as they were issued.
    sleeps: BTreeMap<(Instant, u64), v8::Global<v8::PromiseResolver>>,
    /// Each queued sleep's deadline by its number, which is how JavaScript wakes it.
    deadlines: HashMap<u64, Instant>,
    slept: u64,
    /// Whether a due sleep goes before a completed call when both are ready; it
    /// alternates, so neither can starve the other.
    sleeps_first: bool,
}

impl Host {
    pub(super) fn new(
        request: impl FnMut(String) -> BoxFuture<'static, Settlement> + Send + 'static,
        console: impl Console + 'static,
        limits: MemoryLimits,
        stop: CancellationToken,
    ) -> Self {
        Self {
            request: Box::new(request),
            console: Box::new(console),
            limits,
            stop,
            started: Instant::now(),
            issued: Vec::new(),
            calls: JoinSet::new(),
            pending: HashMap::new(),
            sleeps: BTreeMap::new(),
            deadlines: HashMap::new(),
            slept: 0,
            sleeps_first: true,
        }
    }
}

/// Runs the script on its own thread, returning its outcome and the host work
/// still in flight when it ended.
pub(super) async fn run(
    source: SubmittedSource,
    config: String,
    host: Host,
) -> (Result<Finished, JsError>, JoinSet<Settlement>) {
    let runtime = Handle::current();
    let _abandoned = host.stop.clone().drop_guard();
    let (sender, outcome) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("skyhook-script".into())
        .stack_size(STACK)
        .spawn(move || {
            let _ = sender.send(execute(&runtime, &source, &config, host));
        });
    if let Err(error) = spawned {
        return (
            Err(JsError::Initialization(error.to_string())),
            JoinSet::new(),
        );
    }
    outcome.await.expect("a script thread reports its outcome")
}

fn execute(
    runtime: &Handle,
    source: &SubmittedSource,
    config: &str,
    host: Host,
) -> (Result<Finished, JsError>, JoinSet<Settlement>) {
    let _entered = runtime.enter();
    let limits = host.limits;
    let mut engine = match Engine::new(limits) {
        Ok(engine) => engine,
        Err(error) => return (Err(JsError::Initialization(error)), host.calls),
    };
    let (stop, isolate) = (host.stop.clone(), engine.isolate.thread_safe_handle());
    let _terminate = AbortOnDropHandle::new(runtime.spawn(async move {
        stop.cancelled().await;
        isolate.terminate_execution();
    }));
    engine.isolate.set_slot(host);
    let outcome = {
        v8::scope!(let scope, &mut engine.isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        match functions(scope) {
            Some(host) => script(scope, config, source, host, |scope| {
                settle_next(runtime, scope)
            }),
            None => Err(JsError::Initialization(
                "could not create the host functions".into(),
            )),
        }
    };
    let outcome = if engine.exhausted() {
        Err(JsError::HeapLimit(limits.heap))
    } else {
        outcome
    };
    let host = engine
        .isolate
        .remove_slot::<Host>()
        .expect("the isolate keeps its host");
    (outcome, host.calls)
}

/// Waits for the next sleep to come due or host work to settle its promise, or
/// for the script to stop.
fn settle_next(runtime: &Handle, scope: &mut v8::PinScope<'_, '_>) -> Result<(), JsError> {
    enum Next {
        Woken(v8::Global<v8::PromiseResolver>),
        Settled(v8::Global<v8::PromiseResolver>, Settlement),
    }
    let Host {
        console,
        stop,
        issued,
        calls,
        pending,
        sleeps,
        deadlines,
        sleeps_first,
        ..
    } = scope
        .get_slot_mut::<Host>()
        .expect("the isolate keeps its host");
    console.flush();
    for (resolver, task) in issued.drain(..) {
        pending.insert(calls.spawn(task).id(), resolver);
    }
    let due = sleeps.first_key_value().map(|(&(deadline, _), _)| deadline);
    let next = runtime.block_on(async {
        let woken = async {
            let Some(due) = due else {
                return std::future::pending().await;
            };
            // An overdue sleep is ready now, without a turn of the runtime's timer.
            if due > Instant::now() {
                tokio::time::sleep_until(due.into()).await;
            }
            let ((_, slept), resolver) = sleeps.pop_first().expect("the due sleep is queued");
            deadlines.remove(&slept);
            Next::Woken(resolver)
        };
        let settled = async {
            let Some(settled) = calls.join_next_with_id().await else {
                return std::future::pending().await;
            };
            let (task, settlement) =
                settled.unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()));
            let resolver = pending
                .remove(&task)
                .expect("a host task's promise awaits it");
            Next::Settled(resolver, settlement)
        };
        tokio::pin!(woken, settled);
        let (first, second): (
            Pin<&mut dyn Future<Output = Next>>,
            Pin<&mut dyn Future<Output = Next>>,
        ) = if *sleeps_first {
            (woken, settled)
        } else {
            (settled, woken)
        };
        tokio::select! {
            biased;
            () = stop.cancelled() => None,
            next = first => Some(next),
            next = second => Some(next),
        }
    });
    *sleeps_first = !matches!(next, Some(Next::Woken(_)));
    let Some(next) = next else {
        return Err(JsError::Cancelled);
    };
    v8::scope!(let scope, scope);
    match next {
        Next::Woken(resolver) => {
            let undefined = v8::undefined(scope).into();
            v8::Local::new(scope, resolver).resolve(scope, undefined)
        }
        Next::Settled(resolver, Settlement::Response(text)) => {
            let resolver = v8::Local::new(scope, resolver);
            match v8::String::new(scope, &text) {
                Some(text) => resolver.resolve(scope, text.into()),
                None => resolver.reject(
                    scope,
                    error(scope, "the host response is too large to be a string"),
                ),
            }
        }
        Next::Settled(resolver, Settlement::Failed(message)) => {
            let resolver = v8::Local::new(scope, resolver);
            resolver.reject(scope, error(scope, &message))
        }
    };
    Ok(())
}

fn error<'s>(scope: &v8::PinScope<'s, '_>, message: &str) -> v8::Local<'s, v8::Value> {
    let message = v8::String::new(scope, message).unwrap_or_else(|| v8::String::empty(scope));
    v8::Exception::error(scope, message)
}

/// Returns a promise for the host, with its resolver.
fn promise(
    scope: &mut v8::PinScope<'_, '_>,
    mut returned: v8::ReturnValue<v8::Value>,
) -> Option<v8::Global<v8::PromiseResolver>> {
    let resolver = v8::PromiseResolver::new(scope)?;
    returned.set(resolver.get_promise(scope).into());
    Some(v8::Global::new(scope, resolver))
}

/// The script's tool call, sleep, wake, console and clock functions, in the order
/// the runtime's `install` takes them.
fn functions<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<[v8::Local<'s, v8::Value>; 5]> {
    Some([
        v8::Function::new(scope, call)?.into(),
        v8::Function::new(scope, sleep)?.into(),
        v8::Function::new(scope, wake)?.into(),
        v8::Function::new(scope, log)?.into(),
        v8::Function::new(scope, now)?.into(),
    ])
}

fn call(
    scope: &mut v8::PinScope<'_, '_>,
    arguments: v8::FunctionCallbackArguments,
    returned: v8::ReturnValue<v8::Value>,
) {
    let request = arguments.get(0).to_rust_string_lossy(scope);
    let host = scope
        .get_slot_mut::<Host>()
        .expect("the isolate keeps its host");
    let task = (host.request)(request);
    if let Some(resolver) = promise(scope, returned) {
        let host = scope
            .get_slot_mut::<Host>()
            .expect("the isolate keeps its host");
        host.issued.push((resolver, task));
    }
}

/// `[promise, number]`: the sleep, and the number that wakes it. The runtime's
/// `sleep` admits only delays it can wait; its deadline is taken here, at the call.
fn sleep(
    scope: &mut v8::PinScope<'_, '_>,
    arguments: v8::FunctionCallbackArguments,
    mut returned: v8::ReturnValue<v8::Value>,
) {
    let milliseconds = arguments.get(0).number_value(scope).unwrap_or(f64::NAN);
    let delay = Duration::try_from_secs_f64(milliseconds / 1000.0);
    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        return;
    };
    let promise = resolver.get_promise(scope).into();
    let host = scope
        .get_slot_mut::<Host>()
        .expect("the isolate keeps its host");
    host.slept += 1;
    let slept = host.slept;
    match delay {
        Ok(delay) => {
            let deadline = Instant::now() + delay;
            host.deadlines.insert(slept, deadline);
            let resolver = v8::Global::new(scope, resolver);
            let host = scope
                .get_slot_mut::<Host>()
                .expect("the isolate keeps its host");
            host.sleeps.insert((deadline, slept), resolver);
        }
        Err(failure) => {
            let exception = error(scope, &failure.to_string());
            resolver.reject(scope, exception);
        }
    }
    let number = v8::Number::new(scope, slept as f64).into();
    returned.set(v8::Array::new_with_elements(scope, &[promise, number]).into());
}

/// Settles a sleep before its deadline, as clearing its timer does, so nothing
/// waits for it.
fn wake(
    scope: &mut v8::PinScope<'_, '_>,
    arguments: v8::FunctionCallbackArguments,
    _: v8::ReturnValue<v8::Value>,
) {
    let Some(slept) = arguments.get(0).integer_value(scope) else {
        return;
    };
    let host = scope
        .get_slot_mut::<Host>()
        .expect("the isolate keeps its host");
    let slept = u64::try_from(slept).unwrap_or_default();
    let woken =
        (host.deadlines.remove(&slept)).and_then(|deadline| host.sleeps.remove(&(deadline, slept)));
    if let Some(resolver) = woken {
        let undefined = v8::undefined(scope).into();
        v8::Local::new(scope, resolver).resolve(scope, undefined);
    }
}

fn log(
    scope: &mut v8::PinScope<'_, '_>,
    arguments: v8::FunctionCallbackArguments,
    _: v8::ReturnValue<v8::Value>,
) {
    let text = arguments.get(0).to_rust_string_lossy(scope);
    let host = scope
        .get_slot_mut::<Host>()
        .expect("the isolate keeps its host");
    if let Err(failure) = host.console.log(&text) {
        let exception = error(scope, &failure);
        scope.throw_exception(exception);
    }
}

fn now(
    scope: &mut v8::PinScope<'_, '_>,
    _: v8::FunctionCallbackArguments,
    mut returned: v8::ReturnValue<v8::Value>,
) {
    let started = scope
        .get_slot::<Host>()
        .expect("the isolate keeps its host")
        .started;
    returned.set_double(started.elapsed().as_secs_f64() * 1000.0);
}

/// Installs the runtime with `host` (its tool call, sleep, wake, console and clock
/// functions), then runs the submitted body to its outcome. `settle_next`
/// waits while every promise the script awaits is pending.
pub(super) fn script<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    config: &str,
    source: &SubmittedSource,
    host: [v8::Local<'s, v8::Value>; 5],
    mut settle_next: impl FnMut(&mut v8::PinScope<'_, '_>) -> Result<(), JsError>,
) -> Result<Finished, JsError> {
    v8::tc_scope!(let scope, scope);
    let runtime = engine::runtime(scope);
    let export = |scope: &v8::PinScope<'s, '_>, name| -> Option<v8::Local<'s, v8::Function>> {
        let key = v8::String::new(scope, name)?;
        runtime?.get(scope, key.into())?.try_into().ok()
    };
    let (Some(install), Some(run), Some(fail)) = (
        export(scope, "install"),
        export(scope, "run"),
        export(scope, "fail"),
    ) else {
        return Err(JsError::Initialization(
            "the runtime snapshot lacks its entry points".into(),
        ));
    };
    let config = v8::String::new(scope, config).and_then(|config| v8::json::parse(scope, config));
    let config = config.ok_or_else(|| raised(scope, JsError::Initialization))?;
    let undefined = v8::undefined(scope).into();
    let [call, sleep, wake, log, now] = host;
    install
        .call(scope, undefined, &[config, call, sleep, wake, log, now])
        .ok_or_else(|| raised(scope, JsError::Initialization))?;
    let Some(module) = source.compile(scope) else {
        let error = scope
            .exception()
            .ok_or_else(|| raised(scope, JsError::Syntax))?;
        return Err(JsError::Syntax(thrown(scope, fail, error)?.message));
    };
    // Only input that closes the wrapper early runs anything before the body.
    let evaluated = (module.instantiate_module(scope, engine::resolve))
        .and_then(|_| module.evaluate(scope))
        .and_then(|promise| promise.try_cast::<v8::Promise>().ok());
    let evaluated = match evaluated {
        Some(promise) => settle(scope, promise, &mut settle_next)?,
        None => Err(scope
            .exception()
            .ok_or_else(|| raised(scope, JsError::Execution))?),
    };
    if let Err(error) = evaluated {
        let failure = Some(thrown(scope, fail, error)?);
        return Ok(Finished {
            returned: Returned::default(),
            failure,
        });
    }
    let body = (module.get_module_namespace().to_object(scope))
        .zip(v8::String::new(scope, "default"))
        .and_then(|(namespace, key)| namespace.get(scope, key.into()));
    let promise = (body.and_then(|body| run.call(scope, undefined, &[body])))
        .and_then(|promise| promise.try_cast::<v8::Promise>().ok())
        .ok_or_else(|| raised(scope, JsError::Execution))?;
    match settle(scope, promise, &mut settle_next)? {
        Ok(encoded) => serde_json::from_str(&encoded.to_rust_string_lossy(scope))
            .map_err(|error| JsError::InvalidOutput(error.to_string())),
        Err(error) => Err(JsError::Execution(error.to_rust_string_lossy(scope))),
    }
}

/// Runs microtasks and V8's own foreground tasks, and waits for host work between
/// them, until `promise` settles: its value, or the reason it was rejected.
fn settle<'s>(
    scope: &mut v8::PinnedRef<'s, v8::TryCatch<v8::HandleScope>>,
    promise: v8::Local<'s, v8::Promise>,
    settle_next: &mut impl FnMut(&mut v8::PinScope<'_, '_>) -> Result<(), JsError>,
) -> Result<Result<v8::Local<'s, v8::Value>, v8::Local<'s, v8::Value>>, JsError> {
    loop {
        scope.perform_microtask_checkpoint();
        while engine::pump(scope) {
            scope.perform_microtask_checkpoint();
        }
        if terminated(scope) {
            return Err(JsError::Cancelled);
        }
        match promise.state() {
            v8::PromiseState::Pending => settle_next(scope)?,
            v8::PromiseState::Fulfilled => return Ok(Ok(promise.result(scope))),
            v8::PromiseState::Rejected => return Ok(Err(promise.result(scope))),
        }
    }
}

/// The failure a thrown value reports.
fn thrown(
    scope: &mut v8::PinnedRef<'_, v8::TryCatch<v8::HandleScope>>,
    fail: v8::Local<'_, v8::Function>,
    error: v8::Local<'_, v8::Value>,
) -> Result<Failure, JsError> {
    let undefined = v8::undefined(scope).into();
    let encoded =
        (fail.call(scope, undefined, &[error])).ok_or_else(|| raised(scope, JsError::Execution))?;
    serde_json::from_str(&encoded.to_rust_string_lossy(scope))
        .map_err(|error| JsError::InvalidOutput(error.to_string()))
}

fn terminated(scope: &v8::PinnedRef<'_, v8::TryCatch<v8::HandleScope>>) -> bool {
    scope.has_terminated() || scope.is_execution_terminating()
}

/// What a call that returned nothing raised: termination, or its exception.
fn raised(
    scope: &v8::PinnedRef<'_, v8::TryCatch<v8::HandleScope>>,
    failure: impl FnOnce(String) -> JsError,
) -> JsError {
    if terminated(scope) {
        JsError::Cancelled
    } else {
        failure(engine::exception(scope))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};

    use crate::tests::bounded;
    use crate::tool::diagnostic::Cause;
    use crate::tool::javascript::tests::{echo_runtime, evaluate, plain, test_runtime};
    use crate::tool::{ToolOptions, ToolOutput, ToolRegistryBuilder};

    #[tokio::test]
    async fn cancellation_stops_sleep_and_pending_promises_without_keeping_unawaited_work_alive() {
        // Cancellation ends the script only once a call in flight has ended, both
        // when the script settles first and when it awaits what never settles.
        for source in [
            "await Promise.all([sleep(60000), tool.entered({})]); return 'finished';",
            "await new Promise(() => { tool.entered({}).then(() => {}); });",
        ] {
            let entered = Arc::new(tokio::sync::Notify::new());
            let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut builder = ToolRegistryBuilder::default();
            let (notify, end) = (entered.clone(), ended.clone());
            builder
                .register_product::<crate::tool::registry::Arguments, Value, _, _>(
                    "entered",
                    "",
                    ToolOptions::default(),
                    move |context, _| {
                        notify.notify_one();
                        let end = end.clone();
                        async move {
                            context.cancelled().await;
                            tokio::task::yield_now().await;
                            end.store(true, std::sync::atomic::Ordering::SeqCst);
                            Ok(ToolOutput::default())
                        }
                    },
                )
                .unwrap();
            let (_scope, executor, context) = test_runtime(builder).await;
            let cancellation = context.cancellation_token();
            let (result, ()) = bounded(async {
                tokio::join!(evaluate(source, executor, context), async {
                    bounded(entered.notified()).await;
                    cancellation.cancel();
                })
            })
            .await;
            assert_eq!(result.unwrap_err().diagnostic().cause, Cause::Cancelled);
            assert!(ended.load(std::sync::atomic::Ordering::SeqCst), "{source}");
        }
        let unawaited = bounded(plain(
            "sleep(60000); setTimeout(() => {}, 60000); return 'finished';",
        ));
        assert_eq!(unawaited.await.unwrap().value["value"], "finished");
    }

    /// Timers due together run in the order they were set, and a later deadline
    /// waits for its time whatever was set after it.
    #[tokio::test]
    async fn timers_run_by_deadline_then_the_order_they_were_set() {
        let source = "const order = [];
await new Promise(resolve => {
  clearTimeout(setTimeout(() => order.push('cleared'), 0));
  setTimeout(() => order.push('a'), 0);
  setTimeout(() => order.push('b'), 0);
  setTimeout(() => { order.push('later'); resolve(); }, 100);
  setTimeout(() => order.push('c'), 0);
  queueMicrotask(() => order.push('micro'));
});
return order;";
        let output = bounded(plain(source)).await.unwrap();
        assert_eq!(
            output.value["value"],
            json!(["micro", "a", "b", "c", "later"])
        );
    }

    /// Timers that are always due do not starve a tool call's response.
    #[tokio::test]
    async fn due_timers_and_tool_responses_take_turns() {
        let (_scope, executor, context) = echo_runtime().await;
        // Each tick outlasts the timer's resolution, so one interval is always
        // overdue when the other's tick ends.
        let source = "const tick = () => { const end = performance.now() + 2; while (performance.now() < end); };
const intervals = [setInterval(tick, 0), setInterval(tick, 0)];
const response = await tool.echo({value: 'answered'});
intervals.forEach(clearInterval);
return response.result;";
        let output = bounded(evaluate(source, executor, context)).await.unwrap();
        assert_eq!(output.value["value"], "answered");
    }
}
