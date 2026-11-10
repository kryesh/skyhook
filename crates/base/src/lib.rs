//! Generic primitives with no role of their own, shared by the worker, the host and
//! the products built on them.

// The derives name this crate by its path, within it too.
extern crate self as skyhook_base;

pub mod bounded;
pub mod enums;
pub mod hex;
pub mod limits;
pub mod newtype;
pub mod schema;

/// Run blocking `work` off the async workers, its panic resumed here.
pub async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    joined(spawn_blocking(work).await).await
}

/// Start blocking `work` off the async workers. Within [`Work::scope`] it counts as
/// that scope's work until it returns, even once nothing awaits it.
pub fn spawn_blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    let token = WORK.try_with(|scope| scope.0.token()).ok();
    tokio::task::spawn_blocking(move || {
        let _counted = token;
        work()
    })
}

tokio::task_local! {
    static WORK: Work;
}

/// The blocking work started by futures run in its scopes, and the tasks spawned on
/// it. Dropping a future cannot stop a blocking thread it started, nor a task meant to
/// outlive it, so whatever owns the futures waits for this before it says they are gone.
#[derive(Clone, Default)]
pub struct Work(tokio_util::task::TaskTracker);

impl Work {
    /// `future`, the blocking work it starts counted here.
    pub fn scope<F: Future>(&self, future: F) -> impl Future<Output = F::Output> + use<F> {
        WORK.scope(self.clone(), future)
    }

    /// Run `task` on `runtime`, counted here until it ends, as the blocking work it
    /// starts is until that returns.
    pub fn spawn_on(
        &self,
        task: impl Future<Output = ()> + Send + 'static,
        runtime: &tokio::runtime::Handle,
    ) {
        self.0.spawn_on(self.scope(task), runtime);
    }

    /// Once every piece of blocking work started in this one's scopes, and every task
    /// spawned on it, has returned.
    pub async fn finished(&self) {
        self.0.close();
        self.0.wait().await;
    }
}

/// What a finished task returned, its panic resumed here. A task the runtime cancelled
/// as it shut down returned nothing, so this never completes: whatever awaits it is
/// dropped with the runtime.
pub async fn joined<T>(result: Result<T, tokio::task::JoinError>) -> T {
    match result.map_err(tokio::task::JoinError::try_into_panic) {
        Ok(value) => value,
        Err(Ok(panic)) => std::panic::resume_unwind(panic),
        Err(Err(_cancelled)) => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    /// Work is finished only once the blocking work its tasks started has returned,
    /// even work a task left behind as it ended.
    #[tokio::test]
    async fn work_finishes_once_its_tasks_blocking_work_has_returned() {
        let work = super::Work::default();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (left, leaving) = tokio::sync::oneshot::channel();
        let returned = Arc::new(AtomicBool::new(false));
        let returning = returned.clone();
        let task = async move {
            drop(super::spawn_blocking(move || {
                let _ = held.recv();
                returning.store(true, Ordering::SeqCst);
            }));
            let _ = left.send(());
        };
        work.spawn_on(task, &tokio::runtime::Handle::current());
        leaving.await.unwrap();
        release.send(()).unwrap();
        work.finished().await;
        assert!(returned.load(Ordering::SeqCst));
    }

    /// The work's value comes back, and its panic is raised again in the caller.
    #[tokio::test]
    async fn blocking_work_returns_its_value_and_resumes_its_panic() {
        assert_eq!(super::blocking(|| 7).await, 7);
        let caller = tokio::spawn(super::blocking(|| panic!("blocking work")));
        let panic = caller.await.unwrap_err().into_panic();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"blocking work"));
    }
}
