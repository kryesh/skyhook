//! Automatic capture assembly contracts; capture ownership lives in captures/.
use super::{tests::fixture, *};

fn capture(manager: &JobManager, id: JobId, field: &str, kind: CaptureKind, bytes: &[u8]) {
    manager.output(id).test_capture(field, kind, bytes);
}

async fn hydrated(manager: &JobManager, job: JobId) -> PresentedOutput {
    let args = OutputArgs::new(job);
    manager
        .inspect_output_with_captures(args, &Default::default())
        .await
        .unwrap()
}

#[tokio::test]
async fn automatic_hydration_reads_live_and_terminal_captures_in_descriptor_order() {
    let (_root, manager, job) = fixture(None).await;
    // More than the concurrency bound, with raw/incomplete JSON and escaped pointers.
    let fields: Vec<_> = (0..7)
        .map(|i| format!("/result/events~1custom/{i}~0key"))
        .collect();
    for terminal in [false, true] {
        let bytes: &[u8] = if terminal {
            b"{\"partial\":\n42"
        } else {
            b"{\"partial\":"
        };
        for field in &fields {
            capture(&manager, job, field, CaptureKind::Json, bytes);
        }
        if terminal {
            manager.test_finish(job, json!({"status":"done"})).await;
        }
        let output = hydrated(&manager, job).await;
        assert_eq!(output.state.is_terminal(), terminal);
        let captures = output.view()["captures"].as_array().unwrap();
        assert_eq!(captures.len(), fields.len());
        for (capture, field) in captures.iter().zip(&fields) {
            assert_eq!(
                (&capture["field"], &capture["kind"], &capture["complete"]),
                (&json!(field), &json!("json"), &json!(false))
            );
            let preview = &capture["output"]["preview"];
            assert_eq!(
                (&preview["field"], &preview["lines"][0]),
                (&json!(field), &json!("{\"partial\":"))
            );
            assert_eq!(
                preview["lines"].as_array().unwrap().len(),
                if terminal { 2 } else { 1 }
            );
            // Capture pages are not recursively hydrated.
            for nested in capture["output"]["captures"].as_array().unwrap() {
                assert!(nested.get("output").is_none());
            }
        }
    }
}

#[tokio::test]
async fn automatic_hydration_distinguishes_absent_from_present_and_null() {
    let (_root, manager, job) = fixture(None).await;
    for field in ["/result/absent", "/result/null", "/result/present"] {
        capture(&manager, job, field, CaptureKind::Text, b"raw capture");
    }
    manager
        .test_finish(job, json!({"null":null,"present":"structured value"}))
        .await;
    let output = hydrated(&manager, job).await;
    assert_eq!(output.view()["result"]["present"], "structured value");
    // Existing null-elision presentation stays intact, without turning its null
    // into permission to hydrate an abandoned capture at the same pointer.
    assert!(output.view().pointer("/result/null").is_none());
    for capture in output.view()["captures"].as_array().unwrap() {
        assert_eq!(
            capture.get("output").is_some(),
            capture["field"] == "/result/absent"
        );
    }
}

#[tokio::test]
async fn automatic_hydration_retains_nested_page_continuations() {
    let (_root, manager, job) = fixture(None).await;
    capture(
        &manager,
        job,
        "/result/log",
        CaptureKind::Text,
        "line\n".repeat(150).as_bytes(),
    );
    let output = hydrated(&manager, job).await;
    // The live whole-result page is empty but retains its polling position;
    // the capture has an independent source-page continuation.
    assert_eq!(
        output.view()["preview"],
        json!({"field": "/result", "lines": [], "next_start": 1})
    );
    let preview = &output.view()["captures"][0]["output"]["preview"];
    assert_eq!(preview["field"], "/result/log");
    assert_eq!(preview["lines"].as_array().unwrap().len(), 100);
    assert_eq!(preview["next_start"], 101);
    assert!(preview.get("next_offset").is_none());
}

#[tokio::test]
async fn automatic_hydration_embeds_page_errors_but_propagates_initial_errors() {
    let (_root, manager, job) = fixture(None).await;
    // Discovery succeeds, but this page cannot decode its capture.
    capture(&manager, job, "/result/bad", CaptureKind::Text, b"\xffbad");
    capture(&manager, job, "/result/good", CaptureKind::Text, b"good");
    let output = hydrated(&manager, job).await;
    let captures = &output.view()["captures"];
    assert!(!captures[0]["output"]["error"].as_str().unwrap().is_empty());
    assert_eq!(captures[1]["output"]["preview"]["lines"], json!(["good"]));
    let mut invalid = OutputArgs::new(job);
    invalid.start = Some(0);
    let result = manager
        .inspect_output_with_captures(invalid, &Default::default())
        .await;
    assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    let unknown = OutputArgs::new(JobId::new(job.get() + 100).unwrap());
    assert!(
        manager
            .inspect_output_with_captures(unknown, &Default::default())
            .await
            .is_err()
    );
}
