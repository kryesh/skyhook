//! Validate and authorize arguments, then select where an invocation runs.

use super::*;

impl ToolExecutor {
    async fn resolve_workspace_invocation(
        &self,
        tool: &crate::tool::RegisteredTool,
        arguments: &Value,
    ) -> Result<SelectedLocation, ExecutionError> {
        if tool.placement() == ToolPlacement::Host {
            return Ok(SelectedLocation {
                location: self.shared.root_location.clone(),
                route: None,
            });
        }
        let explicit = if tool.placement() == ToolPlacement::TargetedWorkspace {
            if arguments.get("target").is_some() && !self.capabilities.contains(Capability::Targets)
            {
                return Err(ToolError::InvalidArguments(
                    "target selection requires the targets capability".to_owned(),
                )
                .into());
            }
            match arguments.get("target") {
                Some(Value::String(target)) => Some(target.as_str()),
                Some(Value::Null) | None => None,
                Some(_) => {
                    return Err(
                        ToolError::InvalidArguments("target must be a string".to_owned()).into(),
                    );
                }
            }
        } else if arguments.get("target").is_some() {
            return Err(ToolError::InvalidArguments(format!(
                "tool `{}` does not accept a target",
                tool.name()
            ))
            .into());
        } else {
            None
        };
        let selected = explicit.unwrap_or(&self.caller_location.target);
        if selected == ROOT_TARGET {
            return Ok(SelectedLocation {
                location: ExecutionLocation::select(
                    &self.caller_location,
                    &self.shared.root_location.workspace,
                    if explicit.is_some() {
                        crate::execution::LocationSelection::Root
                    } else {
                        crate::execution::LocationSelection::Inherit
                    },
                ),
                route: None,
            });
        }
        let router = self.shared.router.as_ref().ok_or_else(|| {
            ToolError::InvalidArguments(
                "remote targets are unavailable in this tool runtime".to_owned(),
            )
        })?;
        let route = router
            .resolve(selected)
            .await
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let definition = route.destination();
        Ok(SelectedLocation {
            location: ExecutionLocation::select(
                &self.caller_location,
                &self.shared.root_location.workspace,
                if explicit.is_some() {
                    crate::execution::LocationSelection::Other(definition)
                } else {
                    crate::execution::LocationSelection::Inherit
                },
            ),
            route: Some(PlannedRemote {
                route,
                router: router.clone(),
            }),
        })
    }

    async fn prepare_invocation(
        &self,
        kind: InvocationKind,
        agent: &AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
        authorization_scope: Option<u64>,
    ) -> Result<PreparedInvocation, ExecutionError> {
        let tool = self
            .shared
            .registry
            .get(name)
            .ok_or_else(|| ExecutionError::UnknownTool(name.to_owned()))?;
        let spec = tool
            .spec(&self.capabilities, agent)
            .ok_or_else(|| ToolError::InvalidArguments(format!("tool `{name}` is unavailable")))?;
        spec.validate_arguments(&arguments)?;
        validate_invocation(&spec, kind)?;
        let original_arguments = arguments.clone();
        let (mut handler_arguments, background) =
            self.shared.registry.split_execution(&spec, arguments)?;
        let job_name = tool.take_job_name(&mut handler_arguments)?;
        Ok(PreparedInvocation {
            tool,
            original_arguments,
            handler_arguments,
            background,
            job_name,
            authorization_scope: self
                .authorization_scope(parent, authorization_scope)
                .await?,
        })
    }

    pub(super) async fn plan_registered(
        &self,
        kind: InvocationKind,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
        authorization_scope: Option<u64>,
    ) -> Result<InvocationPlan, ExecutionError> {
        let PreparedInvocation {
            tool,
            original_arguments,
            handler_arguments: mut arguments,
            background,
            job_name,
            authorization_scope,
        } = self
            .prepare_invocation(kind, &agent, name, arguments, parent, authorization_scope)
            .await?;
        let selected = self
            .resolve_workspace_invocation(&tool, &original_arguments)
            .await?;
        if tool.placement() == ToolPlacement::TargetedWorkspace {
            arguments
                .as_object_mut()
                .ok_or(ToolError::ArgumentsMustBeObject)?
                .remove("target");
        }
        tool.validate_arguments(&arguments)?;
        let (path_permissions, read_error) = if selected.route.is_none() {
            preflight_path_arguments(
                &tool,
                &selected.location.target,
                &selected.location.workspace,
                &self.shared.root_location.workspace,
                &mut arguments,
            )
            .await?
        } else {
            (Vec::new(), None)
        };
        // Remote location frames spell the workspace as text. Local handlers use
        // the native path, and argument paths are checked where they are spelled.
        if selected.route.is_some() {
            path_text(&selected.location.workspace)?;
        }
        let argument_permissions = tool.argument_permissions(&selected.location, &arguments)?;
        let mut capabilities = tool.capabilities();
        for permission in &argument_permissions {
            if !self.capabilities.contains(permission.capability) {
                return Err(ExecutionError::UnavailableTool(name.to_owned()));
            }
            capabilities.retain(|candidate| *candidate != permission.capability);
        }
        // An unresolved read still requires the ordinary workspace authorization,
        // as well as approval for the unresolved path below.
        if read_error.is_none() {
            for permission in &path_permissions {
                capabilities.retain(|candidate| *candidate != permission.capability);
            }
        }
        let mut permissions =
            scope_capabilities(capabilities, &selected.location, tool.permission_resource());
        permissions.extend(path_permissions);
        // Destination-derived permissions are approved once on the worker, which
        // forwards them to the host with its actual target identity. This avoids
        // duplicate network prompts while still authorizing before any request.
        permissions.extend(argument_permissions.into_iter().filter(|permission| {
            selected.route.is_none()
                || !matches!(
                    permission.resource,
                    ResourceId::Path { .. } | ResourceId::Network { .. }
                )
        }));
        let authorization_arguments = if let Some(route) = &selected.route {
            permissions.push(route.route.permission());
            serde_json::json!({
                "tool": original_arguments,
                "route": route.route.authorization_arguments(),
            })
        } else {
            original_arguments.clone()
        };
        // Schema-valid input that the typed handler rejects still owns a job,
        // approval, and failure. A remote dispatch admits only on its destination.
        let dispatch = match (read_error, selected.route) {
            (Some(output), _) => InvocationDispatch::ReadError(output),
            (None, Some(remote)) => InvocationDispatch::Remote { remote, arguments },
            (None, None) => InvocationDispatch::Local(tool.admit(arguments)),
        };
        Ok(InvocationPlan {
            origin: if matches!(kind, InvocationKind::Model) {
                self.model_origin.clone()
            } else {
                None
            },
            agent,
            tool,
            original_arguments,
            authorization_arguments,
            caller_location: self.caller_location.clone(),
            execution_location: selected.location,
            permissions,
            parent,
            authorization_scope,
            background,
            job_name,
            dispatch,
        })
    }

    async fn authorization_scope(
        &self,
        parent: Option<JobId>,
        requested: Option<u64>,
    ) -> Result<Option<u64>, JobError> {
        Ok(match (requested, parent) {
            (Some(scope), _) => Some(scope),
            (None, Some(parent)) => self.shared.jobs.authorization_scope(parent).await?,
            (None, None) => None,
        })
    }
}

struct SelectedLocation {
    location: ExecutionLocation,
    route: Option<PlannedRemote>,
}

async fn preflight_path_arguments(
    tool: &crate::tool::RegisteredTool,
    target: &str,
    workspace: &std::path::Path,
    authorization_root: &std::path::Path,
    arguments: &mut Value,
) -> Result<(Vec<PermissionUse>, Option<ToolOutput>), ToolError> {
    if !arguments.is_object() {
        return Err(ToolError::ArgumentsMustBeObject);
    }
    let mut permissions = Vec::new();
    let mut read_error = None;
    for spec in tool.path_arguments(arguments)? {
        let Some(input) = spec.input(arguments)?.map(str::to_owned) else {
            continue;
        };
        let resolved = match resolve_for_authorization(workspace, &input, spec.kind).await {
            Ok(resolved) => resolved,
            Err(error) => {
                let Some(output) = tool.read_error_output(&input, &error) else {
                    return Err(error);
                };
                // Canonicalization failed, so this path is not proven to be within
                // the authorization root. Require exact path authorization even for
                // apparently local paths, then return the captured failure without
                // retrying the handler (which could now access a changed target).
                let path = lexical_path(workspace, &input)?;
                let capability = spec.access.capability();
                path_text(&path)?;
                let resource = ResourceId::path(target, &path);
                permissions.push(
                    PermissionUse::new(capability, resource.clone())
                        .with_grant(ApprovalGrant::exact(capability, resource)),
                );
                read_error = Some(output);
                continue;
            }
        };
        // Both the existing handler JSON and permission resource are Unicode
        // boundaries. Never authorize a replacement-character alias.
        let value = Value::String(path_text(&resolved.path)?.to_owned());
        spec.rewrite(arguments, value)?;
        if matches!(spec.binding, crate::tool::registry::PathBinding::Pointer(_))
            || !resolved.path.starts_with(authorization_root)
        {
            let capability = spec.access.capability();
            let resource = ResourceId::path(target, &resolved.path);
            let grant = if resolved.directory {
                ApprovalGrant::descendants(capability, resource.clone())
            } else {
                ApprovalGrant::exact(capability, resource.clone())
            };
            permissions.push(PermissionUse::new(capability, resource).with_grant(grant));
        }
    }
    Ok((permissions, read_error))
}

/// Validate the existing string-only permission/wire boundary without changing
/// native path identity. Lossless byte-path protocols are a separate migration.
fn path_text(path: &std::path::Path) -> Result<&str, ToolError> {
    path.to_str().ok_or_else(|| {
        ToolError::InvalidArguments(
            "native path cannot be represented losslessly by the permission or wire format"
                .to_owned(),
        )
    })
}

fn scope_capabilities(
    capabilities: Vec<Capability>,
    location: &ExecutionLocation,
    override_resource: Option<&ResourceId>,
) -> Vec<PermissionUse> {
    let resource = override_resource
        .cloned()
        .unwrap_or_else(|| ResourceId::workspace(&location.target, &location.workspace));
    capabilities
        .into_iter()
        .map(|capability| PermissionUse::new(capability, resource.clone()))
        .collect()
}

fn validate_invocation(
    tool: &crate::tool::ToolSpec,
    kind: InvocationKind,
) -> Result<(), ExecutionError> {
    match kind {
        InvocationKind::Host => Ok(()),
        InvocationKind::Model if tool.exposure == ToolExposure::ScriptOnly => {
            Err(ExecutionError::ModelHidden(tool.name.clone()))
        }
        InvocationKind::Script if tool.script_binding == ScriptBinding::Unavailable => {
            Err(ExecutionError::ScriptUnavailable(tool.name.clone()))
        }
        InvocationKind::Model | InvocationKind::Script => Ok(()),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::*;
    use crate::{
        target::{TargetDefinition, TargetRegistry},
        tests::RecordingPolicy,
        tool::{
            ToolOptions, ToolRegistryBuilder,
            policy::{AuthorizationRequest, PolicyDecision, PolicyFuture},
        },
    };

    /// Terse host/model invocations without a parent job for tests across the crate.
    impl ToolExecutor {
        pub(crate) async fn run_host(
            &self,
            agent: &AgentId,
            name: &str,
            arguments: Value,
        ) -> Result<ExecutionResult, ExecutionError> {
            self.execute(agent.clone(), name, arguments, None).await
        }

        pub(crate) async fn run_model(
            &self,
            agent: &AgentId,
            name: &str,
            arguments: Value,
        ) -> Result<ExecutionResult, ExecutionError> {
            self.execute_model(agent.clone(), name, arguments, None)
                .await
        }
    }

    #[derive(Deserialize, JsonSchema)]
    struct PathArgs {
        #[serde(rename = "path")]
        _path: String,
    }

    pub(in crate::tool::executor) fn router(
        targets: TargetRegistry,
        policy: Arc<dyn Policy>,
    ) -> TargetRouter {
        let authorization = AuthorizationCoordinator::new(policy);
        let remote = crate::remote::RemoteManager::new(
            crate::remote::EmbeddedShimCatalog::default(),
            Arc::new(crate::remote::RejectSensitivePrompts),
            authorization.clone(),
        );
        TargetRouter::new(targets, remote, authorization)
    }

    fn network_builder() -> ToolRegistryBuilder {
        use crate::tool::{PathArgument, PathKind, policy::PathAccess};
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_dynamic(
                "network_test",
                "Exercise invocation-derived authorization",
                serde_json::json!({"type":"object","properties":{
                    "url":{"type":"string"}, "body":{}, "save_to":{}, "redirect":{}, "insecure":{}
                }}),
                ToolOptions::new(vec![Capability::Network])
                    .placement(ToolPlacement::TargetedWorkspace)
                    .background()
                    .named()
                    .argument_validator(|arguments| {
                        if arguments["url"] != "https://initial.test" {
                            return Err(ToolError::InvalidArguments("invalid test URL".to_owned()));
                        }
                        Ok(())
                    })
                    .argument_permissions(|location, _arguments| {
                        Ok(vec![PermissionUse::new(
                            Capability::Network,
                            ResourceId::network(&location.target, "https://initial.test"),
                        )])
                    })
                    .argument_paths(|arguments| {
                        let mut paths = Vec::new();
                        if arguments["body"]["kind"] == "file" {
                            paths.push(PathArgument::pointer(
                                "/body/path",
                                PathAccess::Read,
                                PathKind::Existing,
                            ));
                        }
                        if let Some(parts) = arguments["body"]["parts"].as_array() {
                            for (index, part) in parts.iter().enumerate() {
                                if part.get("path").is_some() {
                                    paths.push(PathArgument::pointer(
                                        format!("/body/parts/{index}/path"),
                                        PathAccess::Read,
                                        PathKind::Existing,
                                    ));
                                }
                            }
                        }
                        if arguments.get("save_to").is_some() {
                            paths.push(PathArgument::pointer(
                                "/save_to",
                                PathAccess::Write,
                                PathKind::Writable,
                            ));
                        }
                        Ok(paths)
                    }),
                |context, arguments| async move {
                    if arguments["redirect"] == true {
                        context.authorize_network("https://redirect.test").await?;
                    }
                    Ok(ToolOutput::new(arguments))
                },
            )
            .unwrap();
        builder
    }

    async fn plan(
        executor: &ToolExecutor,
        agent: &AgentId,
        name: &str,
        arguments: Value,
    ) -> InvocationPlan {
        executor
            .plan_registered(
                InvocationKind::Host,
                agent.clone(),
                name,
                arguments,
                None,
                None,
            )
            .await
            .unwrap()
    }

    fn network_use(origin: &str) -> PermissionUse {
        PermissionUse::new(Capability::Network, ResourceId::network("root", origin))
    }

    #[tokio::test]
    async fn network_only_invocations_have_exact_origin_permissions_each_time() {
        let runtime = crate::tests::TestRuntime::new().await;
        let policy = RecordingPolicy::allowing();
        let mut capabilities = CapabilitySet::default();
        capabilities.remove(Capability::Read);
        capabilities.remove(Capability::Write);
        let executor = runtime
            .executor_with_policy(network_builder(), policy.clone())
            .with_capabilities(capabilities);
        for _ in 0..2 {
            let arguments = serde_json::json!({"url":"https://initial.test"});
            executor
                .run_host(&runtime.agent, "network_test", arguments)
                .await
                .unwrap();
        }
        let requests = policy.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_eq!(request.permissions, [network_use("https://initial.test")]);
            assert!(request.permissions[0].proposed_grant.is_none());
        }
    }

    #[tokio::test]
    async fn network_paths_and_invalid_arguments_fail_before_approval() {
        let runtime = crate::tests::TestRuntime::new().await;
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(runtime.root.path().join("upload"), "data").unwrap();
        std::fs::write(outside.path().join("upload"), "data").unwrap();
        for directory in [std::path::Path::new(""), outside.path()] {
            let upload = directory.join("upload");
            for (removed, extra) in [
                (
                    Capability::Read,
                    serde_json::json!({"body":{"kind":"file","path":upload}}),
                ),
                (
                    Capability::Read,
                    serde_json::json!({"body":{"kind":"multipart","parts":[{"name":"upload","path":upload}]}}),
                ),
                (
                    Capability::Write,
                    serde_json::json!({"save_to":directory.join("download")}),
                ),
            ] {
                let policy = RecordingPolicy::allowing();
                let mut capabilities = CapabilitySet::default();
                capabilities.remove(removed);
                let executor = runtime
                    .executor_with_policy(network_builder(), policy.clone())
                    .with_capabilities(capabilities);
                let mut arguments = serde_json::json!({"url":"https://initial.test"});
                arguments
                    .as_object_mut()
                    .unwrap()
                    .extend(extra.as_object().unwrap().clone());
                let error = executor
                    .run_host(&runtime.agent, "network_test", arguments)
                    .await
                    .unwrap_err();
                assert!(
                    matches!(error, ExecutionError::UnavailableTool(_)),
                    "{error:?}"
                );
                assert!(policy.requests.lock().unwrap().is_empty());
            }
        }
        // Invalid arguments fail before approval or path resolution.
        let policy = RecordingPolicy::allowing();
        let executor = runtime.executor_with_policy(network_builder(), policy.clone());
        let arguments =
            serde_json::json!({"url":"invalid", "body":{"kind":"file", "path":"missing"}});
        let error = executor
            .run_host(&runtime.agent, "network_test", arguments)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ExecutionError::Tool(ToolError::InvalidArguments(_))
        ));
        assert!(policy.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn network_nested_paths_are_rewritten_and_authorized_including_in_root() {
        let runtime = crate::tests::TestRuntime::new().await;
        std::fs::write(runtime.root.path().join("upload"), "data").unwrap();
        let policy = RecordingPolicy::allowing();
        let executor = runtime.executor_with_policy(network_builder(), policy.clone());
        let arguments = serde_json::json!({
            "url":"https://initial.test", "body":{"kind":"multipart","parts":[{"name":"upload","path":"upload"},{"name":"message","text":"not a file"}]}, "save_to":"download"
        });
        let plan = plan(&executor, &runtime.agent, "network_test", arguments.clone()).await;
        let (upload, download) = (
            runtime.root.path().join("upload"),
            runtime.root.path().join("download"),
        );
        for (capability, path) in [(Capability::Read, &upload), (Capability::Write, &download)] {
            let resource = ResourceId::path("root", path);
            assert!(
                plan.permissions
                    .iter()
                    .any(|p| p.capability == capability && p.resource == resource)
            );
        }
        assert_eq!(plan.permissions.len(), 3);
        assert!(policy.requests.lock().unwrap().is_empty());
        // The echoing handler receives rewritten nested and save_to paths.
        let echoed = executor
            .run_host(&runtime.agent, "network_test", arguments)
            .await
            .unwrap()
            .output
            .value;
        assert_eq!(
            echoed.pointer("/body/parts/0/path"),
            Some(&serde_json::json!(upload))
        );
        assert_eq!(echoed["save_to"], serde_json::json!(download));
    }

    #[tokio::test]
    async fn remote_network_permissions_are_deferred_but_other_dynamic_permissions_are_not() {
        let runtime = crate::tests::TestRuntime::new().await;
        let policy = RecordingPolicy::allowing();
        let mut builder = network_builder();
        let exec = PermissionUse::new(Capability::Exec, ResourceId::session("dynamic-command"));
        let permission = exec.clone();
        builder
            .register_dynamic(
                "dynamic_exec",
                "test generic dynamic permissions",
                serde_json::json!({"type":"object","properties":{}}),
                ToolOptions::new(vec![Capability::Exec])
                    .placement(ToolPlacement::TargetedWorkspace)
                    .argument_permissions(move |_, _| Ok(vec![permission.clone()])),
                |_, _| async { Ok(ToolOutput::new(Value::Null)) },
            )
            .unwrap();
        let targets =
            TargetRegistry::from_definitions([TargetDefinition::test("build", "/build", None)])
                .unwrap();
        let mut capabilities = CapabilitySet::default();
        capabilities.insert(Capability::Targets);
        let executor = runtime
            .executor_with_policy(builder, policy.clone())
            .with_target_router(router(targets, policy.clone()))
            .with_capabilities(capabilities);
        let arguments = serde_json::json!({"url":"https://initial.test", "target":"build"});
        let network = plan(&executor, &runtime.agent, "network_test", arguments).await;
        assert!(matches!(
            network.dispatch,
            InvocationDispatch::Remote { .. }
        ));
        assert!(
            !network
                .permissions
                .iter()
                .any(|p| p.capability == Capability::Network)
        );
        let arguments = serde_json::json!({"target":"build"});
        let command = plan(&executor, &runtime.agent, "dynamic_exec", arguments).await;
        assert!(command.permissions.contains(&exec));
        assert!(policy.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn runtime_network_authorization_preserves_invocation_and_denies_redirect() {
        let runtime = crate::tests::TestRuntime::new().await;
        let policy = RecordingPolicy::deciding(|request| {
            if request.permissions.iter().any(|permission| {
                matches!(&permission.resource, ResourceId::Network { origin, .. } if origin == "https://redirect.test")
            }) {
                PolicyDecision::Deny { reason: "redirect denied".to_owned() }
            } else {
                PolicyDecision::allow()
            }
        });
        let executor = runtime.executor_with_policy(network_builder(), policy.clone());
        let arguments =
            serde_json::json!({"url":"https://initial.test", "redirect":true,"insecure":true});
        let error = executor
            .run_host(&runtime.agent, "network_test", arguments)
            .await
            .unwrap_err();
        assert!(matches!(error, ExecutionError::Denied(_)), "{error:?}");
        let requests = policy.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            (&requests[0].job, &requests[0].agent),
            (&requests[1].job, &requests[1].agent)
        );
        assert_eq!(requests[1].arguments["insecure"], true);
        assert_eq!(
            requests[1].arguments["network_origin"],
            "https://redirect.test"
        );
        assert_eq!(
            requests[1].permissions,
            [network_use("https://redirect.test")]
        );
    }

    #[tokio::test]
    async fn missing_builtin_reads_still_require_policy_approval() {
        struct DenyReads;
        impl Policy for DenyReads {
            fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
                assert!(
                    request
                        .permissions
                        .iter()
                        .any(|p| p.capability == Capability::Read)
                );
                Box::pin(async {
                    PolicyDecision::Deny {
                        reason: "read forbidden".into(),
                    }
                })
            }
        }
        let runtime = crate::tests::TestRuntime::new().await;
        let mut builder = ToolRegistryBuilder::default();
        crate::tool::builtins::register_worker_tools(&mut builder, runtime.store.clone()).unwrap();
        let executor = runtime.executor_with_policy(builder, Arc::new(DenyReads));
        for path in ["missing/nested/file", "../outside/missing"] {
            let arguments = serde_json::json!({"path":path});
            let error = executor
                .execute_model(runtime.agent.clone(), "read", arguments, None)
                .await
                .unwrap_err();
            assert!(matches!(error, ExecutionError::Denied(_)));
        }
    }

    #[tokio::test]
    async fn a_registered_name_read_does_not_opt_into_io_recovery() {
        let runtime = crate::tests::TestRuntime::new().await;
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<PathArgs, String, _, _>(
                "read",
                "custom read",
                ToolOptions::new(vec![Capability::Read]).path_argument(
                    "path",
                    crate::tool::policy::PathAccess::Read,
                    crate::tool::PathKind::Existing,
                ),
                |_context, _arguments| async {
                    Err(ToolError::Io(std::io::ErrorKind::PermissionDenied.into()))
                },
            )
            .unwrap();
        let executor = runtime.executor(builder);
        let read = |path: &str| {
            executor.execute(
                runtime.agent.clone(),
                "read",
                serde_json::json!({"path":path}),
                None,
            )
        };
        let error = read("missing/nested").await.unwrap_err();
        assert!(
            matches!(error, ExecutionError::Tool(ToolError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound)
        );
        assert!(matches!(
            read(".").await.unwrap_err(),
            ExecutionError::Failed { .. }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_unicode_argument_paths_fail_but_path_free_tools_run_in_non_unicode_workspaces() {
        use std::os::unix::{ffi::OsStringExt, fs::symlink};
        let runtime = crate::tests::TestRuntime::new().await;
        let non_unicode = |name: &[u8]| {
            runtime
                .root
                .path()
                .join(std::ffi::OsString::from_vec(name.to_vec()))
        };
        let native = non_unicode(b"native-\xff");
        std::fs::write(&native, "secret").unwrap();
        symlink(&native, runtime.root.path().join("unicode-alias")).unwrap();
        let workspace = non_unicode(b"workspace-\xff");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("upload"), "data").unwrap();
        let executor = |workspace: std::path::PathBuf, policy| {
            ToolExecutor::new(
                network_builder().build(),
                policy,
                runtime.jobs.clone(),
                workspace,
            )
        };
        // A canonical argument path is spelled in the string permission/resource
        // boundary, so it fails before approval wherever its bytes come from.
        for (workspace, arguments) in [
            (
                runtime.root.path().to_owned(),
                serde_json::json!({
                    "url": "https://initial.test", "body": {"kind": "file", "path": "unicode-alias"}
                }),
            ),
            (
                workspace.clone(),
                serde_json::json!({
                    "url": "https://initial.test", "body": {"kind": "file", "path": "upload"}
                }),
            ),
        ] {
            let policy = RecordingPolicy::allowing();
            let result = executor(workspace, policy.clone())
                .run_host(&runtime.agent, "network_test", arguments)
                .await;
            assert!(
                matches!(result, Err(ExecutionError::Tool(ToolError::InvalidArguments(message))) if message.contains("losslessly"))
            );
            assert!(policy.requests.lock().unwrap().is_empty());
        }
        // A tool without path arguments never spells the workspace as text.
        let policy = RecordingPolicy::allowing();
        let arguments = serde_json::json!({"url":"https://initial.test"});
        let result = executor(workspace, policy.clone())
            .run_host(&runtime.agent, "network_test", arguments.clone())
            .await
            .unwrap();
        assert_eq!(result.output.value, arguments);
        assert_eq!(policy.requests.lock().unwrap().len(), 1);
    }

    /// Schema-valid arguments that the typed handler rejects still create a job
    /// and request approval; the job then fails with the admission error.
    #[tokio::test]
    async fn typed_admission_failures_fail_their_approved_job() {
        #[derive(Deserialize, JsonSchema)]
        struct Count {
            #[serde(rename = "count")]
            _count: u8,
        }
        let runtime = crate::tests::TestRuntime::new().await;
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Count, (), _, _>(
                "count",
                "typed count",
                ToolOptions::new(vec![Capability::Read]),
                |_, _| async { panic!("inadmissible input must not reach the handler") },
            )
            .unwrap();
        let policy = RecordingPolicy::allowing();
        let executor = runtime.executor_with_policy(builder, policy.clone());
        let expected = serde_json::from_value::<Count>(serde_json::json!({"count": 300}))
            .err()
            .unwrap()
            .to_string();
        // The permissive JSON schema accepts this value; `u8` does not.
        let arguments = serde_json::json!({"count": 300});
        let plan = plan(&executor, &runtime.agent, "count", arguments.clone()).await;
        assert!(matches!(plan.dispatch, InvocationDispatch::Local(Err(_))));
        let error = executor
            .run_host(&runtime.agent, "count", arguments)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ExecutionError::Failed { message, .. } if message.contains(&expected)),
            "{error:?}"
        );
        assert_eq!(policy.requests.lock().unwrap().len(), 1);
        let jobs = runtime.jobs.list(&runtime.agent).await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, JobState::Failed);
    }
}
