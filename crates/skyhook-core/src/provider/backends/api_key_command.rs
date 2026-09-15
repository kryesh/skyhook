//! Lazy native-provider credentials. Only successful, validated headers are cached.

use super::Protocol;
use crate::provider::{ProviderError, ProviderErrorKind};
use reqwest::header::HeaderValue;
use std::{process::Stdio, sync::Arc};
use tokio::{io::AsyncReadExt, process::Command, sync::OnceCell};

// Credentials should be small; cap even unsuccessful or never-ending output.
const MAX_STDOUT_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub(super) struct ApiKeyCommand {
    command: String,
    header: Arc<OnceCell<HeaderValue>>,
}

impl ApiKeyCommand {
    pub(super) fn new(command: String) -> Self {
        Self {
            command,
            header: Arc::new(OnceCell::new()),
        }
    }

    pub(super) async fn header(&self, protocol: Protocol) -> Result<HeaderValue, ProviderError> {
        self.header
            .get_or_try_init(|| execute(&self.command, protocol))
            .await
            .cloned()
    }
}

fn failure(message: &'static str) -> ProviderError {
    // Never retain the command, output, exit status, or underlying OS error.
    ProviderError {
        retry_after: None,
        kind: ProviderErrorKind::Authentication,
        message: message.into(),
    }
}

async fn execute(command: &str, protocol: Protocol) -> Result<HeaderValue, ProviderError> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| failure("API key command could not be started"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure("API key command output could not be read"))?;
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    stdout
        .take((MAX_STDOUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| failure("API key command output could not be read"))?;
    if bytes.len() > MAX_STDOUT_BYTES {
        return Err(failure("API key command output exceeded the size limit"));
    }
    let status = child
        .wait()
        .await
        .map_err(|_| failure("API key command could not be awaited"))?;
    if !status.success() {
        return Err(failure("API key command exited unsuccessfully"));
    }
    let key = std::str::from_utf8(&bytes)
        .map_err(|_| failure("API key command output was not valid UTF-8"))?
        .trim();
    if key.is_empty() {
        return Err(failure("API key command output was empty"));
    }
    let mut header = match protocol {
        Protocol::Chat { .. } | Protocol::Responses => {
            let bearer = zeroize::Zeroizing::new(format!("Bearer {key}"));
            HeaderValue::from_str(&bearer)
        }
        Protocol::Anthropic => HeaderValue::from_str(key),
    }
    .map_err(|_| failure("API key command output was not a valid credential header"))?;
    header.set_sensitive(true);
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        Provider,
        backends::{
            ChatReasoningReplay, NativeProvider, OpenAiApi, anthropic_api, openai_compatible,
            transport::tests::{read_request, reply},
        },
        protocol::ModelRequest,
    };
    use std::{path::Path, time::Duration};
    use tokio::{io::AsyncWriteExt, net::TcpListener};

    fn chat() -> Protocol {
        Protocol::Chat {
            reasoning_replay: ChatReasoningReplay::default(),
        }
    }

    fn quote(path: &Path) -> String {
        format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
    }

    fn provider(protocol: Protocol, root: &str, key: Option<String>) -> NativeProvider {
        match protocol {
            Protocol::Chat { .. } => {
                openai_compatible("test", root, OpenAiApi::ChatCompletions, key)
            }
            Protocol::Responses => openai_compatible("test", root, OpenAiApi::Responses, key),
            Protocol::Anthropic => anthropic_api("test", root, key),
        }
        .unwrap()
    }

    fn request() -> ModelRequest {
        ModelRequest {
            max_output_tokens: Some(16),
            ..crate::provider::backends::common::tests::request("test-model")
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
            let request = read_request(&mut socket).await;
            captured.push(request.split_once("\r\n\r\n").unwrap().0.to_owned());
            let empty = reply("200 OK", "Content-Type: text/event-stream\r\n", "");
            socket.write_all(empty.as_bytes()).await.unwrap();
        }
        captured
    }

    fn assert_credential(headers: &str, protocol: Protocol, key: &str) {
        let bearer = format!("Bearer {key}");
        let (name, value, absent, path) = match protocol {
            Protocol::Chat { .. } => ("authorization", &*bearer, "x-api-key", "chat/completions"),
            Protocol::Responses => ("authorization", &*bearer, "x-api-key", "responses"),
            Protocol::Anthropic => ("x-api-key", key, "authorization", "messages"),
        };
        assert!(headers.starts_with(&format!("POST /v1/{path} HTTP/1.1")));
        let values = |wanted: &str| {
            let entries = headers.lines().filter_map(|line| line.split_once(':'));
            let matching = entries.filter(|(header, _)| header.eq_ignore_ascii_case(wanted));
            matching.map(|(_, value)| value.trim()).collect::<Vec<_>>()
        };
        assert_eq!(values(name), [value]);
        assert!(values(absent).is_empty());
        if matches!(protocol, Protocol::Anthropic) {
            assert_eq!(values("anthropic-version"), ["2023-06-01"]);
        }
    }

    #[tokio::test]
    async fn build_open_context_and_unpolled_or_invalid_invokes_are_lazy() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("executed");
        let provider = provider(chat(), "http://127.0.0.1:1/v1", None)
            .with_api_key_command(format!("touch {}; printf secret", quote(&marker)))
            .unwrap();
        let mut context = provider.clone().open_context("context".into()).unwrap();
        assert!(!marker.exists());
        drop(context.invoke(request()));
        let mut empty_model = request();
        empty_model.model.clear();
        let mut other_context = request();
        other_context.correlation = Some("different-context".into());
        for invalid in [empty_model, other_context] {
            assert!(context.invoke(invalid).await.is_err());
        }
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn concurrent_contexts_and_clones_share_one_trimmed_sensitive_header() {
        tokio::time::timeout(Duration::from_secs(10), async {
            for protocol in [chat(), Protocol::Responses, Protocol::Anthropic] {
                let dir = tempfile::tempdir().unwrap();
                let count = dir.path().join("count");
                let (listener, root) = listener().await;
                let provider = provider(protocol, &root, Some("overridden-direct-key".into()))
                    .with_api_key_command(format!(
                        "printf x >> {}; sleep 0.05; printf ' \\t  resolved-key  \\r\\n '",
                        quote(&count)
                    ))
                    .unwrap();
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
                let command = provider.api_key_command.as_ref().unwrap();
                assert!(command.header(protocol).await.unwrap().is_sensitive());
                assert_eq!(std::fs::read(&count).unwrap(), b"x");
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn direct_headers_are_unchanged_without_a_command() {
        tokio::time::timeout(Duration::from_secs(10), async {
            for protocol in [chat(), Protocol::Responses, Protocol::Anthropic] {
                let (listener, root) = listener().await;
                let provider = provider(protocol, &root, Some("direct-key".into()));
                let mut context = provider.open_context("test".into()).unwrap();
                let (result, headers) =
                    tokio::join!(context.invoke(request()), capture(&listener, 1));
                drop(result.unwrap());
                assert_credential(&headers[0], protocol, "direct-key");
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn command_failures_are_sanitized_and_can_retry_from_another_clone() {
        let cases = [
            (
                "printf private-stdout; printf private-stderr >&2; exit 7",
                "API key command exited unsuccessfully",
            ),
            (
                "printf '\\377'",
                "API key command output was not valid UTF-8",
            ),
            ("printf ' \\t\\r\\n '", "API key command output was empty"),
            (
                "printf 'private-stdout\\ninvalid-header'",
                "API key command output was not a valid credential header",
            ),
            (
                "head -c 65537 /dev/zero",
                "API key command output exceeded the size limit",
            ),
        ];
        let run = async {
            for protocol in [chat(), Protocol::Responses, Protocol::Anthropic] {
                for (bad, expected) in cases {
                    let dir = tempfile::tempdir().unwrap();
                    let marker = quote(&dir.path().join("attempted"));
                    let resolver = ApiKeyCommand::new(format!(
                        "# private-command-text\nif test -e {marker}; then printf retry-key; else touch {marker}; {bad}; fi"
                    ));
                    let clone = resolver.clone();
                    let error = resolver.header(protocol).await.unwrap_err();
                    assert_eq!(error.kind, ProviderErrorKind::Authentication);
                    assert_eq!(error.message, expected);
                    let rendered = format!("{error:?} {error}");
                    for forbidden in [
                        "private-command-text",
                        "private-stdout",
                        "private-stderr",
                        "retry-key",
                    ] {
                        assert!(!rendered.contains(forbidden));
                    }
                    assert!(resolver.header.get().is_none());
                    let header = clone.header(protocol).await.unwrap();
                    let expected = match protocol {
                        Protocol::Anthropic => "retry-key",
                        _ => "Bearer retry-key",
                    };
                    assert_eq!(header.to_str().unwrap(), expected);
                    assert!(header.is_sensitive());
                    assert!(resolver.header.get().is_some());
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn command_failure_reaches_invoke_without_http_or_output_disclosure() {
        let command = "printf private-output; printf private-error >&2; exit 42 # private-command";
        let provider = provider(chat(), "http://127.0.0.1:1/v1", None)
            .with_api_key_command(command.into())
            .unwrap();
        let mut context = provider.open_context("test".into()).unwrap();
        let Err(error) = context.invoke(request()).await else {
            panic!("failed command unexpectedly invoked HTTP")
        };
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        assert_eq!(error.message, "API key command exited unsuccessfully");
    }

    #[tokio::test]
    async fn command_stdin_is_closed() {
        let resolver =
            ApiKeyCommand::new("if read value; then exit 1; else printf stdin-closed; fi".into());
        let header =
            tokio::time::timeout(Duration::from_secs(5), resolver.header(Protocol::Anthropic))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(header, "stdin-closed");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancelled_invoke_kills_child_and_allows_retry() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pid");
        let quoted = quote(&marker);
        let (listener, root) = listener().await;
        let provider = provider(chat(), &root, None)
            .with_api_key_command(format!(
                "if test -e {quoted}; then printf retry-key; else printf '%s' $$ > {quoted}; exec sleep 30; fi"
            ))
            .unwrap();
        let run = async {
            let mut context = provider.open_context("cancelled".into()).unwrap();
            let mut pending = context.invoke(request());
            let pid = tokio::select! {
                _ = &mut pending => panic!("command should still be running"),
                pid = async {
                    loop {
                        if let Ok(text) = tokio::fs::read_to_string(&marker).await
                            && let Ok(pid) = text.parse::<u32>()
                        {
                            break pid;
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
            assert!(
                provider
                    .api_key_command
                    .as_ref()
                    .unwrap()
                    .header
                    .get()
                    .is_none()
            );
            let mut retry = provider.clone().open_context("retry".into()).unwrap();
            let (result, headers) = tokio::join!(retry.invoke(request()), capture(&listener, 1));
            drop(result.unwrap());
            assert_credential(&headers[0], chat(), "retry-key");
        };
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .unwrap();
    }
}
