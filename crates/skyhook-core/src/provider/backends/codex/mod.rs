//! Skyhook-owned ChatGPT subscription provider. Authentication never imports the
//! official client's credentials. Both transports share the Responses codec.
//! Connection, routing affinity and continuation belong to one context. Only a
//! failed handshake may fall back: the provider never replays after a write attempt.
//! The runtime may explicitly reset/reinvoke an eligible uncommitted response.
pub mod auth;
mod recovery;
mod session;
mod transport;

use super::{common::reasoning_scope, transport as sse};
use crate::provider::{Provider, ProviderContext, ProviderError, ProviderErrorKind};
use session::Context;

pub(super) use transport::is_transport_metadata;

const ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
const WS_ENDPOINT: &str = "wss://chatgpt.com/backend-api/codex/responses";

#[derive(Clone)]
pub struct CodexProvider {
    name: String,
    auth: auth::AuthManager,
    client: reqwest::Client,
    endpoint: String,
    ws_endpoint: String,
}
impl CodexProvider {
    /// No credentials are read and no login is required until invocation.
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            name: "codex".into(),
            auth: auth::AuthManager::new().map_err(auth_error)?,
            client: sse::client()?,
            endpoint: ENDPOINT.into(),
            ws_endpoint: WS_ENDPOINT.into(),
        })
    }

    /// Bind private replay to a configured provider identity, including aliases
    /// using the same Codex endpoint. The default embedding identity is `codex`.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    fn replay_scope(&self) -> String {
        reasoning_scope(&self.name, &self.endpoint)
    }
}
impl Provider for CodexProvider {
    fn open_context(&self, correlation: String) -> Result<Box<dyn ProviderContext>, ProviderError> {
        Ok(Box::new(Context::new(self.clone(), correlation)))
    }
}
fn error(kind: ProviderErrorKind, message: &str) -> ProviderError {
    ProviderError {
        retry_after: None,
        kind,
        message: message.into(),
    }
}
fn auth_error(error: auth::AuthError) -> ProviderError {
    ProviderError {
        retry_after: None,
        kind: ProviderErrorKind::Authentication,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{common::filter_reasoning_scope, responses};
    use super::transport::tests::{reasoning_tool_output, reasoning_tool_request};
    use super::*;

    #[test]
    fn configured_provider_aliases_do_not_share_private_replay() {
        let default = CodexProvider::new().unwrap();
        assert_eq!(default.replay_scope(), reasoning_scope("codex", ENDPOINT));
        let a = default.clone().with_name("alias-a");
        let b = default.with_name("alias-b");
        assert_eq!(a.endpoint, b.endpoint);
        assert_ne!(a.replay_scope(), b.replay_scope());
        let original = reasoning_tool_request(&a.replay_scope());
        let mut same = original.clone();
        filter_reasoning_scope(&mut same, &a.replay_scope());
        assert_eq!(
            responses::encode(&same).unwrap().input[0],
            reasoning_tool_output()[0]
        );
        let mut foreign = original.clone();
        filter_reasoning_scope(&mut foreign, &b.replay_scope());
        let wire = responses::encode(&foreign).unwrap().into_wire();
        assert_eq!(wire["input"].as_array().unwrap().len(), 2);
        assert_eq!(wire["input"][0]["type"], "function_call");
        assert_eq!(wire["input"][1]["type"], "function_call_output");
        assert_eq!(
            responses::encode(&original).unwrap().input[0],
            reasoning_tool_output()[0]
        );
    }
}
