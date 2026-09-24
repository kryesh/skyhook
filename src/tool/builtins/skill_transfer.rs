//! Host-owned assets are sent to the caller's workspace, not the host tool's workspace.
use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::workspace::{atomic_write, resolve_for_authorization};
use crate::{
    target::{TargetRef, TargetRouter},
    tool::{
        PathKind, RegistryError, ToolContext, ToolError, ToolOptions, ToolPlacement,
        diagnostic::{Effects, FailureSite, Operation, PartialContext, Subject},
        invocation::{AdmissionError, LocalCatalogBuilder, LocalError},
        policy::{Capability, PathAccess, PermissionUse, ResourceId},
    },
};

const COPY_TOOL: &str = "__skill_copy";
pub(crate) const MAX_COPY_BYTES: usize = 8 * 1024 * 1024;

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
pub(crate) fn register_worker(builder: &mut LocalCatalogBuilder) -> Result<(), RegistryError> {
    builder.register::<CopyArgs, CopyOutput, _, _>(
        COPY_TOOL,
        "Receive a host-owned skill asset.",
        ToolOptions::new(vec![Capability::Write])
            .script_only()
            .script_unavailable()
            .placement(ToolPlacement::InheritWorkspace)
            .path_argument("to", PathAccess::Write, PathKind::Writable),
        |_context, args| async move {
            let bytes = crate::media::decode_base64_bounded(&args.data_base64, MAX_COPY_BYTES)
                .map_err(|error| {
                    LocalError::invalid_arguments(match error {
                        crate::media::MediaError::TooLarge => "skill asset is too large",
                        _ => "skill asset has invalid base64",
                    })
                })?;
            let to = write(std::path::Path::new(&args.to), &bytes).await?;
            Ok(CopyOutput { to })
        },
    )?;
    Ok(())
}

async fn write(path: &std::path::Path, bytes: &[u8]) -> Result<String, AdmissionError> {
    if tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
    {
        return Err(
            AdmissionError::invalid_arguments("to must name a file, not a directory")
                .operation(Operation::Validate, Subject::path(path))
                .effects(Effects::Unchanged),
        );
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
    let TargetRef::Named(target) = &caller.target else {
        let resolved = resolve_for_authorization(&caller.workspace, to, PathKind::Writable)
            .await
            .map_err(|error| {
                error
                    .at(FailureSite::Execution(caller.clone()))
                    .effects(Effects::Unchanged)
            })?;
        let mut permissions = vec![PermissionUse::new(
            Capability::Write,
            ResourceId::workspace(&caller.target, &caller.workspace),
        )];
        // Host placement retains the session's authorization root even for a child workspace.
        if !resolved
            .path
            .starts_with(&context.execution_location().workspace)
        {
            permissions.push(resolved.permission(Capability::Write, &caller.target));
        }
        router
            .authorize_transfer(
                context,
                "skill",
                permissions,
                serde_json::json!({"to": resolved.path}),
            )
            .await
            .map_err(|error| {
                error
                    .operation(Operation::Authorize, Subject::path(&resolved.path))
                    .at(FailureSite::Execution(caller.clone()))
                    .effects(Effects::Unchanged)
            })?;
        return write(&resolved.path, bytes)
            .await
            .map_err(|error| ToolError::from(error).at(FailureSite::Execution(caller.clone())));
    };
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
        .await
        .map_err(|error| {
            error
                .operation(Operation::Authorize, Subject::path(to))
                .at(FailureSite::Execution(caller.clone()))
                .effects(Effects::Unchanged)
        })?;
    let route = router
        .resolve(target, context.capabilities())
        .await
        .map_err(|error| {
            ToolError::from(error.into_admission_error())
                .operation(
                    Operation::Lookup,
                    Subject::Label("copy destination target".into()),
                )
                .at(FailureSite::Host)
                .effects(Effects::Unchanged)
        })?;
    router
        .authorize_transfer(
            context,
            "connect_target",
            route.permissions(),
            route.authorization_arguments(),
        )
        .await
        .map_err(|error| {
            error
                .operation(
                    Operation::Authorize,
                    Subject::Label("target connection".into()),
                )
                .at(FailureSite::Execution(caller.clone()))
                .effects(Effects::Unchanged)
        })?;
    let connection = router
        .prepare(route, &caller.workspace, context.invocation_subject()?)
        .await
        .map_err(|error| {
            error.into_tool_error().or(PartialContext::new(
                Operation::Connect,
                Subject::working_directory(&caller.workspace),
            )
            .at(FailureSite::Execution(caller.clone()))
            .effects(Effects::Unchanged))
        })?;
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
        .map_err(|error| {
            error
                .into_tool_error()
                .or(PartialContext::new(Operation::Copy, Subject::path(to))
                    .at(FailureSite::Execution(caller.clone())))
        })?;
    Ok(serde_json::from_value::<CopyOutput>(result.value)
        .map_err(|error| {
            ToolError::from(error)
                .operation(
                    Operation::Deserialize,
                    Subject::Label("skill copy result".into()),
                )
                .at(FailureSite::Host)
        })?
        .to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolRegistryBuilder, authorization::AuthorizationError};
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
            diagnostic::Cause,
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

    enum TransportReply {
        ConnectionFailure(RemoteError),
        Worker(LocalError),
    }

    impl Default for TransportReply {
        fn default() -> Self {
            Self::ConnectionFailure(RemoteError::Protocol(
                crate::remote::ProtocolError::Violation("fixture transport reached"),
            ))
        }
    }

    #[derive(Default)]
    struct RecordingFactory {
        requests: Mutex<Vec<ConnectionRequest>>,
        reply: Mutex<TransportReply>,
    }

    impl ConnectionFactory for RecordingFactory {
        fn connect(
            &self,
            request: ConnectionRequest,
        ) -> futures_util::future::BoxFuture<'static, Result<Transport, RemoteError>> {
            self.requests.lock().unwrap().push(request);
            let reply = std::mem::take(&mut *self.reply.lock().unwrap());
            Box::pin(async move {
                match reply {
                    TransportReply::ConnectionFailure(error) => Err(error),
                    TransportReply::Worker(error) => Ok(crate::remote::test_transport(Some(error))),
                }
            })
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
                .join("tests/fixtures/skill-workspace");
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
            std::mem::take(&mut *self.factory.requests.lock().unwrap())
        }
    }

    fn copy(to: impl Into<Value>) -> Value {
        json!({"name":"mixed-assets","path":"assets/payload.bin","to":to.into()})
    }

    fn remote_caller(_: &std::path::Path) -> ExecutionLocation {
        ExecutionLocation::named(
            "remote".parse().unwrap(),
            "/remote-only/agent-override".into(),
        )
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
            json!("assets/payload.bin"),
        ] {
            let result = fixture
                .skill(json!({"name":"mixed-assets","path":path,"to":null}))
                .await;
            assert_eq!(result["state"], "completed", "{result}");
            assert!(result["error"].is_null(), "{result}");
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
            (requests[0].route[0].name.as_str(), &requests[0].workspace),
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
            ResourceId::workspace(&"remote".parse().unwrap(), &caller.workspace)
        );
        assert!(!fixture.runtime.root.path().join("copied.bin").exists());
        fixture.manager.shutdown().await;
    }

    #[tokio::test]
    async fn remote_callers_receive_host_context_for_skill_source_failures() {
        let fixture = Fixture::new(RecordingPolicy::allowing(), remote_caller).await;
        for to in [Value::Null, json!("copied.bin")] {
            let result = fixture
                .skill(json!({"name":"mixed-assets", "path":"missing.bin", "to":to}))
                .await;
            let error = result["error"].as_str().unwrap();
            // The source lives on the host, not at the remote caller's location.
            assert!(error.contains("session host"), "{error}");
        }
        assert!(fixture.connections().is_empty());
        fixture.manager.shutdown().await;
    }

    async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(20), future)
            .await
            .expect("remote skill copy stalled")
    }

    #[tokio::test]
    async fn remote_copy_connection_and_worker_denials_retain_classification() {
        for (reply, operation) in [
            (
                TransportReply::ConnectionFailure(RemoteError::Authorization(
                    AuthorizationError::Denied("connection denied".into()),
                )),
                Operation::Connect,
            ),
            (
                TransportReply::Worker(
                    LocalError::denied("worker denied")
                        .operation(Operation::Authorize, Subject::path("copied.bin"))
                        .effects(Effects::Unchanged),
                ),
                Operation::Authorize,
            ),
        ] {
            let fixture = Fixture::new(RecordingPolicy::allowing(), remote_caller).await;
            *fixture.factory.reply.lock().unwrap() = reply;
            let error = bounded(fixture.executor.run_host(
                &fixture.runtime.agent,
                "skill",
                copy("copied.bin"),
            ))
            .await
            .unwrap_err()
            .into_tool_error();
            let diagnostic = error.diagnostic();
            assert!(matches!(diagnostic.cause, Cause::Denied(_)));
            assert_eq!(diagnostic.context.operation, operation);
            assert_eq!(
                diagnostic.context.site,
                FailureSite::Execution(remote_caller(fixture.runtime.root.path()))
            );
            assert_eq!(fixture.connections().len(), 1);
            fixture.manager.shutdown().await;
        }
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
