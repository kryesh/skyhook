//! Skyhook-owned ChatGPT subscription provider over HTTP/SSE, sharing the Responses
//! codec. Authentication never imports the official client's credentials.
pub mod auth;

use super::{
    common::{filter_reasoning_scope, reasoning_scope},
    responses, transport,
};
use crate::provider::{
    Provider, ProviderContext, ProviderError, ProviderErrorKind, ResponseStream,
    protocol::{ContextId, Message, ModelRequest, Scope, UserContent},
};
use futures_util::{TryStreamExt, stream};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

const ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";

#[derive(Clone)]
pub struct CodexProvider {
    name: String,
    auth: auth::AuthManager,
    client: reqwest::Client,
    endpoint: String,
}
impl CodexProvider {
    /// No credentials are read and no login is required until invocation.
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            name: "codex".into(),
            auth: auth::AuthManager::new().map_err(auth_error)?,
            client: transport::client()?,
            endpoint: ENDPOINT.into(),
        })
    }

    /// Bind private replay to a configured provider identity, including aliases
    /// using the same Codex endpoint. The default embedding identity is `codex`.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    fn replay_scope(&self) -> Scope {
        reasoning_scope(&self.name, &self.endpoint)
    }
}
impl Provider for CodexProvider {
    fn open_context(&self, context: ContextId) -> Result<Box<dyn ProviderContext>, ProviderError> {
        Ok(Box::new(Context {
            provider: self.clone(),
            context,
            routing: Default::default(),
        }))
    }
}

struct Context {
    provider: CodexProvider,
    context: ContextId,
    /// The service's sticky-routing token: received on a turn's first response and
    /// replayed on that turn's later requests, never on another turn's.
    routing: std::sync::Arc<std::sync::Mutex<Option<HeaderValue>>>,
}

const TURN_STATE: &str = "x-codex-turn-state";

/// Whether the request answers tool calls, rather than opening a turn with user
/// input or a wake. Runtime state after the results is part of the same request.
fn continues_turn(request: &ModelRequest) -> bool {
    let runtime_only = |message: &&Message| matches!(message, Message::User(parts) if parts.iter().all(UserContent::is_runtime));
    let mut history = request.history.iter().rev().skip_while(runtime_only);
    matches!(history.next(), Some(Message::Tool(_)))
}

impl ProviderContext for Context {
    fn invoke(&mut self, mut request: ModelRequest) -> ResponseStream {
        let provider = self.provider.clone();
        let context = self.context.clone();
        let routing = self.routing.clone();
        let started = async move {
            let scope = provider.replay_scope();
            filter_reasoning_scope(&mut request, &scope);
            let mut body = responses::encode(&request, &context)?;
            adapt_subscription_request(&mut body);
            let credentials = provider.auth.credentials().await.map_err(auth_error)?;
            let mut headers =
                auth_headers(&credentials.access_token, &credentials.account_id, &context)?;
            {
                let mut token = routing.lock().expect("routing lock");
                if !continues_turn(&request) {
                    *token = None;
                }
                if let Some(token) = &*token {
                    headers.insert(TURN_STATE, token.clone());
                }
            }
            let (response, events) =
                transport::post_sse(&provider.client, &provider.endpoint, headers, &body).await?;
            if let Some(received) = response.get(TURN_STATE) {
                let mut token = routing.lock().expect("routing lock");
                token.get_or_insert_with(|| received.clone());
            }
            Ok::<_, ProviderError>(super::decode_stream(
                events,
                super::Decoder::Codex(responses::Decoder::codex(request.model, scope)),
            ))
        };
        Box::pin(stream::once(started).try_flatten())
    }
}

fn auth_headers(
    token: &str,
    account: &str,
    context: &ContextId,
) -> Result<HeaderMap, ProviderError> {
    let sensitive = |value: &str, message| {
        let mut value = HeaderValue::from_str(value)
            .map_err(|_| error(ProviderErrorKind::Authentication, message))?;
        value.set_sensitive(true);
        Ok::<_, ProviderError>(value)
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        sensitive(&format!("Bearer {token}"), "invalid Codex access token")?,
    );
    headers.insert(
        "chatgpt-account-id",
        sensitive(account, "invalid Codex account identifier")?,
    );
    headers.insert("originator", HeaderValue::from_static("skyhook"));
    // The service derives cache affinity from this header.
    headers.insert(
        "session-id",
        HeaderValue::from_str(context.as_str()).map_err(|_| {
            error(
                ProviderErrorKind::InvalidRequest,
                "invalid context identity header",
            )
        })?,
    );
    Ok(headers)
}

/// Subscription wire constraints; all other settings survive unchanged.
fn adapt_subscription_request(body: &mut Value) {
    body["store"] = json!(false);
    body["include"] = json!(["reasoning.encrypted_content"]);
    let body = body.as_object_mut().expect("encoded request is an object");
    body.entry("instructions").or_insert_with(|| json!(""));
    // The subscription service does not accept an output-token limit; the
    // profile value remains a local context budget only.
    body.remove("max_output_tokens");
}

// Subscription quota notifications are transport metadata, not Responses output.
// Recognize only the observed types; unknown semantic events still fail.
pub(super) fn is_transport_metadata(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("codex.rate_limits" | "codex.response.metadata" | "responsesapi.websocket_timing")
    )
}

fn error(kind: ProviderErrorKind, message: &str) -> ProviderError {
    ProviderError {
        kind,
        message: message.into(),
    }
}
fn auth_error(native: auth::AuthError) -> ProviderError {
    error(ProviderErrorKind::Authentication, &native.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::common::tests::request as base_request;
    use super::super::transport::tests::{Plan, Server, reply};
    use super::*;
    use crate::provider::{
        backends::common::tests::reduce,
        protocol::{AssistantItem, Message, Outcome, ToolCall, ToolResult},
    };
    use futures_util::StreamExt;

    fn reasoning_item() -> Value {
        json!({"type":"reasoning", "id":"rs_private", "summary":[],
            "encrypted_content":"opaque+/=", "future_native":{"state":"keep"}})
    }

    fn reasoning_tool_request(scope: &Scope) -> ModelRequest {
        let replay = super::super::common::replay(
            "responses",
            "gpt-5",
            scope,
            reasoning_item(),
            crate::provider::protocol::Binding::Free,
        );
        let AssistantItem::Reasoning {
            replay: Some(replay),
            ..
        } = AssistantItem::reasoning("rs_private", 0, "", Some(replay))
        else {
            unreachable!()
        };
        // Native reasoning without a readable summary replays from its envelope alone.
        let reasoning = AssistantItem::Reasoning {
            id: crate::provider::protocol::ItemId::try_from("rs_private".to_owned()).unwrap(),
            position: 0.into(),
            blocks: Vec::new(),
            replay: Some(replay),
        };
        ModelRequest {
            history: vec![
                Message::Assistant(vec![
                    reasoning,
                    AssistantItem::tool_call(
                        "fc_1",
                        1,
                        ToolCall::new("call_1", "inspect", json!({"path":"test"})).unwrap(),
                    ),
                ]),
                Message::Tool(vec![ToolResult {
                    call_id: "call_1".into(),
                    name: "inspect".into(),
                    result: json!({"ok":true}),
                    images: vec![],
                    is_error: false,
                }]),
            ],
            max_output_tokens: Some(100),
            ..base_request("gpt-5")
        }
    }

    #[test]
    fn subscription_adaptation_preserves_opaque_settings_and_history() {
        let mut body = json!({"model":"gpt-5", "input":[{"keep":1}], "store":true,
            "include":["other"], "max_output_tokens":100, "instructions":"keep instructions",
            "vendor_options":{"nested":[null, false, {"opaque":"keep"}]}});
        let mut expected = body.clone();
        expected["store"] = json!(false);
        expected["include"] = json!(["reasoning.encrypted_content"]);
        expected
            .as_object_mut()
            .unwrap()
            .remove("max_output_tokens");
        adapt_subscription_request(&mut body);
        assert_eq!(body, expected);

        body.as_object_mut().unwrap().remove("instructions");
        adapt_subscription_request(&mut body);
        assert_eq!(body["instructions"], "");
    }

    #[test]
    fn credentials_are_sensitive_headers() {
        let session = "session".parse::<ContextId>().unwrap();
        let headers = auth_headers("secret", "account", &session).unwrap();
        assert!(headers["authorization"].is_sensitive());
        assert!(headers["chatgpt-account-id"].is_sensitive());
        assert!(auth_headers("bad\ntoken", "account", &session).is_err());
    }

    #[tokio::test]
    async fn invoke_posts_adapted_full_history_and_ignores_transport_metadata() {
        let message = json!({"type":"message", "id":"msg_1", "status":"completed", "role":"assistant",
            "content":[{"type":"output_text","text":"OK","annotations":[]}]});
        let body: String = [
            json!({"type":"codex.rate_limits","rate_limits":{}}),
            json!({"type":"codex.response.metadata","metadata":{}}),
            json!({"type":"responsesapi.websocket_timing","timing":{}}),
            json!({"type":"response.output_item.added","output_index":0,"item":message}),
            json!({"type":"response.output_item.done","output_index":0,"item":message}),
            // Codex omits terminal output; streamed items remain authoritative.
            json!({"type":"response.completed","response":{"id":"r1","status":"completed","output":[]}}),
        ]
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
        let sse = reply("200 OK", "Content-Type: text/event-stream\r\n", &body);
        let server = Server::start(vec![Plan::reply(sse)]).await;
        let directory = tempfile::tempdir().unwrap();
        let provider = CodexProvider {
            name: "codex".into(),
            auth: auth::test_manager(directory.path().to_owned()),
            client: transport::client().unwrap(),
            endpoint: server.url.clone(),
        };
        let mut context = provider.open_context("context".parse().unwrap()).unwrap();
        let request = reasoning_tool_request(&provider.replay_scope());
        let events: Vec<_> = context.invoke(request).collect().await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let reduced = reduce(events.into_iter().map(Result::unwrap));
        assert_eq!(reduced.completion.outcome(), Outcome::Answer);
        assert_eq!(reduced.items()[0].text_content().as_deref(), Some("OK"));

        let requests = server.finish().await;
        assert!(requests[0].starts_with("POST "));
        assert!(requests[0].contains("Bearer test-access-token"));
        assert!(requests[0].contains("session-id: context"));
        let body: Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["instructions"], "");
        assert_eq!(body["input"][0], reasoning_item());
        assert_eq!(body["input"][1]["call_id"], body["input"][2]["call_id"]);
    }

    #[tokio::test]
    async fn turn_state_is_replayed_within_its_turn_only() {
        let done = json!({"type":"response.completed","response":{"id":"r","status":"completed","output":[]}});
        let reply_with = |token: &str| {
            let headers =
                format!("Content-Type: text/event-stream\r\nx-codex-turn-state: {token}\r\n");
            Plan::reply(reply("200 OK", &headers, &format!("data: {done}\n\n")))
        };
        let plans = ["first", "ignored", "second", "unused", "unused"].map(reply_with);
        let server = Server::start(plans.into()).await;
        let directory = tempfile::tempdir().unwrap();
        let provider = CodexProvider {
            name: "codex".into(),
            auth: auth::test_manager(directory.path().to_owned()),
            client: transport::client().unwrap(),
            endpoint: server.url.clone(),
        };
        let mut context = provider.open_context("context".parse().unwrap()).unwrap();
        let mut request = reasoning_tool_request(&provider.replay_scope());
        let exchange = request.history.clone();
        let user = |part| Message::User(vec![part]);
        let state = || {
            user(UserContent::Runtime {
                text: "<skyhook_state>".into(),
            })
        };
        // The opening request, a tool-loop request with persisted state, a wake by a
        // runtime event alone, that new turn's own tool loop, then queued user input.
        let steps: [Vec<Message>; 5] = [
            vec![user(UserContent::Text {
                text: "start".into(),
            })],
            [exchange.clone(), vec![state()]].concat(),
            vec![
                Message::Assistant(vec![AssistantItem::text("t", 0, "done")]),
                state(),
            ],
            exchange,
            vec![user(UserContent::Text {
                text: "queued".into(),
            })],
        ];
        request.history.clear();
        for step in steps {
            request.history.extend(step);
            let events: Vec<_> = context.invoke(request.clone()).collect().await;
            assert!(events.iter().all(Result::is_ok), "{events:?}");
        }
        let requests = server.finish().await;
        let sent = |request: &String| {
            let line = request.lines().find(|line| line.starts_with(TURN_STATE));
            line.map(|line| line.rsplit(' ').next().unwrap().to_owned())
        };
        let sent: Vec<_> = requests.iter().map(sent).collect();
        assert_eq!(
            sent,
            [
                None,
                Some("first".into()),
                None,
                Some("second".into()),
                None
            ]
        );
    }

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
        let context = "context".parse::<ContextId>().unwrap();
        assert_eq!(
            responses::encode(&same, &context).unwrap()["input"][0],
            reasoning_item()
        );
        let mut foreign = original;
        filter_reasoning_scope(&mut foreign, &b.replay_scope());
        let wire = responses::encode(&foreign, &context).unwrap();
        assert_eq!(wire["input"].as_array().unwrap().len(), 2);
        assert_eq!(wire["input"][0]["type"], "function_call");
    }
}
