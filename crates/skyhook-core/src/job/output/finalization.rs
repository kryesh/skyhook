//! Admission of completed, destination-bound capture receipts into saved output.
//! IO completion is not JSON validity or terminal publication. Validate first,
//! then install the existing compact document/fields references in one owner.
use super::*;
use std::io;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Persist a terminal product, referencing only its explicitly completed captures.
/// JSON containers retain empty-container references; JSON scalars are decoded
/// inline (a reference cannot distinguish a JSON-encoded string from raw Text),
/// so their captures stay discoverable but unreferenced.
/// Existing null/bool/number fields always remain authoritative. Known kinds can
/// install a missing field; Unknown kinds reference only an existing string or
/// container shape. Byte validity alone must never reinterpret text as JSON.
///
/// A receipt that fails binding, byte, or pointer validation never fails the
/// terminal product: it stays unreferenced, so discovery reports that capture
/// as incomplete while the job's real outcome is still published. Only failures
/// persisting the document, its references, or registrations are errors.
pub(crate) fn save_completed(
    output: &Output,
    document: &Value,
    completed: Vec<CompletedCapture>,
) -> Result<(), ToolError> {
    // An unreadable inventory leaves every receipt unbound (and so incomplete).
    let saved = Saved::load(output).ok();
    let registered = saved
        .as_ref()
        .map(|saved| {
            saved
                .captures
                .values()
                .map(|capture| {
                    let kind = CaptureKind::parse(&capture.kind);
                    (capture.pointer.clone(), (capture.id, kind))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let fields = completed
        .iter()
        .map(CompletedCapture::field)
        .collect::<Vec<_>>();
    let bound = completed
        .iter()
        .enumerate()
        .filter_map(|(index, capture)| {
            let field = capture.field();
            let &(id, kind) = registered.get(field)?;
            (capture.belongs_to(output.job, id)
                && kind == capture.kind()
                // Every receipt for a duplicate or overlapping field is unbound:
                // neither can be referenced without shadowing the other.
                && fields.iter().enumerate().all(|(other, candidate)| {
                    other == index
                        || (*candidate != field
                            && !nested(candidate, field)
                            && !nested(field, candidate))
                }))
            .then_some((id, capture))
        })
        .collect::<Vec<_>>();

    let mut document = document.clone();
    let mut references = BTreeSet::new();
    let mut kinds = Vec::new();
    for (id, capture) in bound {
        let field = capture.field();
        let Some(saved) = &saved else { break };
        let Some((kind, value)) = admitted_value(&document, saved, capture).ok().flatten() else {
            continue;
        };
        if install(&mut document, field, value.clone()).is_err() {
            continue;
        }
        if kind == CaptureKind::Text || value.is_array() || value.is_object() {
            references.insert(field.to_owned());
        }
        kinds.push((id, kind));
    }
    // Unknown registrations resolve only for admitted receipts.
    for (id, kind) in kinds {
        output
            .db
            .resolve_capture_kind(id, kind.as_str())
            .map_err(database)?;
    }
    save_document(output, &document, &references)
}

fn nested(field: &str, ancestor: &str) -> bool {
    field
        .strip_prefix(ancestor)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Interpret one bound receipt against the terminal document. `Ok(None)` is a
/// deliberately unreferenced capture; errors are validation failures. Neither
/// is a persistence failure, so the caller publishes both as incomplete.
fn admitted_value(
    document: &Value,
    saved: &Saved,
    capture: &CompletedCapture,
) -> io::Result<Option<(CaptureKind, Value)>> {
    let field = capture.field();
    let source = || {
        saved
            .capture(field)
            .ok_or_else(|| invalid("completed capture is missing"))
    };
    Ok(Some(match (capture.kind(), document.pointer(field)) {
        // A terminal primitive historically abandons the raw capture, even
        // when the sender completed its bytes. Do not parse or replace it.
        (_, Some(Value::Null | Value::Bool(_) | Value::Number(_))) => return Ok(None),
        (CaptureKind::Text, _) | (CaptureKind::Unknown, Some(Value::String(_))) => {
            validate_utf8(source()?, &mut [0; 64 * 1024])?;
            (CaptureKind::Text, Value::String(String::new()))
        }
        (CaptureKind::Json, _)
        | (CaptureKind::Unknown, Some(Value::Object(_) | Value::Array(_))) => {
            (CaptureKind::Json, validated_json_reference(source()?)?)
        }
        // Historical Unknown is only a hint. Scalars and missing
        // fields never referenced its bytes; retain that distinction.
        (CaptureKind::Unknown, _) => return Ok(None),
    }))
}

/// Validate JSON without constructing its container tree. Scalars are then
/// decoded inline; only the scalar string case can allocate proportional bytes.
fn validated_json_reference(source: Source) -> io::Result<Value> {
    let mut source = BufReader::new(source);
    let first = loop {
        let buffer = source.fill_buf()?;
        if buffer.is_empty() {
            return Err(invalid("completed JSON capture is empty"));
        }
        if let Some(first) = buffer
            .iter()
            .find(|byte| !matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        {
            break *first;
        }
        let count = buffer.len();
        source.consume(count);
    };
    if !matches!(first, b'{' | b'[') {
        return serde_json::from_reader(source).map_err(invalid_json);
    }
    let mut decoder = serde_json::Deserializer::from_reader(source);
    serde::de::IgnoredAny::deserialize(&mut decoder).map_err(invalid_json)?;
    decoder.end().map_err(invalid_json)?;
    Ok(if first == b'{' { json!({}) } else { json!([]) })
}

fn invalid_json(error: serde_json::Error) -> io::Error {
    invalid(format!("invalid completed JSON capture: {error}"))
}

/// Validate UTF-8 in bounded memory. At most three bytes of an incomplete
/// trailing sequence carry over between chunks of the caller's buffer.
fn validate_utf8(mut source: impl io::Read, buffer: &mut [u8]) -> io::Result<()> {
    const CARRY: usize = 3;
    assert!(buffer.len() > CARRY, "UTF-8 validation buffer is too small");
    let not_utf8 = || invalid("completed text capture is not UTF-8");
    let mut carried = 0;
    loop {
        let read = match source.read(&mut buffer[carried..]) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if read == 0 {
            return if carried == 0 {
                Ok(())
            } else {
                Err(not_utf8())
            };
        }
        let filled = carried + read;
        carried = match std::str::from_utf8(&buffer[..filled]) {
            Ok(_) => 0,
            // Only a truncated final sequence may continue in the next chunk.
            Err(error) if error.error_len().is_none() => {
                let valid = error.valid_up_to();
                buffer.copy_within(valid..filled, 0);
                filled - valid
            }
            Err(_) => return Err(not_utf8()),
        };
    }
}

/// Preserve existing object/array shape. Missing object ancestors become objects;
/// a JSON Pointer alone is not evidence that a numeric member denotes an array.
/// The whole path is checked before mutation, so a rejected field leaves the
/// document unchanged.
fn install(document: &mut Value, field: &str, value: Value) -> io::Result<()> {
    if field.is_empty() {
        *document = value;
        return Ok(());
    }
    let Some(pointer) = field.strip_prefix('/') else {
        return Err(invalid("completed capture field must be a JSON Pointer"));
    };
    let keys = pointer
        .split('/')
        .map(|key| key.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>();
    let (last, ancestors) = keys.split_last().expect("split yields a component");
    let mut probe = Some(&*document);
    for key in &keys {
        let Some(current) = probe else {
            break; // Missing ancestors are created as objects.
        };
        probe = match current {
            Value::Object(map) => map.get(key),
            Value::Array(items) => items.get(array_index(key, items.len())?),
            _ => return Err(invalid("completed capture ancestor is not a container")),
        };
    }
    let mut cursor = document;
    for key in ancestors {
        cursor = slot(cursor, key);
    }
    *slot(cursor, last) = value;
    Ok(())
}

fn array_index(key: &str, length: usize) -> io::Result<usize> {
    let index = key
        .parse::<usize>()
        .map_err(|_| invalid("capture array index is invalid"))?;
    if key != index.to_string() || index > length {
        return Err(invalid("capture array index is out of range"));
    }
    Ok(index)
}

/// Descend into a slot already accepted by `install`'s check.
fn slot<'a>(cursor: &'a mut Value, key: &str) -> &'a mut Value {
    match cursor {
        Value::Object(map) => map.entry(key).or_insert_with(|| json!({})),
        Value::Array(items) => {
            let index = array_index(key, items.len()).expect("checked capture index");
            if index == items.len() {
                items.push(json!({}));
            }
            &mut items[index]
        }
        _ => unreachable!("checked capture ancestor"),
    }
}

#[cfg(test)]
#[path = "finalization_tests.rs"]
mod tests;
