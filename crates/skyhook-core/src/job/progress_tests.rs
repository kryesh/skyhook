use super::*;
use crate::{
    provider::protocol::{AssistantContent, Message, Usage},
    session::{CompactionCheckpoint, ModelPurpose, SESSION_FORMAT_VERSION},
};

async fn started(jobs: &JobManager, agent: &AgentId, owner: JobId, target: &str) {
    let location = ExecutionLocation::named(target, format!("/{target}/work").into());
    jobs.store()
        .append(
            agent.clone(),
            SessionEvent::AgentStarted {
                parent: None,
                owner_job: Some(owner),
                model_profile: "test".into(),
                max_context: None,
                agent_profile: None,
                location: location.clone(),
            },
        )
        .await
        .unwrap();
    jobs.set_agent_location(owner, location).await.unwrap();
}

async fn committed(jobs: &JobManager, agent: &AgentId) {
    jobs.store()
        .append(
            agent.clone(),
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![AssistantContent::text("answer", 0, "done")]),
            },
        )
        .await
        .unwrap();
}

async fn snapshot(jobs: &JobManager, owner: &AgentId, targets: bool) -> Value {
    let mut capabilities = CapabilitySet::default();
    if targets {
        capabilities.insert(Capability::Targets);
    }
    serde_json::to_value(jobs.active_states(owner, &capabilities, i64::MAX).await).unwrap()
}

#[tokio::test]
async fn active_progress_is_recursive_exclusive_and_location_filtered() {
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::create(directory.path()).await.unwrap();
    let root = AgentId::root(store.id());
    let child = root.child(1);
    let grandchild = child.child(1);
    let great_grandchild = grandchild.child(1);
    let jobs = JobManager::new(store);
    let delegated = jobs
        .create(JobSpec::test(root.clone(), "agent"))
        .await
        .unwrap();
    let ordinary = jobs
        .create(JobSpec::test(root.clone(), "exec"))
        .await
        .unwrap();
    // Even queued agents without AgentStarted have zero counters; tools do not.
    let initial = snapshot(&jobs, &root, false).await;
    assert_eq!(initial[0]["turns"], 0);
    assert_eq!(initial[0]["tool_calls"], 0);
    assert!(initial[0].get("children").is_none());
    assert!(initial[1].get("turns").is_none());
    assert!(initial[1].get("tool_calls").is_none());
    started(&jobs, &child, delegated.id, "child").await;
    committed(&jobs, &child).await;
    committed(&jobs, &child).await;
    committed(&jobs, &root).await; // Not child progress.
    let script = jobs
        .create(JobSpec::test(child.clone(), "script"))
        .await
        .unwrap();
    let nested = jobs
        .create(JobSpec {
            parent: Some(script.id),
            ..JobSpec::test(child.clone(), "exec")
        })
        .await
        .unwrap();
    jobs.finish(nested.id, JobOutcome::Completed(ToolOutput::default()))
        .await
        .unwrap();
    let delegated_again = jobs
        .create(JobSpec {
            parent: Some(script.id),
            ..JobSpec::test(child.clone(), "agent")
        })
        .await
        .unwrap();
    started(&jobs, &grandchild, delegated_again.id, "grandchild").await;
    jobs.transition(delegated_again.id, JobState::AwaitingApproval)
        .await
        .unwrap();
    committed(&jobs, &grandchild).await;
    let deepest = jobs
        .create(JobSpec::test(grandchild.clone(), "agent"))
        .await
        .unwrap();
    started(&jobs, &great_grandchild, deepest.id, "deepest").await;
    committed(&jobs, &great_grandchild).await;
    jobs.create(JobSpec::test(great_grandchild.clone(), "read"))
        .await
        .unwrap();
    let terminal = jobs
        .create(JobSpec::test(child.clone(), "agent"))
        .await
        .unwrap();
    jobs.finish(terminal.id, JobOutcome::Completed(ToolOutput::default()))
        .await
        .unwrap();

    let state = snapshot(&jobs, &root, false).await;
    assert_eq!(state.as_array().unwrap().len(), 2);
    assert_eq!(state[0]["job"], delegated.id.get());
    assert_eq!(state[1]["job"], ordinary.id.get());
    assert_eq!(state[0]["turns"], 2);
    assert_eq!(state[0]["tool_calls"], 4); // Script, nested exec, two agent jobs.
    assert_eq!(state[0]["children"].as_array().unwrap().len(), 1);
    let grand = &state[0]["children"][0];
    assert_eq!(grand["job"], delegated_again.id.get());
    assert_eq!(grand["state"], "queued"); // Approval stays host-only.
    assert_eq!(grand["turns"], 1);
    assert_eq!(grand["tool_calls"], 1);
    assert_eq!(grand["location"]["workspace"], "/grandchild/work");
    let deepest_state = &grand["children"][0];
    assert_eq!(deepest_state["job"], deepest.id.get());
    assert_eq!(deepest_state["turns"], 1);
    assert_eq!(deepest_state["tool_calls"], 1);
    assert!(deepest_state.get("children").is_none());
    for entry in [&state[0], grand, deepest_state] {
        assert!(entry["location"].get("target").is_none());
    }
    let visible = snapshot(&jobs, &root, true).await;
    assert_eq!(visible[0]["location"]["target"], "child");
    assert_eq!(
        visible[0]["children"][0]["location"]["target"],
        "grandchild"
    );
    assert_eq!(
        visible[0]["children"][0]["children"][0]["location"]["target"],
        "deepest"
    );
    // Polling and a different viewer cannot double count the same journal suffix.
    assert_eq!(snapshot(&jobs, &root, false).await, state);
    let own = snapshot(&jobs, &child, false).await;
    assert_eq!(own[1]["turns"], grand["turns"]);
    assert_eq!(own[1]["tool_calls"], grand["tool_calls"]);
    let future = serde_json::to_value(
        jobs.active_states(&root, &CapabilitySet::default(), 0)
            .await,
    )
    .unwrap();
    assert_eq!(future[0]["age_seconds"], 0);
}

#[test]
fn progress_counts_commits_not_retry_attempts_incomplete_requests_or_compaction() {
    let root = AgentId::root(crate::identity::SessionId::from_bytes([1; 16]));
    let child = root.child(1);
    let job = JobId::new(1).unwrap();
    let events = vec![
        SessionEvent::AgentStarted {
            parent: Some(root),
            owner_job: Some(job),
            model_profile: "test".into(),
            max_context: None,
            agent_profile: None,
            location: ExecutionLocation::root(".".into()),
        },
        SessionEvent::ModelRequested {
            context: 1,
            messages: Vec::new(),
            purpose: ModelPurpose::Agent,
        },
        SessionEvent::ModelFailed {
            request: 2,
            attempt: 1,
            error: "partial stream failed".into(),
        },
        SessionEvent::ModelRequested {
            context: 1,
            messages: Vec::new(),
            purpose: ModelPurpose::Agent,
        },
        SessionEvent::Usage {
            request: Some(4),
            usage: Usage::default(),
        },
        SessionEvent::MessageCommitted {
            message: Message::Assistant(vec![AssistantContent::text("a", 0, "done")]),
        },
        SessionEvent::MessageCommitted {
            message: Message::User(Vec::new()),
        },
        SessionEvent::MessageCommitted {
            message: Message::Tool(Vec::new()),
        },
        SessionEvent::ModelRequested {
            context: 1,
            messages: Vec::new(),
            purpose: ModelPurpose::Compaction,
        },
        SessionEvent::Compaction {
            checkpoint: CompactionCheckpoint {
                schema_version: 1,
                previous: None,
                frontier: 8,
                message: Message::User(Vec::new()),
                todos: Vec::new(),
                retained: Vec::new(),
                request: 9,
                max_context: 1000,
                before_tokens: 100,
                after_tokens: 10,
            },
        },
        SessionEvent::CompactionFailed {
            request: Some(9),
            error: "failed".into(),
        },
        SessionEvent::ModelRequested {
            context: 1,
            messages: Vec::new(),
            purpose: ModelPurpose::Agent,
        }, // Incomplete: no commit.
    ];
    let records = events
        .into_iter()
        .enumerate()
        .map(|(index, event)| EventRecord {
            version: SESSION_FORMAT_VERSION,
            sequence: index as u64 + 1,
            timestamp_millis: 0,
            agent: child.clone(),
            event,
        })
        .collect::<Vec<_>>();
    let mut progress = progress::Progress::default();
    progress.project(&records[..5]);
    assert_eq!(
        serde_json::to_value(progress.for_job(job)).unwrap()["turns"],
        0
    );
    progress.project(&records);
    progress.project(&records); // Replay/overlapping suffixes are idempotent.
    assert_eq!(
        serde_json::to_value(progress.for_job(job)).unwrap(),
        serde_json::json!({"turns": 1, "tool_calls": 0})
    );
}

#[tokio::test]
async fn progress_survives_retained_resume_and_replay() {
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::create(directory.path()).await.unwrap();
    let root = AgentId::root(store.id());
    let child = root.child(1);
    let jobs = JobManager::new(store.clone());
    let delegated = jobs
        .create(JobSpec {
            accepts_input: true,
            ..JobSpec::test(root.clone(), "agent")
        })
        .await
        .unwrap();
    started(&jobs, &child, delegated.id, "child").await;
    committed(&jobs, &child).await;
    let tool = jobs
        .create(JobSpec::test(child.clone(), "read"))
        .await
        .unwrap();
    jobs.finish(tool.id, JobOutcome::Completed(ToolOutput::default()))
        .await
        .unwrap();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    jobs.set_resume_handler(
        delegated.id,
        Arc::new({
            let jobs = jobs.clone();
            let child = child.clone();
            let release = release.clone();
            move |_, _| {
                let jobs = jobs.clone();
                let child = child.clone();
                let release = release.clone();
                Box::pin(async move {
                    committed(&jobs, &child).await;
                    let tool = jobs.create(JobSpec::test(child, "read")).await.unwrap();
                    jobs.finish(tool.id, JobOutcome::Completed(ToolOutput::default()))
                        .await
                        .unwrap();
                    release.acquire().await.unwrap().forget();
                    Ok(ToolOutput::default())
                })
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(snapshot(&jobs, &root, false).await[0]["turns"], 1);
    jobs.finish(delegated.id, JobOutcome::Completed(ToolOutput::default()))
        .await
        .unwrap();
    assert!(
        snapshot(&jobs, &root, false)
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    jobs.send(delegated.id, Value::Null).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = snapshot(&jobs, &root, false).await;
            if state[0]["tool_calls"] == 2 {
                assert_eq!(state[0]["turns"], 2);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    release.add_permits(1);
    jobs.wait(delegated.id, Some(Duration::from_secs(5)), true)
        .await
        .unwrap();
    jobs.clear_resume_handler(delegated.id).await;

    let records = store.records().await;
    let restored = JobManager::restore(store, &records).await.unwrap();
    // Completed agents remain omitted after replay, but retain progress for resumption.
    assert!(
        snapshot(&restored, &root, false)
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        serde_json::to_value(restored.inner.progress.lock().await.for_job(delegated.id)).unwrap(),
        serde_json::json!({"turns": 2, "tool_calls": 2})
    );
    // Exercise the same projection after replay without resetting the retained identity.
    committed(&restored, &child).await;
    snapshot(&restored, &root, false).await;
    assert_eq!(
        serde_json::to_value(restored.inner.progress.lock().await.for_job(delegated.id)).unwrap()["turns"],
        3
    );
}

#[tokio::test]
async fn pending_progress_probe_does_not_claim_delivery() {
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::create(directory.path()).await.unwrap();
    let owner = AgentId::root(store.id());
    let other = owner.child(1);
    let jobs = JobManager::new(store);
    let foreground = jobs
        .create(JobSpec::test(owner.clone(), "read"))
        .await
        .unwrap();
    jobs.finish(foreground.id, JobOutcome::Completed(ToolOutput::default()))
        .await
        .unwrap();
    assert!(!jobs.has_pending(&owner).await);
    let tool = jobs
        .create(JobSpec {
            background: true,
            ..JobSpec::test(owner.clone(), "read")
        })
        .await
        .unwrap();
    assert!(!jobs.has_pending(&owner).await);
    jobs.finish(tool.id, JobOutcome::Completed(ToolOutput::default()))
        .await
        .unwrap();
    assert!(!jobs.has_pending(&other).await);
    assert!(jobs.has_pending(&owner).await);
    assert!(jobs.has_pending(&owner).await);
    assert_eq!(jobs.take_pending(&owner).await.unwrap().len(), 1);
    assert!(!jobs.has_pending(&owner).await);

    let question = jobs
        .create(JobSpec::test(owner.clone(), "ask"))
        .await
        .unwrap();
    jobs.transition(question.id, JobState::Running)
        .await
        .unwrap();
    jobs.request_input(question.id, serde_json::json!({"prompt": "answer?"}))
        .await
        .unwrap();
    assert!(jobs.has_pending(&owner).await);
    jobs.wait(question.id, None, true).await.unwrap();
    assert!(!jobs.has_pending(&owner).await);
}
