//! Validate and authorize arguments, then select where an invocation runs.

use super::*;
use crate::{
    target::TargetPath,
    tool::{
        JobLocation, PathArgument, PathKind,
        authorization::{AuthorizationArguments, SourceArguments},
        diagnostic::Cause,
        invocation::{
            PathPreflight, assemble_permissions, preflight_path_arguments, scope_capabilities,
        },
        policy::{PathAccess, PathText},
        registry::{AgentLevel, SOURCE_ARGUMENT, TARGET, split_envelope},
    },
};

impl ToolExecutor {
    fn resolve_workspace_invocation(
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
        self.select_location(explicit)
    }

    fn select_location(&self, explicit: Option<TargetRef>) -> Result<SelectedLocation, ToolError> {
        let router = self.shared.router.as_ref();
        let selected = crate::target::select_location(
            &self.caller_location,
            &self.shared.root_location.workspace,
            explicit.as_ref(),
            &self.capabilities,
            router,
        )?;
        Ok(SelectedLocation {
            location: selected.location,
            // A route is only resolved through a router.
            route: selected
                .route
                .zip(router.cloned())
                .map(|(route, router)| PlannedRemote { route, router }),
        })
    }

    /// Select where a host tool's input places its job, before the job exists:
    /// as the job shows it, with its workspace as written, and where it runs.
    /// A workspace is a path argument there, resolved here for a local job and
    /// by a remote worker as it starts in it.
    async fn locate_job(
        &self,
        requested: JobLocation,
    ) -> Result<(ExecutionLocation, ExecutionLocation, PathPreflight), ToolError> {
        let selected = self.select_location(requested.target).map_err(|error| {
            error
                .operation(Operation::Lookup, Subject::argument([TARGET]))
                .at(FailureSite::Host)
                .effects(Effects::NotStarted)
        })?;
        let mut location = selected.location;
        let Some(mut workspace) = requested.workspace else {
            return Ok((location.clone(), location, PathPreflight::deferred()));
        };
        if workspace.is_empty() {
            return Err(ToolError::invalid_arguments("workspace cannot be empty")
                .operation(Operation::Validate, Subject::argument(["workspace"]))
                .effects(Effects::NotStarted));
        }
        if selected.route.is_some() {
            location.workspace = location.workspace.join(workspace);
            return Ok((location.clone(), location, PathPreflight::deferred()));
        }
        let spelled = std::path::absolute(location.workspace.join(&workspace));
        let spelled = spelled.map_err(|error| ToolError::io(error).effects(Effects::NotStarted))?;
        let argument = PathArgument::new(&mut workspace, PathAccess::Read, PathKind::Workspace);
        let root = &self.shared.root_location.workspace;
        let preflight = preflight_path_arguments(vec![argument], &location, root).await?;
        // As typed, `..` kept: resolving would disclose where a link leads
        // before approval, and collapsing would claim a location it never means.
        let written = ExecutionLocation {
            target: location.target.clone(),
            workspace: spelled.components().collect(),
        };
        location.workspace = workspace.into();
        Ok((written, location, preflight))
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
        mut arguments: Value,
        parent: Option<JobId>,
    ) -> Result<InvocationPlan, ToolError> {
        let tool = (self.shared.registry.get(name)).ok_or_else(|| ToolError::unknown_tool(name))?;
        let spec = tool
            .spec(&self.capabilities, AgentLevel::of(&agent))
            .ok_or_else(|| ToolError::unavailable(name))?;
        // Only model output is repaired; host and script callers must match exactly.
        if matches!(kind, InvocationKind::Model) {
            crate::tool::coerce::coerce_arguments(&spec.input_schema, &mut arguments);
        }
        let mut arguments = spec.validate_arguments(arguments)?;
        validate_invocation(&spec, kind)?;
        let original_arguments = arguments.clone();
        let ExecutionEnvelope { launch, target } = split_envelope(&spec, &tool, &mut arguments)?;
        let selected = self
            .resolve_workspace_invocation(&tool, target)
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
        let (job_location, job_runs_in, located) = match admitted.job_location() {
            Some(requested) => self.locate_job(requested).await?,
            None => {
                let location = selected.location.clone();
                (location.clone(), location, PathPreflight::deferred())
            }
        };
        // A remote destination resolves its own paths.
        let mut path = if selected.route.is_none() {
            let root = &self.shared.root_location.workspace;
            preflight_path_arguments(admitted.paths(), &selected.location, root)
                .await
                .map_err(invalid)?
        } else {
            // Remote location frames spell the workspace as text.
            PathText::new(&selected.location.workspace)?;
            PathPreflight::deferred()
        };
        path.permissions.extend(located.permissions);
        path.unresolved = path.unresolved.or(located.unresolved);
        let mut permissions = assemble_permissions(
            &tool,
            &selected.location,
            &self.capabilities,
            derived,
            &path,
            selected.route.is_some(),
        )?;
        let PathPreflight {
            paths: path_facts,
            unresolved,
            ..
        } = path;
        let unresolved = unresolved.map(invalid);
        if let Some(route) = &selected.route {
            permissions.extend(route.route.permissions());
        }
        let authorization_arguments = AuthorizationArguments {
            tool: original_arguments.clone(),
            route: (selected.route.as_ref()).map(|route| route.route.authorization_arguments()),
            source: None,
        };
        let invocation = admitted.invoke();
        let result_policy = invocation.result_policy();
        // A remote destination admits the call again where it runs.
        let dispatch = match selected.route {
            Some(remote) => InvocationDispatch::Remote { remote, arguments },
            None => InvocationDispatch::Local(invocation),
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
            job_runs_in,
            permissions,
            path_facts,
            parent,
            launch,
            result_policy,
            dispatch,
            source: None,
            unresolved,
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
            self.select_location(source.target).map_err(|error| {
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
                let mut path = source.path;
                let argument = PathArgument::new(&mut path, PathAccess::Read, PathKind::Existing);
                let root = &self.shared.root_location.workspace;
                let preflight = preflight_path_arguments(vec![argument], &location, root).await?;
                permissions.extend(preflight.permissions);
                if plan.unresolved.is_none() {
                    plan.unresolved = preflight.unresolved.map(ToolError::from);
                }
                SourcePlan::Local {
                    path: path.into(),
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
pub(super) mod tests {
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::*;
    use crate::{
        target::TargetDefinition,
        tests::RecordingPolicy,
        tool::{
            PathArgument, ToolOptions, ToolRegistryBuilder,
            executor::failure_response,
            policy::{
                Approval, AuthorizationRequest, PathAccess, PathText, PolicyDecision, ResourceId,
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
            self.execute_as(InvocationKind::Model, agent.clone(), name, arguments, None)
                .await
        }
    }

    pub(in crate::tool::executor) fn build() -> [TargetDefinition; 1] {
        [TargetDefinition::test("build", "/build", None)]
    }

    fn network_builder() -> ToolRegistryBuilder {
        let mut builder = ToolRegistryBuilder::default();
        let register = |builder: &mut crate::tool::invocation::LocalCatalogBuilder| {
            builder.register_product::<Arguments, Value, _, _>(
                "network_test",
                "Exercise invocation-derived authorization",
                ToolOptions::new(vec![Capability::Network])
                    .placement(ToolPlacement::TargetedWorkspace)
                    .background()
                    .named()
                    .argument_permissions(|location, arguments: &Arguments| {
                        if arguments.get("url") != Some(&"https://initial.test".into()) {
                            return Err(crate::tool::AdmissionError::invalid_arguments(
                                "invalid test URL",
                            ));
                        }
                        Ok(vec![PermissionUse::new(
                            Capability::Network,
                            ResourceId::network(&location.target, "https://initial.test"),
                        )])
                    })
                    .argument_paths(|arguments: &mut Arguments| {
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
                    let arguments = Value::Object(arguments);
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

    /// The filesystem tools, and a host tool whose job runs in `workspace`.
    fn spawning_builder() -> ToolRegistryBuilder {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_local(crate::tool::builtins::filesystem::register)
            .unwrap();
        builder
            .register_product::<Arguments, Value, _, _>(
                "spawn",
                "Start a job in a workspace",
                ToolOptions::default().job_location(|arguments: &Arguments| JobLocation {
                    target: None,
                    workspace: (arguments.get("workspace").and_then(Value::as_str))
                        .map(str::to_owned),
                }),
                |_, _| async { Ok(ToolOutput::new(Value::Null)) },
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

    /// A path resolves against the invocation's workspace and authorizes as the
    /// absolute path it resolves to when that lies outside the root. A job's
    /// workspace resolves as a path that need not exist yet.
    #[tokio::test]
    async fn absolute_and_child_workspace_paths_are_authorized_against_the_root() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = runtime.executor(spawning_builder());
        let reads = |plan: &InvocationPlan, path: &std::path::Path| {
            let path = PathText::new(path).unwrap();
            let resource = ResourceId::path(&crate::target::TargetRef::Root, &path);
            let mut permissions = plan.permissions.iter();
            permissions.any(|p| p.capability == Capability::Read && p.resource == resource)
        };
        let outside = tempfile::tempdir().unwrap();
        let outside = std::fs::canonicalize(outside.path())
            .unwrap()
            .join("outside.txt");
        std::fs::write(&outside, "").unwrap();
        let arguments = serde_json::json!({"path": outside});
        let planned = plan(&executor, &runtime.agent, "read", arguments).await;
        assert!(reads(&planned, &outside));
        let elsewhere = outside.parent().unwrap();
        std::os::unix::fs::symlink(elsewhere, runtime.root.path().join("link")).unwrap();
        let arguments = serde_json::json!({"workspace": "link/.."});
        let spawned = plan(&executor, &runtime.agent, "spawn", arguments).await;
        let written = runtime.root.path().join("link/..");
        assert_eq!(spawned.job_location.workspace, written);
        assert_eq!(spawned.job_runs_in.workspace, elsewhere.parent().unwrap());
        let child = runtime.root.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let child = std::fs::canonicalize(child).unwrap();
        std::fs::write(child.join("input.txt"), "").unwrap();
        let arguments = serde_json::json!({"workspace": "./child/"});
        let spawned = plan(&executor, &runtime.agent, "spawn", arguments).await;
        assert_eq!(spawned.job_location.workspace, child);
        let executor = executor.with_location(spawned.job_location);
        let arguments = serde_json::json!({"path": "input.txt"});
        let planned = plan(&executor, &runtime.agent, "read", arguments).await;
        let input = child.join("input.txt");
        assert!(planned.path_facts.iter().any(|fact| {
            fact.role == crate::tool::diagnostic::PathRole::Resolved && fact.path == input
        }));
        // The root is authorized whole, so a path inside it needs no grant of its own.
        assert!(!reads(&planned, &input));
        let arguments = serde_json::json!({"workspace": "../new"});
        let spawned = plan(&executor, &runtime.agent, "spawn", arguments).await;
        assert!(spawned.permissions.is_empty());
        let created = runtime.root.path().join("new/file");
        let arguments = serde_json::json!({"path": "file", "content": "", "create_parents": true});
        (executor.with_location(spawned.job_location))
            .run_host(&runtime.agent, "write", arguments)
            .await
            .unwrap();
        assert!(created.is_file());
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
            .register_product::<Arguments, Value, _, _>(
                "dynamic_exec",
                "test generic dynamic permissions",
                ToolOptions::new(vec![Capability::Exec])
                    .placement(ToolPlacement::TargetedWorkspace)
                    .argument_permissions(move |_, _| Ok(vec![permission.clone()])),
                |_, _| async { Ok(ToolOutput::new(Value::Null)) },
            )
            .unwrap();
        let mut capabilities = CapabilitySet::default();
        capabilities.insert(Capability::Targets);
        let router = TargetRouter::test(&runtime.store, build(), policy.clone()).await;
        let executor = runtime
            .executor_with_policy(builder, policy.clone())
            .with_target_router(router)
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
        let router = TargetRouter::test(&runtime.store, build(), policy.clone()).await;
        let executor = runtime
            .executor_with_policy(builder, policy.clone())
            .with_target_router(router);
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

    /// Whether a path outside the root exists is disclosed only with approval:
    /// a missing one is refused as a person refuses, without asking anyone,
    /// unless the policy settles the call by itself. Inside the root, a missing
    /// path fails before approval as before.
    #[tokio::test]
    async fn outside_paths_disclose_existence_only_with_approval() {
        use crate::tool::diagnostic::IoKind;
        use std::sync::atomic::{AtomicUsize, Ordering};
        /// Settles workspace permissions; decides outside paths as `outside`
        /// does, or else asks a person, who refuses.
        struct Table {
            outside: Option<PolicyDecision>,
            consulted: AtomicUsize,
            asked: AtomicUsize,
        }
        impl Policy for Table {
            fn authorize(&self, request: AuthorizationRequest) -> Approval<'_> {
                self.consulted.fetch_add(1, Ordering::SeqCst);
                let path = |p: &PermissionUse| matches!(p.resource, ResourceId::Path { .. });
                match &self.outside {
                    _ if !request.permissions.iter().any(path) => {
                        Approval::Decided(PolicyDecision::allow())
                    }
                    Some(decision) => Approval::Decided(decision.clone()),
                    None => Approval::Ask(Box::pin(async {
                        self.asked.fetch_add(1, Ordering::SeqCst);
                        PolicyDecision::refused()
                    })),
                }
            }
        }
        let runtime = crate::tests::TestRuntime::new().await;
        let root = runtime.root.path();
        let elsewhere = tempfile::tempdir().unwrap();
        let elsewhere = std::fs::canonicalize(elsewhere.path()).unwrap();
        std::fs::write(root.join("inside"), "inside").unwrap();
        std::os::unix::fs::symlink(elsewhere.join("linked"), root.join("link")).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("hop"), root.join("back")).unwrap();
        std::fs::create_dir(elsewhere.join("aim")).unwrap();
        let (file, linked, directory, climbed, hop, back, returned, via, aim) = (
            elsewhere.join("file"),
            elsewhere.join("linked"),
            elsewhere.join("directory"),
            elsewhere.join("climbed"),
            elsewhere.join("hop"),
            elsewhere.join("back"),
            elsewhere.join("returned"),
            elsewhere.join("via"),
            elsewhere.join("aim"),
        );
        // Back to `to`, through a directory outside the root.
        let through = |to: &std::path::Path| {
            let mut through = climbed.clone();
            through.extend(climbed.components().skip(1).map(|_| ".."));
            through.join(to.strip_prefix("/").unwrap())
        };
        enum Made {
            File,
            Directory,
            Link(std::path::PathBuf),
        }
        let inside = || Made::Link(root.join("inside"));
        // Each call, and the entry that makes the path it names exist.
        let calls = [
            ("read", serde_json::json!({"path": file}), &file, Made::File),
            (
                "read",
                serde_json::json!({"path": "link"}),
                &linked,
                Made::File,
            ),
            (
                "read",
                serde_json::json!({"path": through(&root.join("inside"))}),
                &climbed,
                Made::Directory,
            ),
            // Out through a link and back in through another.
            ("read", serde_json::json!({"path": "back"}), &hop, inside()),
            // An outside link back in.
            ("read", serde_json::json!({"path": back}), &back, inside()),
            (
                "write",
                serde_json::json!({"path": directory.join("new"), "content": ""}),
                &directory,
                Made::Directory,
            ),
            // A job's workspace outside the root, or an outside link back into it.
            (
                "spawn",
                serde_json::json!({"workspace": climbed}),
                &climbed,
                Made::Directory,
            ),
            (
                "spawn",
                serde_json::json!({"workspace": returned}),
                &returned,
                Made::Link(root.to_owned()),
            ),
            // An outside link to another outside directory, and a detour back.
            (
                "spawn",
                serde_json::json!({"workspace": via}),
                &via,
                Made::Link(aim),
            ),
            (
                "spawn",
                serde_json::json!({"workspace": through(root)}),
                &climbed,
                Made::Directory,
            ),
            // Back out of an outside link, which `..` does not return through.
            (
                "spawn",
                serde_json::json!({"workspace": "link/.."}),
                &linked,
                Made::Directory,
            ),
        ];
        let unattended = PolicyDecision::Deny {
            reason: "approval requires an interactive interface".into(),
        };
        for outside in [None, Some(PolicyDecision::allow()), Some(unattended)] {
            let policy = Arc::new(Table {
                outside: outside.clone(),
                consulted: AtomicUsize::new(0),
                asked: AtomicUsize::new(0),
            });
            let executor = runtime.executor_with_policy(spawning_builder(), policy.clone());
            let call = async |tool: &str, arguments: &Value| {
                let result = executor.run_model(&runtime.agent, tool, arguments.clone());
                match result.await {
                    Ok(result) => Ok(result.is_error),
                    Err(error) => {
                        let cause = error.diagnostic().cause;
                        let viewer = executor.diagnostic_viewer();
                        Err((failure_response(error, viewer).value, cause))
                    }
                }
            };
            // How listing every job shows the latest, but for its id.
            let latest = async || {
                let jobs = executor.jobs().list(&runtime.agent).await.unwrap();
                let job = jobs.into_iter().max_by_key(|job| job.id)?;
                let mut view = job.metadata_view(executor.diagnostic_viewer()).into_value();
                view.as_object_mut()?.remove("id");
                Some(view.to_string())
            };
            let not_found = |result: &Result<bool, (Value, Cause)>| {
                matches!(
                    result,
                    Err((
                        _,
                        Cause::Io {
                            kind: IoKind::NotFound,
                            ..
                        }
                    ))
                )
            };
            assert_eq!(
                call("read", &serde_json::json!({"path": "inside"})).await,
                Ok(false)
            );
            let consulted = policy.consulted.load(Ordering::SeqCst);
            let missing = call("read", &serde_json::json!({"path": "missing"})).await;
            assert!(not_found(&missing), "{missing:?}");
            assert_eq!(policy.consulted.load(Ordering::SeqCst), consulted);
            for (tool, arguments, made, kind) in &calls {
                let _ = std::fs::remove_dir_all(made);
                let _ = std::fs::remove_file(made);
                let (missing, missing_job) = (call(tool, arguments).await, latest().await);
                match kind {
                    Made::File => std::fs::write(made, "outside").unwrap(),
                    Made::Directory => std::fs::create_dir(made).unwrap(),
                    Made::Link(target) => std::os::unix::fs::symlink(target, made).unwrap(),
                }
                let existing = call(tool, arguments).await;
                match (&outside, existing) {
                    (Some(PolicyDecision::Allow { .. }), existing) => {
                        assert_eq!(existing, Ok(false), "{arguments}");
                        assert!(not_found(&missing), "{missing:?}");
                    }
                    // Nor does its refused job, however its path is spelled.
                    (_, Err((denied, Cause::Denied(_)))) => {
                        let missing = (missing.map_err(|(output, _)| output), missing_job);
                        assert_eq!(missing, (Err(denied), latest().await), "{arguments}");
                    }
                    (_, existing) => panic!("{arguments} was not denied: {existing:?}"),
                }
            }
            // Only a path that exists is put to a person.
            let asked = if outside.is_none() { calls.len() } else { 0 };
            assert_eq!(policy.asked.load(Ordering::SeqCst), asked);
        }
    }

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
                .register_product::<Arguments, Value, _, _>(
                    "scoped",
                    "workspace-scoped writes",
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
        assert!(runtime.jobs.list(&runtime.agent).await.unwrap().is_empty());
    }
}
