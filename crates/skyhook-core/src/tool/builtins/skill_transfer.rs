//! Host-owned assets are sent to the caller's workspace, not the host tool's workspace.
use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::workspace::{atomic_write, resolve_for_authorization, resolve_writable};
use crate::{
    target::TargetRouter,
    tool::{
        PathKind, RegistryError, ToolContext, ToolError, ToolOptions, ToolPlacement,
        ToolRegistryBuilder,
        policy::{ApprovalGrant, Capability, PathAccess, PermissionUse, ResourceId},
    },
};

const COPY_TOOL: &str = "__skill_copy";
const MAX_COPY_BYTES: usize = 8 * 1024 * 1024;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CopyArgs {
    to: String,
    data_base64: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct CopyOutput {
    to: String,
}

/// Only installed in remote workers, never in the host/model/script registry.
pub(crate) fn register_worker(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    builder.register::<CopyArgs, CopyOutput, _, _>(
        COPY_TOOL,
        "Receive a host-owned skill asset.",
        ToolOptions::new(vec![Capability::Write])
            .script_only()
            .script_unavailable()
            .placement(ToolPlacement::InheritWorkspace)
            .path_argument("to", PathAccess::Write, PathKind::Writable),
        |context, args| async move {
            if args.data_base64.len() > MAX_COPY_BYTES.div_ceil(3) * 4 {
                return Err(ToolError::InvalidArguments(
                    "skill asset is too large".into(),
                ));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(args.data_base64)
                .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
            if bytes.len() > MAX_COPY_BYTES {
                return Err(ToolError::InvalidArguments(
                    "skill asset is too large".into(),
                ));
            }
            let to = write(&context.execution_location.workspace, &args.to, &bytes).await?;
            Ok(CopyOutput { to })
        },
    )?;
    Ok(())
}

async fn write(workspace: &std::path::Path, to: &str, bytes: &[u8]) -> Result<String, ToolError> {
    let path = resolve_writable(workspace, to).await?;
    if tokio::fs::metadata(&path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
    {
        return Err(ToolError::InvalidArguments(
            "to must name a file, not a directory".into(),
        ));
    }
    atomic_write(&path, bytes).await?;
    Ok(path.to_string_lossy().into_owned())
}

pub(super) async fn copy(
    router: &TargetRouter,
    context: &ToolContext,
    to: &str,
    bytes: &[u8],
) -> Result<String, ToolError> {
    let caller = &context.caller_location;
    if caller.is_root() {
        let resolved = resolve_for_authorization(&caller.workspace, to, PathKind::Writable).await?;
        let mut permissions = vec![PermissionUse::new(
            Capability::Write,
            ResourceId::workspace(&caller.target, &caller.workspace),
        )];
        // Host placement retains the session's authorization root even for a child workspace.
        if !resolved
            .path
            .starts_with(&context.execution_location.workspace)
        {
            let resource = ResourceId::path(&caller.target, &resolved.path);
            let grant = if resolved.directory {
                ApprovalGrant::descendants(Capability::Write, resource.clone())
            } else {
                ApprovalGrant::exact(Capability::Write, resource.clone())
            };
            permissions.push(PermissionUse::new(Capability::Write, resource).with_grant(grant));
        }
        router
            .authorize_transfer(
                context,
                "skill",
                permissions,
                serde_json::json!({"to": resolved.path}),
            )
            .await?;
        return write(&caller.workspace, &resolved.path.to_string_lossy(), bytes).await;
    }
    router
        .authorize_transfer(
            context,
            "skill",
            vec![PermissionUse::new(
                Capability::Write,
                ResourceId::workspace(&caller.target, &caller.workspace),
            )],
            serde_json::json!({"to": to}),
        )
        .await?;
    let route = router
        .resolve(&caller.target)
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))?;
    router
        .authorize_transfer(
            context,
            "connect_target",
            vec![route.permission()],
            route.authorization_arguments(),
        )
        .await?;
    let connection = router
        .prepare(route, &caller.workspace, &context.authorization)
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))?;
    // The worker resolves `to` and requests path permissions on its own filesystem.
    // Base64 keeps the maximum 8 MiB asset safely below the protocol's 16 MiB frame limit.
    let result = connection
        .execute(
            COPY_TOOL.into(),
            serde_json::json!({
                "to": to,
                "data_base64": base64::engine::general_purpose::STANDARD.encode(bytes),
            }),
            context,
        )
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))?;
    Ok(serde_json::from_value::<CopyOutput>(result.value)?.to)
}

#[cfg(test)]
mod tests {
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
        runtime: &crate::tests::TestRuntime,
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
        let runtime = crate::tests::TestRuntime::new().await;
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
        let runtime = crate::tests::TestRuntime::new().await;
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
        let runtime = crate::tests::TestRuntime::new().await;
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
}
