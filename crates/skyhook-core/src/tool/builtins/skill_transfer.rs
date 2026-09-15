//! Host-owned assets are sent to the caller's workspace, not the host tool's workspace.
use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::workspace::{atomic_write, resolve_for_authorization};
use crate::{
    target::TargetRouter,
    tool::{
        PathKind, RegistryError, ToolContext, ToolError, ToolOptions, ToolPlacement,
        ToolRegistryBuilder,
        policy::{ApprovalGrant, Capability, PathAccess, PermissionUse, ResourceId},
    },
};

const COPY_TOOL: &str = "__skill_copy";
pub(super) const MAX_COPY_BYTES: usize = 8 * 1024 * 1024;

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
        |_context, args| async move {
            // Reject the encoded allocation budget before asking the decoder to allocate.
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
            let to = write(std::path::Path::new(&args.to), &bytes).await?;
            Ok(CopyOutput { to })
        },
    )?;
    Ok(())
}

async fn write(path: &std::path::Path, bytes: &[u8]) -> Result<String, ToolError> {
    if tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
    {
        return Err(ToolError::InvalidArguments(
            "to must name a file, not a directory".into(),
        ));
    }
    atomic_write(path, bytes).await?;
    Ok(path.to_string_lossy().into_owned())
}

pub(super) async fn copy(
    router: &TargetRouter,
    context: &ToolContext,
    to: &str,
    bytes: &[u8],
) -> Result<String, ToolError> {
    let caller = context.caller_location();
    if caller.is_root() {
        let resolved = resolve_for_authorization(&caller.workspace, to, PathKind::Writable).await?;
        let mut permissions = vec![PermissionUse::new(
            Capability::Write,
            ResourceId::workspace(&caller.target, &caller.workspace),
        )];
        // Host placement retains the session's authorization root even for a child workspace.
        if !resolved
            .path
            .starts_with(&context.execution_location().workspace)
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
        return write(&resolved.path, bytes).await;
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
        .prepare(route, &caller.workspace, context.invocation_subject()?)
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
        tests::RecordingPolicy,
        tool::{
            authorization::AuthorizationCoordinator,
            executor::ToolExecutor,
            policy::{CapabilitySet, PolicyDecision},
        },
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    fn denying(
        reject: impl Fn(&PermissionUse) -> bool + Send + Sync + 'static,
    ) -> Arc<RecordingPolicy> {
        RecordingPolicy::deciding(move |request| {
            if request.permissions.iter().any(&reject) {
                PolicyDecision::Deny {
                    reason: "fixture denied destination".into(),
                }
            } else {
                PolicyDecision::allow()
            }
        })
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

    struct Fixture {
        runtime: crate::tests::TestRuntime,
        executor: ToolExecutor,
        manager: RemoteManager,
        factory: Arc<RecordingFactory>,
    }

    impl Fixture {
        async fn new(
            policy: Arc<RecordingPolicy>,
            caller: impl FnOnce(&std::path::Path) -> ExecutionLocation,
        ) -> Self {
            let runtime = crate::tests::TestRuntime::new().await;
            let factory = Arc::new(RecordingFactory::default());
            let authorization = AuthorizationCoordinator::new(policy);
            let manager = RemoteManager::new(
                EmbeddedShimCatalog::default(),
                Arc::new(RejectSensitivePrompts),
                authorization.clone(),
            )
            .with_connection_factory(factory.clone());
            let targets = TargetRegistry::from_definitions([TargetDefinition::test(
                "remote",
                "/configured-remote-workspace",
                None,
            )]);
            let router =
                TargetRouter::new(targets.unwrap(), manager.clone(), authorization.clone());
            let mut builder = ToolRegistryBuilder::default();
            let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/skill-workspace");
            let skills = super::super::skills::HostSkills::discover(&fixtures).await;
            let (store, jobs) = (runtime.store.clone(), runtime.jobs.clone());
            super::super::register_coding_tools(&mut builder, store, jobs, skills, router.clone())
                .unwrap();
            let mut capabilities = CapabilitySet::default();
            capabilities.insert(Capability::Targets);
            let root = runtime.root.path().to_path_buf();
            let location = caller(&root);
            let executor = ToolExecutor::with_authorization(
                builder.build(),
                authorization,
                runtime.jobs.clone(),
                root,
            )
            .with_target_router(router)
            .with_location(location)
            .with_capabilities(capabilities);
            Self {
                runtime,
                executor,
                manager,
                factory,
            }
        }

        async fn skill(&self, args: Value) -> Value {
            self.executor
                .run_model(&self.runtime.agent, "skill", args)
                .await
                .unwrap()
                .output
                .value
        }

        fn connections(&self) -> Vec<ConnectionRequest> {
            std::mem::take(&mut *self.factory.0.lock().unwrap())
        }
    }

    fn copy(to: impl Into<Value>) -> Value {
        json!({"name":"mixed-assets","path":"assets/payload.bin","to":to.into()})
    }

    fn remote_caller(_: &std::path::Path) -> ExecutionLocation {
        ExecutionLocation::named("remote", "/remote-only/agent-override".into())
    }

    #[tokio::test]
    async fn remote_skill_reads_stay_host_owned_and_copy_routes_to_caller_override() {
        let policy = RecordingPolicy::allowing();
        let fixture = Fixture::new(policy.clone(), remote_caller).await;
        let caller = remote_caller(fixture.runtime.root.path());
        assert!(fixture.executor.registry().get(COPY_TOOL).is_none());
        for path in [
            Value::Null,
            json!("references/note.txt"),
            json!("assets/pixel.png"),
        ] {
            fixture
                .skill(json!({"name":"mixed-assets","path":path,"to":null}))
                .await;
        }
        fixture
            .executor
            .run_model(&fixture.runtime.agent, "skills", json!({}))
            .await
            .unwrap();
        assert!(fixture.connections().is_empty());
        let result = fixture.skill(copy("remote-only/subdir/copied.bin")).await;
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("fixture transport reached")
        );
        let requests = fixture.connections();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            (requests[0].target.as_str(), &requests[0].workspace),
            ("remote", &caller.workspace)
        );
        let requests = policy.requests.lock().unwrap().clone();
        let writes: Vec<_> = requests
            .iter()
            .flat_map(|r| &r.permissions)
            .filter(|p| p.capability == Capability::Write)
            .collect();
        assert_eq!(writes.len(), 1);
        assert_eq!(
            writes[0].resource,
            ResourceId::workspace("remote", &caller.workspace)
        );
        assert!(!fixture.runtime.root.path().join("copied.bin").exists());
        fixture.manager.shutdown().await;
    }

    #[tokio::test]
    async fn remote_destination_write_denial_precedes_any_connection() {
        let policy = denying(|permission| permission.capability == Capability::Write);
        let fixture = Fixture::new(policy, remote_caller).await;
        let result = fixture.skill(copy("copied.bin")).await;
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("fixture denied destination")
        );
        assert!(fixture.connections().is_empty());
        assert!(!fixture.runtime.root.path().join("copied.bin").exists());
        fixture.manager.shutdown().await;
    }

    #[tokio::test]
    async fn local_child_copy_uses_caller_workspace_and_rejects_outside_symlink() {
        let policy = denying(|permission| matches!(permission.resource, ResourceId::Path { .. }));
        let fixture =
            Fixture::new(policy, |root| ExecutionLocation::root(root.join("child"))).await;
        let child = fixture.runtime.root.path().join("child");
        tokio::fs::create_dir(&child).await.unwrap();
        fixture
            .executor
            .run_model(&fixture.runtime.agent, "skill", copy("copied.bin"))
            .await
            .unwrap();
        assert!(child.join("copied.bin").exists());
        assert!(!fixture.runtime.root.path().join("copied.bin").exists());
        let outside = tempfile::tempdir().unwrap();
        let destination = outside.path().join("denied.bin");
        tokio::fs::write(&destination, b"unchanged").await.unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&destination, child.join("escape")).unwrap();
        for to in [destination.to_string_lossy().into_owned(), "escape".into()] {
            let result = fixture.skill(copy(to)).await;
            assert!(
                result["error"]
                    .as_str()
                    .unwrap()
                    .contains("fixture denied destination")
            );
            assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"unchanged");
        }
        assert!(fixture.connections().is_empty());
        fixture.manager.shutdown().await;
    }
}
