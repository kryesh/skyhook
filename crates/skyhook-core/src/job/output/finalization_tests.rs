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

fn completed(output: &Output, field: &str, kind: CaptureKind, bytes: &[u8]) -> CompletedCapture {
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
    let result = json!({"result":{"raw":"ordinary value".repeat(1000)},"capture_complete":true});
    save_completed(&output, &result, vec![]).unwrap();
    assert_eq!(document(&output), result);
    assert!(output.test_fields().is_empty());
    assert_eq!(
        output.test_bytes("/result/raw").unwrap(),
        b"abandoned raw bytes"
    );
}
