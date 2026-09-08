//! Opt-in probes, never part of offline CI. Run with --ignored --exact and
//! explicitly supply SKYHOOK_LIVE_{OPENAI,ANTHROPIC}_{BASE_URL,MODEL,API_KEY}.
use super::*;
use crate::provider::protocol::{Message, UserContent};

async fn probe(provider: NativeProvider, model: String) {
    let mut context = provider.open_context("native-live-smoke".into()).unwrap();
    let request = ModelRequest {
        model,
        system: vec![],
        messages: vec![Message::User(vec![UserContent::Text {
            text: "Reply with the single word OK.".into(),
        }])],
        tools: vec![],
        response_schema: None,
        reasoning: None,
        max_output_tokens: Some(128),
        correlation: None,
    };
    let mut stream = context.invoke(request).await.unwrap();
    let mut finished = false;
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk.unwrap() {
            ResponseChunk::BlockEnded {
                content: crate::provider::protocol::BlockContent::Text { text: part },
                ..
            } => text.push_str(&part),
            ResponseChunk::ResponseEnded { stop_reason } => {
                assert_ne!(
                    stop_reason,
                    crate::provider::protocol::StopReason::MaxTokens
                );
                finished = true;
            }
            _ => {}
        }
    }
    assert!(finished && !text.trim().is_empty());
}
fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("explicit {name} required for live smoke test"))
}
async fn openai(api: OpenAiApi) {
    let provider = openai_compatible(
        "live",
        env("SKYHOOK_LIVE_OPENAI_BASE_URL"),
        api,
        Some(env("SKYHOOK_LIVE_OPENAI_API_KEY")),
    )
    .unwrap();
    probe(provider, env("SKYHOOK_LIVE_OPENAI_MODEL")).await;
}
#[tokio::test]
#[ignore = "opt-in live API call; requires explicit endpoint/model/credential environment"]
async fn openai_chat() {
    openai(OpenAiApi::ChatCompletions).await;
}
#[tokio::test]
#[ignore = "opt-in live API call; requires explicit endpoint/model/credential environment"]
async fn openai_responses() {
    openai(OpenAiApi::Responses).await;
}
#[tokio::test]
#[ignore = "opt-in live API call; requires explicit endpoint/model/credential environment"]
async fn anthropic_messages() {
    let provider = anthropic_api(
        "live",
        env("SKYHOOK_LIVE_ANTHROPIC_BASE_URL"),
        Some(env("SKYHOOK_LIVE_ANTHROPIC_API_KEY")),
    )
    .unwrap();
    probe(provider, env("SKYHOOK_LIVE_ANTHROPIC_MODEL")).await;
}
