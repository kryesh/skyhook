//! The agent's state at a request: date, live jobs and todos.
use chrono::Local;

use crate::{
    execution::ExecutionLocation, identity::AgentId, job::JobManager, session::RuntimeState,
    tool::policy::CapabilitySet,
};

pub(super) fn runtime_state_content(
    jobs: &JobManager,
    store: &crate::session::SessionStore,
    agent: &AgentId,
    capabilities: &CapabilitySet,
    location: &ExecutionLocation,
) -> RuntimeState {
    let todos = store
        .inspect_todos(agent, None)
        .expect("own todo list is always readable");
    let now = Local::now();
    RuntimeState {
        date: now.format("%Y-%m-%d").to_string(),
        jobs: jobs.active_states(agent, capabilities, now.timestamp_millis()),
        todos,
        location: location.clone(),
    }
}
