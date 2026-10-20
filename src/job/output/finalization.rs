//! Admission of completed, destination-bound capture receipts into saved output.
use super::*;
use std::io;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Prepare a terminal product, referencing only its explicitly completed captures.
/// JSON containers are referenced; JSON scalars are decoded inline. Existing
/// null/bool/number fields remain authoritative; a capture can install a missing
/// field.
///
/// A receipt that fails validation never fails the terminal product: its capture
/// stays unreferenced and is reported incomplete. Only reading a capture it
/// references can fail, a storage failure. The fields the schema declares join
/// the `presented` the producer reported.
pub(crate) fn prepare_completed(
    output: &Output,
    schema: &Value,
    (result, captures_complete): (Option<Value>, bool),
    completed: Vec<CompletedCapture>,
    mut presented: Presented,
) -> io::Result<OutputRows> {
    // An unreadable inventory leaves every receipt unbound (and so incomplete).
    let saved = Saved::load(output).ok();
    let empty = BTreeMap::new();
    let registered = saved.as_ref().map_or(&empty, |saved| &saved.captures);
    let fields = completed
        .iter()
        .map(CompletedCapture::field)
        .collect::<Vec<_>>();
    let bound = completed
        .iter()
        .enumerate()
        .filter_map(|(index, capture)| {
            let field = capture.field();
            let registration = registered.get(field)?;
            (capture.belongs_to(output.job, registration.id)
                && registration.kind == capture.kind()
                // Every receipt for a duplicate or overlapping field is unbound:
                // neither can be referenced without shadowing the other.
                && fields.iter().enumerate().all(|(other, candidate)| {
                    other == index
                        || (*candidate != field
                            && !field.contains(candidate)
                            && !candidate.contains(field))
                }))
            .then_some(capture)
        })
        .collect::<Vec<_>>();

    let has_result = result.is_some();
    let mut document = json!({"result": result});
    let mut references = BTreeSet::new();
    for capture in bound {
        let field = capture.field();
        let Some(saved) = &saved else { break };
        let Some(value) = admitted_value(&document, saved, capture).ok().flatten() else {
            continue;
        };
        if install(&mut document, field, value.clone()).is_err() {
            continue;
        }
        if capture.kind() == CaptureKind::Text || value.is_array() || value.is_object() {
            references.insert(field.clone());
        }
    }
    let root = FieldPointer::result();
    // Cut-off output is never taken as JSON, however it happens to end.
    let candidates = match captures_complete {
        true => projection::json_text_fields(&document["result"], &root, schema),
        false => BTreeSet::new(),
    };
    presented.extend(projection::presented_fields(
        &document["result"],
        &root,
        schema,
    ));
    let result = has_result.then(|| document["result"].take());
    prepare_document(
        output,
        registered,
        (result, captures_complete),
        Stored {
            referenced: &references,
            candidates: &candidates,
            presented,
        },
    )
}

/// Interpret one bound receipt against the terminal document. `Ok(None)` is a
/// deliberately unreferenced capture; errors are validation failures. Neither
/// is a persistence failure, so the caller publishes both as incomplete.
fn admitted_value(
    document: &Value,
    saved: &Saved,
    capture: &CompletedCapture,
) -> io::Result<Option<Value>> {
    let field = capture.field();
    let source = || {
        saved
            .capture(field)
            .ok_or_else(|| invalid("completed capture is missing"))
    };
    // A terminal primitive abandons the raw capture, even when the sender
    // completed its bytes.
    if let Some(Value::Null | Value::Bool(_) | Value::Number(_)) = document.pointer(field.as_str())
    {
        return Ok(None);
    }
    Ok(Some(match capture.kind() {
        CaptureKind::Text => {
            validate_utf8(source()?, &mut [0; IO_BUFFER_BYTES])?;
            Value::String(String::new())
        }
        CaptureKind::Json => validated_json_reference(source()?)?,
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

/// Missing ancestors become objects. The whole path is checked before mutation,
/// so a rejected field leaves the document unchanged.
fn install(document: &mut Value, field: &FieldPointer, value: Value) -> io::Result<()> {
    if field.is_root() {
        *document = value;
        return Ok(());
    }
    let keys = field.segments().collect::<Vec<_>>();
    let (last, ancestors) = keys.split_last().expect("split yields a component");
    let mut probe = Some(&*document);
    for key in &keys {
        let Some(current) = probe else {
            break;
        };
        let Value::Object(map) = current else {
            return Err(invalid("completed capture ancestor is not an object"));
        };
        probe = map.get(key);
    }
    let mut cursor = document;
    for key in ancestors {
        cursor = slot(cursor, key);
    }
    *slot(cursor, last) = value;
    Ok(())
}

fn slot<'a>(cursor: &'a mut Value, key: &str) -> &'a mut Value {
    let Value::Object(map) = cursor else {
        unreachable!("checked capture ancestor")
    };
    map.entry(key).or_insert_with(|| json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobSpec, tests::runtime};
    use crate::tool::ToolOutput;

    /// Two running jobs of one session: receipts are bound to exactly one of them.
    async fn outputs() -> (tempfile::TempDir, JobManager, Output, Output) {
        let (root, manager, agent) = runtime().await;
        let mut outputs = Vec::new();
        for name in ["first", "second"] {
            let id = manager
                .test_running(JobSpec::test(agent.clone(), name))
                .await
                .into_test_id();
            outputs.push(manager.output(id).await);
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
        let mut writer = PendingCapture::create(output, &pointer(field), kind)
            .unwrap()
            .open();
        writer.write_all(bytes).unwrap();
        writer.finish().unwrap()
    }

    /// Complete `output`'s job with `result` and the receipts `captures`.
    async fn finish(
        manager: &JobManager,
        output: &Output,
        result: Value,
        captures: Vec<CompletedCapture>,
    ) {
        let completed = ToolOutput::new(result).with_captures(captures);
        manager.test_complete(output.job, completed).await;
    }

    fn pointer(field: &str) -> FieldPointer {
        field.parse().unwrap()
    }

    fn document(output: &Output) -> Value {
        output.test_document().unwrap()
    }

    fn saved(output: &Output) -> Saved {
        Saved::load(output).unwrap()
    }

    #[tokio::test]
    async fn completed_receipts_install_omitted_fields_and_preserve_existing_wire_references() {
        let (_root, manager, output, _) = outputs().await;
        let captures = [
            ("/result/text", CaptureKind::Text, "é\ntext".as_bytes()),
            ("/result/matches", CaptureKind::Json, br#"{"a":[1,true]}"#),
            ("/result/a~1b/~0key", CaptureKind::Json, br#"[null,"x"]"#),
        ]
        .map(|(field, kind, bytes)| completed(&output, field, kind, bytes));
        finish(&manager, &output, json!({}), captures.into()).await;
        let document = document(&output);
        assert_eq!(
            document["result"],
            json!({"text":"","matches":{},"a/b":{"~key":[]}})
        );
        let referenced = output.test_fields();
        assert_eq!(
            referenced
                .iter()
                .map(FieldPointer::as_str)
                .collect::<Vec<_>>(),
            ["/result/a~1b/~0key", "/result/matches", "/result/text"]
        );
        let hydrated = hydrate(&saved(&output), document).unwrap();
        let expected = json!({"text":"é\ntext","matches":{"a":[1,true]},"a/b":{"~key":[null,"x"]}});
        assert_eq!(hydrated["result"], expected);
        assert!(
            captures::available_captures(&saved(&output), true)
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
            let (_root, manager, output, other) = outputs().await;
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
                    let raw = saved(&output).captures[&pointer(field)].id;
                    output.db.delete_capture(raw).unwrap();
                    drop(
                        PendingCapture::create(&output, &pointer(field), CaptureKind::Json)
                            .unwrap(),
                    );
                }
                "overlap" => proofs.push(completed(
                    &output,
                    "/result/raw/nested",
                    CaptureKind::Text,
                    b"nested",
                )),
                "duplicate" => proofs.push(proofs[0].clone()),
                "missing" => {
                    let raw = saved(&output).captures[&pointer(field)].id;
                    output.db.delete_capture(raw).unwrap();
                }
                _ => {}
            }
            let target = if failure == "job" { &other } else { &output };
            finish(&manager, target, json!({"scalar":"text"}), proofs).await;
            let document = document(target);
            assert_eq!(document["result"]["scalar"], "text", "{failure}");
            assert!(document.pointer(field).is_none(), "{failure}");
            let referenced = target.test_fields();
            let expected: &[&str] = if failure == "job" {
                &[]
            } else {
                &["/result/ok"]
            };
            assert_eq!(
                referenced
                    .iter()
                    .map(FieldPointer::as_str)
                    .collect::<Vec<_>>(),
                expected,
                "{failure}"
            );
            let captures = captures::available_captures(&saved(&output), true);
            let raw = captures
                .iter()
                .find(|capture| capture.field.as_str() == field);
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

    /// Existing primitives abandon even completed bytes of every kind: invalid
    /// UTF-8/JSON is deliberately not inspected, since IO completion alone did not
    /// reference it.
    #[tokio::test]
    async fn existing_primitives_abandon_completed_captures() {
        for kind in [CaptureKind::Text, CaptureKind::Json] {
            let (_root, manager, output, _) = outputs().await;
            let initial = json!({"null":null, "bool":true, "number":4});
            let captures = ["null", "bool", "number"]
                .map(|field| completed(&output, &format!("/result/{field}"), kind, b"\xff"));
            finish(&manager, &output, initial.clone(), captures.into()).await;
            assert_eq!(document(&output)["result"], initial);
            assert!(output.test_fields().is_empty());
            assert!(
                captures::available_captures(&saved(&output), true)
                    .iter()
                    .all(|capture| !capture.complete)
            );
        }
    }

    #[tokio::test]
    async fn unrelated_capture_is_neither_adopted_nor_overwritten_by_ordinary_results() {
        let (_root, manager, output, _) = outputs().await;
        let _unreferenced = completed(
            &output,
            "/result/raw",
            CaptureKind::Text,
            b"abandoned raw bytes",
        );
        let result = json!({"raw":"ordinary value".repeat(1000)});
        finish(&manager, &output, result.clone(), vec![]).await;
        assert_eq!(document(&output)["result"], result);
        assert!(output.test_fields().is_empty());
        assert_eq!(
            output.test_bytes("/result/raw").unwrap(),
            b"abandoned raw bytes"
        );
    }
}
