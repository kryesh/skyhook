//! The agent's state at a request: date, live jobs and todos.
use chrono::Local;

use crate::{
    agent::{TodoItem, todo::TodoStore},
    execution::ExecutionLocation,
    identity::AgentId,
    job::JobManager,
    session::{RuntimeState, UserPart},
    tool::policy::CapabilitySet,
};

pub(super) async fn runtime_state_content(
    jobs: &JobManager,
    todos: &TodoStore,
    agent: &AgentId,
    capabilities: &CapabilitySet,
    location: &ExecutionLocation,
) -> UserPart {
    let items = todos
        .inspect(agent, None)
        .await
        .expect("own todo list is always readable")
        .items;
    runtime_state_with_todos(jobs, agent, capabilities, items, location).await
}

/// Preview a candidate compaction's state without publishing its todos.
pub(super) async fn runtime_state_with_todos(
    jobs: &JobManager,
    agent: &AgentId,
    capabilities: &CapabilitySet,
    todos: Vec<TodoItem>,
    location: &ExecutionLocation,
) -> UserPart {
    let now = Local::now();
    let jobs = jobs
        .active_states(agent, capabilities, now.timestamp_millis())
        .await;
    UserPart::State {
        state: RuntimeState {
            date: now.format("%Y-%m-%d").to_string(),
            jobs,
            todos,
            location: location.clone(),
        },
    }
}
