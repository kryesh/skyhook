use super::*;
use futures_util::StreamExt;

async fn server(replies: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/responses", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut buffer = [0u8; 1024];
                let n = socket.read(&mut buffer).await.unwrap();
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|s| s.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8(bytes).unwrap());
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
        requests
    });
    (url, task)
}
fn reply(status: &str, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",
        body.len()
    )
}
#[tokio::test]
async fn wire_headers_body_and_eof() {
    let (url, task) = server(vec![reply(
        "200 OK",
        "Content-Type: text/event-stream; charset=utf-8\r\n",
        "data: one\r\n\r\ndata: two",
    )])
    .await;
    let body = serde_json::json!({"model":"literal-model","stream":true});
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer test-key".parse().unwrap());
    let events = post_sse(&client().unwrap(), &url, headers, &body)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        events.into_iter().collect::<Result<Vec<_>, _>>().unwrap(),
        vec![
            SseEvent {
                event: None,
                data: "one".into()
            },
            SseEvent {
                event: None,
                data: "two".into()
            }
        ]
    );
    let requests = task.await.unwrap();
    assert!(requests[0].contains("authorization: Bearer test-key"));
    assert!(requests[0].contains("accept: text/event-stream"));
    assert_eq!(
        serde_json::from_str::<Value>(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap(),
        body
    );
}
#[tokio::test]
async fn only_clear_rejection_retried() {
    let (url, task) = server(vec![
        reply("429 Too Many Requests", "Retry-After: 0\r\n", ""),
        reply(
            "200 OK",
            "Content-Type: text/event-stream\r\n",
            "data: done\n\n",
        ),
    ])
    .await;
    drop(
        post_sse(&client().unwrap(), &url, HeaderMap::new(), &Value::Null)
            .await
            .unwrap(),
    );
    assert_eq!(task.await.unwrap().len(), 2);
    for status in [
        "502 Bad Gateway",
        "503 Service Unavailable",
        "504 Gateway Timeout",
    ] {
        let (url, task) = server(vec![reply(status, "", "secret-token")]).await;
        let error = post_sse(&client().unwrap(), &url, HeaderMap::new(), &Value::Null)
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error.kind,
            ProviderErrorKind::Response | ProviderErrorKind::Timeout
        ));
        assert!(!error.message.contains("secret-token"));
        assert_eq!(task.await.unwrap().len(), 1);
    }
}
#[tokio::test]
async fn long_retry_after_and_codex_once_do_not_retry() {
    for (once, wait) in [
        (false, "60"),
        (false, "Wed, 21 Oct 2030 07:28:00 GMT"),
        (true, "0"),
    ] {
        let (url, task) = server(vec![reply(
            "429 Too Many Requests",
            &format!("Retry-After: {wait}\r\n"),
            "reflected secret",
        )])
        .await;
        let client = client().unwrap();
        let result = if once {
            post_sse_once(&client, &url, HeaderMap::new(), &Value::Null).await
        } else {
            post_sse(&client, &url, HeaderMap::new(), &Value::Null).await
        };
        assert_eq!(result.err().unwrap().kind, ProviderErrorKind::RateLimited);
        assert_eq!(task.await.unwrap().len(), 1);
    }
}
#[tokio::test]
async fn body_failure_after_output_is_not_replayed() {
    let wire = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 1000\r\nConnection: close\r\n\r\ndata: first\n\n".to_owned();
    let (url, task) = server(vec![wire]).await;
    let mut events = post_sse(&client().unwrap(), &url, HeaderMap::new(), &Value::Null)
        .await
        .unwrap();
    assert_eq!(events.next().await.unwrap().unwrap().data, "first");
    assert!(events.next().await.unwrap().is_err());
    assert!(events.next().await.is_none());
    assert_eq!(task.await.unwrap().len(), 1);
}
