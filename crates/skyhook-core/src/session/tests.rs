use super::{fixture::started, *};

async fn fresh() -> (tempfile::TempDir, SessionStore, SessionId, AgentId) {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::create(root.path()).await.unwrap();
    let id = store.id();
    let agent = started(&store, root.path()).await;
    (root, store, id, agent)
}

#[tokio::test]
async fn accepted_append_survives_lost_waiter_and_close_drains() {
    let (root, store, id, agent) = fresh().await;
    let mut events = store.subscribe();
    let (reached, resume) = store.pause_append_at(AppendBoundary::Write).await;
    let accepted = store.accept_append(agent.clone(), SessionEvent::AgentInterrupted);
    let accepted = accepted.await.unwrap();
    let identity = accepted.identity();
    reached.await.unwrap();
    assert!(
        events.try_recv().is_err(),
        "publication must follow the commit"
    );
    drop(accepted);
    let closing = tokio::spawn({
        let store = store.clone();
        async move { store.close().await }
    });
    tokio::task::yield_now().await;
    assert!(!closing.is_finished(), "close must drain accepted work");
    resume.send(()).unwrap();
    closing.await.unwrap().unwrap();
    let record = events.recv().await.unwrap();
    assert_eq!(record.sequence, identity.sequence);
    assert_eq!(store.records().await.last(), Some(&record));
    let closed = store.append(agent, SessionEvent::AgentCompleted).await;
    assert!(matches!(closed, Err(SessionError::Closed)));
    drop(store);
    let (_, replay) = SessionStore::open(root.path(), id).await.unwrap();
    assert_eq!(replay.last(), Some(&record));
}

/// Failures after acceptance poison the writer with the exact recovery identity;
/// reopening resolves the append from what the database actually committed.
#[tokio::test]
async fn failures_poison_the_writer_and_reopen_resolves_the_commit() {
    for (fault, durable) in [
        (CommitFault::RollBack, false),
        (CommitFault::ReportAfterCommit, true),
        (CommitFault::Panic, false),
    ] {
        let (root, store, id, agent) = fresh().await;
        let before = store.records().await.len();
        store.fault_next_commit(fault).await;
        let accepted = store.accept_append(agent.clone(), SessionEvent::AgentInterrupted);
        let accepted = accepted.await.unwrap();
        let identity = accepted.identity();
        let recovery_is = |result: Result<_, SessionError>, indeterminate: bool| match result {
            Err(SessionError::AppendIndeterminate(recovery)) if indeterminate => {
                recovery.identity == identity
            }
            Err(SessionError::AppendUnavailable(recovery)) if !indeterminate => {
                recovery.identity == identity
            }
            _ => false,
        };
        assert!(recovery_is(accepted.committed().await.map(drop), true));
        assert_eq!(store.records().await.len(), before);
        assert!(recovery_is(
            store.reconciled_records().await.map(drop),
            false
        ));
        let retry = store
            .append(agent.clone(), SessionEvent::AgentCompleted)
            .await;
        assert!(recovery_is(retry.map(drop), false));
        assert!(matches!(
            store.close().await,
            Err(SessionError::AppendUnavailable(_))
        ));
        drop(store);
        let (reopened, replay) = SessionStore::open(root.path(), id).await.unwrap();
        assert_eq!(replay.len(), before + usize::from(durable));
        assert_eq!(
            replay.iter().any(|record| record.id == identity.event),
            durable
        );
        assert_eq!(reopened.reconciled_records().await.unwrap(), replay);
        let next = reopened
            .append(agent, SessionEvent::AgentCompleted)
            .await
            .unwrap();
        assert_eq!(next.sequence, replay.len() as u64 + 1);
        assert_ne!(next.id, identity.event);
    }
}

#[tokio::test]
async fn rejected_appends_are_definite_and_leave_the_writer_healthy() {
    let (_root, store, _id, agent) = fresh().await;
    let before = store.records().await;
    // A child that never started owns no rows; its entry is rejected by the schema.
    let unknown = store
        .append(agent.child(9), SessionEvent::AgentCompleted)
        .await;
    assert!(matches!(
        unknown,
        Err(SessionError::Database(DbError::Sql(_)) | SessionError::Database(DbError::Rejected(_)))
    ));
    assert_eq!(store.records().await, before);
    let next = store
        .append(agent, SessionEvent::AgentCompleted)
        .await
        .unwrap();
    assert_eq!(next.sequence, before.len() as u64 + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acknowledged_append_releases_owned_store_before_immediate_reopen() {
    for _ in 0..32 {
        let (_root, store, id, agent) = fresh().await;
        let root = _root.path().to_path_buf();
        store
            .append(agent, SessionEvent::AgentInterrupted)
            .await
            .unwrap();
        drop(store);
        let (_, records) = SessionStore::open(&root, id).await.unwrap();
        assert_eq!(records.len(), 3);
    }
}

#[tokio::test]
async fn owner_lock_excludes_competing_writers_but_not_readers() {
    let (root, store, id, agent) = fresh().await;
    store
        .append(agent.clone(), SessionEvent::AgentInterrupted)
        .await
        .unwrap();
    let path = root.path().join(id.to_string()).join(DATABASE_FILE);
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        SessionStore::open(root.path(), id).await,
        Err(SessionError::AlreadyOpen(locked)) if locked == id
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    // Readers see committed records while the owner is live.
    let read = SessionStore::read_records(root.path(), id).await.unwrap();
    assert_eq!(read, store.records().await);
    // Model a descriptor briefly inherited by a concurrently spawned process.
    let inherited_lock = store.inherit_lock().await.unwrap();
    drop(store);
    let (store, records) = SessionStore::open(root.path(), id).await.unwrap();
    drop(inherited_lock);
    assert_eq!(records, read);
    let appended = store
        .append(agent, SessionEvent::AgentCompleted)
        .await
        .unwrap();
    assert_eq!(store.records().await.last(), Some(&appended));
}

#[tokio::test]
async fn unsupported_databases_are_rejected_without_being_rewritten() {
    let (root, store, id, _agent) = fresh().await;
    drop(store);
    let path = root.path().join(id.to_string()).join(DATABASE_FILE);
    for pragma in ["user_version = 3", "application_id = 1"] {
        {
            let raw = libsql::Builder::new_local(&path).build();
            let raw = futures_util::FutureExt::now_or_never(raw).unwrap().unwrap();
            let connection = raw.connect().unwrap();
            futures_util::FutureExt::now_or_never(
                connection.execute_batch(&format!("PRAGMA {pragma}")),
            )
            .unwrap()
            .unwrap();
        }
        let archive = std::fs::read(&path).unwrap();
        assert!(matches!(
            SessionStore::open(root.path(), id).await,
            Err(SessionError::UnsupportedVersion(_))
        ));
        assert!(matches!(
            SessionStore::read_records(root.path(), id).await,
            Err(SessionError::UnsupportedVersion(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), archive);
    }
}

#[tokio::test]
async fn ephemeral_store_keeps_events_and_images_in_memory() {
    let root = tempfile::tempdir().unwrap();
    let store = SessionStore::create_ephemeral(root.path()).await.unwrap();
    let directory = store.directory().to_path_buf();
    let agent = started(&store, root.path()).await;
    store
        .append(agent, SessionEvent::AgentInterrupted)
        .await
        .unwrap();
    assert_eq!(store.records().await.len(), 3);
    let png = crate::tests::png(b"image");
    let image = store
        .store_image(Some("image.png".to_owned()), &png)
        .await
        .unwrap();
    let limit = crate::media::MAX_IMAGE_BYTES as usize;
    assert_eq!(
        store.read_blob(&image.blob, limit).await.unwrap(),
        png.bytes()
    );
    for file in [DATABASE_FILE, LOCK_FILE] {
        assert!(!directory.join(file).exists());
    }
}
