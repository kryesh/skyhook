//! Validate arguments, select where an invocation and its paths are, and resolve
//! those on the session host now.

use super::resolution::{
    CallSite, Pending, PendingSource, PlacedJob, PlannedConnection, PlannedSource, Targets,
    invalid, refused_workspace,
};
use super::*;
use crate::target::Route;
use crate::tool::admission::admit_paths;
use crate::tool::authorization::{AuthorizationArguments, SourceArguments};
use crate::tool::catalog::{JobLocation, TARGET, split_envelope};
use skyhook_tools::target::TargetedPath;
use skyhook_tools::{diagnostic::Cause, spec::SOURCE_ARGUMENT};
use skyhook_worker::op::TargetPath;

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

    /// Select where a target-relative operation runs: the caller's location, or an
    /// explicit target, which requires the targets capability. The root runs in the
    /// root workspace; another target in its configured workspace, but naming the
    /// caller's own target keeps the caller's workspace. The route to a named target
    /// is a snapshot, not evidence of continued registry freshness.
    fn select_location(&self, explicit: Option<TargetRef>) -> Result<SelectedLocation, ToolError> {
        if explicit.is_some() && !self.capabilities.contains(&Capability::Targets) {
            return Err(ToolError::invalid_arguments(
                "target selection requires the targets capability",
            ));
        }
        self.locate(explicit)
    }

    /// Where the caller's own target, or an `explicit` one, runs or reads.
    pub(super) fn locate(
        &self,
        explicit: Option<TargetRef>,
    ) -> Result<SelectedLocation, ToolError> {
        let caller = &self.caller_location;
        let Some(name) = explicit.as_ref().unwrap_or(&caller.target).name() else {
            let location = match explicit {
                Some(_) => self.shared.root_location.clone(),
                None => caller.clone(),
            };
            return Ok(SelectedLocation {
                location,
                route: None,
            });
        };
        let route = self.shared.router.resolve(name).map_err(ToolError::from)?;
        let location = if caller.target.name() == Some(name) {
            caller.clone()
        } else {
            let destination = &route.destination;
            ExecutionLocation::named(destination.name.clone(), destination.workspace.clone())
        };
        Ok(SelectedLocation {
            location,
            route: Some(route),
        })
    }

    /// Select where a host tool's input places its job, before the job exists:
    /// as the job shows it, and where its workspace resolves. On the session host
    /// it resolves now, and the job shows it as that target spelled it; on another
    /// target the job's connection opens in it, and the job shows it as written.
    async fn locate_job(
        &self,
        requested: JobLocation,
        connections: &mut Vec<PlannedConnection>,
    ) -> Result<(ExecutionLocation, Option<PlacedJob>), ToolError> {
        let selected = self.select_location(requested.target).map_err(|error| {
            error
                .operation(Operation::Lookup, Subject::argument([TARGET]))
                .at(FailureSite::Host)
                .effects(Effects::NotStarted)
        })?;
        let mut location = selected.location;
        let workspace = requested.workspace;
        if workspace.as_ref().is_some_and(TargetPath::is_empty) {
            return Err(refused_workspace("workspace cannot be empty"));
        }
        // A remote job's workspace resolves where it runs, as its connection opens;
        // it is journaled as written.
        if let Some(route) = selected.route {
            let mut segments = vec![location.workspace.clone()];
            if let Some(workspace) = workspace {
                segments.push(workspace.clone());
                location.workspace = workspace;
            }
            let placed = PlacedJob::Connection(connect(connections, route, segments));
            return Ok((location, Some(placed)));
        }
        let Some(requested) = workspace else {
            return Ok((location, None));
        };
        let target = location.target.clone();
        let (workspace, job, preflight) = self.resolve_job(location, requested).await?;
        let spelled = ExecutionLocation { target, workspace };
        Ok((spelled, Some(PlacedJob::Root(job, preflight))))
    }

    pub(super) async fn plan_registered(
        &self,
        kind: InvocationKind,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> Result<InvocationPlan, ToolError> {
        let surface = self.surface_for_agent(&agent);
        let spec = surface.offered(name)?;
        let tool = (self.shared.registry.get(name)).ok_or_else(|| ToolError::unknown_tool(name))?;
        let mut arguments = spec.admit(kind, arguments)?;
        validate_invocation(spec, kind)?;
        let original_arguments = arguments.clone();
        let ExecutionEnvelope { launch, target } = split_envelope(spec, &tool, &mut arguments)?;
        let selected = self
            .resolve_workspace_invocation(&tool, target)
            .map_err(|error| {
                error.or(
                    PartialContext::new(Operation::Lookup, Subject::argument([TARGET]))
                        .at(FailureSite::Host),
                )
            })?;
        // Validation failures belong to the selected location, not the caller's.
        let invalid = invalid(&tool, &selected.location);
        let mut admitted = tool.admit(&arguments).map_err(&invalid)?;
        let derived = admitted.permissions(&selected.location).map_err(&invalid)?;
        let mut connections = Vec::new();
        let route = selected.route.as_ref().map(Route::authorization_arguments);
        let call = (selected.route).map(|route| {
            let workspace = vec![selected.location.workspace.clone()];
            connect(&mut connections, route, workspace)
        });
        // What resolves on the session host resolves now, even for a call that also
        // connects: a bad path there fails before the job exists.
        let (job_location, job) = match admitted.job_location() {
            Some(requested) => self.locate_job(requested, &mut connections).await?,
            None => (selected.location.clone(), None),
        };
        let site = match call {
            Some(index) => CallSite::Connection(index),
            None => {
                let paths = admitted.paths();
                let preflight = admit_paths(&self.shared.root, paths, &selected.location);
                CallSite::Root(preflight.await.map_err(&invalid)?)
            }
        };
        // A host call connects where its input places the job, approved as its own
        // route would be.
        let route = route.or_else(|| match job {
            Some(PlacedJob::Connection(index)) => {
                Some(connections[index].route.authorization_arguments())
            }
            _ => None,
        });
        let (source, source_arguments) = match admitted.source() {
            None => (None, None),
            Some(source) => {
                let planned = self.plan_source(&tool, source, &mut connections);
                let (source, arguments) = planned.await?;
                (Some(source), Some(arguments))
            }
        };
        let authorization_arguments = AuthorizationArguments {
            tool: original_arguments,
            route,
            source: source_arguments,
        };
        let output_schema =
            (admitted.result_schema()).or_else(|| tool.output_schema(&self.capabilities));
        let mut call = PlannedCall {
            origin: match kind {
                InvocationKind::Model => self.model_origin.clone(),
                InvocationKind::Host | InvocationKind::Script => None,
            },
            agent,
            tool,
            authorization_arguments,
            caller_location: self.caller_location.clone(),
            execution_location: selected.location,
            job_location,
            parent,
            launch,
            fit: match kind {
                InvocationKind::Script => crate::job::output::Fit::Whole,
                InvocationKind::Model | InvocationKind::Host => crate::job::output::Fit::Preview,
            },
            output_schema,
            admitted,
        };
        let pending = Pending {
            call: site,
            source,
            job,
            derived,
        };
        let admission = match connections.is_empty() {
            true => {
                let root = Targets {
                    root: &self.shared.root,
                    connections: &[],
                };
                Admission::Resolved(self.resolve(&mut call, pending, root, None).await?)
            }
            false => Admission::Routed {
                connections,
                pending,
            },
        };
        Ok(InvocationPlan { call, admission })
    }

    /// Plan a source argument's read where the file lives, authorized with the
    /// call it feeds; one on the session host resolves now.
    async fn plan_source(
        &self,
        tool: &crate::tool::RegisteredTool,
        source: &TargetedPath,
        connections: &mut Vec<PlannedConnection>,
    ) -> Result<(PlannedSource, SourceArguments), ToolError> {
        if !self.capabilities.contains(&Capability::Read) {
            return Err(ToolError::unavailable(tool.name()));
        }
        let SelectedLocation { location, route } = self
            .select_location(source.target.clone())
            .map_err(|error| {
                error.or(PartialContext::new(
                    Operation::Lookup,
                    Subject::argument([SOURCE_ARGUMENT, TARGET]),
                )
                .at(FailureSite::Host))
            })?;
        let mut arguments = SourceArguments {
            path: source.path.clone(),
            target: location.target.clone(),
            route: None,
        };
        let path = source.path.clone();
        let source = match route {
            None => {
                let root = self.shared.root.clone();
                let (source, preflight) = PendingSource::resolve(root, location, path).await?;
                PlannedSource::Root(source, preflight)
            }
            Some(route) => {
                arguments.route = Some(route.authorization_arguments());
                let workspace = vec![location.workspace.clone()];
                PlannedSource::Connection(PendingSource {
                    target: connect(connections, route, workspace),
                    location,
                    path,
                })
            }
        };
        Ok((source, arguments))
    }
}

/// Add the connection along `route` into `workspace` to `connections`; its index.
fn connect(
    connections: &mut Vec<PlannedConnection>,
    route: Route,
    workspace: Vec<TargetPath>,
) -> usize {
    connections.push(PlannedConnection { route, workspace });
    connections.len() - 1
}

pub(super) struct SelectedLocation {
    pub(super) location: ExecutionLocation,
    pub(super) route: Option<Route>,
}

fn validate_invocation(
    tool: &crate::tool::catalog::ToolSpec,
    kind: InvocationKind,
) -> Result<(), ToolError> {
    match kind {
        InvocationKind::Script if tool.script_binding == ScriptBinding::Unavailable => {
            Err(ToolError::cause(Cause::ScriptUnavailable {
                tool: tool.name.clone(),
            }))
        }
        InvocationKind::Host | InvocationKind::Model | InvocationKind::Script => Ok(()),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::assert_matches;

    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::super::resolution::{Admission, Resolution};
    use super::*;
    use crate::target::TargetDefinition;
    use crate::tool::catalog::ToolOptions;
    use crate::tool::policy::ResourceId;
    use crate::{
        tests::RecordingPolicy,
        tool::{
            ToolRegistryBuilder,
            executor::failure_response,
            policy::{Approval, AuthorizationRequest, Policy, PolicyDecision},
        },
    };
    use skyhook_tools::arguments::Arguments;
    use skyhook_tools::path::{JobWorkspace, PathAccess, PathArgument};
    use skyhook_worker::op::PathKind;

    impl InvocationPlan {
        /// What a call on the session host alone resolved as it was planned.
        pub(in crate::tool::executor) fn resolved(&self) -> &Resolution {
            match &self.admission {
                Admission::Resolved(resolution) => resolution,
                Admission::Routed { .. } => panic!("a call that connects resolves once connected"),
            }
        }

        /// The connections a call opens and what resolves once they do.
        fn routed(&self) -> (&[PlannedConnection], &Pending) {
            match &self.admission {
                Admission::Routed {
                    connections,
                    pending,
                } => (connections, pending),
                Admission::Resolved(_) => panic!("a call on the session host opens nothing"),
            }
        }
    }

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
        [crate::target::tests::definition("build", "/build", None)]
    }

    /// `network_test`'s input: a URL, and a file to upload or a path to save to.
    #[serde_with::skip_serializing_none]
    #[derive(Deserialize, serde::Serialize, JsonSchema)]
    struct NetworkArgs {
        url: String,
        body: Option<FileBody>,
        save_to: Option<TargetPath>,
    }

    #[derive(Deserialize, serde::Serialize, JsonSchema)]
    struct FileBody {
        kind: String,
        path: TargetPath,
    }

    fn network_builder() -> ToolRegistryBuilder {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_product::<NetworkArgs, Value, _, _>(
                "network_test",
                "Exercise invocation-derived authorization",
                ToolOptions::new(vec![Capability::Network])
                    .placement(ToolPlacement::TargetedWorkspace)
                    .background()
                    .named()
                    .argument_permissions(|location, arguments: &NetworkArgs| {
                        if arguments.url != "https://initial.test" {
                            return Err(skyhook_tools::error::AdmissionError::invalid_arguments(
                                "invalid test URL",
                            ));
                        }
                        Ok(vec![PermissionUse::new(
                            Capability::Network,
                            ResourceId::network(&location.target, "https://initial.test"),
                        )])
                    })
                    .argument_paths(|arguments: &mut NetworkArgs| {
                        let exact =
                            |path, access, kind| PathArgument::new(path, access, kind).exact();
                        let upload = (arguments.body.iter_mut()).map(|body| {
                            exact(&mut body.path, PathAccess::Read, PathKind::Existing)
                        });
                        let download = (arguments.save_to.iter_mut())
                            .map(|path| exact(path, PathAccess::Write, PathKind::Writable));
                        upload.chain(download).collect()
                    }),
                |_, arguments| async move { Ok(ToolOutput::new(serde_json::to_value(arguments)?)) },
            )
            .unwrap();
        builder
    }

    /// The filesystem tools, and a host tool whose job runs in `workspace`.
    fn spawning_builder() -> ToolRegistryBuilder {
        let mut builder = ToolRegistryBuilder::default();
        crate::tool::target_tools::filesystem(&mut builder).unwrap();
        builder
            .register_product::<Arguments, Value, _, _>(
                "spawn",
                "Start a job in a workspace",
                ToolOptions::default().job_location(|arguments: &Arguments| JobLocation {
                    target: None,
                    workspace: (arguments.get("workspace").and_then(Value::as_str)).map(Into::into),
                }),
                |context, _| async move {
                    context.create_job_workspace().await?;
                    Ok(ToolOutput::new(Value::Null))
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
            ResourceId::network(&skyhook_tools::target::TargetRef::Root, origin),
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
                capabilities.remove(&removed);
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
        // Invalid arguments, and schema-valid ones the typed handler rejects,
        // fail before approval, path resolution or a job.
        #[derive(Deserialize, JsonSchema)]
        #[serde(try_from = "i64")]
        #[schemars(with = "i64")]
        struct Even;
        impl TryFrom<i64> for Even {
            type Error = &'static str;
            fn try_from(value: i64) -> Result<Self, &'static str> {
                (value % 2 == 0).then_some(Self).ok_or("count must be even")
            }
        }
        #[derive(Deserialize, JsonSchema)]
        struct Count {
            #[serde(rename = "count")]
            _count: Even,
        }
        let mut builder = network_builder();
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
        let arguments =
            serde_json::json!({"url":"invalid", "body":{"kind":"file", "path":"missing"}});
        let error = executor
            .run_host(&runtime.agent, "network_test", arguments)
            .await
            .unwrap_err();
        assert_matches!(error.diagnostic().cause, Cause::InvalidArguments(_));
        let error = executor
            .run_host(&runtime.agent, "count", serde_json::json!({"count": 3}))
            .await
            .unwrap_err();
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.context.subject, Subject::argument(["count"]));
        assert_matches!(diagnostic.cause, Cause::InvalidArguments(message) if message == "count must be even",
            "{error:?}"
        );
        assert!(policy.requests.lock().unwrap().is_empty());
        assert!(runtime.jobs.list(&runtime.agent).await.unwrap().is_empty());
    }

    /// A path resolves against the invocation's workspace and authorizes as the
    /// absolute path it resolves to when that lies outside the root. A job's
    /// missing workspace inside the root is a write, created before the job starts.
    #[tokio::test]
    async fn absolute_and_child_workspace_paths_are_authorized_against_the_root() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = runtime.executor(spawning_builder());
        let reads = |plan: &InvocationPlan, path: &std::path::Path| {
            let resource = ResourceId::path(&skyhook_tools::target::TargetRef::Root, path);
            let mut permissions = plan.resolved().permissions.iter();
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
        let written = crate::tests::text(runtime.root.path().join("link/.."));
        assert_eq!(spawned.call.job_location.workspace, written);
        let runs_in = crate::tests::text(elsewhere.parent().unwrap());
        assert_eq!(spawned.resolved().job.runs_in.workspace, runs_in);
        let child = runtime.root.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let child = std::fs::canonicalize(child).unwrap();
        std::fs::write(child.join("input.txt"), "").unwrap();
        // The job shows its workspace as its target spelled it: made absolute, a
        // trailing separator kept, nothing resolved.
        let arguments = serde_json::json!({"workspace": "./child/"});
        let spawned = plan(&executor, &runtime.agent, "spawn", arguments).await;
        assert_eq!(
            spawned.call.job_location.workspace,
            crate::tests::text(child.join(""))
        );
        assert_eq!(
            spawned.resolved().job.runs_in.workspace,
            crate::tests::text(&child)
        );
        let executor = executor.with_location(spawned.call.job_location.clone());
        let arguments = serde_json::json!({"path": "input.txt"});
        let planned = plan(&executor, &runtime.agent, "read", arguments).await;
        let input = child.join("input.txt");
        assert!(planned.resolved().path_facts.iter().any(|fact| {
            let resolved = skyhook_worker::diagnostic::PathRole::Resolved;
            fact.role == resolved && fact.path == crate::tests::text(&input)
        }));
        // The root is authorized whole, so a path inside it needs no grant of its own.
        assert!(!reads(&planned, &input));
        let arguments = serde_json::json!({"workspace": "../new"});
        let created = runtime.root.path().join("new");
        let mut read_only = CapabilitySet::default();
        read_only.remove(&Capability::Write);
        let refused = (executor.clone().with_capabilities(read_only))
            .run_host(&runtime.agent, "spawn", arguments.clone())
            .await
            .unwrap_err();
        let diagnostic = refused.diagnostic();
        assert_eq!(diagnostic.context.subject, Subject::argument(["workspace"]));
        assert!(!created.exists());
        executor
            .run_host(&runtime.agent, "spawn", arguments)
            .await
            .unwrap();
        assert!(created.is_dir());
        // A workspace outside the root that resolved is never created, even once gone.
        let gone = elsewhere.join("gone");
        std::fs::create_dir(&gone).unwrap();
        let arguments = serde_json::json!({"workspace": gone});
        let agent = runtime.agent.clone();
        let spawned = executor.create(InvocationKind::Host, agent, "spawn", arguments, None);
        let spawned = spawned.await.unwrap();
        assert_eq!(spawned.plan.resolved().job.creates, JobWorkspace::Exists);
        std::fs::remove_dir(&gone).unwrap();
        executor.run(spawned).await.unwrap();
        assert!(!gone.exists());
    }

    #[tokio::test]
    async fn network_nested_paths_are_rewritten_and_authorized_including_in_root() {
        let runtime = crate::tests::TestRuntime::new().await;
        std::fs::write(runtime.root.path().join("upload"), "data").unwrap();
        let policy = RecordingPolicy::allowing();
        let executor = runtime.executor_with_policy(network_builder(), policy);
        let arguments = serde_json::json!({
            "url":"https://initial.test", "body":{"kind":"file","path":"upload"}, "save_to":"download"
        });
        let plan = plan(&executor, &runtime.agent, "network_test", arguments.clone()).await;
        let (upload, download) = (
            runtime.root.path().join("upload"),
            runtime.root.path().join("download"),
        );
        for (capability, path) in [(Capability::Read, &upload), (Capability::Write, &download)] {
            let resource = ResourceId::path(&skyhook_tools::target::TargetRef::Root, path);
            assert!(
                (plan.resolved().permissions.iter())
                    .any(|p| p.capability == capability && p.resource == resource)
            );
        }
        assert_eq!(plan.resolved().permissions.len(), 3);
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
    /// A remote call's permissions, network and dynamic ones alike, are assembled
    /// once it is connected.
    async fn remote_permissions_are_assembled_once_connected() {
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
        let router = TargetRouter::test(&runtime.store, build(), policy).await;
        let executor = (runtime.routed_executor(builder, router)).with_capabilities(capabilities);
        let arguments = serde_json::json!({"url":"https://initial.test", "target":"build"});
        let network = plan(&executor, &runtime.agent, "network_test", arguments).await;
        let (connections, pending) = network.routed();
        let is_network = |p: &PermissionUse| p.capability == Capability::Network;
        assert!(pending.derived.iter().any(is_network));
        // The route is approved on its own, before anything connects.
        let routes: Vec<_> = (connections.iter())
            .flat_map(|connection| connection.route.permissions())
            .collect();
        let is_route = |p: &PermissionUse| p.capability == Capability::Targets;
        assert!(!routes.is_empty() && routes.iter().all(is_route));
        let arguments = serde_json::json!({"target":"build"});
        let command = plan(&executor, &runtime.agent, "dynamic_exec", arguments).await;
        assert!(command.routed().1.derived.contains(&exec));
    }

    #[tokio::test]
    async fn sources_plan_as_reads_on_their_own_target_under_the_same_approval() {
        let runtime = crate::tests::TestRuntime::new().await;
        let policy = RecordingPolicy::allowing();
        let mut builder = ToolRegistryBuilder::default();
        crate::tool::target_tools::register(&mut builder).unwrap();
        let router = TargetRouter::test(&runtime.store, build(), policy).await;
        let executor = runtime.routed_executor(builder, router);
        let mut capabilities = CapabilitySet::default();
        capabilities.insert(Capability::Targets);
        let targeted = executor.clone().with_capabilities(capabilities);
        let agent = &runtime.agent;

        let remote = serde_json::json!({"path":"copy.bin", "source":{"path":"/build/out.bin", "target":"build"}});
        let plan_remote = plan(&targeted, agent, "write", remote.clone()).await;
        let (connections, pending) = plan_remote.routed();
        assert!(matches!(pending.call, CallSite::Root(_)));
        assert!(matches!(
            pending.source,
            Some(PlannedSource::Connection { .. })
        ));
        let mut routes = connections.iter().flat_map(|c| c.route.permissions());
        assert!(routes.any(|p| p.capability == Capability::Targets));
        assert_eq!(
            plan_remote.call.authorization_arguments.document()["source"]["path"],
            "/build/out.bin"
        );

        std::fs::write(runtime.root.path().join("local.bin"), b"bytes").unwrap();
        let local = serde_json::json!({"path":"copy.bin", "source":{"path":"local.bin"}});
        let plan_local = plan(&executor, agent, "write", local.clone()).await;
        assert!(plan_local.resolved().source.is_some());
        // Reading the source needs Read even though the tool itself needs only Write.
        let mut capabilities = CapabilitySet::default();
        capabilities.remove(&Capability::Read);
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
    }

    /// A call that connects resolves its paths on the session host as it is planned:
    /// a bad local destination or source fails before its job exists and before any
    /// approval.
    #[tokio::test]
    async fn a_routed_calls_bad_local_paths_fail_before_its_job_exists() {
        use std::os::unix::{ffi::OsStringExt, fs::symlink};
        let runtime = crate::tests::TestRuntime::new().await;
        let native = std::ffi::OsString::from_vec(b"native-\xff".to_vec());
        let native = runtime.root.path().join(native);
        std::fs::write(&native, b"bytes").unwrap();
        symlink(&native, runtime.root.path().join("alias")).unwrap();
        let policy = RecordingPolicy::allowing();
        let router = TargetRouter::test(&runtime.store, build(), policy.clone()).await;
        let mut builder = ToolRegistryBuilder::default();
        crate::tool::target_tools::register(&mut builder).unwrap();
        let mut capabilities = CapabilitySet::default();
        capabilities.insert(Capability::Targets);
        let executor = (runtime.routed_executor(builder, router)).with_capabilities(capabilities);
        let remote = serde_json::json!({"path": "/build/out.bin", "target": "build"});
        // A write runs where its caller works: here, or on `build`.
        let on_build = ExecutionLocation::named("build".parse().unwrap(), "/build".into());
        let local = serde_json::json!({"path": "alias", "target": "root"});
        for (executor, arguments) in [
            (
                executor.clone(),
                serde_json::json!({"path": "alias", "source": remote}),
            ),
            (
                executor.with_location(on_build),
                serde_json::json!({"path": "copy.bin", "source": local}),
            ),
        ] {
            let ran = executor.run_host(&runtime.agent, "write", arguments.clone());
            let error = crate::tests::bounded(ran).await.unwrap_err();
            assert_matches!(
                error.diagnostic().cause,
                Cause::Unspelled { .. },
                "{arguments}"
            );
        }
        assert!(policy.requests.lock().unwrap().is_empty());
        assert!(runtime.jobs.list(&runtime.agent).await.unwrap().is_empty());
    }

    /// Network-only calls plan without Read or Write.
    #[tokio::test]
    async fn network_calls_need_no_file_capabilities() {
        let runtime = crate::tests::TestRuntime::new().await;
        let policy = RecordingPolicy::allowing();
        let mut capabilities = CapabilitySet::default();
        capabilities.remove(&Capability::Read);
        capabilities.remove(&Capability::Write);
        let executor = runtime
            .executor_with_policy(network_builder(), policy.clone())
            .with_capabilities(capabilities);
        let arguments = serde_json::json!({"url":"https://initial.test"});
        (executor.run_host(&runtime.agent, "network_test", arguments.clone()))
            .await
            .unwrap();
        let all = policy.requests.lock().unwrap();
        assert_eq!(all[0].permissions, [network_use("https://initial.test")]);
        assert_eq!(all[0].arguments, serde_json::json!({"tool": arguments}));
    }

    /// Whether a path outside the root exists is disclosed only with approval:
    /// a missing one is refused as a person refuses, without asking anyone,
    /// unless the policy settles the call by itself. Inside the root, a missing
    /// path fails before approval as before.
    #[tokio::test]
    async fn outside_paths_disclose_existence_only_with_approval() {
        use skyhook_worker::op::IoKind;
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
            // A write creates missing parents, but none under a missing entry's `..`.
            (
                "write",
                serde_json::json!({"path": directory.join("../new"), "content": ""}),
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
                        Cause::Io(skyhook_worker::op::OsError {
                            kind: IoKind::NotFound,
                            ..
                        })
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

    /// A path that resolves to one that is not text fails before approval and before
    /// its job exists, with the bytes its target spelled it in.
    #[tokio::test]
    async fn paths_resolving_to_non_text_fail_before_a_job_exists() {
        use std::os::unix::{ffi::OsStringExt, fs::symlink};
        let runtime = crate::tests::TestRuntime::new().await;
        let name = std::ffi::OsString::from_vec(b"native-\xff".to_vec());
        let native = runtime.root.path().join(name);
        std::fs::write(&native, "secret").unwrap();
        symlink(&native, runtime.root.path().join("unicode-alias")).unwrap();
        let policy = RecordingPolicy::allowing();
        let executor = runtime.executor_with_policy(network_builder(), policy.clone());
        let arguments = serde_json::json!({
            "url": "https://initial.test", "body": {"kind": "file", "path": "unicode-alias"}
        });
        let error = executor
            .run_host(&runtime.agent, "network_test", arguments)
            .await
            .unwrap_err();
        let native = native.into_os_string().into_vec();
        assert_eq!(error.diagnostic().cause, Cause::Unspelled { native });
        assert!(policy.requests.lock().unwrap().is_empty());
        assert!(runtime.jobs.list(&runtime.agent).await.unwrap().is_empty());
    }

    /// A denied write creates nothing: the policy sees the resolved destination,
    /// through symlinks, before any directory exists, and a path that climbs
    /// out of a missing entry fails without asking.
    #[tokio::test]
    async fn write_creates_nothing_before_authorization() {
        use skyhook_worker::op::IoKind;
        use std::os::unix::fs::symlink;
        let runtime = crate::tests::TestRuntime::new().await;
        let workspace = runtime.root.path().join("workspace");
        let outside = runtime.root.path().join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, workspace.join("link")).unwrap();
        symlink(outside.join("missing"), workspace.join("dangling")).unwrap();
        let resolved = ResourceId::path(
            &skyhook_tools::target::TargetRef::Root,
            outside.join("nested/file.txt"),
        );
        for deny_write in [false, true] {
            let mut builder = ToolRegistryBuilder::default();
            crate::tool::target_tools::filesystem(&mut builder).unwrap();
            // Denies every path resource, plus every write when `deny_write` is set.
            let policy = RecordingPolicy::deciding(move |request| {
                let deny = request.permissions.iter().any(|permission| {
                    (deny_write && permission.capability == Capability::Write)
                        || matches!(permission.resource, ResourceId::Path { .. })
                });
                match deny {
                    true => PolicyDecision::Deny {
                        reason: "write fixture denied".to_owned(),
                    },
                    false => PolicyDecision::allow(),
                }
            });
            let router = TargetRouter::over(&runtime.store, policy.clone());
            let jobs = runtime.jobs.clone();
            let executor = ToolExecutor::new(builder.build(), jobs, workspace.clone(), router);
            let write = async |path: &str| {
                let arguments = serde_json::json!({"path": path, "content": "text"});
                executor.run_host(&runtime.agent, "write", arguments).await
            };
            let mut paths = vec![
                "link/nested/file.txt".to_owned(),
                outside.join("nested/file.txt").to_str().unwrap().to_owned(),
                "../outside/nested/file.txt".to_owned(),
            ];
            if deny_write {
                paths.push("local/nested/file.txt".to_owned());
            }
            assert!(write("dangling/nested/file.txt").await.is_err());
            // Nothing resolves under a missing entry, so these fail unasked
            // rather than step back out through a link.
            let asked = policy.requests.lock().unwrap().len();
            for path in [
                "missing/../../outside/nested/file.txt",
                "missing/../link/pwned",
                "missing/../../workspace/link/pwned",
            ] {
                let error = write(path).await.unwrap_err();
                assert_matches!(
                    error.diagnostic().cause,
                    Cause::Io(skyhook_worker::op::OsError {
                        kind: IoKind::NotFound,
                        ..
                    }),
                    "{path}: {error:?}"
                );
            }
            assert_eq!(policy.requests.lock().unwrap().len(), asked);
            for path in paths {
                let error = write(&path).await.unwrap_err();
                assert_matches!(
                    error.diagnostic().cause,
                    Cause::Denied(_),
                    "{path}: {error:?}"
                );
                if path.starts_with("link/") {
                    let requests = policy.requests.lock().unwrap();
                    let asked = &requests.last().unwrap().permissions;
                    assert!(asked.iter().any(|p| p.resource == resolved), "{asked:?}");
                }
            }
        }
        for created in [
            outside.join("nested"),
            outside.join("missing"),
            outside.join("pwned"),
            workspace.join("missing"),
            workspace.join("local"),
        ] {
            assert!(!created.exists(), "{}", created.display());
        }
    }

    #[tokio::test]
    async fn selection_resolves_inherit_root_and_other_targets() {
        let build = ExecutionLocation::named("build".parse().unwrap(), "/caller-override".into());
        let overridden = ExecutionLocation::root("/override".into());
        let session = crate::tests::TestRuntime::new().await;
        let targets = [
            crate::target::tests::definition("build", "/configured", None),
            crate::target::tests::definition("other", "relative directory/../project", None),
        ];
        let policy = crate::tests::RecordingPolicy::allowing();
        let router = TargetRouter::test(&session.store, targets, policy).await;
        let jobs = session.jobs.clone();
        let executor = ToolExecutor::new(ToolRegistry::default(), jobs, "/root".into(), router);
        let mut capabilities = CapabilitySet::default();
        capabilities.insert(Capability::Targets);
        let [root_target, current, other] =
            ["root", "build", "other"].map(|name| name.parse::<TargetRef>().unwrap());
        let select = |caller: &ExecutionLocation, explicit: Option<&TargetRef>, capabilities| {
            let executor = executor.clone().with_location(caller.clone());
            (executor.with_capabilities(capabilities)).select_location(explicit.cloned())
        };
        for (caller, explicit, expected) in [
            (&build, None, build.clone()),
            (&overridden, None, overridden.clone()),
            (
                &build,
                Some(&root_target),
                ExecutionLocation::root("/root".into()),
            ),
            (
                &overridden,
                Some(&root_target),
                ExecutionLocation::root("/root".into()),
            ),
            // An explicit current target keeps the caller's workspace override.
            (&build, Some(&current), build.clone()),
            (
                &build,
                Some(&other),
                ExecutionLocation::named(
                    "other".parse().unwrap(),
                    "relative directory/../project".into(),
                ),
            ),
        ] {
            let selected = select(caller, explicit, capabilities.clone()).unwrap();
            assert_eq!(selected.location, expected);
            assert_eq!(
                selected.route.map(|route| route.destination.name.clone()),
                expected.target.name().cloned()
            );
        }
        // Naming any target needs the capability; inheriting one does not.
        let without = CapabilitySet::default();
        assert!(select(&overridden, Some(&root_target), without.clone()).is_err());
        assert!(select(&build, None, without).is_ok());
        let unknown = "missing".parse::<TargetRef>().unwrap();
        assert!(select(&build, Some(&unknown), capabilities).is_err());
    }
}
