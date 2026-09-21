//! Consuming startup obligations and one completion owner for every worker.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use tokio::{
    sync::{Notify, mpsc},
    task::{AbortHandle, JoinHandle},
};

use super::{CancellationToken, JobError, JobId, JobManager, JobOutcome};
use crate::tool::{ToolError, ToolOutput};

/// A fresh job's start-or-fail obligation. Dropping it cancels the job and
/// leaves finalization to the manager.
#[must_use = "a job lease must be started or failed"]
pub struct JobLease {
    input: Option<mpsc::Receiver<serde_json::Value>>,
    completion: CompletionPermit,
}

impl JobLease {
    pub(super) fn new(
        jobs: JobManager,
        id: JobId,
        cancellation: CancellationToken,
        input: mpsc::Receiver<serde_json::Value>,
    ) -> Self {
        Self {
            input: Some(input),
            completion: CompletionPermit::new(jobs, id, cancellation),
        }
    }

    pub fn id(&self) -> JobId {
        self.completion.id
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.completion.cancellation.clone()
    }

    pub(crate) fn take_input(&mut self) -> mpsc::Receiver<serde_json::Value> {
        self.input
            .take()
            .expect("job input is transferred only once")
    }

    pub(crate) async fn fail(self, outcome: JobOutcome) {
        self.completion.complete(outcome).await;
    }

    pub(crate) async fn start_supervised<F>(self, future: F) -> Result<(), JobError>
    where
        F: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        self.completion.start(future, "tool handler panicked").await
    }

    /// Manager-operation fixtures deliberately have no actual startup worker.
    #[cfg(test)]
    pub(crate) fn into_test_id(self) -> JobId {
        self.into_test_fixture().id()
    }

    #[cfg(test)]
    pub(crate) fn into_test_fixture(mut self) -> Self {
        self.completion.jobs.take();
        self
    }
}

/// The only authority to supervise and finalize this invocation.
pub(super) struct CompletionPermit {
    jobs: Option<JobManager>,
    id: JobId,
    cancellation: CancellationToken,
}

impl CompletionPermit {
    pub(super) fn new(jobs: JobManager, id: JobId, cancellation: CancellationToken) -> Self {
        Self {
            jobs: Some(jobs),
            id,
            cancellation,
        }
    }

    async fn complete(mut self, outcome: JobOutcome) {
        let jobs = self
            .jobs
            .take()
            .expect("completion permit is consumed once");
        // The caller may disappear at the next await, but completion may not.
        let _ = spawn_completion(jobs, self.id, async move { outcome }).await;
    }

    pub(super) async fn start<F>(
        mut self,
        future: F,
        panic_message: &'static str,
    ) -> Result<(), JobError>
    where
        F: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        let worker = WorkerGuard(tokio::spawn(future));
        let jobs = self
            .jobs
            .as_ref()
            .expect("completion permit is consumed once");
        jobs.attach_task(self.id, worker.abort_handle()).await?;
        let jobs = self
            .jobs
            .take()
            .expect("completion permit is consumed once");
        // No suspension point between relinquishing the permit and supervising.
        spawn_completion(jobs, self.id, async move {
            match worker.join().await {
                Ok(Ok(output)) => JobOutcome::Completed(output),
                Ok(Err(error)) => error.into(),
                Err(error) if error.is_cancelled() => ToolError::Cancelled.into(),
                Err(_) => ToolError::Failed(panic_message.to_owned()).into(),
            }
        });
        Ok(())
    }
}

impl Drop for CompletionPermit {
    fn drop(&mut self) {
        let Some(jobs) = self.jobs.take() else { return };
        self.cancellation.cancel();
        // Outside a running runtime only cancellation is possible; replay recovers.
        if tokio::runtime::Handle::try_current().is_ok() {
            spawn_completion(jobs, self.id, async { ToolError::Cancelled.into() });
        }
    }
}

/// Spawn the manager-owned completion task, counted by supervision from admission.
fn spawn_completion(
    jobs: JobManager,
    id: JobId,
    outcome: impl Future<Output = JobOutcome> + Send + 'static,
) -> JoinHandle<()> {
    let active = jobs.inner.supervision.enter();
    tokio::spawn(async move {
        let outcome = outcome.await;
        crate::tool::executor::persist_completion(&jobs, id, outcome).await;
        drop(jobs);
        drop(active);
    })
}

/// Counts owned completion tasks, including cleanup admitted synchronously by Drop.
#[derive(Default)]
pub(super) struct Supervision {
    active: AtomicUsize,
    changed: Notify,
}

impl Supervision {
    pub(super) fn enter(self: &Arc<Self>) -> ActiveSupervision {
        self.active.fetch_add(1, Ordering::AcqRel);
        ActiveSupervision(self.clone())
    }

    async fn drain(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
}

pub(super) struct ActiveSupervision(Arc<Supervision>);

impl Drop for ActiveSupervision {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.changed.notify_waiters();
        }
    }
}

impl JobManager {
    /// Call after stopping startup producers and cancelling/draining live jobs.
    /// Completion owners must release their store clones before the writer drain.
    pub(crate) async fn drain_supervisors(&self) {
        self.inner.supervision.drain().await;
    }
}

/// Aborts a spawned worker if startup fails or is cancelled before supervision.
struct WorkerGuard<T>(JoinHandle<T>);

impl<T> WorkerGuard<T> {
    fn abort_handle(&self) -> AbortHandle {
        self.0.abort_handle()
    }

    async fn join(mut self) -> Result<T, tokio::task::JoinError> {
        (&mut self.0).await
    }
}

impl<T> Drop for WorkerGuard<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobError, JobId, JobManager, JobSpec, JobState};
    use std::time::Duration;
    use tokio::sync::oneshot;

    struct Stopped(Option<oneshot::Sender<()>>);

    impl Drop for Stopped {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }

    async fn leased(tool: &str) -> (tempfile::TempDir, JobManager, JobLease, JobId) {
        let (root, jobs, agent) = crate::job::tests::runtime().await;
        let lease = jobs.create(JobSpec::test(agent, tool)).await.unwrap();
        let id = lease.id();
        (root, jobs, lease, id)
    }

    async fn state(jobs: &JobManager, id: JobId) -> JobState {
        jobs.metadata(id).await.unwrap().state
    }

    #[tokio::test]
    async fn worker_guard_aborts_on_rejected_attachment() {
        let (_root, jobs, _agent) = crate::job::tests::runtime().await;
        let (started, starting) = oneshot::channel();
        let (stopped, stopping) = oneshot::channel();
        let worker = WorkerGuard(tokio::spawn(async move {
            let _stopped = Stopped(Some(stopped));
            let _ = started.send(());
            std::future::pending::<()>().await;
        }));
        starting.await.unwrap();
        let attached = jobs
            .attach_task(JobId::new(99).unwrap(), worker.abort_handle())
            .await;
        assert!(matches!(attached, Err(JobError::Unknown(_))));
        drop(worker);
        tokio::time::timeout(Duration::from_secs(1), stopping)
            .await
            .unwrap()
            .unwrap();
    }

    /// Every startup obligation is consumed exactly once: abandonment cancels,
    /// explicit failure and supervised panics fail, and a terminal result stays.
    #[tokio::test]
    async fn startup_obligations_settle_through_the_drained_owner() {
        for (tool, expected, error) in [
            ("abandoned", JobState::Cancelled, None),
            ("rejected", JobState::Failed, Some("startup rejected")),
            ("panic", JobState::Failed, Some("tool handler panicked")),
            ("terminal", JobState::Completed, None),
        ] {
            let (_root, jobs, lease, id) = leased(tool).await;
            let cancellation = lease.cancellation_token();
            match tool {
                "abandoned" => drop(lease),
                "rejected" => {
                    lease
                        .fail(ToolError::Failed("startup rejected".into()).into())
                        .await
                }
                "panic" => lease
                    .start_supervised(async { panic!("handler panic") })
                    .await
                    .unwrap(),
                _ => {
                    jobs.test_finish(id, serde_json::json!(42)).await;
                    let output = async { Ok(ToolOutput::new(serde_json::json!(0))) };
                    let started = lease.start_supervised(output).await;
                    assert!(matches!(started, Err(JobError::AlreadyTerminal(_))));
                }
            }
            jobs.drain_supervisors().await;
            let result = jobs.metadata(id).await.unwrap();
            assert_eq!(result.state, expected, "{tool}");
            if let Some(error) = error {
                assert!(
                    result
                        .error
                        .as_deref()
                        .is_some_and(|message| message.contains(error))
                );
            }
            if tool == "rejected" {
                assert!(!cancellation.is_cancelled());
            }
            if tool == "abandoned" {
                assert_eq!(state(&jobs.test_replay().await, id).await, expected);
            }
        }
    }

    #[tokio::test]
    async fn supervisor_drain_waits_for_worker_and_finalization() {
        let (_root, jobs, lease, id) = leased("supervised").await;
        jobs.transition(id, JobState::AwaitingApproval)
            .await
            .unwrap();
        jobs.transition(id, JobState::Running).await.unwrap();
        let (release, released) = oneshot::channel();
        let work = async move {
            released.await.unwrap();
            Ok(ToolOutput::new(serde_json::json!({"done": true})))
        };
        lease.start_supervised(work).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), jobs.drain_supervisors())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        jobs.drain_supervisors().await;
        assert_eq!(state(&jobs, id).await, JobState::Completed);
    }

    #[tokio::test]
    async fn cancelling_start_during_attachment_aborts_and_finalizes() {
        let (_root, jobs, lease, id) = leased("attachment").await;
        let map = jobs.inner.jobs.lock().await;
        let (started, starting) = oneshot::channel();
        let (stopped, stopping) = oneshot::channel();
        let start = tokio::spawn(lease.start_supervised(async move {
            let _stopped = Stopped(Some(stopped));
            let _ = started.send(());
            std::future::pending::<Result<ToolOutput, ToolError>>().await
        }));
        starting.await.unwrap();
        start.abort();
        assert!(start.await.unwrap_err().is_cancelled());
        drop(map);
        tokio::time::timeout(Duration::from_secs(1), stopping)
            .await
            .unwrap()
            .unwrap();
        jobs.drain_supervisors().await;
        assert_eq!(state(&jobs, id).await, JobState::Cancelled);
    }
}
