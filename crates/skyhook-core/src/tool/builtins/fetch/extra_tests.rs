use super::*;

#[tokio::test]
async fn methods_and_body_encodings_reach_the_server() {
    let runtime = crate::test_support::TestRuntime::new().await;
    tokio::fs::write(runtime.root.path().join("upload.txt"), b"file payload")
        .await
        .unwrap();
    let executor = executor(&runtime);
    let cases = [
        ("GET", None, ""),
        ("HEAD", None, ""),
        ("OPTIONS", None, ""),
        ("DELETE", None, ""),
        ("TRACE", None, ""),
        (
            "PROPFIND",
            Some(json!({"kind":"text","value":"custom payload"})),
            "custom payload",
        ),
        ("POST", Some(json!({"kind":"json","value":null})), "null"),
        (
            "PATCH",
            Some(json!({"kind":"json","value":{"enabled":true}})),
            "{\"enabled\":true}",
        ),
        (
            "POST",
            Some(json!({"kind":"form","fields":[["key","a b"],["key","&"]]})),
            "key=a+b&key=%26",
        ),
        (
            "PUT",
            Some(json!({"kind":"base64","value":"Ynl0ZXM="})),
            "bytes",
        ),
        (
            "PUT",
            Some(json!({"kind":"file","path":"upload.txt"})),
            "file payload",
        ),
    ];
    let (url, task) = server(
        cases
            .iter()
            .map(|_| response("200 OK", "Content-Type: text/plain\r\n", ""))
            .collect(),
    )
    .await;
    for (method, body, _) in &cases {
        let mut arguments = json!({"url":url,"method":method});
        if let Some(body) = body {
            arguments["body"] = body.clone();
        }
        let result = executor
            .execute(runtime.agent.clone(), "fetch", arguments, None)
            .await
            .unwrap();
        assert_eq!(result.output.value["status"], 200);
    }
    let requests = task.await.unwrap();
    for (request, (method, _, expected)) in requests.iter().zip(cases) {
        assert!(
            request.starts_with(&format!("{method} / HTTP/1.1\r\n")),
            "{request}"
        );
        assert_eq!(request.split_once("\r\n\r\n").unwrap().1, expected);
    }
}

#[test]
fn multipart_is_not_advertised_and_is_rejected() {
    let mut builder = ToolRegistryBuilder::default();
    register(&mut builder).unwrap();
    let registry = builder.build();
    let capabilities = crate::tool::policy::CapabilitySet::default();
    let surface = registry.surface(&capabilities);
    let spec = surface.get("fetch").unwrap();
    assert!(spec.input_schema["$defs"].get("MultipartPart").is_none());
    let body_kinds: Vec<_> = spec.input_schema["$defs"]["RequestBody"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|variant| variant["properties"]["kind"]["const"].as_str().unwrap())
        .collect();
    assert_eq!(body_kinds, ["text", "json", "form", "base64", "file"]);
    let old_input = json!({"url":"https://example.org", "body":{"kind":"multipart", "parts":[]}});
    assert!(serde_json::from_value::<FetchArgs>(old_input.clone()).is_err());
    assert!(
        !jsonschema::validator_for(&spec.input_schema)
            .unwrap()
            .is_valid(&old_input)
    );
}

async fn raw_response_server(response: Vec<u8>) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut buffer = [0u8; 4096];
        assert!(socket.read(&mut buffer).await.unwrap() > 0);
        socket.write_all(&response).await.unwrap();
    });
    (url, task)
}

#[tokio::test]
async fn decompressed_bytes_are_counted_and_limited() {
    // Gzip of 4096 ASCII A bytes, with deterministic zero mtime.
    let compressed = STANDARD
        .decode("H4sIAAAAAAAC/+3BAQ0AAADCoGzvX8oeDigAAADg3QBANKb+ABAAAA==")
        .unwrap();
    let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", compressed.len()).into_bytes();
    response.extend_from_slice(&compressed);
    let runtime = crate::test_support::TestRuntime::new().await;
    let executor = executor(&runtime);
    let (url, task) = raw_response_server(response.clone()).await;
    let result = executor
        .execute(
            runtime.agent.clone(),
            "fetch",
            json!({"url":url,"max_bytes":4096}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.output.value["received_bytes"], 4096);
    assert_eq!(result.output.value["body"]["text"], "A".repeat(4096));
    task.await.unwrap();
    let (url, task) = raw_response_server(response).await;
    assert!(
        executor
            .execute(
                runtime.agent.clone(),
                "fetch",
                json!({"url":url,"max_bytes":100}),
                None
            )
            .await
            .is_err()
    );
    task.await.unwrap();
}

#[tokio::test]
async fn cancellation_preserves_destination_and_cleans_temporary_download() {
    let runtime = crate::test_support::TestRuntime::new().await;
    let destination = runtime.root.path().join("saved.txt");
    tokio::fs::write(&destination, b"original").await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 4096];
        assert!(socket.read(&mut buffer).await.unwrap() > 0);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10000\r\n\r\npartial")
            .await
            .unwrap();
        let _ = ready_tx.send(());
        // Hold the response open until the test ends; aborted explicitly below.
        std::future::pending::<()>().await;
    });
    let executor = executor(&runtime);
    let agent = runtime.agent.clone();
    let pending = tokio::spawn(async move {
        executor
            .execute(
                agent,
                "fetch",
                json!({"url":url,"save_to":"saved.txt","overwrite":true}),
                None,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), ready_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runtime.jobs.cancel_all(&runtime.agent).await, 1);
    assert!(
        tokio::time::timeout(Duration::from_secs(10), pending)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    server_task.abort();
    let _ = server_task.await;
    assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"original");
    let entries: Vec<_> = std::fs::read_dir(runtime.root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        entries.len(),
        2,
        "only sessions and original destination should remain: {entries:?}"
    );
}

#[tokio::test]
async fn response_headers_are_opt_in_for_http_results_and_failures() {
    let runtime = crate::test_support::TestRuntime::new().await;
    let executor = executor(&runtime);
    let output_schema = serde_json::to_value(schema_for!(FetchResultSchema)).unwrap();
    let validator = jsonschema::validator_for(&output_schema).unwrap();
    for include_headers in [None, Some(false), Some(true)] {
        for (status, extra, options, error_kind) in [
            ("200 OK", "", json!({}), None),
            ("404 Not Found", "", json!({}), None),
            ("200 OK", "", json!({"max_bytes":1}), Some("size_limit")),
            (
                "200 OK",
                "",
                json!({"text":true}),
                Some("extraction_failure"),
            ),
            (
                "302 Found",
                "Location: /again\r\n",
                json!({"max_redirects":0}),
                Some("redirect_failure"),
            ),
        ] {
            let (url, task) = server(vec![response(
                status,
                &format!(
                    "Content-Type: application/pdf\r\nX-Result: one\r\nX-Result: two\r\n{extra}"
                ),
                "body",
            )])
            .await;
            let mut arguments = options;
            arguments["url"] = json!(url);
            arguments["headers"] = json!({"X-Request":["one", "two"]});
            if let Some(include) = include_headers {
                arguments["include_headers"] = json!(include);
            }
            let result = executor
                .execute(runtime.agent.clone(), "fetch", arguments, None)
                .await;
            let output = if let Some(kind) = error_kind {
                let output = result.unwrap_err().into_failure().output.unwrap().value;
                assert_eq!(output["diagnostic"]["error_kind"], kind);
                output
            } else {
                let output = result.unwrap().output.value;
                assert!(output.get("diagnostic").is_none());
                assert_eq!(output["body"], json!({"kind":"base64", "data":"Ym9keQ=="}));
                output
            };
            let status: u16 = status.split_whitespace().next().unwrap().parse().unwrap();
            assert_eq!(output["status"], status);
            assert_eq!(output["ok"], (200..300).contains(&status));
            if include_headers == Some(true) {
                assert_eq!(output["headers"]["x-result"], json!(["one", "two"]));
                assert_eq!(
                    output["headers"]["content-type"],
                    json!(["application/pdf"])
                );
            } else {
                assert!(output.get("headers").is_none(), "{output}");
            }
            assert!(validator.is_valid(&output), "{output}");
            let requests = task.await.unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].contains("x-request: one\r\nx-request: two"));
        }
    }
}

#[tokio::test]
async fn transport_failures_have_no_response_headers_even_with_opt_in() {
    let runtime = crate::test_support::TestRuntime::new().await;
    let executor = executor(&runtime);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    for include_headers in [None, Some(false), Some(true)] {
        let mut arguments = json!({"url":url});
        if let Some(include) = include_headers {
            arguments["include_headers"] = json!(include);
        }
        let error = executor
            .execute(runtime.agent.clone(), "fetch", arguments, None)
            .await
            .unwrap_err();
        let output = error.into_failure().output.unwrap().value;
        assert_eq!(output["diagnostic"]["error_kind"], "connection_refused");
        assert!(output.get("status").is_none());
        assert!(output.get("headers").is_none());
    }
}

#[tokio::test]
async fn response_body_deadline_preserves_opt_in_headers() {
    let runtime = crate::test_support::TestRuntime::new().await;
    let executor = executor(&runtime);
    for include_headers in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            assert!(socket.read(&mut buffer).await.unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nX-Result: waiting\r\n\r\n")
                .await
                .unwrap();
            // Wait for the client to close after its total deadline.
            let _ = socket.read(&mut buffer).await;
        });
        let mut arguments = json!({"url":url,"timeout":1});
        if include_headers {
            arguments["include_headers"] = json!(true);
        }
        let error = executor
            .execute(runtime.agent.clone(), "fetch", arguments, None)
            .await
            .unwrap_err();
        let output = error.into_failure().output.unwrap().value;
        assert_eq!(output["status"], 200);
        assert_eq!(output["diagnostic"]["timeout"]["kind"], "total");
        if include_headers {
            assert_eq!(output["headers"]["x-result"], json!(["waiting"]));
        } else {
            assert!(output.get("headers").is_none());
        }
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
    }
}

#[test]
fn failure_progress_does_not_restore_headers_from_previous_output_by_default() {
    let progress = FetchProgress::new(&args(json!({"url":"https://example.org"})));
    let error = progress.failure(ToolError::with_output(
        "previous failure",
        ToolOutput::new(json!({"headers":{"x-result":["hidden"]}, "detail":"retained"})),
    ));
    let ToolError::FailedWithOutput { output, .. } = error else {
        panic!("expected structured failure");
    };
    assert!(output.value.get("headers").is_none());
    assert_eq!(output.value["detail"], "retained");
}
