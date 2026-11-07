//! The host side of the web globals the runtime defines once, in the snapshot: UTF-8
//! text coding and structured cloning on V8's own conversions, and URLs.

use std::borrow::Cow;

use std::sync::Arc;

use v8::{MapFnTo, ValueDeserializerHelper, ValueSerializerHelper};

use super::{engine::Memory, urls};

/// The snapshot's host functions, in the order the runtime's `prepare` takes them.
fn callbacks() -> [v8::FunctionCallback; 10] {
    [
        utf8_length.map_fn_to(),
        encode_into.map_fn_to(),
        decode.map_fn_to(),
        clone.map_fn_to(),
        brand.map_fn_to(),
        urls::parse.map_fn_to(),
        urls::get.map_fn_to(),
        urls::set.map_fn_to(),
        urls::form_parse.map_fn_to(),
        urls::form_serialize.map_fn_to(),
    ]
}

/// What every isolate from the snapshot must name alike.
pub(super) fn references() -> Cow<'static, [v8::ExternalReference]> {
    (callbacks().map(|function| v8::ExternalReference { function }))
        .to_vec()
        .into()
}

pub(super) fn functions<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Option<Vec<v8::Local<'s, v8::Value>>> {
    (callbacks().into_iter())
        .map(|callback| Some(v8::Function::new_raw(scope, callback)?.into()))
        .collect()
}

/// Host functions make values only through these, which make none once the script
/// is terminating: past its heap limit, it has only the headroom to unwind.
fn live(scope: &v8::PinScope<'_, '_>) -> Option<()> {
    let exhausted = (scope.get_slot::<Arc<Memory>>()).is_some_and(|memory| memory.exhausted());
    (!exhausted && !scope.is_execution_terminating()).then_some(())
}

pub(super) fn string<'s>(
    scope: &v8::PinScope<'s, '_>,
    text: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    live(scope)?;
    Some(v8::String::new(scope, text)?.into())
}

pub(super) fn array<'s>(
    scope: &v8::PinScope<'s, '_>,
    elements: impl IntoIterator<Item = Option<v8::Local<'s, v8::Value>>>,
) -> Option<v8::Local<'s, v8::Value>> {
    let elements = elements.into_iter().collect::<Option<Vec<_>>>()?;
    live(scope)?;
    Some(v8::Array::new_with_elements(scope, &elements).into())
}

fn number<'s>(scope: &v8::PinScope<'s, '_>, number: usize) -> Option<v8::Local<'s, v8::Value>> {
    live(scope)?;
    Some(v8::Number::new(scope, number as f64).into())
}

fn utf8_length(
    scope: &mut v8::PinScope<'_, '_>,
    arguments: v8::FunctionCallbackArguments,
    mut returned: v8::ReturnValue<v8::Value>,
) {
    if let Ok(text) = arguments.get(0).try_cast::<v8::String>() {
        returned.set_double(text.utf8_length(scope) as f64);
    }
}

/// Writes the whole characters of a string that fit, unpaired surrogates as U+FFFD:
/// the UTF-16 units read and the bytes written.
fn encode_into(
    scope: &mut v8::PinScope<'_, '_>,
    arguments: v8::FunctionCallbackArguments,
    mut returned: v8::ReturnValue<v8::Value>,
) {
    let (Ok(text), Ok(destination)) = (
        arguments.get(0).try_cast::<v8::String>(),
        arguments.get(1).try_cast::<v8::Uint8Array>(),
    ) else {
        return;
    };
    let Some(buffer) = destination.buffer(scope) else {
        return;
    };
    let store = buffer.get_backing_store();
    let start = destination.byte_offset();
    let cells = &store[start..start + destination.byte_length()];
    // SAFETY: `Cell<u8>` has `u8`'s layout, and no JavaScript runs to touch the
    // buffer while V8 writes into it.
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(cells.as_ptr().cast_mut().cast::<u8>(), cells.len())
    };
    let mut read = 0;
    let flags = v8::WriteFlags::kReplaceInvalidUtf8;
    let written = text.write_utf8_v2(scope, bytes, flags, Some(&mut read));
    if let Some(counts) = array(scope, [number(scope, read), number(scope, written)]) {
        returned.set(counts);
    }
}

/// Decodes UTF-8, replacing malformed sequences or, when fatal, rejecting them.
fn decode(
    scope: &mut v8::PinScope<'_, '_>,
    arguments: v8::FunctionCallbackArguments,
    mut returned: v8::ReturnValue<v8::Value>,
) {
    let Ok(view) = arguments.get(0).try_cast::<v8::Uint8Array>() else {
        return;
    };
    let mut bytes = vec![0; view.byte_length()];
    view.copy_contents(&mut bytes);
    let fatal = arguments.get(1).boolean_value(scope);
    if fatal && !v8::simdutf::validate_utf8(&bytes) {
        throw(
            scope,
            v8::Exception::type_error,
            "TextDecoder input is not valid UTF-8",
        );
        return;
    }
    if live(scope).is_none() {
        return;
    }
    match v8::String::new_from_utf8(scope, &bytes, v8::NewStringType::Normal) {
        Some(text) => returned.set(text.into()),
        None => throw(
            scope,
            v8::Exception::range_error,
            "decoded text is too long",
        ),
    }
}

/// The key of the brand that keeps an object out of structured clones.
fn uncloneable<'s>(scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Private>> {
    let name = v8::String::new(scope, "skyhook:uncloneable")?;
    Some(v8::Private::for_api(scope, Some(name)))
}

/// Brands an object, such as a URL whose state lives in private fields, that a
/// structured clone would copy as an empty object, so cloning it throws instead.
fn brand(
    scope: &mut v8::PinScope<'_, '_>,
    arguments: v8::FunctionCallbackArguments,
    _: v8::ReturnValue<v8::Value>,
) {
    if let (Ok(object), Some(key)) = (
        arguments.get(0).try_cast::<v8::Object>(),
        uncloneable(scope),
    ) {
        let branded = v8::Boolean::new(scope, true);
        object.set_private(scope, key, branded.into());
    }
}

/// A structured clone, through V8's own serialization. A copy refused buffer
/// storage throws the RangeError a buffer past its limit does.
fn clone(
    scope: &mut v8::PinScope<'_, '_>,
    arguments: v8::FunctionCallbackArguments,
    mut returned: v8::ReturnValue<v8::Value>,
) {
    let context = scope.get_current_context();
    let serializer = v8::ValueSerializer::new(scope, Box::new(Cloning));
    serializer.write_header();
    if serializer.write_value(context, arguments.get(0)) != Some(true) {
        return;
    }
    let data = serializer.release();
    if live(scope).is_none() {
        return;
    }
    let refusals = scope
        .get_slot::<Arc<Memory>>()
        .map(|memory| (memory.clone(), memory.refusals()));
    v8::tc_scope!(let scope, scope);
    let deserializer = v8::ValueDeserializer::new(scope, Box::new(Cloning), &data);
    if deserializer.read_header(context) == Some(true)
        && let Some(value) = deserializer.read_value(context)
    {
        returned.set(value);
        return;
    }
    if refusals.is_some_and(|(memory, before)| memory.refusals() > before)
        && !scope.has_terminated()
    {
        scope.reset();
        throw(
            scope,
            v8::Exception::range_error,
            "Array buffer allocation failed",
        );
    }
    // What this scope caught reaches the script.
    scope.rethrow();
}

struct Cloning;

impl v8::ValueSerializerImpl for Cloning {
    fn throw_data_clone_error<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        message: v8::Local<'s, v8::String>,
    ) {
        let error = v8::Exception::error(scope, message);
        if let (Ok(object), Some(key), Some(name)) = (
            error.try_cast::<v8::Object>(),
            v8::String::new(scope, "name"),
            v8::String::new(scope, "DataCloneError"),
        ) {
            object.define_own_property(
                scope,
                key.into(),
                name.into(),
                v8::PropertyAttribute::DONT_ENUM,
            );
        }
        scope.throw_exception(error);
    }

    fn has_custom_host_object(&self, _: &v8::Isolate) -> bool {
        true
    }

    fn is_host_object<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        object: v8::Local<'s, v8::Object>,
    ) -> Option<bool> {
        object.has_private(scope, uncloneable(scope)?)
    }

    /// Only branded objects are host objects, and none can be cloned.
    fn write_host_object<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        object: v8::Local<'s, v8::Object>,
        _: &dyn ValueSerializerHelper,
    ) -> Option<bool> {
        let name = object.get_constructor_name().to_rust_string_lossy(scope);
        let message = v8::String::new(scope, &format!("{name} object could not be cloned."))?;
        self.throw_data_clone_error(scope, message);
        None
    }
}

impl v8::ValueDeserializerImpl for Cloning {}

fn throw(
    scope: &mut v8::PinScope<'_, '_>,
    kind: impl for<'a> Fn(&v8::PinScope<'a, '_>, v8::Local<'a, v8::String>) -> v8::Local<'a, v8::Value>,
    message: &str,
) {
    if let Some(message) = v8::String::new(scope, message) {
        let error = kind(scope, message);
        scope.throw_exception(error);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::tool::javascript::tests::evaluate_wrapper;

    /// UTF-8 text coding, base64 strings, structured clones and timers follow their
    /// browser contracts.
    #[test]
    fn web_globals_follow_their_browser_contracts() {
        for (source, expected) in [
            (
                r#"
const bytes = new TextEncoder().encode("é\uD800"), decoder = new TextDecoder(" UTF8 ");
let fatal, label;
try { new TextDecoder("utf-8", {fatal: true}).decode(new Uint8Array([0xff])); } catch (error) { fatal = error.constructor.name; }
try { new TextDecoder("latin1"); } catch (error) { label = error.constructor.name; }
const thrown = make => { try { make(); } catch (error) { return error.constructor.name; } };
return {bytes: Array.from(bytes), text: decoder.decode(bytes), replaced: decoder.decode(new Uint8Array([0x61, 0xff])),
  bom: decoder.decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x62])),
  into: new TextEncoder().encodeInto("aé€", new Uint8Array(3)), fatal, label,
  labels: [new TextDecoder("unicode11utf8").encoding, thrown(() => new TextDecoder("\u00a0utf-8"))],
  symbol: thrown(() => new TextEncoder().encode(Symbol()))};
"#,
                json!({"bytes": [0xc3, 0xa9, 0xef, 0xbf, 0xbd], "text": "é\u{fffd}", "replaced": "a\u{fffd}",
                       "bom": "b", "into": {"read": 2, "written": 3}, "fatal": "TypeError", "label": "RangeError",
                       "labels": ["utf-8", "RangeError"], "symbol": "TypeError"}),
            ),
            (
                r#"
const names = [];
for (const bad of [() => atob("a"), () => atob("ab$c"), () => btoa("€")]) {
  try { bad(); } catch (error) { names.push(error.name); }
}
return {decoded: atob(" aGk=\n"), unpadded: atob("aGk"), encoded: btoa("hiÿ"), names};
"#,
                json!({"decoded": "hi", "unpadded": "hi", "encoded": "aGn/",
                       "names": ["InvalidCharacterError", "InvalidCharacterError", "InvalidCharacterError"]}),
            ),
            // `half` and its copy together exceed the test's buffer limit, so the clone
            // itself is refused.
            (
                r#"
const map = new Map(), graph = {map, when: new Date(0), bytes: new Uint8Array([1, 2])};
map.set("self", graph);
const copy = structuredClone(graph);
const thrown = make => { try { make(); } catch (error) { return error.name; } };
return {distinct: copy !== graph && copy.map !== map, cyclic: copy.map.get("self") === copy,
  when: copy.when, bytes: copy.bytes, rejected: thrown(() => structuredClone(() => 0)),
  hosted: [{deep: [new URL("https://example.com/")]}, new URLSearchParams(), new TextEncoder(), new TextDecoder(),
    new WorkPool(1)].map(value => thrown(() => structuredClone(value))),
  empty: thrown(() => structuredClone()), large: (() => { const half = new Uint8Array(2 ** 24 + 1); return thrown(() => structuredClone(half)); })()};
"#,
                json!({"distinct": true, "cyclic": true, "when": "1970-01-01T00:00:00.000Z",
                       "bytes": [1, 2], "rejected": "DataCloneError",
                       "hosted": ["DataCloneError", "DataCloneError", "DataCloneError", "DataCloneError", "DataCloneError"], "empty": "TypeError", "large": "RangeError"}),
            ),
            (
                r#"
const fired = [], logged = [];
let ticks = 0, capped;
__skyhookConsoleLog = message => { logged.push(message.split("\n")[0]); };
clearTimeout(setTimeout(() => fired.push("cleared"), 0));
clearTimeout(12345);
setTimeout(() => { throw new Error("late"); }, 0);
setTimeout(async () => { throw new Error("rejected"); }, 0);
queueMicrotask(() => { throw new Error("micro"); });
queueMicrotask(() => tool.echo({value: "never run"}));
await new Promise(resolve => setTimeout((a, b) => { fired.push(a + b); resolve(); }, NaN, 1, 2));
await new Promise(resolve => {
  const id = setInterval(() => { if (++ticks === 3) { clearInterval(id); resolve(); } }, -5);
});
try { setTimeout(() => {}, 1800001); } catch (error) { capped = error instanceof TypeError; }
return {fired, ticks, logged, capped, calls: __testCalls.length};
"#,
                json!({"fired": [3], "ticks": 3, "logged": ["Uncaught late", "Uncaught micro", "Uncaught rejected"],
                       "capped": true, "calls": 0}),
            ),
            // Scripts run in UTC, while an explicit zone still applies.
            (
                r#"
const local = new Date(2026, 0, 2, 3, 4, 5);
const tokyo = new Intl.DateTimeFormat("en-US", {timeZone: "Asia/Tokyo", hour: "2-digit", hourCycle: "h23"});
return {offset: local.getTimezoneOffset(), iso: local.toISOString(), tokyo: tokyo.format(new Date(0)),
  zones: [Intl.DateTimeFormat().resolvedOptions().timeZone, Temporal.Now.timeZoneId()]};
"#,
                json!({"offset": 0, "iso": "2026-01-02T03:04:05.000Z", "tokyo": "09", "zones": ["UTC", "UTC"]}),
            ),
            // Clearing a timer wakes the sleep it waits on, so nothing keeps it.
            (
                r#"
const sleeping = new Set();
let slept = 0;
__skyhookHostSleep = () => { sleeping.add(++slept); return [new Promise(() => {}), slept]; };
__skyhookHostWake = sleep => sleeping.delete(sleep);
const timeout = setTimeout(() => {}, 1000), interval = setInterval(() => {}, 5);
const before = sleeping.size;
clearTimeout(timeout);
clearInterval(interval);
return [before, sleeping.size];
"#,
                json!([2, 0]),
            ),
        ] {
            assert_eq!(evaluate_wrapper(source), expected, "{source}");
        }
    }
}
