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
                    explicit,
                    None,
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
        let definition = route
            .definitions
            .last()
            .expect("validated target routes are nonempty");
        Ok(SelectedLocation {
            location: ExecutionLocation::select(
                &self.caller_location,
                &self.shared.root_location.workspace,
                explicit,
                Some(&definition.workspace),
            ),
            route: Some(route),
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
                || !matches!(permission.resource.namespace.as_str(), "path" | "network")
        }));
        let authorization_arguments = if let Some(route) = &selected.route {
            permissions.push(route.permission());
            serde_json::json!({
                "tool": original_arguments,
                "route": route.authorization_arguments(),
            })
        } else {
            original_arguments.clone()
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
            handler_arguments: arguments,
            caller_location: self.caller_location.clone(),
            execution_location: selected.location,
            permissions,
            parent,
            authorization_scope,
            background,
            job_name,
            dispatch: read_error.map_or_else(
                || {
                    selected
                        .route
                        .map_or(InvocationDispatch::Local, InvocationDispatch::Remote)
                },
                InvocationDispatch::ReadError,
            ),
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

#[derive(Debug)]
struct SelectedLocation {
    location: ExecutionLocation,
    route: Option<ResolvedRoute>,
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
        let value = match &spec.pointer {
            Some(pointer) => arguments.pointer(pointer),
            None => arguments.get(&spec.name),
        };
        let input = match value {
            Some(Value::String(path)) => path.clone(),
            Some(_) => {
                return Err(ToolError::InvalidArguments(format!(
                    "{} must be a string",
                    spec.name
                )));
            }
            None => match &spec.default {
                Some(default) => default.clone(),
                None if spec.pointer.is_some() => {
                    return Err(ToolError::InvalidArguments(format!(
                        "path pointer `{}` does not identify an argument",
                        spec.name
                    )));
                }
                None => continue,
            },
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
                let resource = ResourceId::path(target, &path);
                permissions.push(
                    PermissionUse::new(capability, resource.clone())
                        .with_grant(ApprovalGrant::exact(capability, resource)),
                );
                read_error = Some(output);
                continue;
            }
        };
        let value = Value::String(resolved.path.to_string_lossy().into_owned());
        if let Some(pointer) = &spec.pointer {
            *arguments.pointer_mut(pointer).ok_or_else(|| {
                ToolError::InvalidArguments(format!(
                    "path pointer `{pointer}` does not identify an argument"
                ))
            })? = value;
        } else {
            arguments
                .as_object_mut()
                .expect("validated object")
                .insert(spec.name.clone(), value);
        }
        if spec.pointer.is_some() || !resolved.path.starts_with(authorization_root) {
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
        tool::{
            ToolOptions, ToolRegistryBuilder,
            policy::{AuthorizationRequest, PolicyDecision, PolicyFuture},
        },
    };

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

    #[derive(Default)]
    struct NetworkPolicy {
        requests: std::sync::Mutex<Vec<AuthorizationRequest>>,
        deny_redirect: bool,
    }

    impl Policy for NetworkPolicy {
        fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
            let denied = self.deny_redirect
                && request.permissions.iter().any(|permission| {
                    permission
                        .resource
                        .segments
                        .last()
                        .is_some_and(|origin| origin == "https://redirect.test")
                });
            self.requests.lock().unwrap().push(request);
            Box::pin(async move {
                if denied {
                    PolicyDecision::Deny {
                        reason: "redirect denied".to_owned(),
                    }
                } else {
                    PolicyDecision::allow()
                }
            })
        }
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

    fn network_executor(
        runtime: &crate::tests::TestRuntime,
        policy: &Arc<NetworkPolicy>,
    ) -> ToolExecutor {
        ToolExecutor::new(
            network_builder().build(),
            policy.clone(),
            runtime.jobs.clone(),
            runtime.root.path().to_path_buf(),
        )
    }

    #[tokio::test]
    async fn network_only_invocations_have_exact_origin_permissions_each_time() {
        let runtime = crate::tests::TestRuntime::new().await;
        let policy = Arc::new(NetworkPolicy::default());
        let mut capabilities = CapabilitySet::default();
        capabilities.remove(Capability::Read);
        capabilities.remove(Capability::Write);
        let executor = network_executor(&runtime, &policy).with_capabilities(capabilities);
        assert!(executor.surface().get("network_test").is_some());
        for _ in 0..2 {
            executor
                .execute(
                    runtime.agent.clone(),
                    "network_test",
                    serde_json::json!({"url":"https://initial.test"}),
                    None,
                )
                .await
                .unwrap();
        }
        let requests = policy.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_eq!(
                request.permissions,
                vec![PermissionUse::new(
                    Capability::Network,
                    ResourceId::network("root", "https://initial.test")
                )]
            );
            assert!(request.permissions[0].proposed_grant.is_none());
        }
    }

    #[tokio::test]
    async fn network_paths_inside_and_outside_root_require_dynamic_capabilities() {
        let runtime = crate::tests::TestRuntime::new().await;
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(runtime.root.path().join("upload"), "data").unwrap();
        std::fs::write(outside.path().join("upload"), "data").unwrap();
        for directory in [std::path::Path::new(""), outside.path()] {
            for (removed, extra) in [
                (
                    Capability::Read,
                    serde_json::json!({"body":{"kind":"file","path":directory.join("upload")}}),
                ),
                (
                    Capability::Read,
                    serde_json::json!({"body":{"kind":"multipart","parts":[{"name":"upload","path":directory.join("upload")}]}}),
                ),
                (
                    Capability::Write,
                    serde_json::json!({"save_to":directory.join("download")}),
                ),
            ] {
                let policy = Arc::new(NetworkPolicy::default());
                let mut capabilities = CapabilitySet::default();
                capabilities.remove(removed);
                let executor = network_executor(&runtime, &policy).with_capabilities(capabilities);
                let mut arguments = serde_json::json!({"url":"https://initial.test"});
                arguments
                    .as_object_mut()
                    .unwrap()
                    .extend(extra.as_object().unwrap().clone());
                let error = executor
                    .execute(runtime.agent.clone(), "network_test", arguments, None)
                    .await
                    .unwrap_err();
                assert!(
                    matches!(error, ExecutionError::UnavailableTool(_)),
                    "{error:?}"
                );
                assert!(policy.requests.lock().unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn network_nested_paths_are_rewritten_and_authorized_including_in_root() {
        let runtime = crate::tests::TestRuntime::new().await;
        std::fs::write(runtime.root.path().join("upload"), "data").unwrap();
        let policy = Arc::new(NetworkPolicy::default());
        let executor = network_executor(&runtime, &policy);
        let plan = executor.plan_registered(InvocationKind::Host, runtime.agent.clone(), "network_test", serde_json::json!({
            "url":"https://initial.test", "body":{"kind":"multipart","parts":[{"name":"upload","path":"upload"},{"name":"message","text":"not a file"}]}, "save_to":"download"
        }), None, None).await.unwrap();
        let upload = runtime.root.path().join("upload");
        let download = runtime.root.path().join("download");
        assert_eq!(
            plan.handler_arguments
                .pointer("/body/parts/0/path")
                .unwrap(),
            &serde_json::json!(upload)
        );
        assert_eq!(
            plan.handler_arguments["save_to"],
            serde_json::json!(download)
        );
        assert!(
            plan.permissions
                .iter()
                .any(|p| p.capability == Capability::Read
                    && p.resource == ResourceId::path("root", &upload))
        );
        assert!(
            plan.permissions
                .iter()
                .any(|p| p.capability == Capability::Write
                    && p.resource == ResourceId::path("root", &download))
        );
        assert_eq!(plan.permissions.len(), 3);
        assert!(policy.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn remote_network_permissions_are_deferred_but_other_dynamic_permissions_are_not() {
        let runtime = crate::tests::TestRuntime::new().await;
        let policy = Arc::new(NetworkPolicy::default());
        let mut builder = network_builder();
        builder
            .register_dynamic(
                "dynamic_exec",
                "test generic dynamic permissions",
                serde_json::json!({"type":"object","properties":{}}),
                ToolOptions::new(vec![Capability::Exec])
                    .placement(ToolPlacement::TargetedWorkspace)
                    .argument_permissions(|_, _| {
                        Ok(vec![PermissionUse::new(
                            Capability::Exec,
                            ResourceId::session("dynamic-command"),
                        )])
                    }),
                |_, _| async { Ok(ToolOutput::new(Value::Null)) },
            )
            .unwrap();
        let targets =
            TargetRegistry::from_definitions([TargetDefinition::test("build", "/build", None)])
                .unwrap();
        let mut capabilities = CapabilitySet::default();
        capabilities.insert(Capability::Targets);
        let executor = ToolExecutor::new(
            builder.build(),
            policy.clone(),
            runtime.jobs.clone(),
            runtime.root.path().to_path_buf(),
        )
        .with_target_router(router(targets, policy.clone()))
        .with_capabilities(capabilities);
        let network = executor
            .plan_registered(
                InvocationKind::Host,
                runtime.agent.clone(),
                "network_test",
                serde_json::json!({"url":"https://initial.test", "target":"build"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(matches!(network.dispatch, InvocationDispatch::Remote(_)));
        assert!(
            !network
                .permissions
                .iter()
                .any(|permission| permission.capability == Capability::Network)
        );
        let command = executor
            .plan_registered(
                InvocationKind::Host,
                runtime.agent.clone(),
                "dynamic_exec",
                serde_json::json!({"target":"build"}),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(command.permissions.contains(&PermissionUse::new(
            Capability::Exec,
            ResourceId::session("dynamic-command")
        )));
        assert!(policy.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_network_arguments_fail_before_approval_or_path_resolution() {
        let runtime = crate::tests::TestRuntime::new().await;
        let policy = Arc::new(NetworkPolicy::default());
        let executor = network_executor(&runtime, &policy);
        let error = executor
            .execute(
                runtime.agent.clone(),
                "network_test",
                serde_json::json!({"url":"invalid", "body":{"kind":"file", "path":"missing"}}),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ExecutionError::Tool(ToolError::InvalidArguments(_))
        ));
        assert!(policy.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn runtime_network_authorization_preserves_invocation_and_denies_redirect() {
        let runtime = crate::tests::TestRuntime::new().await;
        let policy = Arc::new(NetworkPolicy {
            deny_redirect: true,
            ..NetworkPolicy::default()
        });
        let executor = network_executor(&runtime, &policy);
        let error = executor
            .execute(
                runtime.agent.clone(),
                "network_test",
                serde_json::json!({"url":"https://initial.test", "redirect":true,"insecure":true}),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ExecutionError::Denied(_)), "{error:?}");
        let requests = policy.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].job, requests[1].job);
        assert_eq!(requests[0].agent, requests[1].agent);
        assert_eq!(requests[1].arguments["insecure"], true);
        assert_eq!(
            requests[1].arguments["network_origin"],
            "https://redirect.test"
        );
        assert_eq!(
            requests[1].permissions,
            vec![PermissionUse::new(
                Capability::Network,
                ResourceId::network("root", "https://redirect.test")
            )]
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
                        .any(|permission| permission.capability == Capability::Read)
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
        let executor = ToolExecutor::new(
            builder.build(),
            Arc::new(DenyReads),
            runtime.jobs.clone(),
            runtime.root.path().to_owned(),
        );
        for path in ["missing/nested/file", "../outside/missing"] {
            let error = executor
                .execute_model(
                    runtime.agent.clone(),
                    "read",
                    serde_json::json!({"path":path}),
                    None,
                )
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
                    Err(ToolError::Io(std::io::Error::from(
                        std::io::ErrorKind::PermissionDenied,
                    )))
                },
            )
            .unwrap();
        let executor = runtime.executor(builder);
        let error = executor
            .execute(
                runtime.agent.clone(),
                "read",
                serde_json::json!({"path":"missing/nested"}),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, ExecutionError::Tool(ToolError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound)
        );
        let error = executor
            .execute(
                runtime.agent.clone(),
                "read",
                serde_json::json!({"path":"."}),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ExecutionError::Failed { .. }));
    }
}
