//! Consuming startup obligations and one completion owner for every worker.

use std::{
    future::Future,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use tokio::{
    sync::{Notify, mpsc},
    task::{AbortHandle, JoinHandle},
};

use super::{CancellationToken, JobError, JobId, JobManager, JobOutcome, JobTransition};
use crate::tool::{ToolError, ToolOutput};

/// The stages a job lease passes through before its worker starts.
pub mod stage {
    /// Created and queued; approval comes next.
    pub struct Created;
    /// Awaiting approval; running comes next.
    pub struct Approving;
    /// Approved and running; a worker may be attached.
    pub struct Running;
}

/// A fresh job's start-or-fail obligation, typed by how far its startup has
/// progressed. Dropping it cancels the job and leaves finalization to the manager.
#[must_use = "a job lease must be started or failed"]
pub struct JobLease<S = stage::Created> {
    input: mpsc::Receiver<serde_json::Value>,
    worker: JobWorker,
    stage: PhantomData<S>,
}

impl<S> JobLease<S> {
    pub fn id(&self) -> JobId {
        self.worker.id
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.worker.owned().1.clone()
    }

    pub(crate) async fn fail(self, outcome: JobOutcome) {
        self.worker.fail(outcome).await;
    }

    /// Journal the next startup transition; a job cancelled meanwhile is terminal.
    async fn advance<N>(self, transition: JobTransition) -> Result<JobLease<N>, JobError> {
        self.worker
            .owned()
            .0
            .advance(self.worker.id, transition)
            .await?;
        Ok(JobLease {
            input: self.input,
            worker: self.worker,
            stage: PhantomData,
        })
    }

    /// Fixtures that only need the job's id hand it an idle owner: cancellation
    /// finalizes it, and finishing it from outside releases the owner.
    #[cfg(test)]
    pub(crate) fn into_test_id(self) -> JobId {
        let id = self.worker.id;
        let (jobs, cancellation) = self.worker.into_owned();
        let active = jobs.inner.supervision.enter();
        tokio::spawn(async move {
            let finished = async {
                loop {
                    let notified = {
                        let entries = jobs.inner.jobs.lock().await;
                        let Some(entry) = entries.get(&id).filter(|entry| entry.end().is_none())
                        else {
                            return;
                        };
                        entry.notify.clone().notified_owned()
                    };
                    notified.await;
                }
            };
            tokio::select! {
                () = cancellation.cancelled() => {
                    let cancelled = ToolError::cancelled().into();
                    crate::tool::executor::persist_completion(&jobs, id, cancelled).await;
                }
                () = finished => {}
            }
            drop(active);
        });
        id
    }
}

impl JobLease<stage::Created> {
    pub(super) fn new(
        jobs: JobManager,
        id: JobId,
        cancellation: CancellationToken,
        input: mpsc::Receiver<serde_json::Value>,
    ) -> Self {
        Self {
            input,
            worker: JobWorker::new(jobs, id, cancellation),
            stage: PhantomData,
        }
    }

    pub async fn await_approval(self) -> Result<JobLease<stage::Approving>, JobError> {
        self.advance(JobTransition::AwaitingApproval).await
    }

    /// Approve and run, for fixtures that start work without a policy.
    #[cfg(test)]
    pub(crate) async fn test_run(self) -> JobLease<stage::Running> {
        self.await_approval().await.unwrap().run().await.unwrap()
    }
}

impl JobLease<stage::Approving> {
    pub async fn run(self) -> Result<JobLease<stage::Running>, JobError> {
        self.advance(JobTransition::Running).await
    }
}

impl JobLease<stage::Running> {
    /// The job's input mailbox and the authority to run its worker.
    pub(crate) fn split(self) -> (mpsc::Receiver<serde_json::Value>, JobWorker) {
        (self.input, self.worker)
    }
}

/// The only authority to supervise and finalize this invocation. Consuming it
/// hands completion to the caller; dropping it first cancels the job.
pub(crate) struct JobWorker {
    id: JobId,
    /// Present until completion is handed on by `into_owned` or cancelled by `Drop`.
    owned: Option<(JobManager, CancellationToken)>,
}

impl JobWorker {
    pub(super) fn new(jobs: JobManager, id: JobId, cancellation: CancellationToken) -> Self {
        Self {
            id,
            owned: Some((jobs, cancellation)),
        }
    }

    fn owned(&self) -> &(JobManager, CancellationToken) {
        self.owned
            .as_ref()
            .expect("a worker owns its job until completion is handed on")
    }

    fn into_owned(mut self) -> (JobManager, CancellationToken) {
        self.owned
            .take()
            .expect("a worker owns its job until completion is handed on")
    }

    pub(crate) async fn fail(self, outcome: JobOutcome) {
        let id = self.id;
        let (jobs, _) = self.into_owned();
        // The caller may disappear at the next await, but completion may not.
        let _ = spawn_completion(jobs, id, async move { outcome }).await;
    }

    pub(crate) async fn start_supervised<F>(self, future: F) -> Result<(), JobError>
    where
        F: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        self.start(future, "tool handler panicked").await
    }

    pub(super) async fn start<F>(
        self,
        future: F,
        panic_message: &'static str,
    ) -> Result<(), JobError>
    where
        F: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        let worker = WorkerGuard(tokio::spawn(future));
        self.owned()
            .0
            .attach_task(self.id, worker.abort_handle())
            .await?;
        let id = self.id;
        let (jobs, _) = self.into_owned();
        // No suspension point between relinquishing the permit and supervising.
        spawn_completion(jobs, id, async move {
            match worker.join().await {
                Ok(Ok(output)) => JobOutcome::Completed(output),
                Ok(Err(error)) => error.into(),
                Err(error) if error.is_cancelled() => ToolError::cancelled().into(),
                Err(_) => ToolError::failed(panic_message).into(),
            }
        });
        Ok(())
    }
}

impl Drop for JobWorker {
    fn drop(&mut self) {
        let Some((jobs, cancellation)) = self.owned.take() else {
            return;
        };
        cancellation.cancel();
        // Outside a running runtime only cancellation is possible; replay recovers.
        if tokio::runtime::Handle::try_current().is_ok() {
            spawn_completion(jobs, self.id, async { ToolError::cancelled().into() });
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
    use crate::{
        job::{JobError, JobId, JobManager, JobSpec, JobState},
        tool::policy::CapabilitySet,
    };
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

    /// Every startup obligation is consumed exactly once: abandonment at any
    /// stage cancels, explicit failure and supervised panics fail, and a job
    /// finished underneath its lease can neither advance nor attach.
    #[tokio::test]
    async fn startup_obligations_settle_through_the_drained_owner() {
        for (tool, expected, error) in [
            ("abandoned", JobState::Cancelled, None),
            ("abandoned-approving", JobState::Cancelled, None),
            ("abandoned-running", JobState::Cancelled, None),
            ("rejected", JobState::Failed, Some("startup rejected")),
            ("panic", JobState::Failed, Some("tool handler panicked")),
            ("terminal", JobState::Completed, None),
            ("terminal-running", JobState::Completed, None),
        ] {
            let (_root, jobs, lease, id) = leased(tool).await;
            let cancellation = lease.cancellation_token();
            match tool {
                "abandoned" => drop(lease),
                "abandoned-approving" => drop(lease.await_approval().await.unwrap()),
                "abandoned-running" => drop(lease.test_run().await),
                "rejected" => {
                    lease
                        .fail(ToolError::failed("startup rejected").into())
                        .await
                }
                "panic" => {
                    let (_input, worker) = lease.test_run().await.split();
                    worker
                        .start_supervised(async { panic!("handler panic") })
                        .await
                        .unwrap();
                }
                "terminal" => {
                    jobs.test_finish(id, serde_json::json!(42)).await;
                    let approving = lease.await_approval().await;
                    assert!(matches!(approving, Err(JobError::AlreadyTerminal(_))));
                }
                _ => {
                    let (_input, worker) = lease.test_run().await.split();
                    jobs.test_finish(id, serde_json::json!(42)).await;
                    let output = async { Ok(ToolOutput::new(serde_json::json!(0))) };
                    let started = worker.start_supervised(output).await;
                    assert!(matches!(started, Err(JobError::AlreadyTerminal(_))));
                }
            }
            jobs.drain_supervisors().await;
            let result = jobs.metadata(id).await.unwrap();
            assert_eq!(result.state, expected, "{tool}");
            if let Some(error) = error {
                assert!(
                    result
                        .rendered_error(&CapabilitySet::default())
                        .is_some_and(|message| message.contains(error))
                );
            }
            if tool == "rejected" {
                assert!(!cancellation.is_cancelled());
            }
            if tool.starts_with("abandoned") {
                assert_eq!(state(&jobs.test_replay().await, id).await, expected);
            }
        }
    }

    #[tokio::test]
    async fn supervisor_drain_waits_for_worker_and_finalization() {
        let (_root, jobs, lease, id) = leased("supervised").await;
        let (_input, worker) = lease.test_run().await.split();
        let (release, released) = oneshot::channel();
        let work = async move {
            released.await.unwrap();
            Ok(ToolOutput::new(serde_json::json!({"done": true})))
        };
        worker.start_supervised(work).await.unwrap();
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
        let (_input, worker) = lease.test_run().await.split();
        let map = jobs.inner.jobs.lock().await;
        let (started, starting) = oneshot::channel();
        let (stopped, stopping) = oneshot::channel();
        let start = tokio::spawn(worker.start_supervised(async move {
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
