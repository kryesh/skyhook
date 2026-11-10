//! An agent's wake window: how long its inbox coalesces work before its loop
//! takes it, and the counters that hold it open.

use std::time::Duration;

use crate::identity::AgentId;
use crate::session::{CoreState, Key, SessionStore, Wait};

/// One bounded coalescing window per agent, shared by its inputs and wakes, so
/// a burst releases as one batch. Its timer runs outside the core.
#[derive(Default)]
pub(super) struct Window {
    /// Counts every input and wake; ahead of `released` while a window is open.
    pub(super) scheduled: u64,
    pub(super) released: u64,
    /// Counts the inputs among them.
    inputs: u64,
    /// `inputs` as of the latest release.
    pub(super) ready_inputs: u64,
    /// `ready_inputs` as of the agent's latest request.
    pub(super) observed_inputs: u64,
}

impl Window {
    pub(super) fn schedule(&mut self, input: bool) {
        self.scheduled += 1;
        self.inputs += u64::from(input);
    }

    pub(super) fn release(&mut self) {
        (self.released, self.ready_inputs) = (self.scheduled, self.inputs);
    }

    pub(super) fn settled(&self) -> bool {
        self.released == self.scheduled
    }

    /// Whether an input is scheduled since the latest release.
    #[cfg(test)]
    pub(super) fn holds_input(&self) -> bool {
        self.inputs > self.ready_inputs
    }
}

/// How a burst of an agent's inputs and wakes is held to release as one batch.
#[derive(Clone)]
pub(super) enum Coalescing {
    /// A batch releases `quiet` after the *most recent* event of a burst, so
    /// events that trickle in (a child's progress reply, then its job completion a
    /// few milliseconds later) become one wake instead of two. `cap` bounds a
    /// window from the burst's first event: a steady stream of notifications would
    /// otherwise reset the quiet timer forever, and both an idle agent's wake and
    /// `flush_events` (which blocks a turn on the open window) must stay bounded
    /// regardless of activity.
    Timed { quiet: Duration, cap: Duration },
    /// Under test: a burst with an input releases at once, and one of wakes for
    /// each permit the test adds.
    #[cfg(test)]
    Held(std::sync::Arc<tokio::sync::Semaphore>),
}

#[cfg(test)]
impl Coalescing {
    /// Releases every event as it arrives.
    pub(super) const NONE: Self = Self::Timed {
        quiet: Duration::ZERO,
        cap: Duration::ZERO,
    };

    /// A held window, and the gate the test adds permits to.
    pub(super) fn held() -> (Self, std::sync::Arc<tokio::sync::Semaphore>) {
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        (Self::Held(gate.clone()), gate)
    }
}

const QUIET: Duration = Duration::from_millis(100);

const CAP: Duration = Duration::from_millis(500);

impl Default for Coalescing {
    fn default() -> Self {
        Self::Timed {
            quiet: QUIET,
            cap: CAP,
        }
    }
}

/// The newest revision `agent` scheduled once its window passes `since`, if
/// any; None once the agent is gone.
fn scheduled(agent: &AgentId, since: Option<u64>) -> Wait<Option<u64>> {
    let (key, agent) = (Key::Agent(agent.clone()), agent.clone());
    Wait::on([key], move |state| match state.agents.get(&agent) {
        None => Some(None),
        Some(facts) => {
            let window = &facts.window;
            let since = since.unwrap_or(window.released);
            (window.scheduled > since).then_some(Some(window.scheduled))
        }
    })
}

/// Release each burst of `agent`'s inputs and wakes as `window` decides: the
/// window is trailing, so activity restarts the quiet timer instead of being cut
/// off into the next window. Ends with the agent.
pub(super) async fn coalesce(store: SessionStore, agent: AgentId, window: Coalescing) {
    while let Ok(Some(opened)) = store.until(scheduled(&agent, None)).await {
        if !window.wait_out(&store, &agent, opened).await {
            return;
        }
        let agent = agent.clone();
        let release = move |state: &mut CoreState| {
            (state.agents.get_mut(&agent)).map(|facts| facts.window.release())
        };
        store.live(release).await;
    }
}

impl Coalescing {
    /// Wait out `agent`'s burst that opened at revision `opened`; false once the
    /// agent is gone.
    async fn wait_out(&self, store: &SessionStore, agent: &AgentId, opened: u64) -> bool {
        match self {
            Self::Timed { quiet, cap } => {
                let more = |since| {
                    let (store, more) = (store.clone(), scheduled(agent, Some(since)));
                    async move { store.until(more).await.ok().flatten() }
                };
                burst(*quiet, *cap, more, opened).await
            }
            #[cfg(test)]
            Self::Held(gate) => {
                let (key, agent) = (Key::Agent(agent.clone()), agent.clone());
                let input = Wait::on([key], move |state| match state.agents.get(&agent) {
                    None => Some(false),
                    Some(facts) => facts.window.holds_input().then_some(true),
                });
                tokio::select! {
                    permit = gate.acquire() => permit.map(tokio::sync::SemaphorePermit::forget).is_ok(),
                    input = store.until(input) => input.unwrap_or(false),
                }
            }
        }
    }
}

/// Wait out the burst that opened at revision `seen`: until `more`, the next
/// revision after the one given, stays quiet for `quiet`, or `cap` passes. False
/// once `more` finds the agent gone.
async fn burst<F>(
    quiet: Duration,
    cap: Duration,
    mut more: impl FnMut(u64) -> F,
    mut seen: u64,
) -> bool
where
    F: Future<Output = Option<u64>>,
{
    let cap = tokio::time::sleep(cap);
    tokio::pin!(cap);
    loop {
        tokio::select! {
            () = &mut cap => return true,
            next = tokio::time::timeout(quiet, more(seen)) => match next {
                Ok(Some(next)) => seen = next,
                Ok(None) => return false,
                Err(_) => return true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A burst driven by `schedule`, as revisions sent on the returned sender,
    /// and how long it lasted.
    async fn timed_burst(
        schedule: impl AsyncFnOnce(&tokio::sync::watch::Sender<u64>),
    ) -> std::time::Duration {
        let (revisions, receiver) = tokio::sync::watch::channel(1);
        let more = move |since: u64| {
            let mut receiver = receiver.clone();
            async move {
                let next = receiver.wait_for(|revision| *revision > since).await;
                Some(*next.unwrap())
            }
        };
        let burst = tokio::spawn(async move {
            let start = tokio::time::Instant::now();
            assert!(burst(QUIET, CAP, more, 1).await);
            start.elapsed()
        });
        settle().await;
        schedule(&revisions).await;
        burst.await.unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn trailing_window_coalesces_a_split_burst_into_one_batch() {
        // Each gap ends inside the open quiet period, so every event resets it.
        let quiet = QUIET;
        let gap = quiet - std::time::Duration::from_millis(20);
        let lasted = timed_burst(async |revisions| {
            for revision in 2..=4 {
                tokio::time::advance(gap).await;
                revisions.send_replace(revision);
                settle().await;
            }
        })
        .await;
        // Four events spread over 240ms release once, a quiet period after the last.
        assert_eq!(lasted, gap * 3 + quiet);
    }

    #[tokio::test(start_paused = true)]
    async fn continuous_activity_cannot_postpone_a_batch_past_the_cap() {
        let step = std::time::Duration::from_millis(10);
        let lasted = timed_burst(async |revisions| {
            // Activity never quiets down: only the cap can end this window.
            for revision in 2..=60 {
                tokio::time::advance(step).await;
                revisions.send_replace(revision);
                settle().await;
            }
        })
        .await;
        assert_eq!(lasted, CAP);
    }

    /// Drives one window's timers without the harness: paused time plus the
    /// full session would race the provider gates, and only the release time
    /// is under test here. One yield lets the window task register or rearm
    /// its timers before the next `advance`.
    async fn settle() {
        tokio::task::yield_now().await;
    }
}
