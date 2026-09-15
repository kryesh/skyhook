use super::*;

fn job1() -> JobId {
    JobId::new(1).unwrap()
}

fn completed(
    directory: &Path,
    job: JobId,
    field: &str,
    kind: CaptureKind,
    bytes: &[u8],
) -> CompletedCapture {
    let mut writer = PendingCapture::create(job, directory, field, kind)
        .unwrap()
        .open();
    writer.write_all(bytes).unwrap();
    writer.finish().unwrap()
}

fn document(directory: &Path) -> Value {
    serde_json::from_reader(File::open(directory.join("document.json")).unwrap()).unwrap()
}

fn complete_document() -> Value {
    json!({"result":{},"capture_complete":true})
}

#[test]
fn completed_receipts_install_omitted_fields_and_preserve_existing_wire_references() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let captures = [
        ("/result/text", CaptureKind::Text, "é\ntext".as_bytes()),
        ("/result/matches", CaptureKind::Json, br#"{"a":[1,true]}"#),
        ("/result/a~1b/~0key", CaptureKind::Json, br#"[null,"x"]"#),
    ]
    .map(|(field, kind, bytes)| completed(path, job1(), field, kind, bytes));
    save_completed(path, job1(), &complete_document(), captures.into()).unwrap();
    let saved = document(path);
    assert_eq!(
        saved["result"],
        json!({"text":"","matches":{},"a/b":{"~key":[]}})
    );
    // fields.json preserves document traversal order, not lexical order.
    let mut referenced = fields(path).unwrap();
    referenced.sort();
    assert_eq!(
        referenced,
        ["/result/a~1b/~0key", "/result/matches", "/result/text"]
    );
    let hydrated = hydrate(path, saved, u64::MAX).unwrap();
    let expected = json!({"text":"é\ntext","matches":{"a":[1,true]},"a/b":{"~key":[null,"x"]}});
    assert_eq!(hydrated["result"], expected);
    assert!(
        captures::available_captures(path, true)
            .unwrap()
            .iter()
            .all(|capture| capture.complete)
    );
}

/// Invalid receipts never fail terminal publication: the product is saved, and
/// each rejected capture stays unreferenced (reported incomplete when its file
/// still exists). JSON validation covers the full payload, not only its prefix.
#[test]
fn invalid_capture_receipts_publish_the_product_with_incomplete_captures() {
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
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        save(
            path,
            &json!({"result":{"prior":true},"capture_complete":false}),
        )
        .unwrap();
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
        let mut proofs = vec![completed(path, job1(), field, kind, bytes)];
        // A valid sibling receipt is still referenced beside a rejected one.
        proofs.push(completed(
            path,
            job1(),
            "/result/ok",
            CaptureKind::Text,
            b"fine",
        ));
        match failure {
            "registration" => drop(register_capture(path, field, CaptureKind::Json).unwrap()),
            "overlap" => proofs.push(completed(
                path,
                job1(),
                "/result/raw/nested",
                CaptureKind::Text,
                b"nested",
            )),
            "duplicate" => proofs.push(proofs[0].clone()),
            "missing" => std::fs::remove_file(field_file(path, field)).unwrap(),
            _ => {}
        }
        let target = if failure == "job" {
            JobId::new(2).unwrap()
        } else {
            job1()
        };
        let product = json!({"result":{"scalar":"text"},"capture_complete":true});
        save_completed(path, target, &product, proofs).expect(failure);
        let saved = document(path);
        assert_eq!(saved["result"]["scalar"], "text", "{failure}");
        assert!(saved.pointer(field).is_none(), "{failure}");
        let referenced = fields(path).unwrap();
        let expected: &[&str] = if failure == "job" {
            &[]
        } else {
            &["/result/ok"]
        };
        assert_eq!(referenced, expected, "{failure}");
        let captures = captures::available_captures(path, true).unwrap();
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
#[test]
fn unknown_uses_existing_shape_only_and_preserves_scalar_or_absent_inventory() {
    for kind in [CaptureKind::Unknown, CaptureKind::Text, CaptureKind::Json] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
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
            .map(|(field, bytes)| completed(path, job1(), &format!("/result/{field}"), kind, bytes))
            .collect();
        save_completed(path, job1(), &initial, captures).unwrap();
        let hydrated = hydrate(path, document(path), u64::MAX).unwrap();
        if unknown {
            assert_eq!(hydrated["result"]["text"], r#"{"looks":"json"}"#);
            assert_eq!(hydrated["result"]["object"], json!({"x":1}));
            assert_eq!(hydrated["result"]["array"], json!([1, 2]));
            assert!(hydrated.pointer("/result/missing").is_none());
        } else {
            assert_eq!(document(path), initial);
        }
        for (field, _) in scalars {
            assert_eq!(hydrated["result"][field], initial["result"][field]);
        }
        assert_eq!(fields(path).unwrap().len(), if unknown { 3 } else { 0 });
        for descriptor in captures::available_captures(path, true).unwrap() {
            let shaped = matches!(
                descriptor.field.as_str(),
                "/result/text" | "/result/object" | "/result/array"
            );
            assert_eq!(descriptor.complete, unknown && shaped);
        }
    }
}

#[test]
fn unrelated_capture_is_neither_adopted_nor_overwritten_by_ordinary_results() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let _unreferenced = completed(
        path,
        job1(),
        "/result/raw",
        CaptureKind::Text,
        b"abandoned raw bytes",
    );
    let result = json!({"result":{"raw":"ordinary value".repeat(1000)},"capture_complete":true});
    save_completed(path, job1(), &result, vec![]).unwrap();
    assert_eq!(document(path), result);
    assert!(fields(path).unwrap().is_empty());
    assert_eq!(
        std::fs::read(field_file(path, "/result/raw")).unwrap(),
        b"abandoned raw bytes"
    );
}
