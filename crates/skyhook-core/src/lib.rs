//! Skyhook is a provider-neutral coding-agent harness whose registered tools are
//! available both as model tool calls and inside a sandboxed JavaScript runtime.

pub mod agent;
pub mod config;
pub mod execution;
mod fs;
pub mod identity;
pub mod job;
pub mod mcp;
pub mod media;
pub mod provider;
pub mod remote;
pub mod session;
pub mod target;
pub mod tool;

pub(crate) fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    use sha2::Digest as _;
    use std::fmt::Write as _;

    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a string cannot fail");
            output
        })
}

#[cfg(test)]
mod tests {
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
}
