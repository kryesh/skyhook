//! One V8 platform per process, and a startup snapshot of the evaluated runtime
//! modules from which each script's isolate starts.

use std::{
    collections::HashMap,
    ffi::c_void,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

/// Each module follows those it imports; the last is the entry point.
const MODULES: &[(&str, &str)] = &[
    ("skyhook:errors", include_str!("js/errors.js")),
    ("skyhook:url", include_str!("js/url.js")),
    ("skyhook:web", include_str!("js/web.js")),
    ("skyhook:values", include_str!("js/values.js")),
    ("skyhook:console", include_str!("js/console.js")),
    ("skyhook:tools", include_str!("js/tools.js")),
    ("skyhook:pool", include_str!("js/pool.js")),
    ("skyhook:runtime", include_str!("js/runtime.js")),
];

pub(super) const MIB: usize = 1024 * 1024;
/// Granted once at the heap limit so a terminated script can unwind: it exceeds
/// the largest single allocation V8 makes, a growing array's copy, whatever the limit.
const HEADROOM: usize = 2048 * MIB;

/// The memory one script's isolate may use: its JavaScript heap, and its ArrayBuffer
/// storage outside the heap.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MemoryLimits {
    pub(crate) heap: usize,
    pub(crate) buffers: usize,
}

impl MemoryLimits {
    /// What every script call gets.
    pub(crate) const SCRIPT: Self = Self {
        heap: 512 * MIB,
        buffers: 512 * MIB,
    };
}

struct Snapshot {
    blob: Box<[u8]>,
    /// Where the runtime module's namespace is kept in the snapshot's context.
    runtime: usize,
}

static SNAPSHOT: OnceLock<Result<Snapshot, String>> = OnceLock::new();
static PLATFORM: OnceLock<v8::SharedRef<v8::Platform>> = OnceLock::new();

fn snapshot() -> Result<&'static Snapshot, String> {
    let snapshot = SNAPSHOT.get_or_init(|| {
        // The protected platform admits isolates only on descendants of the
        // thread that initialized it; each script runs on a thread of its own.
        let platform =
            PLATFORM.get_or_init(|| v8::new_unprotected_default_platform(0, false).make_shared());
        // A refused buffer's retry collections then never check the heap limit
        // themselves, which `near_heap_limit` relies on.
        v8::V8::set_flags_from_string("--late-heap-limit-check");
        v8::V8::initialize_platform(platform.clone());
        v8::V8::initialize();
        // Scripts compute in UTC whatever the host's zone: the default is ICU's, which
        // only V8 reads, so the process's own `TZ` is untouched.
        assert!(
            v8::icu::set_default_time_zone("UTC"),
            "ICU has the UTC zone"
        );
        create()
    });
    snapshot.as_ref().map_err(Clone::clone)
}

/// Runs the foreground tasks V8 posted for `isolate` that are due, without waiting;
/// whether any ran.
pub(super) fn pump(isolate: &v8::Isolate) -> bool {
    PLATFORM
        .get()
        .is_some_and(|platform| v8::Platform::pump_message_loop(platform, isolate, false))
}

/// Compiled from the evaluated runtime modules where it runs, since a snapshot
/// belongs to one V8 build and architecture.
fn create() -> Result<Snapshot, String> {
    let mut creator = v8::Isolate::snapshot_creator(Some(super::web::references()), None);
    let runtime = {
        v8::scope!(let scope, &mut creator);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        v8::tc_scope!(let scope, scope);
        let namespace = evaluate(scope);
        scope.remove_slot::<Modules>();
        let namespace = namespace.ok_or_else(|| exception(scope))?;
        let runtime = scope.add_context_data(context, namespace);
        scope.set_default_context(context);
        runtime
    };
    let blob = (creator.create_blob(v8::FunctionCodeHandling::Keep))
        .ok_or("could not create the runtime snapshot")?;
    Ok(Snapshot {
        blob: blob.to_vec().into(),
        runtime,
    })
}

/// The embedded modules by name, for their imports of one another.
struct Modules(HashMap<&'static str, v8::Global<v8::Module>>);

fn evaluate<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Object>> {
    let mut modules = HashMap::new();
    let mut entry = None;
    for (name, text) in MODULES {
        let module = compile(scope, name, text, 0)?;
        modules.insert(*name, v8::Global::new(scope, module));
        entry = Some(module);
    }
    let entry = entry.expect("the module list is not empty");
    scope.set_slot(Modules(modules));
    entry.instantiate_module(scope, resolve)?;
    entry.evaluate(scope)?;
    scope.perform_microtask_checkpoint();
    if entry.get_status() != v8::ModuleStatus::Evaluated {
        return None;
    }
    let runtime = entry.get_module_namespace().to_object(scope)?;
    let key = v8::String::new(scope, "prepare")?;
    let prepare = v8::Local::<v8::Function>::try_from(runtime.get(scope, key.into())?).ok()?;
    let undefined = v8::undefined(scope).into();
    let host = super::web::functions(scope)?;
    prepare.call(scope, undefined, &host)?;
    Some(runtime)
}

/// The embedded modules import one another; nothing else imports anything.
pub(super) fn resolve<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _attributes: v8::Local<'s, v8::FixedArray>,
    _referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    v8::callback_scope!(unsafe scope, context);
    let name = specifier.to_rust_string_lossy(scope);
    let module =
        (scope.get_slot::<Modules>()).and_then(|modules| modules.0.get(name.as_str()).cloned());
    if let Some(module) = module {
        return Some(v8::Local::new(scope, module));
    }
    let message = v8::String::new(scope, &format!("cannot import {name:?}"))?;
    let error = v8::Exception::type_error(scope, message);
    scope.throw_exception(error);
    None
}

/// Compiles `text` as the module `name`, which also names its stack frames,
/// its lines numbered from `line_offset`.
pub(super) fn compile<'s>(
    scope: &v8::PinScope<'s, '_>,
    name: &str,
    text: &str,
    line_offset: i32,
) -> Option<v8::Local<'s, v8::Module>> {
    let name = v8::String::new(scope, name)?;
    let text = v8::String::new(scope, text)?;
    let origin = v8::ScriptOrigin::new(
        scope,
        name.into(),
        line_offset,
        0,
        false,
        -1,
        None,
        false,
        false,
        true,
        None,
    );
    let mut source = v8::script_compiler::Source::new(text, Some(&origin));
    v8::script_compiler::compile_module(scope, &mut source)
}

/// The pending exception's text.
pub(super) fn exception(scope: &v8::PinnedRef<'_, v8::TryCatch<v8::HandleScope>>) -> String {
    (scope.exception()).map_or_else(String::new, |exception| {
        exception.to_rust_string_lossy(scope)
    })
}

/// The runtime module's namespace, in a context created from the snapshot.
pub(super) fn runtime<'s>(scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Object>> {
    let index = SNAPSHOT.get()?.as_ref().ok()?.runtime;
    scope.get_context_data_from_snapshot_once(index).ok()
}

/// A script's isolate, within its memory limits.
pub(super) struct Engine {
    pub(super) isolate: v8::OwnedIsolate,
    // Dropped after the isolate, whose heap-limit callback reads it.
    containment: Box<Containment>,
}

impl Engine {
    pub(super) fn new(limits: MemoryLimits) -> Result<Self, String> {
        let snapshot = snapshot()?;
        let memory = Arc::new(Memory::default());
        let buffers = Box::into_raw(Box::new(Buffers {
            used: AtomicUsize::new(0),
            limit: limits.buffers,
            memory: memory.clone(),
        }));
        // SAFETY: the allocator owns `buffers` and frees it through `VTABLE.drop`.
        let allocator = unsafe { v8::new_rust_allocator(buffers, &VTABLE) };
        let params = v8::CreateParams::default()
            .snapshot_blob((&snapshot.blob[..]).into())
            .external_references(super::web::references())
            .heap_limits(0, limits.heap)
            .array_buffer_allocator(allocator);
        let mut isolate = v8::Isolate::new(params);
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        isolate.set_slot(memory.clone());
        let containment = Box::new(Containment {
            handle: isolate.thread_safe_handle(),
            memory,
        });
        let data = (&raw const *containment).cast_mut().cast();
        isolate.add_near_heap_limit_callback(near_heap_limit, data);
        Ok(Self {
            isolate,
            containment,
        })
    }

    /// Whether the script reached its heap limit, which fails it however it
    /// ended: some builtins finish before termination takes effect.
    pub(super) fn exhausted(&self) -> bool {
        self.containment.memory.exhausted()
    }
}

struct Containment {
    handle: v8::IsolateHandle,
    memory: Arc<Memory>,
}

/// What the isolate's limits did, kept in its slot: the buffer allocations refused so
/// far, where the latest is in V8's retry, which ends with a last-resort collection
/// that calls the heap-limit callback, then one more attempt; and whether the heap
/// ran out.
#[derive(Default)]
pub(super) struct Memory {
    retry: Mutex<Retry>,
    refusals: AtomicUsize,
    exhausted: AtomicBool,
}

impl Memory {
    pub(super) fn refusals(&self) -> usize {
        self.refusals.load(Ordering::Relaxed)
    }

    pub(super) fn exhausted(&self) -> bool {
        self.exhausted.load(Ordering::Relaxed)
    }

    fn retry(&self) -> std::sync::MutexGuard<'_, Retry> {
        (self.retry.lock()).unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Default)]
enum Retry {
    #[default]
    Idle,
    Refused,
    /// The last-resort collection ran; the next attempt is the final one.
    Collected,
}

/// Terminates the script; only the first call grants the headroom it unwinds
/// in, and V8 aborts the process if a builtin that ignores termination outgrows it.
///
/// V8 calls this from three places: unconditionally as a last-resort collection
/// starts, ignoring the answer; and when the heap is over its limit or keeps
/// failing to shrink near it, aborting unless the limit rises. A refused buffer's
/// retry runs collections that check neither (`--late-heap-limit-check`, and the
/// ineffective-collection count cannot reach its threshold there) and then a
/// last-resort collection. The first call after a refusal is therefore that
/// unconditional one, and says nothing about the heap; any later call in the same
/// retry is V8 finding the heap genuinely exhausted.
unsafe extern "C" fn near_heap_limit(data: *mut c_void, current: usize, _initial: usize) -> usize {
    // SAFETY: `data` is the engine's containment, which outlives its isolate.
    let containment = unsafe { &*data.cast::<Containment>() };
    {
        let mut retry = containment.memory.retry();
        if let Retry::Refused = *retry {
            *retry = Retry::Collected;
            return current;
        }
    }
    if containment.memory.exhausted.swap(true, Ordering::Relaxed) {
        return current;
    }
    containment.handle.terminate_execution();
    current + HEADROOM
}

/// ArrayBuffer storage in use; an allocation past the limit fails, which V8
/// throws as a RangeError once its retry fails too.
struct Buffers {
    used: AtomicUsize,
    limit: usize,
    memory: Arc<Memory>,
}

impl Buffers {
    fn allocate(&self, len: usize, allocate: impl FnOnce(usize) -> *mut c_void) -> *mut c_void {
        let fits = |used: usize| used.checked_add(len).filter(|&total| total <= self.limit);
        let reserved = (self
            .used
            .try_update(Ordering::Relaxed, Ordering::Relaxed, fits))
        .is_ok();
        let data = if reserved {
            allocate(len.max(1))
        } else {
            std::ptr::null_mut()
        };
        if reserved && data.is_null() {
            self.used.fetch_sub(len, Ordering::Relaxed);
        }
        if data.is_null() {
            self.memory.refusals.fetch_add(1, Ordering::Relaxed);
        }
        let mut retry = self.memory.retry();
        *retry = match (data.is_null(), &*retry) {
            (true, Retry::Idle | Retry::Refused) => Retry::Refused,
            _ => Retry::Idle,
        };
        data
    }
}

unsafe extern "C" fn allocate(buffers: &Buffers, len: usize) -> *mut c_void {
    // SAFETY: an ordinary allocation, freed by `free`.
    buffers.allocate(len, |len| unsafe { libc::calloc(len, 1) })
}

unsafe extern "C" fn allocate_uninitialized(buffers: &Buffers, len: usize) -> *mut c_void {
    // SAFETY: an ordinary allocation, freed by `free`.
    buffers.allocate(len, |len| unsafe { libc::malloc(len) })
}

unsafe extern "C" fn free(buffers: &Buffers, data: *mut c_void, len: usize) {
    buffers.used.fetch_sub(len, Ordering::Relaxed);
    // SAFETY: V8 frees only what `allocate` or `allocate_uninitialized` returned.
    unsafe { libc::free(data) }
}

unsafe extern "C" fn release(buffers: *const Buffers) {
    // SAFETY: the allocator's last reference releases the box `Engine::new` leaked.
    drop(unsafe { Box::from_raw(buffers.cast_mut()) });
}

static VTABLE: v8::RustAllocatorVtable<Buffers> = v8::RustAllocatorVtable {
    allocate,
    allocate_uninitialized,
    free,
    drop: release,
};

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use serde_json::{Value, json};

    use crate::tests::bounded;
    use crate::tool::javascript::tests::{LIMITS, plain};
    use skyhook_tools::diagnostic::Cause;
    use skyhook_worker::diagnostic::Effects;

    #[tokio::test]
    async fn memory_limits_reject_buffers_and_end_scripts_that_exhaust_the_heap() {
        // ArrayBuffer storage counts together against its limit, before
        // allocating; storage that is freed restores capacity.
        let source = &format!(
            "const half = {}; const kept = [new ArrayBuffer(half), new ArrayBuffer(half)]; \
             new ArrayBuffer(1);",
            LIMITS.buffers / 2
        );
        let (diagnostic, output) = bounded(plain(source)).await.unwrap_err().into_parts();
        assert_matches!(&diagnostic.cause, Cause::Message(message)
                if message.starts_with("RangeError: Array buffer allocation failed"),
            "{diagnostic:?}"
        );
        assert_eq!(output.unwrap().value["value"], Value::Null);

        let output = bounded(plain(&format!(
            "const exceed = () => {{ {source} }}; let caught = false; \
             try {{ exceed(); }} catch (_) {{ caught = true; }} \
             return {{caught, bytes: new Uint8Array(1024).length}};"
        )))
        .await
        .unwrap();
        assert_eq!(
            output.value["value"],
            json!({"caught": true, "bytes": 1024})
        );
        // A refused buffer stays a catchable RangeError with the heap well past half full.
        let output = bounded(plain(&format!(
            "const kept = new Array({}).fill(1.5); \
             try {{ new ArrayBuffer({}); }} catch (error) {{ return [error.name, kept.length]; }}",
            LIMITS.heap * 3 / 4 / 8,
            LIMITS.buffers + 1
        )))
        .await
        .unwrap();
        assert_eq!(
            output.value["value"],
            json!(["RangeError", LIMITS.heap * 3 / 4 / 8])
        );

        // Exhausting the heap ends the script, which cannot catch it, and fails it
        // even when a builtin that ignores termination lets it return.
        for source in [
            "const kept = []; \
             try { for (;;) kept.push(new Array(1e5).fill(1)); } catch { return 'caught'; }"
                .to_owned(),
            // Two arrays of doubles, each as large as the whole heap.
            format!(
                "const a = new Array({0}).fill(1.5), b = new Array({0}).fill(1.5); \
                 return a.length + b.length;",
                LIMITS.heap / 8
            ),
        ] {
            let diagnostic = bounded(plain(&source)).await.unwrap_err().diagnostic();
            assert_eq!(diagnostic.context.effects, Effects::Unknown);
            assert_eq!(
                diagnostic.cause,
                Cause::Message("JavaScript heap limit (32 MiB) exceeded".into()),
                "{source}"
            );
        }
    }
}
