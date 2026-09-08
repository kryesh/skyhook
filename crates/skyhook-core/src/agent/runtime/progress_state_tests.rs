use super::*;
use crate::{
    job::JobSpec,
    provider::protocol::{AssistantContent, Message},
    session::{SessionEvent, SessionStore},
};

#[tokio::test]
async fn transient_state_includes_live_child_progress_without_journaling_the_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::create(directory.path()).await.unwrap();
    let root = AgentId::root(store.id());
    let child = root.child(1);
    let jobs = JobManager::new(store.clone());
    let delegated = jobs
        .create(JobSpec::test(root.clone(), "agent"))
        .await
        .unwrap();
    store
        .append(
            child.clone(),
            SessionEvent::AgentStarted {
                parent: Some(root.clone()),
                owner_job: Some(delegated.id),
                model_profile: "test".into(),
                max_context: None,
                agent_profile: None,
                location: ExecutionLocation::root(".".into()),
            },
        )
        .await
        .unwrap();
    let now = DateTime::parse_from_rfc3339("2026-09-08T12:00:00+00:00")
        .unwrap()
        .with_timezone(&Local);
    let before = store.records().await;
    let first =
        runtime_state_with_todos_at(&jobs, &root, &CapabilitySet::default(), Vec::new(), now).await;
    assert_eq!(store.records().await, before);
    let decode = |content: UserContent| {
        let UserContent::Runtime { text } = content else {
            panic!("state must be a transient runtime message");
        };
        serde_json::from_str::<serde_json::Value>(
            text.strip_prefix("<skyhook_state>\n")
                .unwrap()
                .strip_suffix("\n</skyhook_state>")
                .unwrap(),
        )
        .unwrap()
    };
    let first = decode(first);
    assert_eq!(first["active_jobs"][0]["turns"], 0);
    assert_eq!(first["active_jobs"][0]["tool_calls"], 0);
    assert!(first["active_jobs"][0].get("children").is_none());
    store
        .append(
            child,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![AssistantContent::text("answer", 0, "done")]),
            },
        )
        .await
        .unwrap();
    let before = store.records().await;
    let second = decode(
        runtime_state_with_todos_at(&jobs, &root, &CapabilitySet::default(), Vec::new(), now).await,
    );
    assert_eq!(store.records().await, before);
    assert_eq!(second["active_jobs"][0]["turns"], 1);
    assert_eq!(second["active_jobs"][0]["tool_calls"], 0);
    assert_eq!(second["date"], now.format("%Y-%m-%d").to_string());
    assert_eq!(second["todos"], serde_json::json!([]));
}
