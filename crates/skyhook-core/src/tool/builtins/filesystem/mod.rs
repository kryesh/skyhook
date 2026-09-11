//! Workspace filesystem tools.

mod mutations;
mod read;

pub(super) use read::detect_image;

use crate::{
    session::SessionStore,
    tool::{RegistryError, ToolRegistryBuilder},
};

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    store: SessionStore,
) -> Result<(), RegistryError> {
    read::register(builder, store)?;
    mutations::register(builder)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::tool::{
        executor::ToolExecutor,
        policy::{AuthorizationRequest, Capability, Policy, PolicyDecision, PolicyFuture},
    };
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct RecordingPolicy {
        requests: Mutex<Vec<AuthorizationRequest>>,
    }

    impl Policy for RecordingPolicy {
        fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
            Box::pin(async move {
                self.requests.lock().await.push(request);
                PolicyDecision::allow()
            })
        }
    }

    #[tokio::test]
    async fn absolute_and_child_workspace_paths_are_authorized_against_the_root() {
        let runtime = crate::tests::TestRuntime::new().await;
        let workspace = runtime.root.path().join("workspace");
        let child_workspace = runtime.root.path().join("child");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&child_workspace).unwrap();
        std::fs::write(child_workspace.join("input.txt"), "child").unwrap();
        let outside = runtime.root.path().join("outside.txt");
        std::fs::write(&outside, "outside").unwrap();
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, runtime.store.clone()).unwrap();
        let policy = Arc::new(RecordingPolicy::default());
        let workspace = std::fs::canonicalize(workspace).unwrap();
        let executor = ToolExecutor::new(
            builder.build(),
            policy.clone(),
            runtime.jobs,
            workspace.clone(),
        );

        let read = executor
            .execute(
                runtime.agent.clone(),
                "read",
                serde_json::json!({"path": outside}),
                None,
            )
            .await
            .unwrap();
        let outside = std::fs::canonicalize(runtime.root.path().join("outside.txt")).unwrap();
        assert_eq!(
            read.output.value["path"],
            outside.to_string_lossy().as_ref()
        );
        let requests = policy.requests.lock().await;
        let permission = &requests.last().unwrap().permissions[0];
        assert_eq!(permission.capability, Capability::Read);
        assert_eq!(
            permission.resource,
            crate::tool::policy::ResourceId::path("root", &outside)
        );
        drop(requests);

        let destination = runtime.root.path().join("new-outside.txt");
        executor
            .execute(
                runtime.agent.clone(),
                "write",
                serde_json::json!({"path": destination, "content": "new"}),
                None,
            )
            .await
            .unwrap();
        let requests = policy.requests.lock().await;
        let permission = &requests.last().unwrap().permissions[0];
        assert_eq!(permission.capability, Capability::Write);
        assert_eq!(permission.resource.namespace, "path");
        drop(requests);

        let child_workspace = std::fs::canonicalize(child_workspace).unwrap();
        let child_executor =
            executor
                .clone()
                .with_location(crate::execution::ExecutionLocation::root(
                    child_workspace.clone(),
                ));
        let read = child_executor
            .execute(
                runtime.agent,
                "read",
                serde_json::json!({"path": "input.txt"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(read.output.value["path"], "input.txt");
        let requests = policy.requests.lock().await;
        assert!(requests.last().unwrap().permissions.iter().any(
            |permission| permission.capability == Capability::Read
                && permission.resource
                    == crate::tool::policy::ResourceId::path(
                        "root",
                        &child_workspace.join("input.txt"),
                    )
        ));
    }
}
