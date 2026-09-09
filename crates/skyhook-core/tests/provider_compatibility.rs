//! Offline HTTP/SSE acceptance through the public, backend-independent provider interface.
//! Payloads are synthetic and source-derived (llama-swap loading, vLLM Hermes/cache usage).
use futures_util::StreamExt;
use serde_json::{Value, json};
use skyhook::provider::{
    Provider, ProviderContext,
    backends::{ChatReasoningReplay, OpenAiApi, openai_compatible},
    protocol::{
        Message, ModelRequest, ResponseAssembler, StopReason, ToolDefinition, ToolResult,
        UserContent,
    },
};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn serve(bodies: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let (header_end, length) = loop {
                let mut buffer = [0; 4096];
                let n = socket.read(&mut buffer).await.unwrap();
                assert_ne!(n, 0, "request closed before body completed");
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break (end + 4, length);
                    }
                }
            };
            requests.push(serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap());
            let reply = format!(
                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
        requests
    });
    (root, task)
}

fn stream(frames: Vec<Value>) -> String {
    let mut body: String = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    body.push_str("data: [DONE]\n\n");
    body
}

fn request(messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        model: "served-model".into(),
        system: Vec::new(),
        messages,
        tools: vec![ToolDefinition {
            name: "lookup".into(),
            description: "Find a value".into(),
            input_schema: json!({"type":"object", "properties":{"q":{"type":"string"}}}),
        }],
        response_schema: None,
        reasoning: None,
        max_output_tokens: Some(512),
        correlation: None,
    }
}

async fn complete(context: &mut dyn ProviderContext, request: ModelRequest) -> ResponseAssembler {
    let mut stream = context.invoke(request).await.unwrap();
    let mut assembler = ResponseAssembler::default();
    while let Some(event) = stream.next().await {
        assembler.push(&event.unwrap()).unwrap();
    }
    assembler
}

async fn assert_provider_replay(policy: Option<ChatReasoningReplay>, field: Option<&str>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let reply = stream(vec![json!({"choices":[{"index":0,"delta":{
            "reasoning_content":"private plan", "content":"answer"
        },"finish_reason":"stop"}]})]);
        let (url, server) = serve(vec![reply; 4]).await;
        let provider = openai_compatible("local", &url, OpenAiApi::ChatCompletions, None).unwrap();
        // None deliberately exercises the public constructor without a builder override.
        let provider = match policy {
            Some(policy) => provider.with_chat_reasoning_replay(policy),
            None => provider,
        };
        for model in ["served-model", "another-model"] {
            let mut context = provider.open_context(model.into()).unwrap();
            let user = Message::User(vec![UserContent::Text {
                text: "hello".into(),
            }]);
            let mut first = request(vec![user.clone()]);
            first.model = model.into();
            let (items, _, _) = complete(&mut *context, first).await.finish().unwrap();
            assert!(items.iter().any(|item| item.replay.is_some()));
            let mut followup = request(vec![user, Message::Assistant(items)]);
            followup.model = model.into();
            complete(&mut *context, followup).await.finish().unwrap();
        }
        let requests = server.await.unwrap();
        for (index, model) in [(1, "served-model"), (3, "another-model")] {
            assert_eq!(requests[index]["model"], model);
            let assistant = &requests[index]["messages"][1];
            assert_eq!(assistant["content"], "answer");
            for candidate in ["reasoning_content", "reasoning"] {
                if field == Some(candidate) {
                    assert_eq!(assistant[candidate], "private plan");
                } else {
                    assert!(assistant.get(candidate).is_none());
                }
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn default_constructor_replays_reasoning_content_for_all_provider_models() {
    assert_eq!(
        ChatReasoningReplay::default(),
        ChatReasoningReplay::ReasoningContent
    );
    assert_provider_replay(None, Some("reasoning_content")).await;
}

#[tokio::test]
async fn reasoning_override_applies_to_all_provider_models() {
    assert_provider_replay(Some(ChatReasoningReplay::Reasoning), Some("reasoning")).await;
}

#[tokio::test]
async fn unsupported_explicitly_disables_reasoning_replay_for_all_provider_models() {
    assert_provider_replay(Some(ChatReasoningReplay::Unsupported), None).await;
}

#[tokio::test]
async fn chat_variations_and_scoped_reasoning_round_trip_through_public_interface() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for (policy, field) in [
            (ChatReasoningReplay::ReasoningContent, "reasoning_content"),
            (ChatReasoningReplay::Reasoning, "reasoning"),
        ] {
            let first = stream(vec![
                json!({"choices":[{"delta":{"reasoning_content":"plan "}}],
                    "usage":{"prompt_tokens":100,"completion_tokens":1,"total_tokens":101}}),
                json!({"choices":[{"index":0,"delta":{"reasoning":"step","tool_calls":[
                    {"index":0,"id":"call_a","type":"function","function":{"name":"lookup"}},
                    {"index":0,"function":{"arguments":"{\"q\":\"x\"}"}}
                ]}}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":6,
                    "total_tokens":106,"prompt_tokens_details":{"cached_tokens":80}}}),
            ]);
            let answer = stream(vec![
                json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]}),
            ]);
            let (url, server) = serve(vec![first, answer.clone(), answer]).await;
            let provider: Arc<dyn Provider> = Arc::new(
                openai_compatible("local", &url, OpenAiApi::ChatCompletions, None)
                    .unwrap()
                    .with_chat_reasoning_replay(policy),
            );
            let mut context = provider.open_context("initial".into()).unwrap();
            let user = Message::User(vec![UserContent::Text {
                text: "find x".into(),
            }]);
            let (items, usage, stop) = complete(&mut *context, request(vec![user.clone()]))
                .await
                .finish()
                .unwrap();
            assert_eq!(stop, StopReason::ToolUse);
            assert_eq!(usage.input_tokens, 20);
            assert_eq!(usage.cached_input_tokens, 80);
            assert_eq!(usage.output_tokens, 6);
            assert!(
                items
                    .iter()
                    .filter_map(|item| item.replay.as_ref())
                    .all(|replay| !replay.scope.is_empty())
            );
            // Persist/restore canonical history without any backend-specific request fields.
            let serialized = serde_json::to_vec(&Message::Assistant(items)).unwrap();
            let assistant: Message = serde_json::from_slice(&serialized).unwrap();
            let history = vec![
                user,
                assistant.clone(),
                Message::Tool(vec![ToolResult {
                    call_id: "call_a".into(),
                    name: "lookup".into(),
                    result: json!({"value":42}),
                    is_error: false,
                    images: Vec::new(),
                }]),
            ];
            drop(context);
            let mut resumed = provider.open_context("resumed".into()).unwrap();
            complete(&mut *resumed, request(history.clone()))
                .await
                .finish()
                .unwrap();
            // Same URL/model, different configured provider identity: no private replay crossing.
            let foreign: Arc<dyn Provider> = Arc::new(
                openai_compatible("other-provider", &url, OpenAiApi::ChatCompletions, None)
                    .unwrap()
                    .with_chat_reasoning_replay(policy),
            );
            let mut foreign_context = foreign.open_context("foreign".into()).unwrap();
            complete(&mut *foreign_context, request(history.clone()))
                .await
                .finish()
                .unwrap();
            assert_eq!(serde_json::to_vec(&assistant).unwrap(), serialized);
            let requests = server.await.unwrap();
            assert_eq!(requests[0]["n"], 1);
            assert_eq!(requests[1]["messages"][1][field], "plan step");
            assert_eq!(requests[1]["messages"][1]["tool_calls"][0]["id"], "call_a");
            assert_eq!(requests[1]["messages"][2]["tool_call_id"], "call_a");
            assert!(requests[2]["messages"][1].get(field).is_none());
            assert_eq!(requests[2]["messages"][1]["tool_calls"][0]["id"], "call_a");
        }
    })
    .await
    .unwrap();
}
