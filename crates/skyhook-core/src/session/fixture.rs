//! Reusable session fixtures. Every journaled entry references a started agent, so
//! tests start the agents they write for, usually in an in-memory database.

use std::path::Path;

use super::{ProfileSnapshot, SessionEvent, SessionStore};
use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    provider::profile::ModelProfile,
    tool::policy::Capability,
};

/// An in-memory session whose root agent has started.
pub(crate) struct MemorySession {
    /// Workspace directory; the database itself is in memory.
    pub root: tempfile::TempDir,
    pub store: SessionStore,
    pub agent: AgentId,
}

impl MemorySession {
    pub(crate) async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create_ephemeral(root.path()).await.unwrap();
        let agent = started(&store, root.path()).await;
        Self { root, store, agent }
    }

    /// Start `parent`'s child `index`, optionally owned by `owner_job`.
    pub(crate) async fn start_child(
        &self,
        parent: &AgentId,
        index: u32,
        owner_job: Option<JobId>,
    ) -> AgentId {
        start_child(&self.store, parent, index, owner_job, self.root.path()).await
    }
}

/// Journal a session start and its root agent; returns the root.
pub(crate) async fn started(store: &SessionStore, workspace: &Path) -> AgentId {
    let agent = AgentId::root(store.id());
    store
        .append_all(start_events(&agent, workspace))
        .await
        .unwrap();
    agent
}

/// Start `parent`'s child `index` in any store.
pub(crate) async fn start_child(
    store: &SessionStore,
    parent: &AgentId,
    index: u32,
    owner_job: Option<JobId>,
    workspace: &Path,
) -> AgentId {
    let child = parent.child(index);
    let location = ExecutionLocation::root(workspace.to_path_buf());
    store
        .append(
            child.clone(),
            child_started(Some(parent.clone()), owner_job, location),
        )
        .await
        .unwrap();
    child
}

pub(crate) fn start_events(agent: &AgentId, workspace: &Path) -> Vec<(AgentId, SessionEvent)> {
    vec![
        (
            agent.clone(),
            SessionEvent::SessionStarted {
                targets: Vec::new(),
                capabilities: Capability::ALL.to_vec(),
                max_child_depth: 4,
            },
        ),
        (agent.clone(), agent_started(None, workspace)),
    ]
}

pub(crate) fn profile() -> ProfileSnapshot {
    ProfileSnapshot {
        name: "test".into(),
        profile: ModelProfile::new("test", "test", None, 128_000, 4096, false),
    }
}

pub(crate) fn agent_started(parent: Option<AgentId>, workspace: &Path) -> SessionEvent {
    child_started(
        parent,
        None,
        ExecutionLocation::root(workspace.to_path_buf()),
    )
}

/// An agent start owned by `owner_job` at `location`, for fixtures that script children.
pub(crate) fn child_started(
    parent: Option<AgentId>,
    owner_job: Option<JobId>,
    location: ExecutionLocation,
) -> SessionEvent {
    SessionEvent::AgentStarted {
        parent,
        owner_job,
        profile: Some(profile()),
        available_depth: 0,
        capabilities: vec![Capability::Read],
        location,
    }
}
