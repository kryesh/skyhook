//! Classify connection loss without granting replay eligibility to protocol or auth errors.
use super::error;
use crate::provider::{CodexWebSocketError, ProviderError, ProviderErrorKind};
use tokio_tungstenite::tungstenite::{self, protocol::frame::coding::CloseCode};

pub(super) fn websocket_error(category: CodexWebSocketError) -> ProviderError {
    let message = match category {
        CodexWebSocketError::EndOfStream => "Codex WebSocket EOF before completion",
        CodexWebSocketError::Closed => "Codex WebSocket closed before completion",
        CodexWebSocketError::Read => "Codex WebSocket read failed",
        CodexWebSocketError::ReadTimeout => "Codex WebSocket read timed out",
        CodexWebSocketError::Ping => "Codex WebSocket ping response failed",
        CodexWebSocketError::Write => "Codex WebSocket write failed",
        CodexWebSocketError::WriteTimeout => "Codex WebSocket write timed out",
    };
    error(ProviderErrorKind::CodexWebSocket(category), message)
}

pub(super) fn socket_error(
    native: tungstenite::Error,
    operation: CodexWebSocketError,
) -> ProviderError {
    use tungstenite::{Error, error::ProtocolError};
    match native {
        Error::ConnectionClosed | Error::AlreadyClosed => {
            websocket_error(CodexWebSocketError::Closed)
        }
        Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => {
            websocket_error(CodexWebSocketError::EndOfStream)
        }
        Error::Io(_) | Error::Tls(_) => websocket_error(operation),
        // Do not turn malformed frames, payloads, or local request errors into
        // replay eligibility. In particular, never include native error text.
        _ => ProviderError::protocol("Codex WebSocket protocol failure"),
    }
}

pub(super) fn close_error(code: Option<CloseCode>) -> ProviderError {
    match code {
        Some(
            CloseCode::Protocol
            | CloseCode::Unsupported
            | CloseCode::Invalid
            | CloseCode::Size
            | CloseCode::Extension,
        ) => ProviderError::protocol("Codex WebSocket rejected protocol or payload"),
        Some(CloseCode::Policy) => error(
            ProviderErrorKind::Authentication,
            "Codex WebSocket policy rejection",
        ),
        None
        | Some(
            CloseCode::Normal
            | CloseCode::Away
            | CloseCode::Status
            | CloseCode::Abnormal
            | CloseCode::Restart
            | CloseCode::Again
            | CloseCode::Error,
        ) => websocket_error(CodexWebSocketError::Closed),
        // Unknown application codes can represent permanent authentication or
        // request rejection. Only allowlisted connection-loss codes are eligible.
        _ => error(
            ProviderErrorKind::Response,
            "Codex WebSocket application rejection",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderRecovery;
    use tungstenite::error::ProtocolError;

    #[test]
    fn codex_errors_are_classified_and_sanitized() {
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
        assert_eq!(
            close_error(Some(CloseCode::from(4001))).recovery(),
            Some(ProviderRecovery::ResetContext)
        );
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
}
