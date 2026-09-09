use super::*;
use crate::provider::protocol::{Message, UserContent};

async fn completed_job() -> (tempfile::TempDir, JobManager, AgentId, JobId) {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::create(root.path()).await.unwrap();
    let owner = AgentId::root(store.id());
    let manager = JobManager::new(store);
    let lease = manager
        .create(JobSpec {
            background: true,
            ..JobSpec::test(owner.clone(), "test")
        })
        .await
        .unwrap();
    manager
        .finish(
            lease.id,
            JobOutcome::Completed(ToolOutput::new(serde_json::json!("answer"))),
        )
        .await
        .unwrap();
    (root, manager, owner, lease.id)
}

fn notification(job: JobId, state: JobState) -> Message {
    Message::User(vec![UserContent::Runtime {
        text: format!(
            "<skyhook_job_events>\n{}\n</skyhook_job_events>",
            serde_json::json!([{"id":job,"state":state}]),
        ),
    }])
}

#[tokio::test]
async fn delivery_snapshot_drop_keeps_output_pending_and_explicit_claims_work() {
    let (_root, manager, owner, job) = completed_job().await;
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    assert_eq!(receipt.envelopes().len(), 1);
    assert_eq!(receipt.envelopes()[0].id, job);
    assert!(manager.has_pending(&owner).await);
    assert!(
        !manager
            .store()
            .records()
            .await
            .iter()
            .any(|record| matches!(
                record.event,
                SessionEvent::JobInjected { .. } | SessionEvent::JobClaimed { .. }
            ))
    );
    drop(receipt);
    manager.claim(job).await.unwrap();
    assert!(
        manager
            .pending_delivery(&owner)
            .await
            .unwrap()
            .envelopes()
            .is_empty()
    );
    // Explicit output access remains available after claiming.
    assert_eq!(
        manager.snapshot(job).await.unwrap().output,
        Some(serde_json::json!("answer"))
    );
}

#[tokio::test]
async fn delivery_snapshot_serializes_explicit_claim_until_drop() {
    let (_root, manager, owner, job) = completed_job().await;
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    let claim = manager.claim(job);
    tokio::pin!(claim);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), claim.as_mut())
            .await
            .is_err()
    );
    drop(receipt);
    claim.await.unwrap();
    assert!(!manager.has_pending(&owner).await);
}

#[tokio::test]
async fn delivery_commit_wins_over_concurrent_explicit_claim_without_duplicate_ack() {
    let (_root, manager, owner, job) = completed_job().await;
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    let claim = manager.claim(job);
    tokio::pin!(claim);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), claim.as_mut())
            .await
            .is_err()
    );
    receipt
        .commit(notification(job, JobState::Completed))
        .await
        .unwrap();
    // The outer transaction can acknowledge child progress before releasing the gate.
    assert!(
        tokio::time::timeout(Duration::from_millis(20), claim.as_mut())
            .await
            .is_err()
    );
    drop(receipt);
    claim.await.unwrap();
    assert!(!manager.has_pending(&owner).await);
    assert!(
        !manager
            .store()
            .records()
            .await
            .iter()
            .any(|record| matches!(record.event, SessionEvent::JobClaimed { .. }))
    );
}

#[tokio::test]
async fn delivery_cancel_before_commit_releases_gate_and_keeps_batch_pending() {
    let (_root, manager, owner, job) = completed_job().await;
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    let task = tokio::spawn(async move {
        std::future::pending::<()>().await;
        receipt.commit(notification(job, JobState::Completed)).await
    });
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    assert_eq!(receipt.envelopes()[0].id, job);
}

#[tokio::test]
async fn delivery_failed_parent_append_does_not_acknowledge() {
    let (_root, manager, owner, job) = completed_job().await;
    let other_root = tempfile::tempdir().unwrap();
    let other_store = SessionStore::create(other_root.path()).await.unwrap();
    let mut receipt = manager.pending_delivery(&owner).await.unwrap();
    // Deterministically fail SessionStore::append before committing anything.
    receipt.owner = AgentId::root(other_store.id());
    assert!(
        receipt
            .commit(notification(job, JobState::Completed))
            .await
            .is_err()
    );
    drop(receipt);
    assert_eq!(
        manager.pending_delivery(&owner).await.unwrap().envelopes()[0].id,
        job
    );
}

#[tokio::test]
async fn delivery_commit_acknowledges_live_and_replay_without_job_injected_event() {
    let (_root, manager, owner, job) = completed_job().await;
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    let sequence = receipt
        .commit(notification(job, JobState::Completed))
        .await
        .unwrap();
    drop(receipt);
    assert!(!manager.has_pending(&owner).await);
    let records = manager.store().records().await;
    assert_eq!(records.last().unwrap().sequence, sequence);
    assert!(matches!(
        records.last().unwrap().event,
        SessionEvent::MessageCommitted { .. }
    ));
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.event, SessionEvent::JobInjected { .. }))
    );
    let restored = JobManager::restore(manager.store().clone(), &records)
        .await
        .unwrap();
    assert!(!restored.has_pending(&owner).await);
    assert_eq!(
        restored.snapshot(job).await.unwrap().output,
        Some(serde_json::json!("answer"))
    );
}

#[tokio::test]
async fn delivery_replay_closes_crash_between_parent_commit_and_memory_ack() {
    let (_root, manager, owner, job) = completed_job().await;
    manager
        .store()
        .append(
            owner.clone(),
            SessionEvent::MessageCommitted {
                message: notification(job, JobState::Completed),
            },
        )
        .await
        .unwrap();
    // Simulate a host crash: only the parent message reached the journal.
    assert!(manager.has_pending(&owner).await);
    let restored = JobManager::restore(manager.store().clone(), &manager.store().records().await)
        .await
        .unwrap();
    assert!(!restored.has_pending(&owner).await);
}

#[tokio::test]
async fn delivery_replay_does_not_acknowledge_user_text_wrong_owner_or_wrong_state() {
    for mode in ["text", "parent_input", "wrong_owner", "wrong_state"] {
        let (_root, manager, owner, job) = completed_job().await;
        let Message::User(mut content) = notification(job, JobState::Completed) else {
            unreachable!()
        };
        let UserContent::Runtime { text } = content.remove(0) else {
            unreachable!()
        };
        let message = match mode {
            "text" => Message::User(vec![UserContent::Text { text }]),
            "parent_input" => Message::User(vec![UserContent::ParentInput { text }]),
            "wrong_state" => notification(job, JobState::WaitingInput),
            _ => notification(job, JobState::Completed),
        };
        let author = if mode == "wrong_owner" {
            owner.child(1)
        } else {
            owner.clone()
        };
        manager
            .store()
            .append(author, SessionEvent::MessageCommitted { message })
            .await
            .unwrap();
        let restored =
            JobManager::restore(manager.store().clone(), &manager.store().records().await)
                .await
                .unwrap();
        assert!(restored.has_pending(&owner).await, "{mode}");
    }
}

#[tokio::test]
async fn delivery_replay_old_notification_does_not_acknowledge_resumed_completion() {
    let (_root, manager, owner, job) = completed_job().await;
    manager
        .pending_delivery(&owner)
        .await
        .unwrap()
        .commit(notification(job, JobState::Completed))
        .await
        .unwrap();
    manager
        .store()
        .append(
            owner.clone(),
            SessionEvent::JobStateChanged {
                job,
                state: JobState::Running,
            },
        )
        .await
        .unwrap();
    manager
        .store()
        .append(
            owner.clone(),
            SessionEvent::JobFinished {
                job,
                state: JobState::Completed,
                output_path: Some(
                    std::path::PathBuf::from("jobs")
                        .join(job.to_string())
                        .join("document.json"),
                ),
                error: None,
                images: Vec::new(),
                denial: None,
            },
        )
        .await
        .unwrap();
    let restored = JobManager::restore(manager.store().clone(), &manager.store().records().await)
        .await
        .unwrap();
    assert!(restored.has_pending(&owner).await);
}

#[tokio::test]
async fn delivery_question_ack_does_not_suppress_later_terminal_result_after_replay() {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::create(root.path()).await.unwrap();
    let owner = AgentId::root(store.id());
    let manager = JobManager::new(store);
    let job = manager
        .create(JobSpec::test(owner.clone(), "agent"))
        .await
        .unwrap()
        .id;
    manager.transition(job, JobState::Running).await.unwrap();
    manager
        .request_input(job, serde_json::json!({"question":"choose"}))
        .await
        .unwrap();
    manager
        .pending_delivery(&owner)
        .await
        .unwrap()
        .commit(notification(job, JobState::WaitingInput))
        .await
        .unwrap();
    manager.resume_input(job).await.unwrap();
    manager
        .finish(
            job,
            JobOutcome::Completed(ToolOutput::new(serde_json::json!("done"))),
        )
        .await
        .unwrap();
    assert!(manager.has_pending(&owner).await);
    let restored = JobManager::restore(manager.store().clone(), &manager.store().records().await)
        .await
        .unwrap();
    assert!(restored.has_pending(&owner).await);
}

#[tokio::test]
async fn delivery_cancelled_question_gets_a_new_terminal_delivery() {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::create(root.path()).await.unwrap();
    let owner = AgentId::root(store.id());
    let manager = JobManager::new(store);
    let job = manager
        .create(JobSpec::test(owner.clone(), "agent"))
        .await
        .unwrap()
        .id;
    manager.transition(job, JobState::Running).await.unwrap();
    manager
        .request_input(job, serde_json::json!({"question":"choose"}))
        .await
        .unwrap();
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    receipt
        .commit(notification(job, JobState::WaitingInput))
        .await
        .unwrap();
    let finish = manager.finish(job, JobOutcome::Cancelled);
    tokio::pin!(finish);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), finish.as_mut())
            .await
            .is_err()
    );
    drop(receipt);
    finish.await.unwrap();
    assert!(manager.has_pending(&owner).await);
    let restored = JobManager::restore(manager.store().clone(), &manager.store().records().await)
        .await
        .unwrap();
    let receipt = restored.pending_delivery(&owner).await.unwrap();
    assert_eq!(receipt.envelopes()[0].state, JobState::Cancelled);
}

#[tokio::test]
async fn delivery_empty_receipt_keeps_gate_through_other_notification_acknowledgements() {
    let (_root, manager, owner, job) = completed_job().await;
    manager.claim(job).await.unwrap();
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    assert!(receipt.envelopes().is_empty());
    receipt
        .commit(Message::User(vec![UserContent::Runtime {
            text: "<skyhook_child_messages>\n[]\n</skyhook_child_messages>".into(),
        }]))
        .await
        .unwrap();
    let next = manager.pending_delivery(&owner);
    tokio::pin!(next);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), next.as_mut())
            .await
            .is_err()
    );
    drop(receipt);
    assert!(next.await.unwrap().envelopes().is_empty());
}

#[tokio::test]
async fn delivery_missing_notification_in_committed_message_stays_pending_live_and_replay() {
    let (_root, manager, owner, _job) = completed_job().await;
    let receipt = manager.pending_delivery(&owner).await.unwrap();
    receipt
        .commit(Message::User(vec![UserContent::Text {
            text: "new user input".into(),
        }]))
        .await
        .unwrap();
    drop(receipt);
    assert!(manager.has_pending(&owner).await);
    let restored = JobManager::restore(manager.store().clone(), &manager.store().records().await)
        .await
        .unwrap();
    assert!(restored.has_pending(&owner).await);
}
