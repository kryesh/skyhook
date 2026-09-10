use super::*;
use crate::provider::{
    Provider,
    backends::{NativeProvider, OpenAiApi, anthropic_api, openai_compatible},
    protocol::{Message, ModelRequest, UserContent},
};
use std::{path::Path, time::Duration};
use tokio::{io::AsyncWriteExt, net::TcpListener};

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

fn provider(protocol: Protocol, root: &str, key: Option<String>) -> NativeProvider {
    match protocol {
        Protocol::Chat => openai_compatible("test", root, OpenAiApi::ChatCompletions, key),
        Protocol::Responses => openai_compatible("test", root, OpenAiApi::Responses, key),
        Protocol::Anthropic => anthropic_api("test", root, key),
    }
    .unwrap()
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "test-model".into(),
        system: vec![],
        messages: vec![Message::User(vec![UserContent::Text {
            text: "hello".into(),
        }])],
        tools: vec![],
        response_schema: None,
        reasoning: None,
        max_output_tokens: Some(16),
        correlation: None,
    }
}

async fn listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/v1", listener.local_addr().unwrap());
    (listener, root)
}

// Driven alongside invokes, not by detached server tasks. A surrounding timeout
// drops both sides if a regression prevents a request from arriving.
async fn capture(listener: &TcpListener, count: usize) -> Vec<String> {
    let mut captured = Vec::new();
    for _ in 0..count {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0; 4096];
            let n = socket.read(&mut buffer).await.unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= end + 4 + length {
                    captured.push(headers);
                    break;
                }
            }
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    }
    captured
}

fn assert_credential(headers: &str, protocol: Protocol, key: &str) {
    let (name, value, absent, path) = match protocol {
        Protocol::Chat => (
            "authorization",
            format!("Bearer {key}"),
            "x-api-key",
            "chat/completions",
        ),
        Protocol::Responses => (
            "authorization",
            format!("Bearer {key}"),
            "x-api-key",
            "responses",
        ),
        Protocol::Anthropic => ("x-api-key", key.into(), "authorization", "messages"),
    };
    assert!(headers.starts_with(&format!("POST /v1/{path} HTTP/1.1")));
    let entries: Vec<_> = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .collect();
    let values: Vec<_> = entries
        .iter()
        .filter(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
        .collect();
    assert_eq!(values, vec![value.as_str()]);
    assert!(
        !entries
            .iter()
            .any(|(header, _)| header.eq_ignore_ascii_case(absent))
    );
    if matches!(protocol, Protocol::Anthropic) {
        assert!(entries.iter().any(|(header, value)| {
            header.eq_ignore_ascii_case("anthropic-version") && value.trim() == "2023-06-01"
        }));
    }
}

#[tokio::test]
async fn build_open_context_and_unpolled_or_invalid_invokes_are_lazy() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("executed");
    let provider = provider(Protocol::Chat, "http://127.0.0.1:1/v1", None)
        .with_api_key_command(format!("touch {}; printf secret", quote(&marker)));
    let mut context = provider.clone().open_context("context".into()).unwrap();
    assert!(!marker.exists());
    drop(context.invoke(request()));
    assert!(!marker.exists());
    let mut invalid = request();
    invalid.model.clear();
    assert!(context.invoke(invalid).await.is_err());
    let mut invalid = request();
    invalid.correlation = Some("different-context".into());
    assert!(context.invoke(invalid).await.is_err());
    assert!(!marker.exists());
}

#[tokio::test]
async fn concurrent_contexts_and_clones_share_one_trimmed_sensitive_header() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for protocol in [Protocol::Chat, Protocol::Responses, Protocol::Anthropic] {
            let dir = tempfile::tempdir().unwrap();
            let count = dir.path().join("count");
            let (listener, root) = listener().await;
            let provider = provider(protocol, &root, Some("overridden-direct-key".into()))
                .with_api_key_command(format!(
                    "printf x >> {}; sleep 0.05; printf ' \\t  resolved-key  \\r\\n '",
                    quote(&count)
                ));
            let mut contexts: Vec<_> = (0..8)
                .map(|i| provider.clone().open_context(i.to_string()).unwrap())
                .collect();
            let invoke = async {
                let calls = contexts.iter_mut().map(|context| context.invoke(request()));
                for result in futures_util::future::join_all(calls).await {
                    drop(result.unwrap());
                }
                // Existing and newly opened contexts reuse cached success.
                drop(contexts[0].invoke(request()).await.unwrap());
                let mut later = provider.open_context("later".into()).unwrap();
                drop(later.invoke(request()).await.unwrap());
            };
            let ((), headers) = tokio::join!(invoke, capture(&listener, 10));
            for headers in headers {
                assert_credential(&headers, protocol, "resolved-key");
            }
            assert_eq!(std::fs::read(&count).unwrap(), b"x");
            assert!(
                provider
                    .api_key_command
                    .as_ref()
                    .unwrap()
                    .header(protocol)
                    .await
                    .unwrap()
                    .is_sensitive()
            );
            assert_eq!(std::fs::read(&count).unwrap(), b"x");
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn direct_headers_are_unchanged_without_a_command() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for protocol in [Protocol::Chat, Protocol::Responses, Protocol::Anthropic] {
            let (listener, root) = listener().await;
            let provider = provider(protocol, &root, Some("direct-key".into()));
            let mut context = provider.open_context("test".into()).unwrap();
            let (result, headers) = tokio::join!(context.invoke(request()), capture(&listener, 1));
            drop(result.unwrap());
            assert_credential(&headers[0], protocol, "direct-key");
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn command_failures_are_sanitized_and_can_retry_from_another_clone() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let cases = [
            ("printf private-stdout; printf private-stderr >&2; exit 7", "API key command exited unsuccessfully"),
            ("printf '\\377'", "API key command output was not valid UTF-8"),
            ("printf ' \\t\\r\\n '", "API key command output was empty"),
            ("printf 'private-stdout\\ninvalid-header'", "API key command output was not a valid credential header"),
            ("head -c 65537 /dev/zero", "API key command output exceeded the size limit"),
        ];
        for protocol in [Protocol::Chat, Protocol::Responses, Protocol::Anthropic] {
            for (bad, expected) in cases {
                let dir = tempfile::tempdir().unwrap();
                let marker = quote(&dir.path().join("attempted"));
                let command = format!(
                    "# private-command-text\nif test -e {marker}; then printf retry-key; else touch {marker}; {bad}; fi"
                );
                let resolver = ApiKeyCommand::new(command);
                let clone = resolver.clone();
                let error = resolver.header(protocol).await.unwrap_err();
                assert_eq!(error.kind, ProviderErrorKind::Authentication);
                assert_eq!(error.message, expected);
                for forbidden in ["private-command-text", "private-stdout", "private-stderr", "retry-key"] {
                    assert!(!format!("{error:?} {error}").contains(forbidden));
                }
                assert!(resolver.header.get().is_none());
                let header = clone.header(protocol).await.unwrap();
                let expected = if matches!(protocol, Protocol::Anthropic) {
                    "retry-key"
                } else {
                    "Bearer retry-key"
                };
                assert_eq!(header.to_str().unwrap(), expected);
                assert!(header.is_sensitive());
                assert!(resolver.header.get().is_some());
            }
        }
    }).await.unwrap();
}

#[tokio::test]
async fn command_failure_reaches_invoke_without_http_or_output_disclosure() {
    let provider = provider(Protocol::Chat, "http://127.0.0.1:1/v1", None).with_api_key_command(
        "printf private-output; printf private-error >&2; exit 42 # private-command".into(),
    );
    let mut context = provider.open_context("test".into()).unwrap();
    let error = match context.invoke(request()).await {
        Ok(_) => panic!("failed command unexpectedly invoked HTTP"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert_eq!(error.message, "API key command exited unsuccessfully");
}

#[tokio::test]
async fn command_stdin_is_closed() {
    let resolver =
        ApiKeyCommand::new("if read value; then exit 1; else printf stdin-closed; fi".into());
    let header = tokio::time::timeout(Duration::from_secs(5), resolver.header(Protocol::Anthropic))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(header, "stdin-closed");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelled_invoke_kills_child_and_allows_retry() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pid");
        let quoted = quote(&marker);
        let (listener, root) = listener().await;
        let provider = provider(Protocol::Chat, &root, None).with_api_key_command(format!(
            "if test -e {quoted}; then printf retry-key; else printf '%s' $$ > {quoted}; exec sleep 30; fi"
        ));
        let mut context = provider.open_context("cancelled".into()).unwrap();
        let mut pending = context.invoke(request());
        let pid = tokio::select! {
            _ = &mut pending => panic!("command should still be running"),
            pid = async {
                loop {
                    if let Ok(text) = tokio::fs::read_to_string(&marker).await {
                        if let Ok(pid) = text.parse::<u32>() {
                            break pid;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            } => pid,
        };
        drop(pending);
        loop {
            // Tokio may reap immediately or briefly leave a killed zombie.
            match tokio::fs::read_to_string(format!("/proc/{pid}/stat")).await {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Ok(stat) if stat.split_once(") ").unwrap().1.starts_with('Z') => break,
                _ => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
        assert!(provider.api_key_command.as_ref().unwrap().header.get().is_none());
        let mut retry = provider.clone().open_context("retry".into()).unwrap();
        let (result, headers) = tokio::join!(retry.invoke(request()), capture(&listener, 1));
        drop(result.unwrap());
        assert_credential(&headers[0], Protocol::Chat, "retry-key");
    }).await.unwrap();
}
