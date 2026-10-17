//! Consuming startup obligations and one completion owner for every worker.

use std::{future::Future, marker::PhantomData};

use tokio::{
    sync::{mpsc, oneshot},
    task::{AbortHandle, JoinHandle},
};

use super::{CancellationToken, JobError, JobId, JobManager, JobOutcome, JobTransition, Run};
use crate::tool::ToolError;

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
        self.worker.run.0
    }

    /// The lease of invocation `run`, at stage `S`: a new job's first, or a
    /// restarted one's.
    pub(super) fn new(
        jobs: JobManager,
        run: Run,
        cancellation: CancellationToken,
        input: mpsc::Receiver<serde_json::Value>,
    ) -> Self {
        Self {
            input,
            worker: JobWorker::new(jobs, run, cancellation),
            stage: PhantomData,
        }
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.worker.owned().1.clone()
    }

    pub(crate) async fn fail(self, outcome: JobOutcome) {
        self.worker.fail(outcome).await;
    }

    /// Journal the next startup transition; a job cancelled meanwhile is terminal.
    async fn advance<N>(self, transition: JobTransition) -> Result<JobLease<N>, JobError> {
        self.worker.owned().0.advance(self.id(), transition).await?;
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
        let run = self.worker.run;
        let (jobs, cancellation) = self.worker.into_owned();
        jobs.clone().inner.supervisor.spawn(async move {
            let finished = jobs.inner.store.until(move |state| {
                let entry = state.jobs.entries.get(&run.0);
                entry
                    .is_none_or(|entry| entry.end().is_some())
                    .then_some(())
            });
            tokio::select! {
                () = cancellation.cancelled() => {
                    jobs.settle(run, ToolError::cancelled().into()).await;
                }
                _ = finished => {}
            }
        });
        run.0
    }
}

impl JobLease<stage::Created> {
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
    /// Await approval again before the work starts.
    pub(crate) async fn reapprove(self) -> Result<JobLease<stage::Approving>, JobError> {
        self.advance(JobTransition::AwaitingApproval).await
    }

    /// The job's input mailbox and the authority to run its worker.
    pub(crate) fn split(self) -> (mpsc::Receiver<serde_json::Value>, JobWorker) {
        (self.input, self.worker)
    }
}

/// The only authority to supervise and finalize this invocation. Consuming it
/// hands completion to the caller; dropping it first cancels the job.
pub(crate) struct JobWorker {
    run: Run,
    /// Present until completion is handed on by `into_owned` or cancelled by `Drop`.
    owned: Option<(JobManager, CancellationToken)>,
}

impl JobWorker {
    pub(super) fn new(jobs: JobManager, run: Run, cancellation: CancellationToken) -> Self {
        Self {
            run,
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
        let run = self.run;
        let (jobs, _) = self.into_owned();
        // The caller may disappear at the next await, but completion may not.
        let _ = spawn_completion(jobs, run, async move { outcome }).await;
    }

    /// Start invocation `run`'s worker, attached to it in core order: refused
    /// once that invocation has ended or a later one runs, and then the worker
    /// never starts its body. The finish that commits aborts what its invocation
    /// attached, so in either order no worker outlives its invocation.
    pub(crate) fn launch<F>(self, future: F, panic_message: &'static str)
    where
        F: Future<Output: Into<JobOutcome> + Send> + Send + 'static,
    {
        let run @ Run(id, invocation) = self.run;
        let (jobs, _) = self.into_owned();
        let (attach, attached) = oneshot::channel();
        let worker = WorkerGuard(jobs.inner.supervisor.spawn(async move {
            attached.await.ok()?;
            Some(future.await)
        }));
        let abort = worker.abort_handle();
        let attaching = jobs.clone();
        // A refused attach drops `attach`, ending the worker before its body.
        jobs.inner.store.enqueue(move |state| {
            let entry = state.jobs.entries.get(&id);
            if entry.is_some_and(|entry| entry.invocation == invocation && entry.end().is_none()) {
                attaching.handles(id, |handles| handles.task_abort = Some((invocation, abort)));
                let _ = attach.send(());
            }
        });
        // No suspension point between relinquishing the permit and supervising.
        spawn_completion(jobs, run, async move {
            match worker.join().await.map(Option::flatten) {
                Ok(Some(outcome)) => outcome.into(),
                Ok(None) => ToolError::cancelled().into(),
                Err(error) if error.is_cancelled() => ToolError::cancelled().into(),
                Err(_) => ToolError::failed(panic_message).into(),
            }
        });
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
            spawn_completion(jobs, self.run, async { ToolError::cancelled().into() });
        }
    }
}

/// Spawn the manager-owned completion task of an invocation.
pub(super) fn spawn_completion(
    jobs: JobManager,
    run: Run,
    outcome: impl Future<Output = JobOutcome> + Send + 'static,
) -> JoinHandle<Option<()>> {
    jobs.clone()
        .inner
        .supervisor
        .spawn(async move { jobs.settle(run, outcome.await).await })
}

/// Every task a session runs, under one root token: workers and their
/// completions, cancellation watchdogs and agent loops. Cancelling the root
/// stops them all.
#[derive(Clone)]
pub(crate) struct Supervisor {
    tasks: tokio_util::task::TaskTracker,
    root: CancellationToken,
}

impl Supervisor {
    pub(crate) fn new(root: CancellationToken) -> Self {
        let tasks = tokio_util::task::TaskTracker::new();
        Self { tasks, root }
    }

    /// Run `task` until it ends, or until the root's cancellation drops it where
    /// it stands, as a crash would.
    pub(crate) fn spawn<T: Send + 'static>(
        &self,
        task: impl Future<Output = T> + Send + 'static,
    ) -> JoinHandle<Option<T>> {
        self.tasks
            .spawn(self.root.clone().run_until_cancelled_owned(task))
    }

    /// A token the root's cancellation cancels.
    pub(crate) fn token(&self) -> CancellationToken {
        self.root.child_token()
    }

    pub(crate) fn stop(&self) {
        self.root.cancel();
    }

    /// Wait for every task spawned so far, and those they spawn, to end.
    pub(crate) async fn drain(&self) {
        self.tasks.close();
        self.tasks.wait().await;
        self.tasks.reopen();
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
        tool::{ToolOutput, policy::CapabilitySet},
    };
    async fn leased(tool: &str) -> (tempfile::TempDir, JobManager, JobLease, JobId) {
        let (root, jobs, agent) = crate::job::tests::runtime().await;
        let lease = jobs.create(JobSpec::test(agent, tool)).await.unwrap();
        let id = lease.id();
        (root, jobs, lease, id)
    }

    fn state(jobs: &JobManager, id: JobId) -> JobState {
        jobs.metadata(id).unwrap().state
    }

    /// Every startup obligation is consumed exactly once: abandonment at any
    /// stage cancels, explicit failure and supervised panics fail, and a job
    /// can neither advance once finished nor keep a worker running, whichever
    /// of its finish and its worker's attachment comes first.
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
            ("finished-running", JobState::Completed, None),
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
                    worker.launch(
                        async { panic!("handler panic") as JobOutcome },
                        "tool handler panicked",
                    );
                }
                "terminal" => {
                    let approving = lease.await_approval().await.unwrap();
                    jobs.test_finish(id, serde_json::json!(42)).await;
                    // A startup step never restarts what finished underneath it.
                    let running = approving.run().await;
                    assert!(matches!(running, Err(JobError::AlreadyTerminal(_))));
                }
                "terminal-running" => {
                    let (_input, worker) = lease.test_run().await.split();
                    jobs.test_finish(id, serde_json::json!(42)).await;
                    worker.launch(
                        std::future::pending::<JobOutcome>(),
                        "tool handler panicked",
                    );
                }
                _ => {
                    let (_input, worker) = lease.test_run().await.split();
                    worker.launch(
                        std::future::pending::<JobOutcome>(),
                        "tool handler panicked",
                    );
                    jobs.test_finish(id, serde_json::json!(42)).await;
                }
            }
            crate::tests::bounded(jobs.supervisor().drain()).await;
            let result = jobs.metadata(id).unwrap();
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
                assert_eq!(state(&jobs.test_replay(), id), expected);
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
        worker.launch(work, "tool handler panicked");
        let drain = jobs.supervisor().drain();
        tokio::pin!(drain);
        assert!(futures_util::poll!(&mut drain).is_pending());
        release.send(()).unwrap();
        crate::tests::bounded(drain).await;
        assert_eq!(state(&jobs, id), JobState::Completed);
    }
}
