//! The agent's state at a request: time, live jobs, todos and the host's section.
use chrono::{SubsecRound, Utc};

use super::{SessionRuntime, TurnContext};
use crate::{agent::HarnessError, session::RuntimeState};

impl SessionRuntime {
    pub(super) async fn runtime_state(
        &self,
        turn: &TurnContext<'_>,
    ) -> Result<RuntimeState, HarnessError> {
        // Only a session with the host's tables, its own, has a host section.
        let host = match (&self.harness.host_state, self.store.extension()) {
            (Some(state), Some(extension)) => tokio::select! {
                biased;
                () = turn.cancellation.cancelled() => return Err(HarnessError::Interrupted),
                host = state.state(extension, turn.agent) => host,
            },
            _ => None,
        };
        let todos = self
            .store
            .inspect_todos(turn.agent, None)
            .expect("own todo list is always readable");
        // The journal keeps milliseconds, so a replayed state is the one sent.
        let now = Utc::now().trunc_subsecs(3);
        Ok(RuntimeState {
            time: now,
            jobs: self.jobs.active_states(
                turn.agent,
                &turn.settings.capabilities,
                now.timestamp_millis(),
            ),
            todos,
            location: turn.location.clone(),
            host,
        })
    }
}
