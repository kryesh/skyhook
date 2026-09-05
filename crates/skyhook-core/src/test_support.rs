use std::sync::Arc;

use crate::{
    identity::AgentId,
    job::JobManager,
    session::SessionStore,
    tool::{ToolRegistryBuilder, executor::ToolExecutor, policy::AllowAll},
};

pub(crate) struct TestRuntime {
    pub root: tempfile::TempDir,
    pub store: SessionStore,
    pub agent: AgentId,
    pub jobs: JobManager,
}

impl TestRuntime {
    pub async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(&root.path().join("sessions"))
            .await
            .unwrap();
        Self {
            agent: AgentId::root(store.id()),
            jobs: JobManager::new(store.clone()),
            store,
            root,
        }
    }

    pub fn executor(&self, builder: ToolRegistryBuilder) -> ToolExecutor {
        ToolExecutor::new(
            builder.build(),
            Arc::new(AllowAll),
            self.jobs.clone(),
            self.root.path().to_path_buf(),
        )
    }
}
