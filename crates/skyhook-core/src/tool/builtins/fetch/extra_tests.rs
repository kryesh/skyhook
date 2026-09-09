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

#[tokio::test]
async fn multipart_upload_contains_text_file_and_binary_parts() {
    let runtime = crate::test_support::TestRuntime::new().await;
    tokio::fs::write(runtime.root.path().join("source.txt"), b"snapshot bytes")
        .await
        .unwrap();
    let (url, task) = server(vec![response("200 OK", "", "")]).await;
    executor(&runtime).execute(runtime.agent.clone(), "fetch", json!({
        "url":url,"method":"POST","body":{"kind":"multipart","parts":[
            {"name":"description","text":"some text"},
            {"name":"file","path":"source.txt","filename":"chosen.txt","content_type":"text/plain"},
            {"name":"binary","base64":"YmluYXJ5","filename":"data.bin","content_type":"application/octet-stream"}
        ]}
    }), None).await.unwrap();
    let requests = task.await.unwrap();
    let request = &requests[0];
    assert!(request.contains("multipart/form-data; boundary="));
    for expected in [
        "name=\"description\"",
        "some text",
        "filename=\"chosen.txt\"",
        "snapshot bytes",
        "filename=\"data.bin\"",
        "binary",
        "application/octet-stream",
    ] {
        assert!(request.contains(expected), "missing {expected}: {request}");
    }
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
        socket.read(&mut buffer).await.unwrap();
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
