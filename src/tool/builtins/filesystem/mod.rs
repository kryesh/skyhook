//! Workspace filesystem tools.
use crate::tool::invocation::LocalCatalogBuilder;

mod mutations;
mod read;

use crate::tool::RegistryError;

pub(crate) fn register(builder: &mut LocalCatalogBuilder) -> Result<(), RegistryError> {
    read::register(builder)?;
    mutations::register(builder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolRegistryBuilder;
    use crate::{
        tests::RecordingPolicy,
        tool::{
            executor::ToolExecutor,
            policy::{Capability, ResourceId},
        },
    };
    use serde_json::json;

    #[tokio::test]
    async fn absolute_and_child_workspace_paths_are_authorized_against_the_root() {
        let runtime = crate::tests::TestRuntime::new().await;
        let (root, agent) = (runtime.root.path(), &runtime.agent);
        let child_workspace = root.join("child");
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        std::fs::create_dir_all(&child_workspace).unwrap();
        std::fs::write(child_workspace.join("input.txt"), "child").unwrap();
        std::fs::write(root.join("outside.txt"), "outside").unwrap();
        let mut builder = ToolRegistryBuilder::default();
        builder.register_local(register).unwrap();
        let policy = RecordingPolicy::allowing();
        let workspace = std::fs::canonicalize(root.join("workspace")).unwrap();
        let executor = ToolExecutor::new(
            builder.build(),
            policy.clone(),
            runtime.jobs.clone(),
            workspace,
        );
        let last_permission =
            || policy.requests.lock().unwrap().last().unwrap().permissions[0].clone();

        let outside = std::fs::canonicalize(root.join("outside.txt")).unwrap();
        let read = executor
            .run_host(agent, "read", json!({"path": root.join("outside.txt")}))
            .await;
        assert_eq!(
            read.unwrap().output.value["path"],
            outside.to_string_lossy().as_ref()
        );
        let permission = last_permission();
        assert_eq!(permission.capability, Capability::Read);
        assert_eq!(
            permission.resource,
            ResourceId::path(&crate::target::TargetRef::Root, &outside)
        );

        let destination = root.join("new-outside.txt");
        let write = json!({"path": destination, "content": "new"});
        executor.run_host(agent, "write", write).await.unwrap();
        let permission = last_permission();
        assert_eq!(permission.capability, Capability::Write);
        assert!(matches!(permission.resource, ResourceId::Path { .. }));

        let child_workspace = std::fs::canonicalize(child_workspace).unwrap();
        let location = crate::execution::ExecutionLocation::root(child_workspace.clone());
        let child_executor = executor.clone().with_location(location);
        let read = child_executor
            .run_host(agent, "read", json!({"path": "input.txt"}))
            .await
            .unwrap();
        assert_eq!(read.output.value["path"], "input.txt");
        let expected = ResourceId::path(
            &crate::target::TargetRef::Root,
            &child_workspace.join("input.txt"),
        );
        let requests = policy.requests.lock().unwrap();
        let permissions = &requests.last().unwrap().permissions;
        assert!(
            permissions
                .iter()
                .any(|p| p.capability == Capability::Read && p.resource == expected)
        );
    }
}
