//! Independent public-boundary tests for actual durable queue reopen.

use super::{tests::*, *};

async fn durable_harness(root: &Path) -> Harness {
    let sessions = root.join("sessions");
    let builder = test_builder(root, &sessions, Arc::new(HangingProvider), true);
    builder.build().await.unwrap()
}

async fn reopen(harness: &Harness, session: SessionHandle) -> SessionHandle {
    let id = session.id();
    session.shutdown().await.unwrap();
    drop(session);
    // A completed runtime worker can release its last Arc after publishing its
    // completion. A real new SessionStore still must acquire the filesystem lock.
    bounded(async {
        loop {
            match harness.resume_session(id).await {
                Ok(session) => return session,
                Err(HarnessError::Session(SessionError::AlreadyOpen(_))) => {
                    tokio::task::yield_now().await
                }
                Err(error) => panic!("reopen failed: {error}"),
            }
        }
    })
    .await
}

async fn prepared_prompt(session: &SessionHandle, text: &str) -> PreparedQueuedPrompt {
    let prompt = QueuedPrompt {
        text: text.into(),
        attachments: vec![],
        options: PromptOptions::default(),
        token: QueuedPromptToken::new().unwrap(),
    };
    session.prepare_queued_prompt(prompt).await.unwrap()
}

async fn committed_user_messages(session: &SessionHandle) -> usize {
    let records = session.runtime.store.records().await;
    count!(&records, SessionEvent::MessageCommitted { message } if matches!(message, Message::User(_)))
}

#[tokio::test]
async fn durable_queue_reopens_immutable_image_draft_without_external_file() {
    let root = tempfile::tempdir().unwrap();
    let harness = durable_harness(root.path()).await;
    let session = harness.new_session().await.unwrap();
    // The attachment names a file that never exists: only the stored blob may back the draft.
    let png = crate::tests::png(b"durable imported image fixture");
    let attachment = crate::media::Attachment::Image {
        file: Some(root.path().join("queued.png")),
        image: png.clone(),
    };
    let token = QueuedPromptToken::new().unwrap();
    let text = "immutable intended text".into();
    let prepared = QueuedPrompt {
        text,
        attachments: vec![attachment.clone()],
        options: PromptOptions::default(),
        token,
    };
    let prepared = session.prepare_queued_prompt(prepared).await.unwrap();
    let identity = prepared.identity();
    let content = prepared.content().to_vec();
    let UserContent::Attachment {
        attachment: crate::media::AttachmentRef::Image(image),
    } = &content[1]
    else {
        panic!("imported image")
    };
    let blob = async |session: &SessionHandle| {
        let limit = crate::media::MAX_IMAGE_BYTES as usize;
        let store = &session.runtime.store;
        store.read_blob(&image.blob, limit).await.unwrap()
    };
    assert_eq!(blob(&session).await, png.bytes());
    let dispatched = committed_user_messages(&session).await;
    // saving a paused draft must not dispatch a model turn
    assert_eq!(dispatched, 0);
    drop(prepared);
    let session = reopen(&harness, session).await;
    let mut recovered = session.recover_queued_prompts().await.unwrap();
    assert_eq!(recovered.len(), 1);
    let recovered = recovered.pop().unwrap();
    assert_eq!(recovered.submission, identity);
    assert_eq!(recovered.content, content);
    assert_eq!(recovered.attachments, vec![attachment]);
    let RecoveredQueuedPromptState::Retry(permit) = recovered.state else {
        panic!("undispatched durable intent grants an exclusive retry permit")
    };
    let duplicate = session.recover_queued_prompts().await.unwrap();
    let state = &duplicate[0].state;
    assert!(matches!(state, RecoveredQueuedPromptState::Unresolved));
    assert_eq!(blob(&session).await, png.bytes());
    let committed = session.enqueue_prepared_queued_prompts(vec![permit]).await;
    let committed = committed.into_iter().next().unwrap().unwrap();
    assert_eq!(committed.submission, identity);
    assert_eq!(committed_user_messages(&session).await, 1);
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn durable_queue_reopens_committed_before_client_ack_and_retires_durably() {
    let root = tempfile::tempdir().unwrap();
    let harness = durable_harness(root.path()).await;
    let session = harness.new_session().await.unwrap();
    let prepared = prepared_prompt(&session, "committed once").await;
    let committed = session
        .enqueue_prepared_queued_prompts(vec![prepared])
        .await;
    let committed = committed.into_iter().next().unwrap().unwrap();
    let session = reopen(&harness, session).await;
    let recovered = session.recover_queued_prompts().await.unwrap();
    assert_eq!(recovered.len(), 1);
    let RecoveredQueuedPromptState::Committed(replayed) = &recovered[0].state else {
        panic!("message commitment survives lost caller acknowledgment")
    };
    assert_eq!(replayed.submission, committed.submission);
    assert_eq!(replayed.append, committed.append);
    let acknowledged = session.acknowledge_queued_prompt(committed.submission);
    acknowledged.await.unwrap();
    let session = reopen(&harness, session).await;
    assert!(session.recover_queued_prompts().await.unwrap().is_empty());
    assert_eq!(committed_user_messages(&session).await, 1);
    session.shutdown().await.unwrap();
}

/// Only a saved draft's holder decides its fate: a retained draft resurfaces on
/// reopen, while an abandoned draft does not.
#[tokio::test]
async fn abandoned_drafts_do_not_resurface_on_reopen() {
    let root = tempfile::tempdir().unwrap();
    let harness = durable_harness(root.path()).await;
    let session = harness.new_session().await.unwrap();
    let conflict = |result: Result<(), HarnessError>| match result {
        Err(HarnessError::Queue(conflict)) => conflict,
        other => panic!("expected a queue conflict, got {other:?}"),
    };
    let retained = prepared_prompt(&session, "retained").await.identity();
    let abandoned = prepared_prompt(&session, "abandoned").await;
    let attempt = abandoned.cancellation_handle();
    let held = session.acknowledge_queued_prompt(attempt.identity()).await;
    assert_eq!(conflict(held), QueueConflict::Held);
    drop(abandoned);
    let uncommitted = session.acknowledge_queued_prompt(attempt.identity()).await;
    assert_eq!(conflict(uncommitted), QueueConflict::Uncommitted);
    session.abandon_queued_prompt(&attempt).await.unwrap();
    session.abandon_queued_prompt(&attempt).await.unwrap();

    let session = reopen(&harness, session).await;
    let recovered = session.recover_queued_prompts().await.unwrap();
    let listed = recovered
        .iter()
        .map(|row| row.submission)
        .collect::<Vec<_>>();
    assert_eq!(listed, [retained]);
    session.shutdown().await.unwrap();
}

/// Within one runtime a dropped draft is never handed out by a scan: only the
/// holder of its cancellation handle may reclaim it, while it stays uncommitted.
#[tokio::test]
async fn same_runtime_drafts_are_released_to_their_handle_not_the_scan() {
    let root = tempfile::tempdir().unwrap();
    let harness = durable_harness(root.path()).await;
    let session = harness.new_session().await.unwrap();
    let conflict = |result: Result<PreparedQueuedPrompt, HarnessError>| match result {
        Err(HarnessError::Queue(conflict)) => conflict,
        other => panic!("expected a queue conflict, got {other:?}"),
    };
    let only_state = async |session: &SessionHandle| {
        let mut rows = session.recover_queued_prompts().await.unwrap();
        assert_eq!(rows.len(), 1);
        rows.pop().unwrap().state
    };
    let prepared = prepared_prompt(&session, "released").await;
    let (attempt, content) = (prepared.cancellation_handle(), prepared.content().to_vec());
    let held = session.reclaim_queued_prompt(&attempt).await;
    assert_eq!(conflict(held), QueueConflict::Held);
    drop(prepared);
    let state = only_state(&session).await;
    assert!(matches!(state, RecoveredQueuedPromptState::Released));
    let permit = session.reclaim_queued_prompt(&attempt).await.unwrap();
    assert_eq!(
        (permit.identity(), permit.content()),
        (attempt.identity(), &content[..])
    );
    let state = only_state(&session).await;
    assert!(matches!(state, RecoveredQueuedPromptState::Unresolved));
    let duplicate = session.reclaim_queued_prompt(&attempt).await;
    assert_eq!(conflict(duplicate), QueueConflict::Held);
    let committed = session.enqueue_prepared_queued_prompts(vec![permit]).await;
    let committed = committed.into_iter().next().unwrap().unwrap();
    let late = session.reclaim_queued_prompt(&attempt).await;
    assert_eq!(conflict(late), QueueConflict::Committed);
    session
        .acknowledge_queued_prompt(committed.submission)
        .await
        .unwrap();
    let retired = session.reclaim_queued_prompt(&attempt).await;
    assert_eq!(conflict(retired), QueueConflict::NotReserved);
    let foreign = QueuedPromptToken::new().unwrap().cancellation_handle();
    let foreign = session.reclaim_queued_prompt(&foreign).await;
    assert_eq!(conflict(foreign), QueueConflict::NotReserved);

    // The row's existing handle also cancels the reclaimed permit, so abandon wins.
    let prepared = prepared_prompt(&session, "abandoned after reclaim").await;
    let attempt = prepared.cancellation_handle();
    drop(prepared);
    let permit = session.reclaim_queued_prompt(&attempt).await.unwrap();
    session.abandon_queued_prompt(&attempt).await.unwrap();
    let rejected = session.enqueue_prepared_queued_prompts(vec![permit]).await;
    assert!(rejected[0].is_err());

    // Shutdown refuses a new reclaim at once rather than waiting for its drain.
    let prepared = prepared_prompt(&session, "reclaimed during shutdown").await;
    let attempt = prepared.cancellation_handle();
    drop(prepared);
    session.shutdown().await.unwrap();
    let stopped = session.reclaim_queued_prompt(&attempt).await;
    assert!(matches!(stopped, Err(HarnessError::AgentStopped)));
    // A dropped draft from the previous process is again a retry permit.
    let session = reopen(&harness, session).await;
    let state = only_state(&session).await;
    assert!(matches!(state, RecoveredQueuedPromptState::Retry(_)));
    session.shutdown().await.unwrap();
}
