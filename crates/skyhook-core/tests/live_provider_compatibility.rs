//! Opt-in live reasoning/tool continuation through Provider/ProviderContext.
//! No model-requested tool is executed; the one declared tool has a synthetic result.
//! Examples (authorized endpoints only):
//! SKYHOOK_LIVE_PROFILE=qwen-red SKYHOOK_LIVE_CHAT_REPLAY=reasoning_content cargo test
//!   -p skyhook-agent-core --test live_provider_compatibility -- --ignored --nocapture
//! qwen defaults to /upstream/<model>/v1; set SKYHOOK_LIVE_ROUTE=general to explicitly
//! exercise llama-swap's general /v1 route instead. No silent route fallback occurs.
use futures_util::StreamExt;
use serde_json::json;
use skyhook::{
    config::{Config, ProviderConfig},
    provider::{
        Provider, ProviderContext, ProviderTimeouts,
        backends::{ChatReasoningReplay, OpenAiApi, openai_compatible},
        protocol::{
            AssistantItem, BlockContent, Message, ModelRequest, ResponseAssembler, StopReason,
            SystemSegment, ToolDefinition, ToolResult, Usage, UserContent,
        },
    },
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const TOOL: &str = "lookup_test_value";
const KEY: &str = "skyhook-replay-test";
const VALUE: &str = "MARIGOLD-4281";

fn model_request(model: &str, reasoning: Option<String>, messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        model: model.into(),
        system: vec![SystemSegment {
            text: "This is a bounded API compatibility test. Use only the supplied lookup_test_value function, exactly once. The lookup value is unknown until its tool result arrives. After receiving that result, answer with exactly its value and nothing else. Do not request any other tool or repeat the lookup.".into(),
            cache: false,
        }],
        messages,
        tools: vec![ToolDefinition {
            name: TOOL.into(),
            description: "Return a synthetic test value for the supplied key. This tool has no external side effects.".into(),
            input_schema: json!({
                "type":"object", "properties":{"key":{"type":"string"}},
                "required":["key"], "additionalProperties":false,
            }),
        }],
        response_schema: None,
        reasoning,
        max_output_tokens: Some(4096),
        correlation: None,
    }
}

async fn complete(
    context: &mut dyn ProviderContext,
    request: ModelRequest,
) -> (Vec<AssistantItem>, Usage, StopReason, usize) {
    let mut stream = context.invoke(request).await.expect("live invoke failed");
    let mut assembler = ResponseAssembler::default();
    let mut events = 0;
    while let Some(event) = stream.next().await {
        let event = event.expect("live stream failed");
        events += 1;
        assembler
            .push(&event)
            .expect("canonical stream lifecycle failed");
    }
    let (items, usage, stop) = assembler.finish().expect("incomplete live response");
    (items, usage, stop, events)
}

fn reasoning_chars(items: &[AssistantItem]) -> usize {
    items
        .iter()
        .flat_map(|item| &item.blocks)
        .map(|block| match &block.content {
            BlockContent::Reasoning { text } => text.chars().count(),
            _ => 0,
        })
        .sum()
}

#[tokio::test]
#[ignore = "requires explicitly authorized qwen-red/qwen inference endpoints"]
async fn live_reasoning_and_tool_replay() {
    tokio::time::timeout(Duration::from_secs(360), async {
        let profile_name = std::env::var("SKYHOOK_LIVE_PROFILE")
            .expect("set SKYHOOK_LIVE_PROFILE=qwen-red or qwen");
        assert!(matches!(profile_name.as_str(), "qwen-red" | "qwen"));
        let config = Config::load(None).await.expect("load user configuration");
        let profile = config.models.get(&profile_name).expect("configured model profile");
        let ProviderConfig::Openai { base_url, api, api_key_env, chat_reasoning_replay, .. } = &config.providers[&profile.provider] else {
            panic!("live compatibility fixture requires a Chat Completions profile");
        };
        assert_eq!(*api, OpenAiApi::ChatCompletions);
        let replay = match std::env::var("SKYHOOK_LIVE_CHAT_REPLAY").ok().as_deref() {
            Some("reasoning_content") => ChatReasoningReplay::ReasoningContent,
            Some("reasoning") => ChatReasoningReplay::Reasoning,
            None => chat_reasoning_replay.unwrap_or_default(),
            _ => panic!("SKYHOOK_LIVE_CHAT_REPLAY must be reasoning_content or reasoning"),
        };
        assert_ne!(replay, ChatReasoningReplay::Unsupported,
            "select the server's request-side reasoning field with SKYHOOK_LIVE_CHAT_REPLAY");
        let mut root = reqwest::Url::parse(base_url).expect("provider API root");
        let route = if profile_name == "qwen" {
            std::env::var("SKYHOOK_LIVE_ROUTE").unwrap_or_else(|_| "upstream".into())
        } else { "sglang".into() };
        assert!(matches!(route.as_str(), "general" | "upstream" | "sglang"));
        if route == "upstream" {
            // Route to the configured upstream by model alias; do not silently fall back
            // to llama-swap's general API (which can rewrite requests/inject loading state).
            root.path_segments_mut().expect("hierarchical URL")
                .clear().extend(["upstream", profile.model.as_str(), "v1"]);
        }
        let model = std::env::var("SKYHOOK_LIVE_MODEL").unwrap_or_else(|_| profile.model.clone());
        let key = api_key_env.as_ref().map(|name| std::env::var(name).expect("configured credential environment"));
        let provider: Arc<dyn Provider> = Arc::new(openai_compatible(
            &profile.provider, root.as_str(), *api, key,
        ).expect("construct provider")
            .with_timeouts(ProviderTimeouts {
                startup: Duration::from_secs(180), read_idle: Duration::from_secs(120),
            })
            .with_chat_reasoning_replay(replay));
        let user = Message::User(vec![UserContent::Text {
            text: format!("Look up the value for key {KEY}. Call the supplied function first, then return its value."),
        }]);
        let started = Instant::now();
        let mut context = provider.open_context("live-replay-initial".into()).unwrap();
        let (items, usage, stop, events) = complete(
            &mut *context, model_request(&model, profile.reasoning.clone(), vec![user.clone()]),
        ).await;
        let first_reasoning = reasoning_chars(&items);
        let envelopes = items.iter().filter_map(|item| item.replay.as_ref()).count();
        println!("profile={profile_name} route={route} phase=tool stop={stop:?} events={events} reasoning_chars={first_reasoning} replay_items={envelopes} prompt_tokens={} cached_tokens={} output_tokens={} elapsed_ms={}",
            usage.input_tokens + usage.cached_input_tokens, usage.cached_input_tokens,
            usage.output_tokens, started.elapsed().as_millis());
        assert!(matches!(stop, StopReason::ToolUse | StopReason::EndTurn), "unexpected tool-phase terminal outcome");
        let calls: Vec<_> = items.iter().flat_map(|item| &item.blocks).filter_map(|block| {
            match &block.content { BlockContent::ToolCall(call) => Some(call.clone()), _ => None }
        }).collect();
        assert_eq!(calls.len(), 1, "model did not return exactly one structured tool call");
        assert_eq!(calls[0].name, TOOL, "unexpected tool name; nothing executed");
        assert_eq!(calls[0].arguments, json!({"key":KEY}), "unexpected arguments; nothing executed");
        assert!(first_reasoning > 0 && envelopes > 0, "server did not expose replayable reasoning before its tool call");
        for envelope in items.iter().filter_map(|item| item.replay.as_ref()) {
            assert_eq!(envelope.protocol, "chat_completions");
            assert_eq!(envelope.model, model);
            assert!(!envelope.scope.is_empty());
        }
        // A file round trip and a fresh provider context exercise replay rather than
        // any hidden in-memory server continuation. No raw reasoning is printed.
        let file = tempfile::NamedTempFile::new().unwrap();
        let assistant = Message::Assistant(items);
        serde_json::to_writer(file.as_file(), &assistant).unwrap();
        let restored: Message = serde_json::from_reader(std::fs::File::open(file.path()).unwrap()).unwrap();
        assert!(restored == assistant, "reasoning history changed during persistence round trip");
        drop(context);
        let mut resumed = provider.open_context("live-replay-resumed".into()).unwrap();
        let (final_items, final_usage, final_stop, final_events) = complete(
            &mut *resumed,
            model_request(&model, profile.reasoning.clone(), vec![
                user, restored,
                Message::Tool(vec![ToolResult {
                    call_id: calls[0].id.clone(), name: calls[0].name.clone(),
                    result: json!({"value":VALUE}), images: Vec::new(), is_error: false,
                }]),
            ]),
        ).await;
        let mut text = String::new();
        for block in final_items.iter().flat_map(|item| &item.blocks) {
            match &block.content {
                BlockContent::Text { text: part } => text.push_str(part),
                BlockContent::ToolCall(_) => panic!("model repeated the tool call; nothing executed"),
                _ => {},
            }
        }
        let answer_matches = text.trim() == VALUE;
        println!("profile={profile_name} route={route} phase=answer stop={final_stop:?} events={final_events} reasoning_chars={} prompt_tokens={} cached_tokens={} output_tokens={} answer_matches={answer_matches} elapsed_ms={}",
            reasoning_chars(&final_items), final_usage.input_tokens + final_usage.cached_input_tokens,
            final_usage.cached_input_tokens, final_usage.output_tokens, started.elapsed().as_millis());
        assert_eq!(final_stop, StopReason::EndTurn);
        assert!(answer_matches, "model final text did not exactly match the synthetic tool result");
    }).await.expect("bounded live test timed out");
}
