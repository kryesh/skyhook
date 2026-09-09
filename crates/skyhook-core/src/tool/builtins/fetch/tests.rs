use super::*;
use serde_json::json;
use tokio::{net::TcpListener, task::JoinHandle};

fn args(value: Value) -> FetchArgs {
    serde_json::from_value(value).unwrap()
}
fn executor(runtime: &crate::test_support::TestRuntime) -> crate::tool::executor::ToolExecutor {
    let mut builder = ToolRegistryBuilder::default();
    register(&mut builder).unwrap();
    runtime.executor(builder)
}
async fn server(responses: Vec<String>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(10), async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let header_end = loop {
                    let mut buffer = [0u8; 4096];
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .map(|n| n.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                while request.len() < header_end + length {
                    let mut buffer = [0u8; 4096];
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                requests.push(String::from_utf8_lossy(&request).into_owned());
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
            requests
        })
        .await
        .expect("fixture server timed out")
    });
    (url, task)
}
fn response(status: &str, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn response_body_payloads_are_truncated_but_full_output_is_retrievable() {
    use crate::job::JobOutputQuery;

    let runtime = crate::test_support::TestRuntime::new().await;
    let executor = executor(&runtime);
    let payload = "body".repeat(1024);
    let header = "h".repeat(3000);
    for (format, field, expected) in [
        ("text", "text", payload.clone()),
        ("base64", "data", STANDARD.encode(payload.as_bytes())),
    ] {
        let (url, task) = server(vec![response(
            "200 OK",
            &format!("Content-Type: text/plain\r\nX-Details: {header}\r\n"),
            &payload,
        )])
        .await;
        let call = executor
            .execute_model(
                runtime.agent.clone(),
                "fetch",
                json!({"url":url, "response_format":format}),
                None,
            )
            .await
            .unwrap();
        task.await.unwrap();
        let view = call.output.value;
        assert_eq!(view["state"], "completed");
        assert_eq!(view["result"]["status"], 200);
        assert_eq!(view["result"]["url"], format!("{url}/"));
        assert_eq!(view["result"]["received_bytes"], payload.len());
        assert_eq!(view["result"]["headers"]["x-details"][0], header);
        assert_eq!(view["result"]["body"]["kind"], format);
        let prefix = view["result"]["body"][field].as_str().unwrap();
        assert!(prefix.len() < expected.len());
        assert!(expected.starts_with(prefix));
        let markers = view["truncated"].as_array().unwrap();
        assert_eq!(markers.len(), 1);
        let pointer = format!("/result/body/{field}");
        assert_eq!(markers[0]["field"], pointer);
        assert_eq!(markers[0]["next_start"], 1);
        assert_eq!(markers[0]["next_offset"], prefix.len());

        let mut query = JobOutputQuery::new(call.job);
        query.field = Some(pointer);
        let full = runtime
            .jobs
            .inspect_output(query.clone(), &Default::default())
            .await
            .unwrap();
        assert_eq!(full["preview"]["lines"], json!([expected]));
        query.start = Some(markers[0]["next_start"].as_u64().unwrap() as usize);
        query.offset = Some(markers[0]["next_offset"].as_u64().unwrap() as usize);
        let remainder = runtime
            .jobs
            .inspect_output(query, &Default::default())
            .await
            .unwrap();
        assert_eq!(
            remainder["preview"]["lines"],
            json!([&expected[prefix.len()..]])
        );
    }

    // Small payloads keep their original shape and need no truncation marker.
    let (url, task) = server(vec![response(
        "200 OK",
        "Content-Type: text/plain\r\n",
        "ok",
    )])
    .await;
    let call = executor
        .execute_model(runtime.agent.clone(), "fetch", json!({"url":url}), None)
        .await
        .unwrap();
    task.await.unwrap();
    assert_eq!(
        call.output.value["result"]["body"],
        json!({"kind":"text", "text":"ok"})
    );
    assert!(call.output.value.get("truncated").is_none());
}

#[tokio::test]
async fn default_user_agent_can_be_overridden() {
    let runtime = crate::test_support::TestRuntime::new().await;
    let executor = executor(&runtime);
    let (url, task) = server(vec![response("200 OK", "", ""); 3]).await;
    for headers in [json!({}), json!({"User-Agent":"CustomClient/2"}), json!({})] {
        executor
            .execute(
                runtime.agent.clone(),
                "fetch",
                json!({"url":url,"headers":headers}),
                None,
            )
            .await
            .unwrap();
    }
    let requests = task.await.unwrap();
    for (request, expected) in requests.iter().zip([
        concat!("Skyhook/", env!("CARGO_PKG_VERSION")),
        "CustomClient/2",
        concat!("Skyhook/", env!("CARGO_PKG_VERSION")),
    ]) {
        let values: Vec<_> = request
            .split("\r\n\r\n")
            .next()
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once(':'))
            .filter(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
            .map(|(_, value)| value.trim())
            .collect();
        assert_eq!(values, vec![expected]);
    }
}

#[test]
fn defaults_validation_and_schemas() {
    let a = args(json!({"url":"https://example.org"}));
    validate(&a).unwrap();
    assert_eq!(a.method, "GET");
    assert_eq!(a.timeout, 30);
    assert_eq!(a.connect_timeout, 10);
    assert_eq!(a.max_bytes, DEFAULT_MAX_BYTES);
    assert_eq!(a.max_redirects, 5);
    assert!(!a.insecure);
    assert_eq!(a.redirects, RedirectPolicy::Safe);
    for value in [
        json!({"url":"file:///etc/passwd"}),
        json!({"url":"https://user:secret@example.org"}),
        json!({"url":"http://example.org","method":"bad method"}),
        json!({"url":"http://example.org","max_bytes":0}),
        json!({"url":"http://example.org","max_bytes":MAX_BYTES+1}),
        json!({"url":"http://example.org","timeout":0}),
        json!({"url":"http://example.org","max_redirects":21}),
        json!({"url":"http://example.org","text":true,"save_to":"out"}),
        json!({"url":"http://example.org","text":true,"response_format":"base64"}),
        json!({"url":"http://example.org","headers":{"a":"bad\r\nheader"}}),
        json!({"url":"http://example.org","headers":{"content-length":"2"}}),
        json!({"url":"http://example.org","auth":{"kind":"bearer","token":"a"},"headers":{"Authorization":"b"}}),
    ] {
        assert!(validate(&args(value.clone())).is_err(), "{value}");
    }
    validate(&args(
        json!({"url":"http://example.org","method":"PROPFIND"}),
    ))
    .unwrap();
    let schema = serde_json::to_value(schema_for!(FetchArgs)).unwrap();
    assert_eq!(schema["properties"]["insecure"]["default"], false);
}

#[test]
fn redirect_methods_and_credential_stripping() {
    for status in [301, 302, 303] {
        assert_eq!(redirect_method(status, &Method::POST), (Method::GET, true));
    }
    for status in [307, 308] {
        assert_eq!(
            redirect_method(status, &Method::POST),
            (Method::POST, false)
        );
    }
    assert_eq!(redirect_method(303, &Method::HEAD), (Method::HEAD, false));
    assert_eq!(redirect_method(302, &Method::PUT), (Method::PUT, false));
    let a = args(
        json!({"url":"https://a.example", "headers":{"Authorization":"secret", "X-Api-Key":"custom", "Cookie":"secret", "Host":"wrong", "Content-Type":"text/plain", "Accept":"text/plain"}}),
    );
    let mut headers = request_headers(&a).unwrap();
    strip_redirect_headers(
        &mut headers,
        &parse_url(&a.url).unwrap(),
        &parse_url("https://a.example/next").unwrap(),
        false,
    );
    assert!(headers.contains_key("authorization"));
    assert!(!headers.contains_key("host"));
    strip_redirect_headers(
        &mut headers,
        &parse_url(&a.url).unwrap(),
        &parse_url("https://b.example").unwrap(),
        true,
    );
    for name in ["authorization", "cookie", "x-api-key", "content-type"] {
        assert!(!headers.contains_key(name));
    }
    assert!(headers.contains_key("accept"));
}

#[tokio::test]
async fn http_errors_query_repeated_headers_and_auth() {
    let (url, task) = server(vec![response(
        "404 Not Found",
        "Content-Type: text/plain\r\nSet-Cookie: one=1\r\nSet-Cookie: two=2\r\n",
        "missing",
    )])
    .await;
    let runtime = crate::test_support::TestRuntime::new().await;
    let result = executor(&runtime).execute(runtime.agent.clone(), "fetch", json!({"url":format!("{url}/p?z=0"),"query":[["q","a b"],["q","c"]], "headers":{"x-repeat":["one","two"]},"auth":{"kind":"basic","username":"u","password":"p"}}), None).await.unwrap();
    let output = result.output.value;
    assert_eq!(output["status"], 404);
    assert_eq!(output["ok"], false);
    assert_eq!(output["body"]["text"], "missing");
    assert_eq!(output["headers"]["set-cookie"], json!(["one=1", "two=2"]));
    let requests = task.await.unwrap();
    assert!(requests[0].starts_with("GET /p?z=0&q=a+b&q=c HTTP/1.1"));
    assert!(requests[0].contains("authorization: Basic dTpw"));
    assert!(requests[0].contains("x-repeat: one\r\nx-repeat: two"));
}

#[tokio::test]
async fn safe_manual_follow_and_cross_origin_credentials() {
    let runtime = crate::test_support::TestRuntime::new().await;
    let executor = executor(&runtime);
    for policy in ["safe", "manual"] {
        let (url, task) = server(vec![response(
            "302 Found",
            "Location: /next\r\n",
            "redirect",
        )])
        .await;
        let result = executor.execute(runtime.agent.clone(), "fetch", json!({"url":url,"method":"POST","body":{"kind":"text","value":"payload"},"redirects":policy}), None).await.unwrap();
        assert_eq!(result.output.value["status"], 302);
        assert_eq!(result.output.value["redirects"], json!([]));
        assert!(task.await.unwrap()[0].ends_with("payload"));
    }
    let (end, end_task) = server(vec![response(
        "200 OK",
        "Content-Type: text/plain\r\n",
        "done",
    )])
    .await;
    let (start, start_task) = server(vec![response(
        "303 See Other",
        &format!("Location: {end}/final\r\n"),
        "",
    )])
    .await;
    let result = executor.execute(runtime.agent.clone(), "fetch", json!({"url":start,"method":"POST","body":{"kind":"text","value":"payload"},"redirects":"follow","auth":{"kind":"bearer","token":"secret"},"headers":{"Cookie":"private=1","X-Api-Key":"custom-secret"}}), None).await.unwrap();
    assert_eq!(result.output.value["method"], "GET");
    assert_eq!(
        result.output.value["redirects"].as_array().unwrap().len(),
        1
    );
    assert!(start_task.await.unwrap()[0].contains("authorization: Bearer secret"));
    let request = &end_task.await.unwrap()[0];
    assert!(request.starts_with("GET /final HTTP/1.1"));
    for secret in ["secret", "private=1", "payload", "content-type"] {
        assert!(!request.contains(secret));
    }
}

#[tokio::test]
async fn follow_307_replays_file_and_multipart_uploads() {
    let runtime = crate::test_support::TestRuntime::new().await;
    tokio::fs::write(runtime.root.path().join("upload.txt"), b"file payload")
        .await
        .unwrap();
    let executor = executor(&runtime);
    let (url, task) = server(vec![
        response("307 Temporary Redirect", "Location: /again\r\n", ""),
        response("200 OK", "Content-Type: text/plain\r\n", "done"),
    ])
    .await;
    executor.execute(runtime.agent.clone(), "fetch", json!({"url":url,"method":"PUT","redirects":"follow","body":{"kind":"file","path":"upload.txt"}}), None).await.unwrap();
    for request in task.await.unwrap() {
        assert!(request.starts_with("PUT "));
        assert!(request.ends_with("file payload"));
    }
    let (url, task) = server(vec![response("200 OK", "", "")]).await;
    executor.execute(runtime.agent.clone(), "fetch", json!({"url":url,"method":"POST","body":{"kind":"multipart","parts":[{"name":"a","text":"hello"},{"name":"f","path":"upload.txt","filename":"custom.txt","content_type":"text/plain"},{"name":"b","base64":"d29ybGQ="}]}}), None).await.unwrap();
    let request = &task.await.unwrap()[0];
    for text in [
        "multipart/form-data; boundary=",
        "name=\"a\"",
        "hello",
        "filename=\"custom.txt\"",
        "file payload",
        "world",
    ] {
        assert!(request.contains(text), "missing {text}: {request}");
    }
}

#[tokio::test]
async fn download_atomicity_limits_and_head() {
    let runtime = crate::test_support::TestRuntime::new().await;
    let destination = runtime.root.path().join("out");
    tokio::fs::write(&destination, b"original").await.unwrap();
    let executor = executor(&runtime);
    let oversized = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\n1234\r\n4\r\n5678\r\n0\r\n\r\n".to_owned();
    let (url, task) = server(vec![oversized]).await;
    assert!(
        executor
            .execute(
                runtime.agent.clone(),
                "fetch",
                json!({"url":url,"save_to":"out","overwrite":true,"max_bytes":5}),
                None
            )
            .await
            .is_err()
    );
    task.await.unwrap();
    assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"original");
    let (url, task) = server(vec![response("200 OK", "", "replacement")]).await;
    let result = executor
        .execute(
            runtime.agent.clone(),
            "fetch",
            json!({"url":url,"save_to":"out","overwrite":true}),
            None,
        )
        .await
        .unwrap();
    task.await.unwrap();
    assert_eq!(result.output.value["body"]["kind"], "file");
    assert_eq!(result.output.value["body"]["bytes"], 11);
    assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"replacement");
    assert!(
        executor
            .execute(
                runtime.agent.clone(),
                "fetch",
                json!({"url":"http://127.0.0.1:1","save_to":"out"}),
                None
            )
            .await
            .is_err()
    );
    let (url, task) = server(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\n".into(),
    ])
    .await;
    let result = executor
        .execute(
            runtime.agent.clone(),
            "fetch",
            json!({"url":url,"method":"HEAD","max_bytes":1}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.output.value["received_bytes"], 0);
    task.await.unwrap();
}

#[tokio::test]
async fn binary_charset_and_regular_file_validation() {
    let a = args(json!({"url":"http://example.org"}));
    assert!(
        matches!(response_body(vec![0, 255, 3], Some("application/octet-stream"), &a.url, &a).await.unwrap(), ResponseBody::Base64 { data } if data == "AP8D")
    );
    assert!(
        matches!(response_body(vec![0xe9], Some("text/plain; charset=windows-1252"), &a.url, &a).await.unwrap(), ResponseBody::Text { text, .. } if text == "é")
    );
    let a = args(json!({"url":"http://example.org","text":true}));
    assert!(
        response_body(vec![0, 255], Some("application/pdf"), &a.url, &a)
            .await
            .is_err()
    );
    let root = tempfile::tempdir().unwrap();
    assert!(
        upload_file(root.path(), ".", MAX_UPLOAD_BYTES)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn redirect_limit_and_truncated_response_fail_without_retries() {
    let runtime = crate::test_support::TestRuntime::new().await;
    let executor = executor(&runtime);
    let (url, task) = server(vec![response("302 Found", "Location: /again\r\n", "")]).await;
    assert!(
        executor
            .execute(
                runtime.agent.clone(),
                "fetch",
                json!({"url":url,"max_redirects":0}),
                None
            )
            .await
            .is_err()
    );
    assert_eq!(task.await.unwrap().len(), 1);
    let (url, task) = server(vec![
        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 100\r\n\r\nshort".into(),
    ])
    .await;
    assert!(
        executor
            .execute(runtime.agent.clone(), "fetch", json!({"url":url}), None)
            .await
            .is_err()
    );
    assert_eq!(task.await.unwrap().len(), 1);
}

#[tokio::test]
async fn extraction_failure_preserves_executed_response_metadata() {
    let runtime = crate::test_support::TestRuntime::new().await;
    let (url, task) = server(vec![response(
        "201 Created",
        "Content-Type: application/pdf\r\nX-Result: created\r\n",
        "binary",
    )])
    .await;
    let error = executor(&runtime)
        .execute(
            runtime.agent.clone(),
            "fetch",
            json!({"url":url,"method":"POST","text":true}),
            None,
        )
        .await
        .unwrap_err();
    let output = error
        .into_failure()
        .output
        .expect("failed processing retains HTTP response");
    assert_eq!(output.value["status"], 201);
    assert_eq!(output.value["method"], "POST");
    assert_eq!(output.value["headers"]["x-result"], json!(["created"]));
    assert_eq!(output.value["received_bytes"], 6);
    assert_eq!(task.await.unwrap().len(), 1);
}

#[tokio::test]
async fn file_upload_snapshot_is_bounded_and_immutable() {
    let root = tempfile::tempdir().unwrap();
    tokio::fs::write(root.path().join("source"), b"original")
        .await
        .unwrap();
    assert!(upload_file(root.path(), "source", 3).await.is_err());
    let data = upload_file(root.path(), "source", 100).await.unwrap();
    tokio::fs::write(root.path().join("source"), b"changed")
        .await
        .unwrap();
    let UploadData::File { snapshot, length } = data else {
        panic!("file snapshot expected")
    };
    assert_eq!(length, 8);
    assert_eq!(tokio::fs::read(snapshot.path()).await.unwrap(), b"original");
}

#[path = "extra_tests.rs"]
mod extra_tests;
