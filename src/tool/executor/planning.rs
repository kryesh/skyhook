//! Validate and authorize arguments, then select where an invocation runs.

use super::*;
use crate::{
    target::TargetPath,
    tool::{
        JobLocation, PathKind,
        authorization::{AuthorizationArguments, SourceArguments},
        diagnostic::{Cause, PathRole},
        invocation::{
            PathOutcome, PathPreflight, assemble_permissions, preflight_path_arguments,
            scope_capabilities,
        },
        path::resolve_for_authorization,
        policy::PathText,
        registry::{AgentLevel, SOURCE_ARGUMENT, TARGET, split_envelope},
    },
};

impl ToolExecutor {
    async fn resolve_workspace_invocation(
        &self,
        tool: &crate::tool::RegisteredTool,
        explicit: Option<TargetRef>,
    ) -> Result<SelectedLocation, ToolError> {
        if tool.placement() == ToolPlacement::Host {
            return Ok(SelectedLocation {
                location: self.shared.root_location.clone(),
                route: None,
            });
        }
        self.select_location(explicit).await
    }

    async fn select_location(
        &self,
        explicit: Option<TargetRef>,
    ) -> Result<SelectedLocation, ToolError> {
        let router = self.shared.router.as_ref();
        let selected = crate::target::select_location(
            &self.caller_location,
            &self.shared.root_location.workspace,
            explicit.as_ref(),
            &self.capabilities,
            router,
        )
        .await?;
        Ok(SelectedLocation {
            location: selected.location,
            // A route is only resolved through a router.
            route: selected
                .route
                .zip(router.cloned())
                .map(|(route, router)| PlannedRemote { route, router }),
        })
    }

    /// Select where a host tool's input places its job, before the job exists.
    async fn locate_job(&self, requested: JobLocation) -> Result<ExecutionLocation, ToolError> {
        let mut location = (self.select_location(requested.target).await)
            .map_err(|error| {
                error
                    .operation(Operation::Lookup, Subject::argument([TARGET]))
                    .at(FailureSite::Host)
                    .effects(Effects::NotStarted)
            })?
            .location;
        if let Some(workspace) = requested.workspace {
            if workspace.as_os_str().is_empty() {
                return Err(ToolError::invalid_arguments("workspace cannot be empty")
                    .operation(Operation::Validate, Subject::argument(["workspace"]))
                    .effects(Effects::NotStarted));
            }
            location.workspace = location.workspace.join(workspace);
        }
        Ok(location)
    }

    fn prepare_invocation(
        &self,
        kind: InvocationKind,
        agent: &AgentId,
        name: &str,
        mut arguments: Value,
    ) -> Result<PreparedInvocation, ToolError> {
        let tool = (self.shared.registry.get(name)).ok_or_else(|| ToolError::unknown_tool(name))?;
        let spec = tool
            .spec(&self.capabilities, AgentLevel::of(agent))
            .ok_or_else(|| ToolError::unavailable(name))?;
        // Only model output is repaired; host and script callers must match exactly.
        if matches!(kind, InvocationKind::Model) {
            crate::tool::coerce::coerce_arguments(&spec.input_schema, &mut arguments);
        }
        let mut handler_arguments = spec.validate_arguments(arguments)?;
        validate_invocation(&spec, kind)?;
        let original_arguments = handler_arguments.clone();
        let envelope = split_envelope(&spec, &tool, &mut handler_arguments)?;
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
    ) -> Result<InvocationPlan, ToolError> {
        let mut plan = self
            .plan_invocation(kind, agent, name, arguments, parent)
            .await?;
        let source = (plan.tool.reads_source())
            .then(|| plan.original_arguments.get(SOURCE_ARGUMENT))
            .flatten()
            .filter(|source| !source.is_null())
            .cloned();
        if let Some(source) = source {
            self.plan_source(source, &mut plan).await?;
        }
        Ok(plan)
    }

    async fn plan_invocation(
        &self,
        kind: InvocationKind,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> Result<InvocationPlan, ToolError> {
        let PreparedInvocation {
            tool,
            original_arguments,
            handler_arguments: arguments,
            envelope: ExecutionEnvelope { launch, target },
        } = self.prepare_invocation(kind, &agent, name, arguments)?;
        let selected = self
            .resolve_workspace_invocation(&tool, target)
            .await
            .map_err(|error| {
                error.or(
                    PartialContext::new(Operation::Lookup, Subject::argument([TARGET]))
                        .at(FailureSite::Host),
                )
            })?;
        // Validation failures belong to the selected location, not the caller's.
        let invalid = |error: crate::tool::AdmissionError| {
            ToolError::from(error).or(PartialContext::new(
                Operation::Validate,
                Subject::Tool(name.to_owned()),
            )
            .at(FailureSite::bound(&selected.location, tool.placement())))
        };
        let mut admitted = tool.admit(&arguments).map_err(invalid)?;
        let derived = admitted.permissions(&selected.location).map_err(invalid)?;
        let job_location = match admitted.job_location() {
            Some(requested) => self.locate_job(requested).await?,
            None => selected.location.clone(),
        };
        // A remote destination resolves its own paths.
        let path = if selected.route.is_none() {
            let root = &self.shared.root_location.workspace;
            preflight_path_arguments(&tool, admitted.paths(), &selected.location, root)
                .await
                .map_err(invalid)?
        } else {
            // Remote location frames spell the workspace as text.
            PathText::new(&selected.location.workspace)?;
            PathPreflight::deferred()
        };
        let mut permissions = assemble_permissions(
            &tool,
            &selected.location,
            &self.capabilities,
            derived,
            &path,
            selected.route.is_some(),
        )?;
        let PathPreflight {
            outcome,
            paths: path_facts,
            ..
        } = path;
        if let Some(route) = &selected.route {
            permissions.extend(route.route.permissions());
        }
        let authorization_arguments = AuthorizationArguments {
            tool: original_arguments.clone(),
            route: (selected.route.as_ref()).map(|route| route.route.authorization_arguments()),
            source: None,
        };
        let invocation = tool.invoke(admitted);
        let result_policy = invocation.result_policy();
        // A remote destination admits the call again where it runs.
        let dispatch = match (outcome, selected.route) {
            (PathOutcome::ReadError { value, diagnostic }, _) => InvocationDispatch::ReadError(
                Box::new(ToolOutput::new(value).with_diagnostic(*diagnostic)),
            ),
            (PathOutcome::Ready, Some(remote)) => InvocationDispatch::Remote { remote, arguments },
            (PathOutcome::Ready, None) => InvocationDispatch::Local(invocation),
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
            job_location,
            permissions,
            path_facts,
            parent,
            launch,
            result_policy,
            dispatch,
            source: None,
        })
    }

    /// Plan a source argument's read where the file lives, authorized with the
    /// call: local paths are resolved now, remote ones by their worker.
    async fn plan_source(&self, source: Value, plan: &mut InvocationPlan) -> Result<(), ToolError> {
        if !self.capabilities.contains(Capability::Read) {
            return Err(ToolError::unavailable(plan.tool.name()));
        }
        let source: TargetPath = serde_json::from_value(source).map_err(|error| {
            ToolError::invalid_arguments(format!("invalid source: {error}"))
                .operation(Operation::Validate, Subject::argument([SOURCE_ARGUMENT]))
        })?;
        let SelectedLocation { location, route } =
            self.select_location(source.target).await.map_err(|error| {
                error.or(PartialContext::new(
                    Operation::Lookup,
                    Subject::argument([SOURCE_ARGUMENT, TARGET]),
                )
                .at(FailureSite::Host))
            })?;
        let mut permissions = scope_capabilities(vec![Capability::Read], &location, None)?;
        let mut authorization_arguments = SourceArguments {
            path: source.path.clone(),
            target: location.target.clone(),
            route: None,
        };
        let source = match route {
            None => {
                let resolved = resolve_for_authorization(
                    &location.workspace,
                    &source.path,
                    PathKind::Existing,
                )
                .await
                .map_err(|error| {
                    error.or(PartialContext::default()
                        .at(FailureSite::Execution(location.clone()))
                        .path(PathRole::Requested, &source.path))
                })?;
                let root = &self.shared.root_location.workspace;
                permissions.extend(resolved.permission_outside(
                    root,
                    Capability::Read,
                    &location.target,
                ));
                SourcePlan::Local {
                    path: resolved.path.into(),
                    location,
                }
            }
            Some(remote) => {
                permissions.extend(remote.route.permissions());
                authorization_arguments.route = Some(remote.route.authorization_arguments());
                SourcePlan::Remote {
                    remote,
                    workspace: location.workspace,
                    path: source.path,
                }
            }
        };
        plan.permissions.extend(permissions);
        plan.authorization_arguments.source = Some(authorization_arguments);
        plan.source = Some(source);
        Ok(())
    }
}

struct SelectedLocation {
    location: ExecutionLocation,
    route: Option<PlannedRemote>,
}

fn validate_invocation(
    tool: &crate::tool::ToolSpec,
    kind: InvocationKind,
) -> Result<(), ToolError> {
    let name = || tool.name.clone();
    match kind {
        InvocationKind::Host => Ok(()),
        InvocationKind::Model if tool.exposure == ToolExposure::ScriptOnly => {
            Err(ToolError::cause(Cause::ModelHidden { tool: name() }))
        }
        InvocationKind::Script if tool.script_binding == ScriptBinding::Unavailable => {
            Err(ToolError::cause(Cause::ScriptUnavailable { tool: name() }))
        }
        InvocationKind::Model | InvocationKind::Script => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::*;
    use crate::{
        target::{TargetDefinition, TargetRegistry},
        tests::RecordingPolicy,
        tool::{
            PathArgument, ToolOptions, ToolRegistryBuilder,
            policy::{
                AuthorizationRequest, PathAccess, PathText, PolicyDecision, PolicyFuture,
                ResourceId,
            },
        },
    };

    /// Terse host/model invocations without a parent job for tests across the crate.
    impl ToolExecutor {
        pub(crate) async fn run_host(
            &self,
            agent: &AgentId,
            name: &str,
            arguments: Value,
        ) -> Result<ExecutionResult, ToolError> {
            self.execute(agent.clone(), name, arguments, None).await
        }

        pub(crate) async fn run_model(
            &self,
            agent: &AgentId,
            name: &str,
            arguments: Value,
        ) -> Result<ExecutionResult, ToolError> {
            self.execute_model(agent.clone(), name, arguments, None)
                .await
        }
    }

    #[derive(Deserialize, JsonSchema)]
    struct PathArgs {
        path: String,
    }

    fn network_builder() -> ToolRegistryBuilder {
        let mut builder = ToolRegistryBuilder::default();
        let register = |builder: &mut crate::tool::invocation::LocalCatalogBuilder| {
            builder.register_dynamic(
                "network_test",
                "Exercise invocation-derived authorization",
                serde_json::json!({"type":"object","properties":{
                    "url":{"type":"string"}, "body":{}, "save_to":{}, "redirect":{}
                }}),
                ToolOptions::new(vec![Capability::Network])
                    .placement(ToolPlacement::TargetedWorkspace)
                    .background()
                    .named()
                    .argument_permissions(|location, arguments: &Value| {
                        if arguments["url"] != "https://initial.test" {
                            return Err(crate::tool::AdmissionError::invalid_arguments(
                                "invalid test URL",
                            ));
                        }
                        Ok(vec![PermissionUse::new(
                            Capability::Network,
                            ResourceId::network(&location.target, "https://initial.test"),
                        )])
                    })
                    .argument_paths(|arguments: &mut Value| {
                        let Value::Object(arguments) = arguments else {
                            return Vec::new();
                        };
                        let mut paths = Vec::new();
                        for (name, value) in arguments {
                            let (path, access, kind) = match (name.as_str(), value) {
                                ("body", body) if body["kind"] == "file" => {
                                    let Some(Value::String(path)) = body.get_mut("path") else {
                                        continue;
                                    };
                                    (path, PathAccess::Read, PathKind::Existing)
                                }
                                ("save_to", Value::String(path)) => {
                                    (path, PathAccess::Write, PathKind::Writable)
                                }
                                _ => continue,
                            };
                            paths.push(PathArgument::new(path, access, kind).exact());
                        }
                        paths
                    }),
                |context: crate::tool::invocation::LocalContext, arguments| async move {
                    if arguments["redirect"] == true {
                        context.authorize_network("https://redirect.test").await?;
                    }
                    Ok(crate::tool::output::ProducedOutput::new(arguments))
                },
            )?;
            Ok(())
        };
        builder.register_local(register).unwrap();
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
                    Cause::Unavailable {
                        tool: "network_test".into()
                    }
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
            let path = PathText::new(path).unwrap();
            let resource = ResourceId::path(&crate::target::TargetRef::Root, &path);
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
            .with_target_router(TargetRouter::test(targets, policy.clone()))
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
    async fn sources_plan_as_reads_on_their_own_target_under_the_same_approval() {
        let runtime = crate::tests::TestRuntime::new().await;
        let policy = RecordingPolicy::allowing();
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_local(crate::tool::builtins::register_local_tools)
            .unwrap();
        let targets =
            TargetRegistry::from_definitions([TargetDefinition::test("build", "/build", None)])
                .unwrap();
        let executor = runtime
            .executor_with_policy(builder, policy.clone())
            .with_target_router(TargetRouter::test(targets, policy.clone()));
        let mut capabilities = CapabilitySet::default();
        capabilities.insert(Capability::Targets);
        let targeted = executor.clone().with_capabilities(capabilities);
        let agent = &runtime.agent;

        let remote = serde_json::json!({"path":"copy.bin", "source":{"path":"/build/out.bin", "target":"build"}});
        let plan_remote = plan(&targeted, agent, "write", remote.clone()).await;
        assert!(matches!(plan_remote.dispatch, InvocationDispatch::Local(_)));
        assert!(matches!(
            plan_remote.source,
            Some(SourcePlan::Remote { .. })
        ));
        assert!(
            plan_remote
                .permissions
                .iter()
                .any(|p| p.capability == Capability::Targets)
        );
        assert_eq!(
            plan_remote.authorization_arguments.document(None)["source"]["path"],
            "/build/out.bin"
        );

        std::fs::write(runtime.root.path().join("local.bin"), b"bytes").unwrap();
        let local = serde_json::json!({"path":"copy.bin", "source":{"path":"local.bin"}});
        let plan_local = plan(&executor, agent, "write", local.clone()).await;
        assert!(matches!(plan_local.source, Some(SourcePlan::Local { .. })));
        // Reading the source needs Read even though the tool itself needs only Write.
        let mut capabilities = CapabilitySet::default();
        capabilities.remove(Capability::Read);
        let error = (executor.clone().with_capabilities(capabilities))
            .plan_registered(
                InvocationKind::Host,
                agent.clone(),
                "write",
                local.clone(),
                None,
            )
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.diagnostic().cause,
            Cause::Unavailable {
                tool: "write".into()
            }
        );

        // Selecting a source target needs the targets capability, and a write
        // takes exactly one of content or source.
        for (executor, arguments) in [
            (&executor, remote),
            (&targeted, serde_json::json!({"path":"copy.bin"})),
            (
                &targeted,
                serde_json::json!({"path":"copy.bin", "content":"x", "source":{"path":"local.bin"}}),
            ),
        ] {
            let planned = executor
                .plan_registered(
                    InvocationKind::Host,
                    agent.clone(),
                    "write",
                    arguments.clone(),
                    None,
                )
                .await;
            assert!(planned.is_err(), "{arguments}");
        }
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
        let arguments = serde_json::json!({
            "url":"https://initial.test", "redirect":true, "network_origin":"argument"
        });
        let error = executor
            .run_host(&runtime.agent, "network_test", arguments.clone())
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
            assert!(request.permissions[0].proposed.is_none());
        }
        let requests = &all[2..];
        assert_eq!(
            (&requests[0].job, &requests[0].agent),
            (&requests[1].job, &requests[1].agent)
        );
        // The redirect is asked under the admitted arguments, beside the tool's own.
        assert_eq!(
            requests[0].arguments,
            serde_json::json!({"tool": arguments})
        );
        assert_eq!(
            requests[1].arguments,
            serde_json::json!({"tool": arguments, "network_origin": "https://redirect.test"})
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
                ToolOptions::new(vec![Capability::Read]).argument_paths(|args: &mut PathArgs| {
                    let (access, kind) = (PathAccess::Read, PathKind::Existing);
                    vec![PathArgument::new(&mut args.path, access, kind)]
                }),
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
    async fn non_unicode_permission_paths_fail_but_path_free_tools_run_in_non_unicode_workspaces() {
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
            let mut builder = network_builder();
            builder
                .register_dynamic(
                    "scoped",
                    "workspace-scoped writes",
                    serde_json::json!({"type":"object","properties":{}}),
                    ToolOptions::new(vec![Capability::Write]),
                    |_, _| async { Ok(ToolOutput::new(Value::Null)) },
                )
                .unwrap();
            ToolExecutor::new(builder.build(), policy, runtime.jobs.clone(), workspace)
        };
        // Canonical argument paths and workspace scopes are spelled in the string
        // permission/resource boundary, so they fail before approval wherever
        // their bytes come from.
        for (workspace, tool, arguments) in [
            (
                runtime.root.path().to_owned(),
                "network_test",
                serde_json::json!({
                    "url": "https://initial.test", "body": {"kind": "file", "path": "unicode-alias"}
                }),
            ),
            (
                workspace.clone(),
                "network_test",
                serde_json::json!({
                    "url": "https://initial.test", "body": {"kind": "file", "path": "upload"}
                }),
            ),
            (workspace.clone(), "scoped", serde_json::json!({})),
        ] {
            let policy = RecordingPolicy::allowing();
            let result = executor(workspace, policy.clone())
                .run_host(&runtime.agent, tool, arguments)
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

    /// Schema-valid arguments that the typed handler rejects fail before a job
    /// or an approval exists.
    #[tokio::test]
    async fn typed_admission_failures_fail_before_approval() {
        #[derive(Deserialize, JsonSchema)]
        struct Count {
            #[serde(rename = "count")]
            _count: u8,
        }
        let runtime = crate::tests::TestRuntime::new().await;
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_unit::<Count, _, _>(
                "count",
                "typed count",
                ToolOptions::new(vec![Capability::Read]),
                |_, _| async { panic!("inadmissible input must not reach the handler") },
            )
            .unwrap();
        let policy = RecordingPolicy::allowing();
        let executor = runtime.executor_with_policy(builder, policy.clone());
        let expected = crate::tool::diagnostic::deserialize_arguments::<Count>(
            serde_json::json!({"count": 300}).as_object().unwrap(),
        )
        .err()
        .unwrap();
        let Cause::InvalidArguments(expected) = expected.diagnostic().cause else {
            panic!("typed admission classification must be retained");
        };
        // The permissive JSON schema accepts this value; `u8` does not.
        let arguments = serde_json::json!({"count": 300});
        let error = executor
            .run_host(&runtime.agent, "count", arguments)
            .await
            .unwrap_err();
        assert!(
            matches!(error.diagnostic().cause, Cause::InvalidArguments(message) if message == expected),
            "{error:?}"
        );
        assert!(policy.requests.lock().unwrap().is_empty());
        assert!(runtime.jobs.list(&runtime.agent).await.is_empty());
    }

    /// Where a hosted local tool runs, the host's admission decides whether its
    /// job has a result, even when a remote destination admits the call again.
    #[tokio::test]
    async fn hosted_unit_calls_keep_their_result_policy() {
        let runtime = crate::tests::TestRuntime::new().await;
        let policy = RecordingPolicy::allowing();
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_local(crate::tool::builtins::register_local_tools)
            .unwrap();
        let targets =
            TargetRegistry::from_definitions([TargetDefinition::test("build", "/build", None)])
                .unwrap();
        let local = runtime
            .executor_with_policy(builder, policy.clone())
            .with_target_router(TargetRouter::test(targets, policy));
        let remote = local.clone().with_location(ExecutionLocation {
            target: "build".parse().unwrap(),
            workspace: "/build".into(),
        });
        std::fs::write(runtime.root.path().join("file"), "a").unwrap();
        let arguments = serde_json::json!({"path":"file", "old":"a", "new":"b"});
        for (executor, remote) in [(local, false), (remote, true)] {
            let replace = plan(&executor, &runtime.agent, "replace", arguments.clone()).await;
            let dispatched = matches!(replace.dispatch, InvocationDispatch::Remote { .. });
            assert_eq!(dispatched, remote);
            assert_eq!(
                replace.result_policy,
                crate::tool::ToolResultPolicy::Nothing
            );
        }
    }
}
