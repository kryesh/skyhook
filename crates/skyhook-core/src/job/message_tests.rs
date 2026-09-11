use super::*;
use crate::provider::protocol::{AssistantContent, BlockContent, Message, UserContent};

async fn child_job(background: bool) -> (tempfile::TempDir, JobManager, AgentId, AgentId, JobId) {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::create(root.path()).await.unwrap();
    let owner = AgentId::root(store.id());
    let child = owner.child(1);
    let manager = JobManager::new(store);
    let job = manager
        .create(JobSpec {
            background,
            ..JobSpec::test(owner.clone(), "agent")
        })
        .await
        .unwrap()
        .id;
    manager
        .store()
        .append(
            child.clone(),
            SessionEvent::AgentStarted {
                parent: Some(owner.clone()),
                owner_job: Some(job),
                model_profile: "test".into(),
                max_context: None,
                location: ExecutionLocation::root(".".into()),
            },
        )
        .await
        .unwrap();
    manager.transition(job, JobState::Running).await.unwrap();
    (root, manager, owner, child, job)
}

fn assistant(text: &str) -> Message {
    Message::Assistant(vec![AssistantContent::text("text", 0, text)])
}

async fn commit(manager: &JobManager, child: &AgentId, job: JobId, text: &str) -> u64 {
    manager
        .commit_child_message(child, job, assistant(text), text.into())
        .await
        .unwrap()
}

fn notification(messages: &[AgentMessage], envelopes: &[JobEnvelope]) -> Message {
    let mut items: Vec<_> = messages
        .iter()
        .map(|message| {
            let mut value = serde_json::to_value(message).unwrap();
            value["kind"] = serde_json::json!("message");
            value
        })
        .collect();
    items.extend(
        envelopes
            .iter()
            .map(|envelope| serde_json::to_value(envelope).unwrap()),
    );
    Message::User(vec![UserContent::Runtime {
        text: format!(
            "<skyhook_job_events>\n{}\n</skyhook_job_events>",
            serde_json::to_string(&items).unwrap()
        ),
    }])
}

async fn finish(manager: &JobManager, job: JobId) {
    manager
        .finish(
            job,
            JobOutcome::Completed(ToolOutput::new(serde_json::json!("saved result"))),
        )
        .await
        .unwrap();
}

async fn replay(manager: &JobManager) -> JobManager {
    JobManager::restore(manager.store().clone(), &manager.store().records().await)
        .await
        .unwrap()
}

#[tokio::test]
async fn messages_are_independent_of_claims_and_foreground_lifecycle() {
    let (_root, manager, owner, child, job) = child_job(false).await;
    let mut wakes = manager.subscribe_completions();
    let first = commit(&manager, &child, job, "first").await;
    assert_eq!(wakes.recv().await.unwrap().agent, owner);
    let second = commit(&manager, &child, job, "second").await;
    assert_eq!(wakes.recv().await.unwrap().job, job);
    finish(&manager, job).await;
    manager.claim(job).await.unwrap();
    assert!(manager.has_pending(&owner).await);
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    assert!(receipt.envelopes().is_empty());
    assert_eq!(
        receipt
            .messages()
            .iter()
            .map(|message| message.message)
            .collect::<Vec<_>>(),
        vec![first, second]
    );
    receipt
        .commit(notification(&receipt.messages()[..1], &[]))
        .await
        .unwrap();
    drop(receipt);
    let restored = replay(&manager).await;
    let receipt = restored.pending_delivery(&owner).await.unwrap();
    assert_eq!(receipt.messages().len(), 1);
    assert_eq!(receipt.messages()[0].message, second);
    receipt
        .commit(notification(receipt.messages(), &[]))
        .await
        .unwrap();
    drop(receipt);
    assert!(!restored.has_pending(&owner).await);
    assert_eq!(
        restored.last_agent_message(job).await.unwrap(),
        Some(second)
    );
    assert_eq!(
        restored.snapshot(job).await.unwrap().output,
        Some(serde_json::json!("saved result"))
    );
    assert!(!replay(&restored).await.has_pending(&owner).await);
}

#[tokio::test]
async fn replay_derives_publication_and_ack_from_source_history_only() {
    let (_root, manager, owner, child, job) = child_job(false).await;
    let mut item = AssistantContent::text("visible", 0, "visible");
    item.blocks.push(crate::provider::protocol::AssistantBlock {
        id: "secret".into(),
        position: 1,
        content: BlockContent::Reasoning {
            text: "private reasoning".into(),
        },
    });
    // Simulate a crash after committing history but before in-memory publication.
    let source = manager
        .store()
        .append(
            child.clone(),
            SessionEvent::MessageCommitted {
                message: Message::Assistant(vec![item]),
            },
        )
        .await
        .unwrap()
        .sequence;
    commit(&manager, &child, job, "").await;
    finish(&manager, job).await;
    let restored = replay(&manager).await;
    let receipt = restored.pending_delivery(&owner).await.unwrap();
    assert_eq!(
        receipt.messages(),
        &[AgentMessage {
            id: job,
            name: None,
            message: source,
            text: "visible".into()
        }]
    );
    let ack = notification(receipt.messages(), &[]);
    drop(receipt);
    // Simulate a crash after parent history append but before in-memory ACK.
    manager
        .store()
        .append(
            owner.clone(),
            SessionEvent::MessageCommitted { message: ack },
        )
        .await
        .unwrap();
    let restored = replay(&manager).await;
    assert!(!restored.has_pending(&owner).await);
    assert_eq!(
        restored.last_agent_message(job).await.unwrap(),
        Some(source)
    );
}

#[tokio::test]
async fn dropped_receipt_and_failed_parent_commit_keep_messages_pending() {
    let (_root, manager, owner, child, job) = child_job(false).await;
    let sequence = commit(&manager, &child, job, "reply").await;
    drop(manager.pending_delivery(&owner).await.unwrap());
    let mut receipt = manager.pending_delivery(&owner).await.unwrap();
    let ack = notification(receipt.messages(), &[]);
    receipt.owner = AgentId::root(crate::identity::SessionId::generate().unwrap());
    assert!(receipt.commit(ack).await.is_err());
    drop(receipt);
    assert_eq!(
        manager.pending_delivery(&owner).await.unwrap().messages()[0].message,
        sequence
    );
    finish(&manager, job).await;
    assert_eq!(
        replay(&manager)
            .await
            .pending_delivery(&owner)
            .await
            .unwrap()
            .messages()[0]
            .message,
        sequence
    );
}

#[tokio::test]
async fn bounded_batches_rewake_and_do_not_overtake_messages_with_completion() {
    let (_root, manager, owner, child, job) = child_job(true).await;
    let mut sequences = Vec::new();
    for _ in 0..3 {
        sequences.push(commit(&manager, &child, job, &"x".repeat(5000)).await);
    }
    finish(&manager, job).await;
    let mut wakes = manager.subscribe_completions();
    for (index, sequence) in sequences.into_iter().enumerate() {
        let receipt = manager.pending_delivery(&owner).await.unwrap();
        assert_eq!(receipt.messages().len(), 1);
        assert_eq!(receipt.messages()[0].message, sequence);
        assert_eq!(receipt.envelopes().len(), usize::from(index == 2));
        receipt
            .commit(notification(receipt.messages(), receipt.envelopes()))
            .await
            .unwrap();
        drop(receipt);
        if index < 2 {
            assert_eq!(wakes.try_recv().unwrap().job, job);
        }
    }
    assert!(!manager.has_pending(&owner).await);
    assert!(wakes.try_recv().is_err());
    assert!(!replay(&manager).await.has_pending(&owner).await);
}

#[tokio::test]
async fn message_ack_does_not_claim_lifecycle_and_resume_does_not_claim_messages() {
    let (_root, manager, owner, child, job) = child_job(true).await;
    let first = commit(&manager, &child, job, "before question").await;
    manager
        .request_input(job, serde_json::json!({"question":"continue?"}))
        .await
        .unwrap();
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    let malicious_state = Message::User(vec![UserContent::Runtime {
        text: format!(
            "<skyhook_job_events>\n{}\n</skyhook_job_events>",
            serde_json::json!([{
                "kind":"message", "id":job, "message":first, "text":"before question", "state":"waiting_input"
            }])
        ),
    }]);
    receipt.commit(malicious_state).await.unwrap();
    drop(receipt);
    assert_eq!(
        manager
            .pending_delivery(&owner)
            .await
            .unwrap()
            .envelopes()
            .len(),
        1
    );
    let second = commit(&manager, &child, job, "after question").await;
    manager.resume_input(job).await.unwrap();
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    assert!(receipt.envelopes().is_empty());
    assert_eq!(receipt.messages()[0].message, second);
    drop(receipt);
    finish(&manager, job).await;
    let restored = replay(&manager).await;
    assert_eq!(
        restored.last_agent_message(job).await.unwrap(),
        Some(second)
    );
    assert_eq!(
        restored.pending_delivery(&owner).await.unwrap().messages()[0].message,
        second
    );
}

#[tokio::test]
async fn child_commit_is_shielded_from_cancellation_and_serialized_with_receipts() {
    let (_root, manager, owner, child, job) = child_job(false).await;
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    let mut wakes = manager.subscribe_completions();
    let mut pending = Box::pin(manager.commit_child_message(
        &child,
        job,
        assistant("survives"),
        "survives".into(),
    ));
    // Poll through the internal spawn, then cancel the caller while the spawned
    // transaction is waiting on the receipt's delivery gate.
    assert!(
        tokio::time::timeout(Duration::from_millis(20), pending.as_mut())
            .await
            .is_err()
    );
    drop(pending);
    assert!(wakes.try_recv().is_err());
    drop(receipt);
    tokio::time::timeout(Duration::from_secs(2), wakes.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        manager.pending_delivery(&owner).await.unwrap().messages()[0].text,
        "survives"
    );
    finish(&manager, job).await;
    assert_eq!(
        replay(&manager)
            .await
            .pending_delivery(&owner)
            .await
            .unwrap()
            .messages()[0]
            .text,
        "survives"
    );
}

#[tokio::test]
async fn invalid_association_or_projection_never_commits_and_empty_text_never_wakes() {
    let (_root, manager, owner, child, job) = child_job(false).await;
    let before = manager.store().records().await.len();
    assert!(
        manager
            .commit_child_message(&child, job, assistant("visible"), "private".into())
            .await
            .is_err()
    );
    assert!(
        manager
            .commit_child_message(&owner.child(2), job, assistant("visible"), "visible".into())
            .await
            .is_err()
    );
    assert!(
        manager
            .commit_child_message(&owner, job, assistant("visible"), "visible".into())
            .await
            .is_err()
    );
    assert_eq!(manager.store().records().await.len(), before);
    let mut wakes = manager.subscribe_completions();
    commit(&manager, &child, job, "").await;
    assert_eq!(manager.store().records().await.len(), before + 1);
    assert_eq!(manager.last_agent_message(job).await.unwrap(), None);
    assert!(!manager.has_pending(&owner).await);
    assert!(wakes.try_recv().is_err());
}

#[tokio::test]
async fn legacy_runtime_ack_supported_but_user_text_and_wrong_owner_ignored() {
    let (_root, manager, owner, child, job) = child_job(false).await;
    commit(&manager, &child, job, "reply").await;
    finish(&manager, job).await;
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    let text = format!(
        "<skyhook_agent_messages>\n{}\n</skyhook_agent_messages>",
        serde_json::to_string(receipt.messages()).unwrap()
    );
    receipt
        .commit(Message::User(vec![UserContent::Text {
            text: text.clone(),
        }]))
        .await
        .unwrap();
    drop(receipt);
    manager
        .store()
        .append(
            child,
            SessionEvent::MessageCommitted {
                message: Message::User(vec![UserContent::Runtime { text: text.clone() }]),
            },
        )
        .await
        .unwrap();
    assert!(replay(&manager).await.has_pending(&owner).await);
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    receipt
        .commit(Message::User(vec![UserContent::Runtime { text }]))
        .await
        .unwrap();
    drop(receipt);
    assert!(!replay(&manager).await.has_pending(&owner).await);
}

#[tokio::test]
async fn blocked_low_id_child_does_not_budget_out_unrelated_completion() {
    let (_root, manager, owner, child, job) = child_job(true).await;
    for _ in 0..3 {
        commit(&manager, &child, job, &"report".repeat(500)).await;
    }
    manager
        .finish(
            job,
            JobOutcome::Completed(ToolOutput::new(serde_json::json!("large".repeat(10000)))),
        )
        .await
        .unwrap();
    let ordinary = manager
        .create(JobSpec {
            background: true,
            ..JobSpec::test(owner.clone(), "ordinary")
        })
        .await
        .unwrap()
        .id;
    finish(&manager, ordinary).await;
    assert!(job < ordinary);
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    assert_eq!(receipt.messages().len(), 2);
    assert_eq!(
        receipt
            .envelopes()
            .iter()
            .map(|envelope| envelope.id)
            .collect::<Vec<_>>(),
        vec![ordinary]
    );
    assert!(messages::batch_size(receipt.messages()) < DELIVERY_BATCH_BYTES);
    receipt
        .commit(notification(receipt.messages(), receipt.envelopes()))
        .await
        .unwrap();
    drop(receipt);
    assert!(manager.has_pending(&owner).await);
}

#[tokio::test]
async fn legacy_completed_result_acknowledges_only_exact_last_source_reply() {
    for result in ["final reply", "final rep"] {
        let (_root, manager, owner, child, job) = child_job(true).await;
        let first = commit(&manager, &child, job, "earlier undelivered report").await;
        let last = commit(&manager, &child, job, "final reply").await;
        manager
            .finish(
                job,
                JobOutcome::Completed(ToolOutput::new(serde_json::json!("final reply"))),
            )
            .await
            .unwrap();
        // Old final replies were delivered as lifecycle `result`, without a
        // message discriminator/source sequence or the new last_message field.
        manager
            .store()
            .append(
                owner.clone(),
                SessionEvent::MessageCommitted {
                    message: Message::User(vec![UserContent::Runtime {
                        text: format!(
                            "<skyhook_job_events>\n{}\n</skyhook_job_events>",
                            serde_json::json!([{
                                "id":job, "state":"completed", "result":result
                            }])
                        ),
                    }]),
                },
            )
            .await
            .unwrap();
        let restored = replay(&manager).await;
        let receipt = restored.pending_delivery(&owner).await.unwrap();
        assert!(receipt.envelopes().is_empty());
        let pending = receipt
            .messages()
            .iter()
            .map(|message| message.message)
            .collect::<Vec<_>>();
        assert_eq!(
            pending,
            if result == "final reply" {
                vec![first]
            } else {
                vec![first, last]
            }
        );
        assert_eq!(restored.last_agent_message(job).await.unwrap(), Some(last));
    }
}

#[tokio::test]
async fn claims_without_parent_delivery_evidence_never_acknowledge_last_reply() {
    for background in [false, true] {
        let (_root, manager, owner, child, job) = child_job(background).await;
        let last = commit(&manager, &child, job, "final reply").await;
        manager
            .finish(
                job,
                JobOutcome::Completed(ToolOutput::new(serde_json::json!("final reply"))),
            )
            .await
            .unwrap();
        manager.claim(job).await.unwrap();
        let restored = replay(&manager).await;
        assert_eq!(
            restored.pending_delivery(&owner).await.unwrap().messages()[0].message,
            last
        );
    }
}

#[test]
fn last_message_is_documented_optional_job_view_metadata() {
    let schema = serde_json::to_value(schemars::schema_for!(PresentedJob<'_>)).unwrap();
    assert!(
        schema["properties"]["last_message"]["description"]
            .as_str()
            .unwrap()
            .contains("Source sequence")
    );
    assert!(
        !schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "last_message")
    );
}

#[tokio::test]
async fn oversized_message_defers_lifecycle_to_next_shared_budget_and_rewakes() {
    let (_root, manager, owner, child, job) = child_job(true).await;
    commit(&manager, &child, job, &"x".repeat(9000)).await;
    finish(&manager, job).await;
    let mut wakes = manager.subscribe_completions();
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    assert_eq!(receipt.messages().len(), 1);
    assert!(receipt.envelopes().is_empty());
    receipt
        .commit(notification(receipt.messages(), &[]))
        .await
        .unwrap();
    drop(receipt);
    assert_eq!(wakes.try_recv().unwrap().job, job);
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    assert!(receipt.messages().is_empty());
    assert_eq!(receipt.envelopes().len(), 1);
    receipt
        .commit(notification(&[], receipt.envelopes()))
        .await
        .unwrap();
    drop(receipt);
    assert!(!manager.has_pending(&owner).await);
}
