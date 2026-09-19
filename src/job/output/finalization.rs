//! Admission of completed, destination-bound capture receipts into saved output.
//! IO completion is not JSON validity or terminal publication. Validate first,
//! then install the existing compact document/fields references in one owner.
use super::*;
use std::io;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Persist a terminal product, referencing only its explicitly completed captures.
/// JSON containers are referenced; JSON scalars are decoded inline. Existing
/// null/bool/number fields remain authoritative. Known kinds can install a missing
/// field; Unknown kinds reference only an existing string or container shape.
///
/// A receipt that fails validation never fails the terminal product: its capture
/// stays unreferenced and is reported incomplete. Only persistence failures are errors.
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
mod tests {
    use super::*;
    use crate::job::{JobSpec, tests::runtime};

    /// Two running jobs of one session: receipts are bound to exactly one of them.
    async fn outputs() -> (tempfile::TempDir, JobManager, Output, Output) {
        let (root, manager, agent) = runtime().await;
        let mut outputs = Vec::new();
        for name in ["first", "second"] {
            let id = manager
                .test_lease(JobSpec::test(agent.clone(), name))
                .await
                .id();
            manager.transition(id, JobState::Running).await.unwrap();
            outputs.push(manager.output(id));
        }
        let second = outputs.pop().unwrap();
        let first = outputs.pop().unwrap();
        (root, manager, first, second)
    }

    fn completed(
        output: &Output,
        field: &str,
        kind: CaptureKind,
        bytes: &[u8],
    ) -> CompletedCapture {
        let mut writer = PendingCapture::create(output, field, kind).unwrap().open();
        writer.write_all(bytes).unwrap();
        writer.finish().unwrap()
    }

    fn document(output: &Output) -> Value {
        output.test_document().unwrap()
    }

    fn saved(output: &Output) -> Saved {
        Saved::load(output).unwrap()
    }

    fn complete_document() -> Value {
        json!({"result":{},"capture_complete":true})
    }

    #[tokio::test]
    async fn completed_receipts_install_omitted_fields_and_preserve_existing_wire_references() {
        let (_root, _manager, output, _) = outputs().await;
        let captures = [
            ("/result/text", CaptureKind::Text, "é\ntext".as_bytes()),
            ("/result/matches", CaptureKind::Json, br#"{"a":[1,true]}"#),
            ("/result/a~1b/~0key", CaptureKind::Json, br#"[null,"x"]"#),
        ]
        .map(|(field, kind, bytes)| completed(&output, field, kind, bytes));
        save_completed(&output, &complete_document(), captures.into()).unwrap();
        let document = document(&output);
        assert_eq!(
            document["result"],
            json!({"text":"","matches":{},"a/b":{"~key":[]}})
        );
        let referenced = output.test_fields();
        assert_eq!(
            referenced,
            ["/result/a~1b/~0key", "/result/matches", "/result/text"]
        );
        let hydrated = hydrate(&saved(&output), document, u64::MAX).unwrap();
        let expected = json!({"text":"é\ntext","matches":{"a":[1,true]},"a/b":{"~key":[null,"x"]}});
        assert_eq!(hydrated["result"], expected);
        assert!(
            captures::available_captures(&saved(&output), true)
                .unwrap()
                .iter()
                .all(|capture| capture.complete)
        );
    }

    /// Invalid receipts never fail terminal publication: the product is saved, and
    /// each rejected capture stays unreferenced (reported incomplete while its row
    /// still exists). JSON validation covers the full payload, not only its prefix.
    #[tokio::test]
    async fn invalid_capture_receipts_publish_the_product_with_incomplete_captures() {
        for (failure, bytes) in [
            ("job", &b"valid"[..]),
            ("registration", &b"valid"[..]),
            ("overlap", &b"valid"[..]),
            ("duplicate", &b"valid"[..]),
            ("missing", &b"valid"[..]),
            ("json", &b"{} trailing"[..]),
            ("utf8", &b"ok\xff"[..]),
            ("ancestor", &b"valid"[..]),
        ] {
            let (_root, _manager, output, other) = outputs().await;
            let prior = json!({"result":{"prior":true},"capture_complete":false});
            save_completed(&output, &prior, Vec::new()).unwrap();
            let kind = if failure == "json" {
                CaptureKind::Json
            } else {
                CaptureKind::Text
            };
            let field = if failure == "ancestor" {
                "/result/scalar/raw"
            } else {
                "/result/raw"
            };
            let mut proofs = vec![completed(&output, field, kind, bytes)];
            // A valid sibling receipt is still referenced beside a rejected one.
            proofs.push(completed(&output, "/result/ok", CaptureKind::Text, b"fine"));
            match failure {
                "registration" => {
                    let raw = saved(&output).captures[field].id;
                    output.db.delete_capture(raw).unwrap();
                    drop(PendingCapture::create(&output, field, CaptureKind::Json).unwrap());
                }
                "overlap" => proofs.push(completed(
                    &output,
                    "/result/raw/nested",
                    CaptureKind::Text,
                    b"nested",
                )),
                "duplicate" => proofs.push(proofs[0].clone()),
                "missing" => {
                    let raw = saved(&output).captures[field].id;
                    output.db.delete_capture(raw).unwrap();
                }
                _ => {}
            }
            let target = if failure == "job" { &other } else { &output };
            let product = json!({"result":{"scalar":"text"},"capture_complete":true});
            save_completed(target, &product, proofs).expect(failure);
            let document = document(target);
            assert_eq!(document["result"]["scalar"], "text", "{failure}");
            assert!(document.pointer(field).is_none(), "{failure}");
            let referenced = target.test_fields();
            let expected: &[&str] = if failure == "job" {
                &[]
            } else {
                &["/result/ok"]
            };
            assert_eq!(referenced, expected, "{failure}");
            let captures = captures::available_captures(&saved(&output), true).unwrap();
            let raw = captures.iter().find(|capture| capture.field == field);
            match failure {
                "missing" => assert!(raw.is_none()),
                _ => assert!(raw.is_some_and(|capture| !capture.complete), "{failure}"),
            }
        }
    }

    #[test]
    fn utf8_validation_is_chunked_and_carries_split_sequences() {
        let valid = "aé€😀z".repeat(3);
        for size in 4..=9 {
            let mut buffer = vec![0; size];
            assert!(
                validate_utf8(valid.as_bytes(), &mut buffer).is_ok(),
                "{size}"
            );
            for invalid in [
                &b"a\xff"[..],
                b"\xe2\x82",
                b"\xf0\x9f\x98a",
                b"\xed\xa0\x80",
            ] {
                let bytes = [valid.as_bytes(), invalid].concat();
                let error = validate_utf8(bytes.as_slice(), &mut buffer).unwrap_err();
                assert_eq!(
                    error.kind(),
                    io::ErrorKind::InvalidData,
                    "{size} {invalid:?}"
                );
            }
        }
    }

    /// Unknown captures reference only an existing string/container shape. Existing
    /// primitives abandon even known completed bytes for every kind: invalid
    /// UTF-8/JSON is deliberately not inspected, since IO completion alone did not
    /// reference it.
    #[tokio::test]
    async fn unknown_uses_existing_shape_only_and_preserves_scalar_or_absent_inventory() {
        for kind in [CaptureKind::Unknown, CaptureKind::Text, CaptureKind::Json] {
            let (_root, _manager, output, _) = outputs().await;
            let unknown = kind == CaptureKind::Unknown;
            let initial = json!({"result":{"text":"", "object":{}, "array":[], "null":null, "bool":true,"number":4},"capture_complete":true});
            let shaped: &[(&str, &[u8])] = if unknown {
                &[
                    ("text", br#"{"looks":"json"}"#),
                    ("object", br#"{"x":1}"#),
                    ("array", b"[1,2]"),
                    ("missing", b"[unfinished"),
                ]
            } else {
                &[]
            };
            let scalars: [(&str, &[u8]); 3] =
                [("null", b"\xff"), ("bool", b"\xff"), ("number", b"\xff")];
            let captures = shaped
                .iter()
                .copied()
                .chain(scalars)
                .map(|(field, bytes)| completed(&output, &format!("/result/{field}"), kind, bytes))
                .collect();
            save_completed(&output, &initial, captures).unwrap();
            let hydrated = hydrate(&saved(&output), document(&output), u64::MAX).unwrap();
            if unknown {
                assert_eq!(hydrated["result"]["text"], r#"{"looks":"json"}"#);
                assert_eq!(hydrated["result"]["object"], json!({"x":1}));
                assert_eq!(hydrated["result"]["array"], json!([1, 2]));
                assert!(hydrated.pointer("/result/missing").is_none());
            } else {
                assert_eq!(document(&output), initial);
            }
            for (field, _) in scalars {
                assert_eq!(hydrated["result"][field], initial["result"][field]);
            }
            assert_eq!(output.test_fields().len(), if unknown { 3 } else { 0 });
            for descriptor in captures::available_captures(&saved(&output), true).unwrap() {
                let shaped = matches!(
                    descriptor.field.as_str(),
                    "/result/text" | "/result/object" | "/result/array"
                );
                assert_eq!(descriptor.complete, unknown && shaped);
            }
        }
    }

    #[tokio::test]
    async fn unrelated_capture_is_neither_adopted_nor_overwritten_by_ordinary_results() {
        let (_root, _manager, output, _) = outputs().await;
        let _unreferenced = completed(
            &output,
            "/result/raw",
            CaptureKind::Text,
            b"abandoned raw bytes",
        );
        let result =
            json!({"result":{"raw":"ordinary value".repeat(1000)},"capture_complete":true});
        save_completed(&output, &result, vec![]).unwrap();
        assert_eq!(document(&output), result);
        assert!(output.test_fields().is_empty());
        assert_eq!(
            output.test_bytes("/result/raw").unwrap(),
            b"abandoned raw bytes"
        );
    }

    /// A capture-backed field is presented as a page at most, so it is budgeted
    /// as at most a page whatever its stored size.
    #[tokio::test]
    async fn presentation_size_budgets_a_large_capture_as_one_page() {
        let (_root, _manager, output, _other) = outputs().await;
        let capture = completed(
            &output,
            "/result/text",
            CaptureKind::Text,
            &vec![b's'; 6 * PAGE_BYTES],
        );
        save_completed(&output, &complete_document(), vec![capture]).unwrap();
        let estimate = presentation_size(&output);
        assert!(
            estimate <= 2 * PAGE_BYTES,
            "capture-backed output estimated at {estimate} bytes"
        );
    }
}
