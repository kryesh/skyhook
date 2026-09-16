//! Creation ownership, admission, and pruning ordering tests.

use super::{tests::terminal, *};
use crate::session::AppendBoundary;

async fn count(jobs: &JobManager, matches: impl Fn(&SessionEvent) -> bool) -> usize {
    let records = jobs.store().records().await;
    records
        .iter()
        .filter(|record| matches(&record.event))
        .count()
}

async fn creation_count(jobs: &JobManager) -> usize {
    count(jobs, |event| {
        matches!(event, SessionEvent::JobCreated { .. })
    })
    .await
}

async fn claimed(jobs: &JobManager, agent: &AgentId, tool: &str) -> JobId {
    let id = jobs.test_create(JobSpec::test(agent.clone(), tool)).await;
    jobs.test_finish(id, serde_json::json!("done")).await;
    jobs.claim(id).await.unwrap();
    id
}

fn prune(jobs: &JobManager) -> tokio::task::JoinHandle<Result<usize, JobError>> {
    let jobs = jobs.clone();
    tokio::spawn(async move { jobs.prune_claimed().await })
}

fn child_of(parent: JobId, agent: &AgentId, tool: &str) -> JobSpec {
    JobSpec {
        parent: Some(parent),
        ..JobSpec::test(agent.clone(), tool)
    }
}

#[tokio::test]
async fn cancellation_before_creation_gate_has_no_admitted_work() {
    let (_root, jobs, agent) = crate::job::tests::runtime().await;
    let operation = jobs.inner.creation_operation.write().await;
    let creating = tokio::spawn({
        let (jobs, agent) = (jobs.clone(), agent.clone());
        async move { jobs.create(JobSpec::test(agent, "not-admitted")).await }
    });
    tokio::task::yield_now().await;
    creating.abort();
    assert!(matches!(creating.await, Err(error) if error.is_cancelled()));
    drop(operation);
    assert_eq!(creation_count(&jobs).await, 0);
    assert_eq!(
        jobs.test_create(JobSpec::test(agent, "first")).await.get(),
        1
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Create,
    Transition,
    Finish,
    Claim,
    Send,
    RequestInput,
    ResumeInput,
}

/// Creates job 1 (except for `Create`) in the state each operation requires.
async fn owned_job(op: Op) -> (tempfile::TempDir, JobManager, AgentId, JobId) {
    let (root, jobs, agent) = crate::job::tests::runtime().await;
    let id = JobId::new(1).unwrap();
    if op != Op::Create {
        let spec = JobSpec {
            accepts_input: true,
            ..JobSpec::test(agent.clone(), "owned")
        };
        assert_eq!(jobs.test_create(spec).await, id);
    }
    match op {
        Op::Claim | Op::Send => {
            let handler: ResumeHandler =
                Arc::new(|value, _| Box::pin(async move { Ok(ToolOutput::new(value.unwrap())) }));
            jobs.set_resume_handler(id, handler).await.unwrap();
            jobs.test_finish(id, serde_json::json!("previous")).await;
        }
        Op::RequestInput | Op::ResumeInput => {
            jobs.transition(id, JobState::Running).await.unwrap();
            if op == Op::ResumeInput {
                let question = serde_json::json!({"question":"before"});
                jobs.request_input(id, question).await.unwrap();
            }
        }
        Op::Create | Op::Transition | Op::Finish => {}
    }
    (root, jobs, agent, id)
}

async fn run(op: Op, jobs: JobManager, agent: AgentId, id: JobId) -> Result<(), JobError> {
    match op {
        Op::Create => jobs
            .create(JobSpec::test(agent, "abandoned"))
            .await
            .map(drop),
        Op::Transition => jobs.transition(id, JobState::Running).await,
        Op::Finish => {
            let output = ToolOutput::new(serde_json::json!({"text":"done"}));
            jobs.finish(id, JobOutcome::Completed(output)).await
        }
        Op::Claim => jobs.claim(id).await,
        Op::Send => jobs.send(id, serde_json::json!("resumed")).await,
        Op::RequestInput => {
            let question = serde_json::json!({"question":"after"});
            jobs.request_input(id, question).await
        }
        Op::ResumeInput => jobs.resume_input(id).await,
    }
}

/// Writer shielding alone is insufficient: every accepted append keeps its
/// ownership gate through a caller abort and publishes both durably and live
/// once the writer resumes. Flush/Sync are indistinguishable from Write here.
#[tokio::test]
async fn cancelled_callers_keep_accepted_publication_owned_at_append_boundaries() {
    use Op::*;
    let ops = [
        Create,
        Transition,
        Finish,
        Claim,
        Send,
        RequestInput,
        ResumeInput,
    ];
    for (op, boundary) in ops
        .into_iter()
        .flat_map(|op| [AppendBoundary::Write, AppendBoundary::Publication].map(|b| (op, b)))
    {
        let case = format!("{op:?} at {boundary:?}");
        let (_root, jobs, agent, id) = owned_job(op).await;
        let state = async |jobs: &JobManager| jobs.metadata(id).await.ok().map(|m| m.state);
        let delivery = async |jobs: &JobManager| jobs.inner.jobs.lock().await[&id].delivery;
        let before = state(&jobs).await;
        let (reached, resume) = jobs.store().pause_append_at(boundary).await;
        let caller = tokio::spawn(run(op, jobs.clone(), agent, id));
        reached.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        // Nothing is published live before the durable append completes, and
        // the cancelled caller must not release its publication gate.
        assert_eq!(state(&jobs).await, before, "{case}");
        let gate_held = match op {
            Create => jobs.inner.creation_operation.try_write().is_err(),
            Claim => {
                assert!(delivery(&jobs).await == DeliveryState::Pending);
                jobs.inner.delivery_operation.try_lock().is_err()
            }
            _ => jobs.operation(id).await.unwrap().try_lock().is_err(),
        };
        assert!(gate_held, "{case}");
        let drain = tokio::spawn({
            let jobs = jobs.clone();
            async move {
                jobs.drain_creations().await;
                jobs.drain_supervisors().await;
            }
        });
        tokio::task::yield_now().await;
        assert!(!drain.is_finished(), "{case}");
        resume.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), drain)
            .await
            .unwrap()
            .unwrap();
        let expected = match op {
            Create => JobState::Cancelled,
            Transition | ResumeInput => JobState::Running,
            Finish | Claim | Send => JobState::Completed,
            RequestInput => JobState::WaitingInput,
        };
        assert_eq!(state(&jobs).await, Some(expected), "{case}");
        let replay = jobs.test_replay().await;
        // Replay deliberately interrupts unfinished work; terminal results,
        // delivery, and durable transition records still agree with the owner.
        let durable = match op {
            Create | Finish | Claim | Send => state(&replay).await == Some(expected),
            Transition | RequestInput | ResumeInput => {
                count(&jobs, |event| {
                    matches!(event, SessionEvent::JobStateChanged { job, state } if *job == id && *state == expected)
                })
                .await
                    > 0
            }
        };
        assert!(durable, "{case}");
        if op == Claim {
            assert!(delivery(&jobs).await == DeliveryState::Claimed);
            assert!(delivery(&replay).await == DeliveryState::Claimed);
        }
        if op == Create {
            assert_eq!(creation_count(&jobs).await, 1);
            let finished = count(
                &jobs,
                |event| matches!(event, SessionEvent::JobFinished { job, .. } if *job == id),
            );
            assert_eq!(finished.await, 1);
        }
    }
}

#[tokio::test]
async fn transition_finish_and_claim_failed_appends_do_not_publish_live_success() {
    for op in [Op::Transition, Op::Finish, Op::Claim] {
        let (_root, jobs, agent, id) = owned_job(op).await;
        jobs.store()
            .fail_append_at(AppendBoundary::Publication)
            .await;
        assert!(matches!(
            run(op, jobs.clone(), agent, id).await,
            Err(JobError::Session(SessionError::AppendIndeterminate(_)))
        ));
        if op == Op::Claim {
            assert!(jobs.inner.jobs.lock().await[&id].delivery == DeliveryState::Pending);
        } else {
            assert_eq!(jobs.metadata(id).await.unwrap().state, JobState::Queued);
            assert!(jobs.operation(id).await.unwrap().try_lock().is_ok());
        }
    }
}

#[tokio::test]
async fn creation_pins_parent_against_prune_and_inherits_concurrent_cancellation() {
    let (_root, jobs, agent) = crate::job::tests::runtime().await;
    let parent = claimed(&jobs, &agent, "parent").await;
    let (reached, resume) = jobs
        .store()
        .pause_append_at(AppendBoundary::Publication)
        .await;
    let creating = tokio::spawn({
        let (jobs, spec) = (jobs.clone(), child_of(parent, &agent, "child"));
        async move { jobs.create(spec).await }
    });
    tokio::time::timeout(Duration::from_secs(3), reached)
        .await
        .unwrap()
        .unwrap();
    let mut pruning = prune(&jobs);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut pruning)
            .await
            .is_err()
    );
    // Cancellation does not need the creation gate or the session writer lock.
    let cancel = tokio::time::timeout(Duration::from_secs(1), jobs.cancel(parent));
    cancel.await.unwrap().unwrap();
    resume.send(()).unwrap();
    let child = creating.await.unwrap().unwrap();
    assert!(child.cancellation_token().is_cancelled());
    let pruned = tokio::time::timeout(Duration::from_secs(3), pruning)
        .await
        .unwrap();
    assert_eq!(pruned.unwrap().unwrap(), 1);
    assert!(matches!(
        jobs.metadata(parent).await,
        Err(JobError::Unknown(_))
    ));
    assert_eq!(terminal(&jobs, child.id()).await.state, JobState::Cancelled);
    assert_eq!(creation_count(&jobs).await, 2);
}

#[tokio::test]
async fn pruning_before_creation_rejects_parent_without_event_or_identity_gap() {
    let (_root, jobs, agent) = crate::job::tests::runtime().await;
    let parent = claimed(&jobs, &agent, "parent").await;
    assert_eq!(jobs.prune_claimed().await.unwrap(), 1);
    let rejected = jobs.create(child_of(parent, &agent, "rejected")).await;
    assert!(matches!(rejected, Err(JobError::Unknown(id)) if id == parent));
    assert_eq!(creation_count(&jobs).await, 1);
    assert_eq!(
        jobs.test_create(JobSpec::test(agent, "next")).await.get(),
        2
    );
}

#[tokio::test]
async fn indeterminate_creation_does_not_publish_map_or_retry_append() {
    let (_root, jobs, agent) = crate::job::tests::runtime().await;
    let sequence = jobs.store().records().await.len() as u64 + 1;
    jobs.store()
        .fail_append_at(AppendBoundary::Publication)
        .await;
    let error = jobs
        .create(JobSpec::test(agent.clone(), "uncertain"))
        .await
        .err()
        .unwrap();
    let JobError::Session(SessionError::AppendIndeterminate(recovery)) = error else {
        panic!("expected explicit recovery-required creation failure: {error}");
    };
    assert_eq!(recovery.identity.sequence, sequence);
    let retried = jobs.create(JobSpec::test(agent, "not-retried")).await;
    assert!(
        matches!(retried, Err(JobError::Session(SessionError::AppendUnavailable(ref later))) if later == &recovery)
    );
    assert!(jobs.inner.jobs.lock().await.is_empty());
    assert_eq!(creation_count(&jobs).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prune_create_race_is_linearized_under_bounded_stress() {
    tokio::time::timeout(Duration::from_secs(10), async {
        for _ in 0..32 {
            let (_root, jobs, agent) = crate::job::tests::runtime().await;
            let parent = claimed(&jobs, &agent, "parent").await;
            let mut creates = tokio::task::JoinSet::new();
            for _ in 0..8 {
                let (jobs, spec) = (jobs.clone(), child_of(parent, &agent, "child"));
                creates.spawn(async move { jobs.create(spec).await });
            }
            let pruning = prune(&jobs);
            let mut accepted = 0;
            while let Some(result) = creates.join_next().await {
                match result.unwrap() {
                    Ok(lease) => {
                        assert_eq!(
                            jobs.metadata(lease.id()).await.unwrap().parent,
                            Some(parent)
                        );
                        accepted += 1;
                    }
                    Err(JobError::Unknown(id)) if id == parent => {}
                    Err(error) => panic!("unexpected create/prune outcome: {error}"),
                }
            }
            assert_eq!(pruning.await.unwrap().unwrap(), 1);
            assert_eq!(creation_count(&jobs).await, accepted + 1);
            assert_eq!(jobs.inner.jobs.lock().await.len(), accepted);
        }
    })
    .await
    .expect("bounded create/prune stress must not deadlock");
}

#[tokio::test]
async fn cancelled_prune_finishes_membership_and_artifact_cleanup() {
    let (_root, jobs, agent) = crate::job::tests::runtime().await;
    let id = claimed(&jobs, &agent, "pruned").await;
    let output = jobs.output(id);
    assert!(output.test_document().is_some());
    let operation = jobs.operation(id).await.unwrap().lock_owned().await;
    let pruning = prune(&jobs);
    tokio::time::timeout(Duration::from_secs(3), async {
        while jobs.inner.creation_operation.try_write().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pruning.abort();
    assert!(pruning.await.unwrap_err().is_cancelled());
    drop(operation);
    jobs.drain_creations().await;
    assert!(matches!(jobs.metadata(id).await, Err(JobError::Unknown(job)) if job == id));
    assert!(output.test_document().is_none());
}
