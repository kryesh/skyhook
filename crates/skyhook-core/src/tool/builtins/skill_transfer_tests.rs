use super::*;
use crate::{
    execution::ExecutionLocation,
    remote::{
        ConnectionFactory, ConnectionRequest, EmbeddedShimCatalog, RejectSensitivePrompts,
        RemoteError, RemoteManager, backend::Transport,
    },
    target::{TargetDefinition, TargetRegistry},
    tool::{
        authorization::AuthorizationCoordinator,
        executor::ToolExecutor,
        policy::{AuthorizationRequest, CapabilitySet, Policy, PolicyDecision, PolicyFuture},
    },
};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct RecordingPolicy {
    requests: Mutex<Vec<AuthorizationRequest>>,
    deny_write: bool,
    deny_paths: bool,
}
impl Policy for RecordingPolicy {
    fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
        let deny = request.permissions.iter().any(|p| {
            (self.deny_write && p.capability == Capability::Write)
                || (self.deny_paths && p.resource.namespace == "path")
        });
        self.requests.lock().unwrap().push(request);
        Box::pin(async move {
            if deny {
                PolicyDecision::Deny {
                    reason: "fixture denied destination".into(),
                }
            } else {
                PolicyDecision::allow()
            }
        })
    }
}
#[derive(Default)]
struct RecordingFactory(Mutex<Vec<ConnectionRequest>>);
impl ConnectionFactory for RecordingFactory {
    fn connect(
        &self,
        request: ConnectionRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<Transport, RemoteError>> {
        self.0.lock().unwrap().push(request);
        Box::pin(async { Err(RemoteError::Protocol("fixture transport reached".into())) })
    }
}

async fn executor(
    runtime: &crate::test_support::TestRuntime,
    policy: Arc<RecordingPolicy>,
    factory: Arc<RecordingFactory>,
    caller: ExecutionLocation,
) -> (ToolExecutor, RemoteManager) {
    let authorization = AuthorizationCoordinator::new(policy);
    let manager = RemoteManager::new(
        EmbeddedShimCatalog::default(),
        Arc::new(RejectSensitivePrompts),
        authorization.clone(),
    )
    .with_connection_factory(factory);
    let router = TargetRouter::new(
        TargetRegistry::from_definitions([TargetDefinition::test(
            "remote",
            "/configured-remote-workspace",
            None,
        )])
        .unwrap(),
        manager.clone(),
        authorization.clone(),
    );
    let mut builder = ToolRegistryBuilder::default();
    let skills = super::super::skills::HostSkills::discover(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/skill-workspace"),
    )
    .await;
    super::super::register_coding_tools(
        &mut builder,
        runtime.store.clone(),
        runtime.jobs.clone(),
        skills,
        router.clone(),
    )
    .unwrap();
    let mut capabilities = CapabilitySet::default();
    capabilities.insert(Capability::Targets);
    let executor = ToolExecutor::with_authorization(
        builder.build(),
        authorization,
        runtime.jobs.clone(),
        runtime.root.path().to_path_buf(),
    )
    .with_target_router(router)
    .with_location(caller)
    .with_capabilities(capabilities);
    (executor, manager)
}

#[tokio::test]
async fn remote_skill_reads_stay_host_owned_and_copy_routes_to_caller_override() {
    let runtime = crate::test_support::TestRuntime::new().await;
    let policy = Arc::new(RecordingPolicy::default());
    let factory = Arc::new(RecordingFactory::default());
    let caller = ExecutionLocation::named("remote", "/remote-only/agent-override".into());
    let (executor, manager) =
        executor(&runtime, policy.clone(), factory.clone(), caller.clone()).await;
    assert!(executor.registry().get(COPY_TOOL).is_none());
    for args in [
        serde_json::json!({"name":"mixed-assets","path":null,"to":null}),
        serde_json::json!({"name":"mixed-assets","path":"references/note.txt","to":null}),
        serde_json::json!({"name":"mixed-assets","path":"assets/pixel.png","to":null}),
    ] {
        executor
            .execute_model(runtime.agent.clone(), "skill", args, None)
            .await
            .unwrap();
    }
    executor
        .execute_model(runtime.agent.clone(), "skills", serde_json::json!({}), None)
        .await
        .unwrap();
    assert!(factory.0.lock().unwrap().is_empty());
    let result = executor.execute_model(runtime.agent.clone(), "skill", serde_json::json!({"name":"mixed-assets","path":"assets/payload.bin","to":"remote-only/subdir/copied.bin"}), None).await;
    assert!(
        result.unwrap().output.value["error"]
            .as_str()
            .unwrap()
            .contains("fixture transport reached")
    );
    {
        let requests = factory.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].target, "remote");
        assert_eq!(requests[0].workspace, caller.workspace);
    }
    {
        let requests = policy.requests.lock().unwrap();
        let writes = requests
            .iter()
            .flat_map(|r| &r.permissions)
            .filter(|p| p.capability == Capability::Write)
            .collect::<Vec<_>>();
        assert_eq!(writes.len(), 1);
        assert_eq!(
            writes[0].resource,
            ResourceId::workspace("remote", &caller.workspace)
        );
    }
    assert!(!runtime.root.path().join("copied.bin").exists());
    manager.shutdown().await;
}

#[tokio::test]
async fn remote_destination_write_denial_precedes_any_connection() {
    let runtime = crate::test_support::TestRuntime::new().await;
    let policy = Arc::new(RecordingPolicy {
        deny_write: true,
        ..Default::default()
    });
    let factory = Arc::new(RecordingFactory::default());
    let caller = ExecutionLocation::named("remote", "/remote-only/agent-override".into());
    let (executor, manager) = executor(&runtime, policy.clone(), factory.clone(), caller).await;
    let result = executor.execute_model(runtime.agent.clone(), "skill", serde_json::json!({"name":"mixed-assets","path":"assets/payload.bin","to":"copied.bin"}), None).await;
    assert!(
        result.unwrap().output.value["error"]
            .as_str()
            .unwrap()
            .contains("fixture denied destination")
    );
    assert!(factory.0.lock().unwrap().is_empty());
    assert!(!runtime.root.path().join("copied.bin").exists());
    manager.shutdown().await;
}

#[tokio::test]
async fn local_child_copy_uses_caller_workspace_and_rejects_outside_symlink() {
    let runtime = crate::test_support::TestRuntime::new().await;
    let child = runtime.root.path().join("child");
    tokio::fs::create_dir(&child).await.unwrap();
    let policy = Arc::new(RecordingPolicy {
        deny_paths: true,
        ..Default::default()
    });
    let factory = Arc::new(RecordingFactory::default());
    let (executor, manager) = executor(
        &runtime,
        policy,
        factory.clone(),
        ExecutionLocation::root(child.clone()),
    )
    .await;
    executor.execute_model(runtime.agent.clone(), "skill", serde_json::json!({"name":"mixed-assets","path":"assets/payload.bin","to":"copied.bin"}), None).await.unwrap();
    assert!(child.join("copied.bin").exists());
    assert!(!runtime.root.path().join("copied.bin").exists());
    let outside = tempfile::tempdir().unwrap();
    let destination = outside.path().join("denied.bin");
    tokio::fs::write(&destination, b"unchanged").await.unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&destination, child.join("escape")).unwrap();
    for to in [destination.to_string_lossy().into_owned(), "escape".into()] {
        let result = executor
            .execute_model(
                runtime.agent.clone(),
                "skill",
                serde_json::json!({"name":"mixed-assets","path":"assets/payload.bin","to":to}),
                None,
            )
            .await;
        assert!(
            result.unwrap().output.value["error"]
                .as_str()
                .unwrap()
                .contains("fixture denied destination")
        );
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"unchanged");
    }
    assert!(factory.0.lock().unwrap().is_empty());
    manager.shutdown().await;
}
