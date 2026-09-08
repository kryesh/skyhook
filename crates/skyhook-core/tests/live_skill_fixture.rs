//! Opt-in live acceptance: SKYHOOK_LIVE_PROFILE=terra|qwen36 cargo test
//! -p skyhook-agent-core --test live_skill_fixture -- --ignored --nocapture
use skyhook::{
    agent::HarnessBuilder,
    config::{Config, ProviderConfig},
    provider::{
        Provider, ProviderContext, ProviderError, ProviderFuture,
        backends::{OpenAiApi, codex::CodexProvider, openai_compatible},
        protocol::{Message, ModelRequest},
    },
    tool::policy::{AllowAll, Capability, CapabilitySet},
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
struct RecordedProvider {
    inner: Arc<dyn Provider>,
    observations: Arc<Mutex<Vec<(usize, usize)>>>,
}
struct RecordedContext {
    inner: Box<dyn ProviderContext>,
    observations: Arc<Mutex<Vec<(usize, usize)>>>,
}
impl Provider for RecordedProvider {
    fn open_context(&self, correlation: String) -> Result<Box<dyn ProviderContext>, ProviderError> {
        Ok(Box::new(RecordedContext {
            inner: self.inner.open_context(correlation)?,
            observations: self.observations.clone(),
        }))
    }
}
impl ProviderContext for RecordedContext {
    fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
        let (mut images, mut reasoning) = (0, 0);
        for message in &request.messages {
            match message {
                Message::Tool(results) => {
                    for result in results {
                        for image in &result.images {
                            assert!(
                                image
                                    .data_base64
                                    .as_ref()
                                    .is_some_and(|data| !data.is_empty()),
                                "unhydrated tool image"
                            );
                            images += 1;
                        }
                    }
                }
                Message::Assistant(blocks) => {
                    reasoning += blocks
                        .iter()
                        .filter(|b| b.kind == skyhook::provider::protocol::ItemKind::Reasoning)
                        .count()
                }
                _ => {}
            }
        }
        self.observations.lock().unwrap().push((images, reasoning));
        eprintln!("request: tool_images={images}, retained_reasoning_blocks={reasoning}");
        self.inner.invoke(request)
    }
}
#[tokio::test]
#[ignore = "live model calls using explicit inexpensive profile and Skyhook-owned credentials"]
async fn live_skill_tool_images() {
    let name = std::env::var("SKYHOOK_LIVE_PROFILE").expect("set terra or qwen36");
    assert!(matches!(name.as_str(), "terra" | "qwen36"));
    let config = Config::load(None).await.unwrap();
    let mut profile = config.models[&name].clone();
    profile.max_output = 8192;
    if name == "terra" {
        profile.reasoning = Some("low".into());
    }
    let provider: Arc<dyn Provider> = match &config.providers[&profile.provider] {
        ProviderConfig::Codex => Arc::new(CodexProvider::new().unwrap()),
        ProviderConfig::Openai {
            base_url,
            api,
            api_key_env,
        } => {
            assert_eq!(*api, OpenAiApi::ChatCompletions);
            let key = api_key_env.as_ref().map(|e| std::env::var(e).unwrap());
            Arc::new(openai_compatible(&profile.provider, base_url, *api, key).unwrap())
        }
        _ => panic!("unexpected provider"),
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let sessions = root.join("target/live-skill-fixture").join(&name);
    let observations = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(RecordedProvider {
        inner: provider,
        observations: observations.clone(),
    });
    let mut caps = CapabilitySet::default();
    for cap in [
        Capability::Write,
        Capability::Exec,
        Capability::Agents,
        Capability::Targets,
    ] {
        caps.remove(cap);
    }
    let harness=HarnessBuilder::new(root.join("tests/fixtures/skill-workspace")).session_root(&sessions)
        .provider(profile.provider.clone(),provider).model_profile(&name,profile).default_model_profile(&name)
        .max_child_depth(0).capabilities(caps).policy(Arc::new(AllowAll))
        .instructions("This is a read-only provider acceptance test in the skill fixture. Follow the user-requested tool route exactly. Do not run commands, inspect image source bytes, or infer pixel colors from filenames. Use image vision. Do not ask questions. Keep final answers short.")
        .build().await.unwrap();
    let session = harness.new_session().await.unwrap();
    eprintln!(
        "profile={name}, session={}, journal={}",
        session.id(),
        sessions
            .join(session.id().to_string())
            .join("events.jsonl")
            .display()
    );
    let first=tokio::time::timeout(Duration::from_secs(180),session.prompt(
        "Load the mixed-assets skill, then call skill with name mixed-assets and path assets/vision.png to view its image attachment. Do not use read, exec, or code to inspect the file. Describe the background color and both colored shapes after viewing the tool image."
    )).await.expect("first-turn deadline");
    eprintln!("first result: {first:?}");
    let first = first.unwrap();
    assert!(
        ["blue", "red", "yellow"]
            .iter()
            .all(|color| first.to_ascii_lowercase().contains(color)),
        "incorrect visual answer: {first}"
    );
    let second=tokio::time::timeout(Duration::from_secs(180),session.prompt(
        "Now verify the background-job image path. Call script with bg:true and source `return await tool.skill({name:\"mixed-assets\",path:\"assets/vision-other.png\"});`. Then use job_output with that script job ID and no field/search/pagination selections to retrieve its complete output and image. This is a DIFFERENT image. View that returned image and describe the background color and both colored shapes. Do not skip the requested tool calls based on earlier history."
    )).await.expect("second-turn deadline");
    eprintln!("second result: {second:?}");
    let second = second.unwrap();
    assert!(
        ["green", "white"]
            .iter()
            .all(|color| second.to_ascii_lowercase().contains(color))
            && (second.to_ascii_lowercase().contains("magenta")
                || second.to_ascii_lowercase().contains("pink")
                || second.to_ascii_lowercase().contains("purple")),
        "incorrect retrieved-image answer: {second}"
    );
    session.shutdown().await.unwrap();
    let journal =
        std::fs::read_to_string(sessions.join(session.id().to_string()).join("events.jsonl"))
            .unwrap();
    let records: Vec<serde_json::Value> = journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        !records.iter().any(|r| r["event"]["type"] == "model_failed"),
        "must not rely on failed retries"
    );
    let content: Vec<_> = records
        .iter()
        .filter(|r| r["event"]["type"] == "message_committed")
        .flat_map(|r| {
            r["event"]["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .collect();
    let calls: Vec<_> = content
        .iter()
        .flat_map(|item| item["blocks"].as_array().into_iter().flatten())
        .map(|block| &block["content"])
        .filter(|block| block["type"] == "tool_call")
        .collect();
    assert!(
        calls.iter().any(|b| b["type"] == "tool_call"
            && b["name"] == "skill"
            && b["arguments"]["path"] == "assets/vision.png"),
        "must invoke the skill image tool"
    );
    assert!(
        calls.iter().any(|b| b["type"] == "tool_call"
            && b["name"] == "script"
            && b["arguments"]["bg"] == true),
        "must invoke a background script"
    );
    assert!(
        content.iter().any(|b| b["name"] == "job_output"
            && b["images"]
                .as_array()
                .is_some_and(|images| !images.is_empty())),
        "must retrieve an image through job_output"
    );
    let observations = observations.lock().unwrap();
    assert!(
        observations.iter().any(|(images, _)| *images >= 2),
        "must replay original and retrieved tool images"
    );
    eprintln!(
        "PASS {name}: {} requests; skill/background tool images hydrated and visually identified",
        observations.len()
    );
}
