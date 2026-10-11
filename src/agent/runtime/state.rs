//! The agent's state at a request: date, live jobs and todos.
use chrono::Local;

use crate::{
    agent::TodoItem, execution::ExecutionLocation, identity::AgentId, job::JobManager,
    session::RuntimeState, tool::policy::CapabilitySet,
};

pub(super) fn runtime_state_content(
    jobs: &JobManager,
    store: &crate::session::SessionStore,
    agent: &AgentId,
    capabilities: &CapabilitySet,
    location: &ExecutionLocation,
) -> RuntimeState {
    let items = store
        .inspect_todos(agent, None)
        .expect("own todo list is always readable");
    runtime_state_with_todos(jobs, agent, capabilities, items, location)
}

/// Preview a candidate compaction's state without publishing its todos.
pub(super) fn runtime_state_with_todos(
    jobs: &JobManager,
    agent: &AgentId,
    capabilities: &CapabilitySet,
    todos: Vec<TodoItem>,
    location: &ExecutionLocation,
) -> RuntimeState {
    let now = Local::now();
    let jobs = jobs.active_states(agent, capabilities, now.timestamp_millis());
    RuntimeState {
        date: now.format("%Y-%m-%d").to_string(),
        jobs,
        todos,
        location: location.clone(),
    }
}
