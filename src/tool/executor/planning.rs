//! Validate and authorize arguments, then select where an invocation runs.

use super::*;
use crate::tool::{
    invocation::{
        PathOutcome, PathPreflight, path_text, preflight_path_arguments, scope_capabilities,
    },
    registry::split_envelope,
};

impl ToolExecutor {
    async fn resolve_workspace_invocation(
        &self,
        tool: &crate::tool::RegisteredTool,
        explicit: Option<TargetRef>,
    ) -> Result<SelectedLocation, ExecutionError> {
        if tool.placement() == ToolPlacement::Host {
            return Ok(SelectedLocation {
                location: self.shared.root_location.clone(),
                route: None,
            });
        }
        if explicit.is_some() && !self.capabilities.contains(Capability::Targets) {
            return Err(ToolError::invalid_arguments(
                "target selection requires the targets capability",
            )
            .into());
        }
        let selected = explicit.as_ref().unwrap_or(&self.caller_location.target);
        let TargetRef::Named(selected) = selected else {
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
        };
        let router = self.shared.router.as_ref().ok_or_else(|| {
            ToolError::invalid_arguments("remote targets are unavailable in this tool runtime")
        })?;
        let route = router
            .resolve(selected, &self.capabilities)
            .await
            .map_err(|error| ToolError::from(error.into_admission_error()))?;
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

    fn prepare_invocation(
        &self,
        kind: InvocationKind,
        agent: &AgentId,
        name: &str,
        mut arguments: Value,
    ) -> Result<PreparedInvocation, ExecutionError> {
        let tool = self
            .shared
            .registry
            .get(name)
            .ok_or_else(|| ExecutionError::UnknownTool(name.to_owned()))?;
        let spec = tool
            .spec(&self.capabilities, agent)
            .ok_or_else(|| ToolError::invalid_arguments(format!("tool `{name}` is unavailable")))?;
        // Only model output is repaired; host and script callers must match exactly.
        if matches!(kind, InvocationKind::Model) {
            crate::tool::coerce::coerce_arguments(&spec.input_schema, &mut arguments);
        }
        spec.validate_arguments(&arguments)?;
        validate_invocation(&spec, kind)?;
        let original_arguments = arguments.clone();
        let (handler_arguments, envelope) = split_envelope(&spec, &tool, arguments)?;
        Ok(PreparedInvocation {
            tool,
            original_arguments,
            handler_arguments,
            envelope,
        })
    }

    pub(super) async fn plan_registered(
        &self,
        kind: InvocationKind,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> Result<InvocationPlan, ExecutionError> {
        let PreparedInvocation {
            tool,
            original_arguments,
            handler_arguments: mut arguments,
            envelope: ExecutionEnvelope { launch, target },
        } = self.prepare_invocation(kind, &agent, name, arguments)?;
        let selected = self
            .resolve_workspace_invocation(&tool, target)
            .await
            .map_err(|error| {
                error.or(
                    PartialContext::new(Operation::Lookup, Subject::argument(["target"]))
                        .at(FailureSite::Host),
                    &self.capabilities,
                )
            })?;
        // Validation failures belong to the selected location, not the caller's.
        let invalid = |error: ExecutionError| {
            error.or(
                PartialContext::new(Operation::Validate, Subject::Tool(name.to_owned())).at(
                    FailureSite::bound(&selected.location, tool.placement() == ToolPlacement::Host),
                ),
                &self.capabilities,
            )
        };
        tool.validate_arguments(&arguments)
            .map_err(|error| invalid(error.into()))?;
        let PathPreflight {
            permissions: path_permissions,
            outcome,
            paths: path_facts,
        } = if selected.route.is_none() {
            preflight_path_arguments(
                &tool,
                &selected.location.target,
                &selected.location.workspace,
                &self.shared.root_location.workspace,
                &mut arguments,
            )
            .await
            .map_err(|error| invalid(error.into()))?
        } else {
            PathPreflight {
                permissions: Vec::new(),
                outcome: PathOutcome::Ready,
                paths: Vec::new(),
            }
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
        if matches!(outcome, PathOutcome::Ready) {
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
            permissions.extend(route.route.permissions());
            serde_json::json!({
                "tool": original_arguments,
                "route": route.route.authorization_arguments(),
            })
        } else {
            original_arguments.clone()
        };
        // Schema-valid input that the typed handler rejects still owns a job,
        // approval, and failure. A remote dispatch admits only on its destination.
        let dispatch = match (outcome, selected.route) {
            (PathOutcome::ReadError { value, diagnostic }, _) => InvocationDispatch::ReadError(
                Box::new(ToolOutput::new(value).with_diagnostic(*diagnostic)),
            ),
            (PathOutcome::Ready, Some(remote)) => InvocationDispatch::Remote { remote, arguments },
            (PathOutcome::Ready, None) => {
                InvocationDispatch::Local(tool.admit(arguments, &original_arguments))
            }
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
            path_facts,
            parent,
            launch,
            dispatch,
        })
    }
}

struct SelectedLocation {
    location: ExecutionLocation,
    route: Option<PlannedRemote>,
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
                            return Err(crate::tool::AdmissionError::invalid_arguments(
                                "invalid test URL",
                            ));
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
            .plan_registered(InvocationKind::Host, agent.clone(), name, arguments, None)
            .await
            .unwrap()
    }

    fn network_use(origin: &str) -> PermissionUse {
        PermissionUse::new(
            Capability::Network,
            ResourceId::network(&crate::target::TargetRef::Root, origin),
        )
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
                assert_eq!(
                    error.diagnostic().cause,
                    Cause::Message("tool `network_test` is unavailable in this context".into())
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
            error.diagnostic().cause,
            Cause::InvalidArguments(_)
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
            "url":"https://initial.test", "body":{"kind":"file","path":"upload"}, "save_to":"download"
        });
        let plan = plan(&executor, &runtime.agent, "network_test", arguments.clone()).await;
        let (upload, download) = (
            runtime.root.path().join("upload"),
            runtime.root.path().join("download"),
        );
        for (capability, path) in [(Capability::Read, &upload), (Capability::Write, &download)] {
            let resource = ResourceId::path(&crate::target::TargetRef::Root, path);
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
            echoed.pointer("/body/path"),
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
        // Network-only calls plan without Read or Write, and no approval is cached.
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
        let arguments =
            serde_json::json!({"url":"https://initial.test", "redirect":true,"insecure":true});
        let error = executor
            .run_host(&runtime.agent, "network_test", arguments)
            .await
            .unwrap_err();
        assert!(
            matches!(error.diagnostic().cause, Cause::Denied(_)),
            "{error:?}"
        );
        let all = policy.requests.lock().unwrap();
        assert_eq!(all.len(), 4);
        for request in &all[..3] {
            assert_eq!(request.permissions, [network_use("https://initial.test")]);
            assert!(request.permissions[0].proposed_grant.is_none());
        }
        let requests = &all[2..];
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
        builder
            .register_local(crate::tool::builtins::register_local_tools)
            .unwrap();
        let executor = runtime.executor_with_policy(builder, Arc::new(DenyReads));
        for path in ["missing/nested/file", "../outside/missing"] {
            let arguments = serde_json::json!({"path":path});
            let error = executor
                .execute_model(runtime.agent.clone(), "read", arguments, None)
                .await
                .unwrap_err();
            assert!(matches!(error.diagnostic().cause, Cause::Denied(_)));
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
                    Err(ToolError::io(std::io::ErrorKind::PermissionDenied.into()))
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
        assert!(matches!(
            error.diagnostic().cause,
            Cause::Io {
                kind: crate::tool::diagnostic::IoKind::NotFound,
                ..
            }
        ));
        assert!(matches!(
            read(".").await.unwrap_err().diagnostic().cause,
            Cause::Io {
                kind: crate::tool::diagnostic::IoKind::PermissionDenied,
                ..
            }
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
                matches!(result.unwrap_err().diagnostic().cause, Cause::InvalidArguments(message) if message.contains("losslessly"))
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
        let expected = crate::tool::diagnostic::deserialize_arguments::<Count>(
            serde_json::json!({"count": 300}),
        )
        .err()
        .unwrap();
        let Cause::InvalidArguments(expected) = expected.diagnostic().cause else {
            panic!("typed admission classification must be retained");
        };
        // The permissive JSON schema accepts this value; `u8` does not.
        let arguments = serde_json::json!({"count": 300});
        let plan = plan(&executor, &runtime.agent, "count", arguments.clone()).await;
        assert!(matches!(plan.dispatch, InvocationDispatch::Local(Err(_))));
        let error = executor
            .run_host(&runtime.agent, "count", arguments)
            .await
            .unwrap_err();
        assert!(
            matches!(error.diagnostic().cause, Cause::InvalidArguments(message) if message == expected),
            "{error:?}"
        );
        assert_eq!(policy.requests.lock().unwrap().len(), 1);
        let jobs = runtime.jobs.list(&runtime.agent).await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, JobState::Failed);
    }
}
