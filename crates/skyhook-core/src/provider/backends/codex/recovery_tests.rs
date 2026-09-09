use super::*;
use crate::provider::{ProviderRecovery, protocol::UserContent};
use tungstenite::error::ProtocolError;

#[test]
fn recovery_is_structured_codex_only_and_sanitized() {
    for category in [
        CodexWebSocketError::EndOfStream,
        CodexWebSocketError::Closed,
        CodexWebSocketError::Read,
        CodexWebSocketError::ReadTimeout,
        CodexWebSocketError::Ping,
        CodexWebSocketError::Write,
        CodexWebSocketError::WriteTimeout,
    ] {
        let error = websocket_error(category);
        assert_eq!(error.kind, ProviderErrorKind::CodexWebSocket(category));
        assert_eq!(error.recovery(), Some(ProviderRecovery::ResetContext));
    }
    for kind in [
        ProviderErrorKind::Authentication,
        ProviderErrorKind::RateLimited,
        ProviderErrorKind::Timeout,
        ProviderErrorKind::Transport,
        ProviderErrorKind::Protocol,
        ProviderErrorKind::InvalidRequest,
        ProviderErrorKind::ContextWindowExceeded,
        ProviderErrorKind::Response,
    ] {
        // Even an identical message cannot grant replay eligibility.
        assert_eq!(error(kind, "Codex WebSocket read failed").recovery(), None);
    }
    for operation in [
        CodexWebSocketError::Read,
        CodexWebSocketError::Write,
        CodexWebSocketError::Ping,
    ] {
        let native = tungstenite::Error::Io(std::io::Error::other(
            "Bearer SECRET wss://user:password@example.invalid/private?token=SECRET",
        ));
        let error = socket_error(native, operation);
        assert_eq!(error.kind, ProviderErrorKind::CodexWebSocket(operation));
        assert!(!format!("{error:?} {error}").contains("SECRET"));
        assert!(!error.message.contains("example.invalid"));
    }
    assert_eq!(
        socket_error(
            tungstenite::Error::Protocol(ProtocolError::ResetWithoutClosingHandshake),
            CodexWebSocketError::Read
        )
        .kind,
        ProviderErrorKind::CodexWebSocket(CodexWebSocketError::EndOfStream),
    );
    assert_eq!(
        socket_error(
            tungstenite::Error::Protocol(ProtocolError::UnmaskedFrameFromClient),
            CodexWebSocketError::Read
        )
        .recovery(),
        None,
    );
    for code in [
        CloseCode::Protocol,
        CloseCode::Unsupported,
        CloseCode::Invalid,
        CloseCode::Size,
        CloseCode::Extension,
        CloseCode::Policy,
    ] {
        assert_eq!(close_error(Some(code)).recovery(), None);
    }
    assert_eq!(
        close_error(Some(CloseCode::from(4001))).kind,
        ProviderErrorKind::Response
    );
    assert_eq!(close_error(Some(CloseCode::from(4001))).recovery(), None);
    for code in [
        None,
        Some(CloseCode::Normal),
        Some(CloseCode::Away),
        Some(CloseCode::Restart),
        Some(CloseCode::Again),
        Some(CloseCode::Error),
    ] {
        assert_eq!(
            close_error(code).recovery(),
            Some(ProviderRecovery::ResetContext)
        );
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "gpt-5".into(),
        system: vec![],
        messages: vec![ProtocolMessage::User(vec![UserContent::Text {
            text: "committed history".into(),
        }])],
        tools: vec![],
        response_schema: None,
        reasoning: None,
        max_output_tokens: None,
        correlation: Some("context".into()),
    }
}

fn provider(address: std::net::SocketAddr, directory: &std::path::Path) -> CodexProvider {
    CodexProvider {
        name: "codex".into(),
        auth: auth::test_manager(directory.to_owned()),
        client: transport::client().unwrap(),
        endpoint: format!("http://{address}/responses"),
        ws_endpoint: format!("ws://{address}/responses"),
    }
}

async fn read_request<S>(socket: &mut WebSocketStream<S>) -> Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match socket.next().await.unwrap().unwrap() {
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            frame => panic!("unexpected test frame: {frame:?}"),
        }
    }
}

async fn completed<S>(socket: &mut WebSocketStream<S>, id: &str)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    socket.send(Message::Text(json!({"type":"response.completed", "response":{"id":id,"status":"completed","output":[]}}).to_string().into())).await.unwrap();
}

#[tokio::test]
// Tungstenite's handshake callback requires its unboxed HTTP ErrorResponse.
#[allow(clippy::result_large_err)]
async fn runtime_reinvoke_uses_fresh_socket_full_history_without_affinity_or_hidden_replay() {
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
    // Run both explicit runtime reset and provider-only failure retirement.
    for explicit_reset in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (retired_tx, retired_rx) = tokio::sync::oneshot::channel();
        let (retry_tx, retry_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(
                tcp,
                |request: &Request, mut response: Response| {
                    assert!(!request.headers().contains_key("x-codex-turn-state"));
                    response.headers_mut().insert(
                        "x-codex-turn-state",
                        HeaderValue::from_static("connection-secret-affinity"),
                    );
                    Ok(response)
                },
            )
            .await
            .unwrap();
            let first = read_request(&mut socket).await;
            completed(&mut socket, "committed-response").await;
            let second = read_request(&mut socket).await;
            assert_eq!(second["previous_response_id"], "committed-response");
            assert_eq!(second["input"], json!([]));
            // Start an uncommitted response, then lose the connection abruptly.
            socket.send(Message::Text(json!({"type":"response.output_item.added", "output_index":0,
                "item":{"id":"partial","type":"function_call","call_id":"not-executed","name":"local_tool","arguments":""}}).to_string().into())).await.unwrap();
            drop(socket);
            // There must be no provider-owned request replay while the runtime
            // is deciding whether this response is safe to retry.
            assert!(
                tokio::time::timeout(Duration::from_millis(30), listener.accept())
                    .await
                    .is_err()
            );
            retired_tx.send(()).unwrap();
            retry_rx.await.unwrap();
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(
                tcp,
                |request: &Request, response: Response| {
                    assert!(!request.headers().contains_key("x-codex-turn-state"));
                    Ok(response)
                },
            )
            .await
            .unwrap();
            let third = read_request(&mut socket).await;
            assert!(third.get("previous_response_id").is_none());
            assert_eq!(third["input"], first["input"]);
            assert!(!third.to_string().contains("not-executed"));
            completed(&mut socket, "retried-response").await;
        });
        let directory = tempfile::tempdir().unwrap();
        let provider = provider(address, directory.path());
        let mut context = provider.open_context("context".into()).unwrap();
        let first = context
            .invoke(request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(first.iter().all(Result::is_ok), "{first:?}");
        let failed = context
            .invoke(request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let error = failed.last().unwrap().as_ref().unwrap_err();
        assert_eq!(error.recovery(), Some(ProviderRecovery::ResetContext));
        retired_rx.await.unwrap();
        if explicit_reset {
            context.reset();
        }
        retry_tx.send(()).unwrap();
        let final_chunks = context
            .invoke(request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(final_chunks.iter().all(Result::is_ok), "{final_chunks:?}");
        server.await.unwrap();
    }
}

#[tokio::test]
async fn reset_detaches_even_a_locked_session_and_clears_http_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let provider = provider("127.0.0.1:1".parse().unwrap(), directory.path());
    let old = Arc::new(Mutex::new(Session {
        continuation: Some(Continuation {
            id: "old".into(),
            input: vec![],
            settings: json!({}),
        }),
        affinity: Some(HeaderValue::from_static("old-affinity")),
        http_only: true,
        ..Session::default()
    }));
    let mut context = Context {
        provider,
        correlation: "context".into(),
        session: old.clone(),
    };
    let _old_guard = old.lock().await;
    context.reset();
    assert!(!Arc::ptr_eq(&context.session, &old));
    let session = context.session.try_lock().unwrap();
    assert!(session.socket.is_none());
    assert!(session.reusable_since.is_none());
    assert!(session.continuation.is_none());
    assert!(session.affinity.is_none());
    assert!(!session.http_only);
}

#[tokio::test]
async fn read_timeout_is_eligible_and_retires_connection_state() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let _socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        stop_rx.await.unwrap();
    });
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
        .await
        .unwrap();
    let session = Arc::new(Mutex::new(Session {
        affinity: Some(HeaderValue::from_static("secret-affinity")),
        ..Session::default()
    }));
    let mut response = ws_stream(
        socket,
        session.clone().lock_owned().await,
        responses::Decoder::codex("gpt-5".into()),
        json!({}),
        vec![],
        "scope".into(),
    );
    tokio::time::pause();
    let error = response.next().await.unwrap().unwrap_err();
    assert_eq!(
        error.kind,
        ProviderErrorKind::CodexWebSocket(CodexWebSocketError::ReadTimeout)
    );
    assert_eq!(error.recovery(), Some(ProviderRecovery::ResetContext));
    drop(response);
    let session = session.lock().await;
    assert!(session.socket.is_none());
    assert!(session.continuation.is_none());
    assert!(session.affinity.is_none());
    stop_tx.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn protocol_authentication_abort_and_cancel_events_never_enable_recovery() {
    for (frame, kind) in [
        (Message::Text("not JSON SECRET".into()), ProviderErrorKind::Protocol),
        (Message::Binary(b"SECRET".to_vec().into()), ProviderErrorKind::Protocol),
        (Message::Text(json!({"type":"error","error":{"code":"invalid_api_key","message":"Bearer SECRET"}}).to_string().into()), ProviderErrorKind::Authentication),
        (Message::Text(json!({"type":"response.failed","response":{"error":{"code":"request_cancelled","message":"SECRET"}}}).to_string().into()), ProviderErrorKind::Response),
        (Message::Text(json!({"type":"response.aborted","message":"SECRET"}).to_string().into()), ProviderErrorKind::Protocol),
        (Message::Close(Some(tungstenite::protocol::CloseFrame { code: CloseCode::from(4001), reason: "Bearer SECRET".into() })), ProviderErrorKind::Response),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            socket.send(frame).await.unwrap();
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}")).await.unwrap();
        let session = Arc::new(Mutex::new(Session {
            affinity: Some(HeaderValue::from_static("SECRET")),
            ..Session::default()
        }));
        let chunks = ws_stream(socket, session.clone().lock_owned().await,
            responses::Decoder::codex("gpt-5".into()), json!({}), vec![], "scope".into())
            .collect::<Vec<_>>().await;
        let error = chunks.last().unwrap().as_ref().unwrap_err();
        assert_eq!(error.kind, kind);
        assert_eq!(error.recovery(), None);
        assert!(!format!("{error:?} {error}").contains("SECRET"));
        let session = session.lock().await;
        assert!(session.socket.is_none());
        assert!(session.continuation.is_none());
        assert!(session.affinity.is_none());
        server.await.unwrap();
    }
}
