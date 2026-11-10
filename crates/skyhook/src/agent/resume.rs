//! The resume hook: what a launch changes about a session it reopened, run once
//! before that session's first work here.

use std::collections::BTreeSet;

use super::{HarnessError, ResumeHook, SessionHandle};
use crate::session::{CoreState, SessionStore, Wait};
use std::sync::Arc;

/// A reopened session's resume hook, and whether it has run in this launch.
pub(super) struct Resume {
    hook: Arc<dyn ResumeHook>,
    extension: crate::session::ExtensionDb,
    ran: tokio::sync::OnceCell<()>,
    /// Every call that joined those waiting on the hook, until it ends.
    calls: tokio_util::task::TaskTracker,
}

impl Resume {
    pub(super) fn new(hook: Arc<dyn ResumeHook>, extension: crate::session::ExtensionDb) -> Self {
        let (ran, calls) = Default::default();
        Self {
            hook,
            extension,
            ran,
            calls,
        }
    }

    /// Once every call that joined has ended, and with it its run of the hook.
    /// Shutdown withdraws them first, so none still waits on the hook.
    pub(super) async fn ended(&self) {
        self.calls.close();
        self.calls.wait().await;
    }
}

/// The calls waiting on the hook, each until it lets them in or an interrupt or
/// shutdown withdraws them.
#[derive(Default)]
pub(super) struct Waiting {
    next: u64,
    calls: BTreeSet<u64>,
}

/// One call's place among those waiting on the hook.
#[derive(Clone, Copy)]
struct Waiter(u64);

impl Waiting {
    fn join(&mut self) -> Waiter {
        self.next += 1;
        self.calls.insert(self.next);
        Waiter(self.next)
    }

    /// Let `waiter` in, unless it was withdrawn first.
    fn leave(&mut self, waiter: Waiter) -> bool {
        self.calls.remove(&waiter.0)
    }

    fn withdrawn(&self, waiter: Waiter) -> bool {
        !self.calls.contains(&waiter.0)
    }

    pub(super) fn withdraw(&mut self) -> usize {
        std::mem::take(&mut self.calls).len()
    }
}

/// A call's place among those waiting on the hook, which it leaves however the
/// call ends. Made in the change that joins it, so a caller that abandons the
/// join leaves too.
struct Joined {
    store: SessionStore,
    waiter: Waiter,
    _call: tokio_util::task::task_tracker::TaskTrackerToken,
}

impl Drop for Joined {
    fn drop(&mut self) {
        let waiter = self.waiter;
        self.store.enqueue(move |state| {
            state.agents.resuming_mut().leave(waiter);
        });
    }
}

impl SessionHandle {
    /// Run the harness's [`ResumeHook`] for a session it reopened, once, as every
    /// call that starts work does first. A host that prepares work of its own
    /// before sending it calls this first. An interrupt or shutdown withdraws a
    /// call waiting here, which returns [`HarnessError::Interrupted`], up to the
    /// moment it is let in: the hook finishing, however it does, and the withdrawal
    /// are decided in the session's order.
    pub async fn resume_work(&self) -> Result<(), HarnessError> {
        self.owned()?;
        let Some(resume) = self
            .runtime
            .resume
            .as_ref()
            .filter(|r| !r.ran.initialized())
        else {
            return Ok(());
        };
        let store = &self.runtime.store;
        let (joining, calls) = (store.clone(), resume.calls.clone());
        let join = move |state: &mut CoreState| match state.phase() {
            crate::session::SessionPhase::Viewing | crate::session::SessionPhase::Running => {
                Ok(Joined {
                    waiter: state.agents.resuming_mut().join(),
                    store: joining,
                    _call: calls.token(),
                })
            }
            phase => Err(phase.refusal()),
        };
        let joined = store.live(join).await?;
        let waiter = joined.waiter;
        let withdrawn =
            move |state: &CoreState| (state.agents.resuming().withdrawn(waiter)).then_some(());
        let ran = resume
            .ran
            .get_or_try_init(|| resume.hook.resumed(&resume.extension));
        let ran = tokio::select! {
            biased;
            ran = ran => ran.map(drop),
            withdrawn = store.until(Wait::on([], withdrawn)) => {
                withdrawn?;
                return Err(HarnessError::Interrupted);
            }
        };
        // Decided in order with every withdrawal: one that withdrew this call as the
        // hook finished keeps it out.
        let enters = move |state: &mut CoreState| state.agents.resuming_mut().leave(waiter);
        match store.live(enters).await {
            true => ran.map_err(HarnessError::Resume),
            false => Err(HarnessError::Interrupted),
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::agent::tests::*;
    use futures_util::FutureExt;
    use tokio::sync::{Semaphore, mpsc};

    /// A hook that reports each call and finishes once the test lets it: failing
    /// once if it `fails`, and as `interrupting` names a session, by interrupting it.
    struct Hook {
        entered: mpsc::UnboundedSender<()>,
        /// Held by each call until it ends.
        running: Arc<()>,
        open: Semaphore,
        fails: std::sync::atomic::AtomicBool,
        interrupting: StdMutex<Option<(SessionHandle, mpsc::UnboundedSender<usize>)>>,
    }

    impl ResumeHook for Hook {
        fn resumed<'a>(
            &'a self,
            _: &'a crate::session::ExtensionDb,
        ) -> crate::agent::ResumeFuture<'a> {
            Box::pin(async {
                let _running = self.running.clone();
                self.entered.send(()).unwrap();
                self.open.acquire().await?.forget();
                let interrupting = self.interrupting.lock().unwrap().take();
                if let Some((session, interrupted)) = interrupting {
                    interrupted.send(session.interrupt().await).unwrap();
                }
                match self.fails.swap(false, Ordering::SeqCst) {
                    true => Err("the run is locked".into()),
                    false => Ok(()),
                }
            })
        }
    }

    struct Reopened {
        _root: tempfile::TempDir,
        hook: Arc<Hook>,
        entered: mpsc::UnboundedReceiver<()>,
        script: Arc<Script>,
        session: SessionHandle,
    }

    /// A session whose turn failed, reopened under a hook that first `fails`.
    async fn reopened(fails: bool) -> Reopened {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let rejected = crate::provider::ProviderErrorKind::InvalidRequest.error("rejected");
        let steps = [Step::fail(rejected), Step::new(answer("continued"))];
        let script = Script::new(steps, &Requests::default());
        let (entered, entries) = mpsc::unbounded_channel();
        let hook = Arc::new(Hook {
            entered,
            running: Arc::default(),
            open: Semaphore::new(0),
            fails: std::sync::atomic::AtomicBool::new(fails),
            interrupting: StdMutex::default(),
        });
        let harness = test_builder(root.path(), &sessions, script.clone(), false)
            .session_schema(crate::session::tests::TABLES)
            .resume_hook(hook.clone())
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        session.prompt("first").await.unwrap_err();
        let id = session.id();
        shutdown_session(session).await;
        let session = harness.resume_session(id).await.unwrap();
        Reopened {
            _root: root,
            hook,
            entered: entries,
            script,
            session,
        }
    }

    fn continuing(
        session: &SessionHandle,
    ) -> tokio::task::JoinHandle<Result<String, HarnessError>> {
        let session = session.clone();
        tokio::spawn(async move { session.continue_turn().await })
    }

    /// An interrupt withdraws a continue waiting on the hook and drops the hook
    /// unfinished: no request starts, the next continue runs it again, and the
    /// interrupted turn still continues.
    #[tokio::test]
    async fn an_interrupt_withdraws_a_continue_waiting_on_the_resume() {
        let mut reopened = reopened(false).await;
        let session = &reopened.session;
        let continued = continuing(session);
        bounded(reopened.entered.recv()).await.unwrap();
        assert_eq!(session.interrupt().await, 1);
        let withdrawn = bounded(continued).await.unwrap();
        assert!(
            matches!(withdrawn, Err(HarnessError::Interrupted)),
            "{withdrawn:?}"
        );
        assert!(!reopened.script.requested_from(1));
        let continued = continuing(session);
        bounded(reopened.entered.recv()).await.unwrap();
        reopened.hook.open.add_permits(1);
        assert_eq!(bounded(continued).await.unwrap().unwrap(), "continued");
        shutdown_session(reopened.session).await;
    }

    /// An interrupt that lands as the hook succeeds or fails still withdraws the
    /// continue waiting on it; the hook, once it succeeded, does not run again.
    #[tokio::test]
    async fn an_interrupt_as_the_resume_finishes_withdraws_the_continue() {
        for fails in [false, true] {
            let mut reopened = reopened(fails).await;
            let session = &reopened.session;
            let (interrupted, mut interrupts) = mpsc::unbounded_channel();
            *reopened.hook.interrupting.lock().unwrap() = Some((session.clone(), interrupted));
            reopened.hook.open.add_permits(2);
            let withdrawn = bounded(session.continue_turn()).await;
            assert!(
                matches!(withdrawn, Err(HarnessError::Interrupted)),
                "{fails}: {withdrawn:?}"
            );
            assert_eq!(interrupts.try_recv(), Ok(1));
            assert!(!reopened.script.requested_from(1));
            assert_eq!(bounded(session.continue_turn()).await.unwrap(), "continued");
            for _ in 0..1 + usize::from(fails) {
                reopened.entered.try_recv().unwrap();
            }
            assert!(reopened.entered.try_recv().is_err());
            shutdown_session(reopened.session).await;
        }
    }

    /// A call that abandons joining the calls waiting on the hook leaves them, so
    /// an interrupt finds none.
    #[tokio::test]
    async fn an_abandoned_resume_leaves_nothing_waiting() {
        let reopened = reopened(false).await;
        let session = &reopened.session;
        assert!(session.resume_work().now_or_never().is_none());
        // The abandoned join has replied, and so left, once this replies.
        session.runtime.store.drain().await.unwrap();
        assert_eq!(session.interrupt().await, 0);
        shutdown_session(reopened.session).await;
    }

    /// Shutdown withdraws a continue waiting on the hook in a session a rename
    /// activated, and returns only once that call of the hook has ended.
    #[tokio::test]
    async fn shutdown_withdraws_the_resume_and_ends_its_hook() {
        let mut reopened = reopened(false).await;
        let session = &reopened.session;
        session.rename(Some("renamed".into())).await.unwrap();
        let continued = continuing(session);
        bounded(reopened.entered.recv()).await.unwrap();
        bounded(session.shutdown()).await.unwrap();
        assert_eq!(Arc::strong_count(&reopened.hook.running), 1);
        let withdrawn = bounded(continued).await.unwrap();
        assert!(
            matches!(withdrawn, Err(HarnessError::Interrupted)),
            "{withdrawn:?}"
        );
        session.close().await.unwrap();
    }

    /// A hook that fails refuses the work with its error, and the next work runs it
    /// again.
    #[tokio::test]
    async fn a_failed_resume_refuses_the_work_until_it_succeeds() {
        let reopened = reopened(true).await;
        let session = &reopened.session;
        reopened.hook.open.add_permits(2);
        let refused = bounded(session.continue_turn()).await;
        assert!(
            matches!(&refused, Err(HarnessError::Resume(error)) if error.to_string() == "the run is locked"),
            "{refused:?}"
        );
        assert!(!reopened.script.requested_from(1));
        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "continued");
        shutdown_session(reopened.session).await;
    }
}
