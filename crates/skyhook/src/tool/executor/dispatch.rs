//! Approve, connect and start jobs after planning.

use super::resolution::{CallReach, PlannedConnection, Resolution, Targets, stopped};
use super::*;
use crate::job::output::TargetFiles;
use crate::job::stage;
use crate::remote::PreparedConnection;

impl ToolExecutor {
    pub(super) fn job_spec(&self, plan: &InvocationPlan) -> JobSpec {
        let call = &plan.call;
        JobSpec {
            role: call.tool.job_role(),
            origin: call.origin.clone(),
            agent: call.agent.clone(),
            parent: call.parent,
            tool: call.tool.name().to_owned(),
            name: call.launch.name.clone(),
            arguments: Value::Object(call.authorization_arguments.tool.clone()),
            output_schema: call.output_schema.clone(),
            accepts_input: call.tool.accepts_input(),
            background: call.launch.background,
            location: call.job_location.clone(),
        }
    }

    /// Authorize, prepare and launch one owned invocation.
    pub(super) async fn start(
        &self,
        plan: InvocationPlan,
        lease: crate::job::JobLease,
    ) -> Result<StartedExecution, ToolError> {
        if lease.cancellation_token().is_cancelled() {
            return Err(self.cancelled(lease).await);
        }
        let lease = lease.await_approval().await.map_err(job_error)?;
        self.admit(plan, lease).await
    }

    /// What restarts a call this executor made once an interruption ended it
    /// before it was accepted: it is planned and asked again, then runs as it
    /// would have. It holds the executor weakly, as jobs must not retain their
    /// own manager.
    pub(crate) fn relaunch(&self) -> crate::job::Relaunch {
        let (shared, model_origin) = (Arc::downgrade(&self.shared), self.model_origin.clone());
        let (caller_location, capabilities) =
            (self.caller_location.clone(), self.capabilities.clone());
        Arc::new(move |lease| {
            let executor = shared.upgrade().map(|shared| ToolExecutor {
                model_origin: model_origin.clone(),
                shared,
                caller_location: caller_location.clone(),
                capabilities: capabilities.clone(),
            });
            Box::pin(async move {
                let Some(executor) = executor else {
                    return;
                };
                let Ok(lease) = lease.reapprove().await else {
                    return;
                };
                match executor.replan(lease.id()).await {
                    Ok(plan) => drop(executor.admit(plan, lease).await),
                    Err(error) => drop(executor.fail_start(lease, error).await),
                }
            })
        })
    }

    /// Approve, connect and launch `plan`, whose job awaits approval. A call
    /// whose paths are all on the session host was resolved as it was planned
    /// and is approved once. One that connects is approved for its routes alone
    /// before anything connects, and runs from then on; once connected, its
    /// targets resolve the paths it names, and the rest of its permissions are
    /// approved together. A target the call names that is redefined before it
    /// connects replans the whole call, so the policy decides on current
    /// definitions.
    async fn admit(
        &self,
        mut plan: InvocationPlan,
        mut lease: crate::job::JobLease<stage::Approving>,
    ) -> Result<StartedExecution, ToolError> {
        let subject = AuthorizationSubject {
            agent: plan.call.agent.clone(),
            job: lease.id(),
            parent: plan.call.parent,
            capabilities: self.capabilities.clone(),
            cancellation: lease.cancellation_token(),
        };
        let mut replanned = false;
        let (call, lease, connections, resolution) = loop {
            let InvocationPlan {
                mut call,
                admission,
            } = plan;
            let (connections, mut pending) = match admission {
                Admission::Resolved(resolution) => {
                    break (call, Lease::Approving(lease), Vec::new(), resolution);
                }
                Admission::Routed {
                    connections,
                    pending,
                } => (connections, pending),
            };
            let routes = (connections.iter())
                .flat_map(|connection| connection.route.permissions())
                .collect();
            // A redefinition before it connects hands back the approving lease.
            let arguments = (&*call.tool, &call.authorization_arguments);
            let approving = match self
                .approve(&subject, arguments, routes, Asking::Allowed, &[])
                .await
            {
                Ok(()) => {
                    let running = lease.run().await.map_err(job_error)?;
                    match self.connect(&connections, &subject).await {
                        Ok(connections) => {
                            let targets = Targets {
                                root: &self.shared.root,
                                connections: &connections,
                            };
                            targets.place(&mut call, &mut pending);
                            let preapproving =
                                self.preapprove(&mut call, &pending, &targets, &subject);
                            let preapproved = preapproving.await;
                            // A target slow to resolve the paths holds up no cancellation.
                            let resolving = self.resolve(&mut call, pending, targets, preapproved);
                            match subject.cancellation.run_until_cancelled(resolving).await {
                                Some(Ok(resolution)) => {
                                    break (call, Lease::Running(running), connections, resolution);
                                }
                                Some(Err(error)) => {
                                    return Err(self.fail_start(running, error).await);
                                }
                                None => {
                                    let cancelled = ToolError::cancelled_before_start();
                                    let cancelled = stopped(preapproved, cancelled);
                                    return Err(self.fail_start(running, cancelled).await);
                                }
                            }
                        }
                        Err(RemoteError {
                            fault: RemoteFault::Authorization(AuthorizationError::TargetRedefined),
                            ..
                        }) if !replanned => running.reapprove().await.map_err(job_error)?,
                        Err(RemoteError {
                            fault: RemoteFault::Cancelled,
                            ..
                        }) => return Err(self.cancelled(running).await),
                        Err(error) => {
                            let error = ToolError::from(error).or(PartialContext::new(
                                Operation::Connect,
                                Subject::Tool(call.tool.name().to_owned()),
                            )
                            .at(FailureSite::Host));
                            return Err(self.fail_start(running, error).await);
                        }
                    }
                }
                Err(AuthorizationError::TargetRedefined) if !replanned => lease,
                Err(error) => {
                    return Err(self
                        .fail_start(lease, refusal(call.tool.name(), error))
                        .await);
                }
            };
            replanned = true;
            lease = approving;
            plan = match self.replan(subject.job).await {
                Ok(plan) => plan,
                Err(error) => return Err(self.fail_start(lease, error).await),
            };
        };
        let Resolution {
            target,
            job,
            source,
            permissions,
            path_facts,
            unresolved,
            performing,
        } = resolution;
        let admitted = Targets {
            root: &self.shared.root,
            connections: &connections,
        }
        .admitted();
        // A call performing as it flew was approved before it flew.
        if performing.is_none() {
            let asking = Asking::for_unresolved(&unresolved);
            let arguments = (&*call.tool, &call.authorization_arguments);
            let approved = self.approve(&subject, arguments, permissions, asking, &admitted);
            if let Err(error) = approved.await {
                return Err(lease.fail(self, refusal(call.tool.name(), error)).await);
            }
        }
        if let Some(unresolved) = unresolved {
            return Err(lease.fail(self, unresolved).await);
        }
        let running = lease.running().await.map_err(job_error)?;
        if running.cancellation_token().is_cancelled() {
            let cancelled = stopped(performing, ToolError::cancelled_before_start());
            return Err(self.fail_start(running, cancelled).await);
        }
        let invocation = call.admitted.invoke();
        let result_policy = invocation.result_policy();
        let tool = call.tool;
        let (input, worker) = running.split();
        // What the call's own output keeps in files on its target, it reads there.
        let produced = TargetFiles::new(target.clone(), call.execution_location.clone());
        let reach = CallReach {
            executor: self.clone(),
            tool: tool.clone(),
            arguments: call.authorization_arguments.tool,
            subject: subject.clone(),
        };
        let mut context = ToolContext::new(
            subject,
            call.execution_location,
            call.caller_location,
            input,
            self.shared.jobs.clone(),
        )
        .invoked((target, job.target), Arc::new(reach))
        .with_job_location(job.runs_in, job.creates)
        .with_fit(call.fit);
        // The managed agent replaces an inherited SSH_AUTH_SOCK only for target management.
        let agent = (context.execution_location().is_root()
            && tool.managed_agent()
            && context.capabilities().contains(&Capability::Targets))
        .then(|| self.shared.router.clone());
        let job = context.job();
        // The handle shows the job as launched, before its worker can finish it.
        let started = if call.launch.background {
            let launched = self.shared.jobs.snapshot(job).await.map_err(job_error)?;
            StartedExecution::Background(Box::new(launched))
        } else {
            StartedExecution::Foreground(job, result_policy, produced)
        };
        worker.launch(
            async move {
                let cancellation = context.cancellation_token();
                let fallback =
                    PartialContext::new(Operation::Execute, Subject::Tool(tool.name().to_owned()))
                        .at(tool.placement().site(context.execution_location()))
                        .paths(path_facts);
                let result = async {
                    if let Some(router) = agent {
                        context
                            .process_environment
                            .extend((router.agent_environment().await)?);
                    }
                    let source = match source {
                        Some(source) => Some(source.open(&context).await?),
                        None => None,
                    };
                    // Cancellation outranks how preparing the call ended.
                    if context.is_cancelled() {
                        return Err(stopped(performing, ToolError::cancelled_before_start()));
                    }
                    invocation.call(context.with_source(source)).await
                }
                .await;
                let outcome = crate::job::JobOutcome::from(
                    cancellation_result(result, cancellation.is_cancelled())
                        .map_err(|error| error.or(fallback)),
                );
                match result_policy {
                    ToolResultPolicy::Nothing => outcome.without_result(),
                    _ => outcome,
                }
            },
            "tool handler panicked",
        );
        Ok(started)
    }

    /// Open the call's connections in order: a failed one stops before any later
    /// one opens.
    async fn connect(
        &self,
        connections: &[PlannedConnection],
        subject: &AuthorizationSubject,
    ) -> Result<Vec<PreparedConnection>, RemoteError> {
        let mut prepared = Vec::with_capacity(connections.len());
        for PlannedConnection { route, workspace } in connections {
            let router = &self.shared.router;
            prepared.push(
                router
                    .prepare(route.clone(), workspace.clone(), subject)
                    .await?,
            );
        }
        Ok(prepared)
    }

    /// Authorize `permissions` of `tool`'s call, from connections `admitted` at those
    /// target revisions.
    pub(super) fn approve<'a>(
        &'a self,
        subject: &'a AuthorizationSubject,
        (tool, arguments): (
            &crate::tool::RegisteredTool,
            &crate::tool::authorization::AuthorizationArguments,
        ),
        permissions: Vec<PermissionUse>,
        asking: Asking,
        admitted: &'a [(skyhook_tools::target::TargetName, u64)],
    ) -> impl Future<Output = Result<(), AuthorizationError>> + Send + 'a {
        let (arguments, tool) = (arguments.document(), tool.name().to_owned());
        (self.shared.router.authorization()).authorize(
            subject,
            tool,
            permissions,
            arguments,
            admitted,
            asking,
        )
    }

    /// Plan job `id`'s call again against the current targets, as it was
    /// journaled and admitted once, unless the location its job journaled no
    /// longer holds.
    async fn replan(&self, id: JobId) -> Result<InvocationPlan, ToolError> {
        let call = self.shared.jobs.snapshot(id).await.map_err(job_error)?;
        let (agent, tool, parent) = (call.agent, call.tool, call.parent);
        let arguments = std::sync::Arc::unwrap_or_clone(call.arguments);
        let planned = self.plan_registered(InvocationKind::Host, agent, &tool, arguments, parent);
        let mut plan = planned
            .await
            .map_err(|error| error.effects(Effects::NotStarted))?;
        // A script's call keeps its whole results when it is planned again.
        if let Some(parent) = parent
            && self
                .shared
                .jobs
                .snapshot(parent)
                .await
                .map_err(job_error)?
                .role
                == crate::job::JobRole::Script
        {
            plan.call.fit = crate::job::output::Fit::Whole;
        }
        if plan.call.job_location != call.location {
            return Err(refusal(
                plan.call.tool.name(),
                AuthorizationError::TargetRedefined,
            ));
        }
        Ok(plan)
    }

    async fn cancelled<S>(&self, lease: crate::job::JobLease<S>) -> ToolError {
        (self.fail_start(lease, ToolError::cancelled_before_start())).await
    }

    async fn fail_start<S>(&self, lease: crate::job::JobLease<S>, error: ToolError) -> ToolError {
        lease
            .fail(ToolError::from_facts(error.facts().clone(), None).into())
            .await;
        error
    }
}

/// A job's lease up to its call's last approval: it runs once its first one
/// passes, which for a call that connects is its routes'.
enum Lease {
    Approving(crate::job::JobLease<stage::Approving>),
    Running(crate::job::JobLease<stage::Running>),
}

impl Lease {
    async fn running(self) -> Result<crate::job::JobLease<stage::Running>, crate::job::JobError> {
        match self {
            Self::Approving(lease) => lease.run().await,
            Self::Running(lease) => Ok(lease),
        }
    }

    async fn fail(self, executor: &ToolExecutor, error: ToolError) -> ToolError {
        match self {
            Self::Approving(lease) => executor.fail_start(lease, error).await,
            Self::Running(lease) => executor.fail_start(lease, error).await,
        }
    }
}

/// How a call reports an authorization that refused it before it started.
pub(super) fn refusal(tool: &str, error: AuthorizationError) -> ToolError {
    if let AuthorizationError::Cancelled = error {
        return ToolError::cancelled_before_start();
    }
    ToolError::from(skyhook_tools::error::AdmissionError::from(error)).or(PartialContext::new(
        Operation::Authorize,
        Subject::Tool(tool.to_owned()),
    )
    .at(FailureSite::Host)
    .effects(Effects::NotStarted))
}

/// Cancellation determines the outcome, not whether already observed effects or
/// output happened. Apply its precedence once, before binding failure facts.
pub(crate) fn cancellation_result<T: Into<ToolOutput>>(
    result: Result<T, ToolError>,
    cancelled: bool,
) -> Result<T, ToolError> {
    if !cancelled {
        return result;
    }
    let (mut diagnostic, output) = match result {
        Err(error) => error.into_facts(),
        Ok(output) => (
            PartialDiagnostic::new(PartialContext::default(), Cause::Cancelled),
            Some(output.into()),
        ),
    };
    diagnostic.cause = Cause::Cancelled;
    Err(ToolError::from_facts(diagnostic, output))
}

#[cfg(test)]
mod tests {
    use super::super::planning::tests::build;
    use super::*;
    use crate::tool::catalog::ToolOptions;
    use crate::{
        job::JobTransition, session::SessionEvent, tests::bounded, tool::ToolRegistryBuilder,
    };
    use skyhook_tools::{arguments::Arguments, diagnostic::Cause};
    use skyhook_worker::diagnostic::Effects;

    /// `tools`, reaching targets through `router`, for a caller holding the targets
    /// capability.
    fn remote_executor(
        runtime: &crate::tests::TestRuntime,
        tools: ToolRegistryBuilder,
        router: &TargetRouter,
    ) -> ToolExecutor {
        let mut capabilities = CapabilitySet::default();
        capabilities.insert(Capability::Targets);
        (runtime.routed_executor(tools, router.clone())).with_capabilities(capabilities)
    }

    fn remote_tool(capabilities: Vec<Capability>) -> ToolRegistryBuilder {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_product::<Arguments, serde_json::Value, _, _>(
                "custom_remote",
                "test",
                ToolOptions::new(capabilities).placement(ToolPlacement::TargetedWorkspace),
                |_, _| async { panic!("remote job must not execute locally") },
            )
            .unwrap();
        builder
    }

    fn spawn_remote(
        executor: &ToolExecutor,
        agent: &AgentId,
    ) -> tokio::task::JoinHandle<Result<ExecutionResult, ToolError>> {
        let (executor, agent) = (executor.clone(), agent.clone());
        let arguments = serde_json::json!({"target":"build"});
        tokio::spawn(async move { executor.run_host(&agent, "custom_remote", arguments).await })
    }

    /// The builtins, run on a target this process's own worker serves: `build`,
    /// whose workspace and authorization root is `workspace`.
    async fn served(
        runtime: &crate::tests::TestRuntime,
        workspace: &std::path::Path,
        policy: Arc<dyn crate::tool::policy::Policy>,
    ) -> (ToolExecutor, TargetRouter) {
        let (executor, router, _) = serving(runtime, workspace, policy).await;
        (executor, router)
    }

    /// [`served`], with the worker's record of the operations it was called for.
    async fn serving(
        runtime: &crate::tests::TestRuntime,
        workspace: &std::path::Path,
        policy: Arc<dyn crate::tool::policy::Policy>,
    ) -> (
        ToolExecutor,
        TargetRouter,
        Arc<crate::remote::ServingFactory>,
    ) {
        let mut builder = ToolRegistryBuilder::default();
        crate::tool::target_tools::register(&mut builder).unwrap();
        let workspace = crate::tests::text(workspace);
        let definition = crate::target::tests::definition("build", workspace, None);
        let router = TargetRouter::test(&runtime.store, [definition], policy).await;
        let factory = Arc::new(crate::remote::ServingFactory::default());
        let router = router.with_connection_factory(factory.clone());
        (remote_executor(runtime, builder, &router), router, factory)
    }

    /// A remote call is approved for its route alone before it connects, then in
    /// advance for its path resolving inside the root, then, as it resolved outside,
    /// for the path its target resolved; its output and images persist on the host,
    /// and a source it holds streams to another machine.
    #[tokio::test]
    async fn remote_calls_resolve_on_their_target_between_two_approvals() {
        use crate::tool::policy::ResourceId;
        use skyhook_worker::{op::CHUNK_BYTES, protocol::flow::WINDOW};
        let runtime = crate::tests::TestRuntime::new().await;
        let scratch = tempfile::tempdir().unwrap();
        let scratch = std::fs::canonicalize(scratch.path()).unwrap();
        let workspace = scratch.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let policy = crate::tests::RecordingPolicy::allowing();
        let (executor, router) = served(&runtime, &workspace, policy.clone()).await;
        let agent = &runtime.agent;
        let requests = || std::mem::take(&mut *policy.requests.lock().unwrap());

        let text = "aé🦀\n".repeat(20_000);
        let outside = scratch.join("outside.txt");
        std::fs::write(&outside, &text).unwrap();
        let arguments = serde_json::json!({"path": outside, "target": "build"});
        let read = executor.run_host(agent, "read", arguments).await.unwrap();
        let saved = runtime.jobs.output(read.job).await;
        assert_eq!(
            saved.test_bytes("/result/content").unwrap(),
            text.as_bytes()
        );
        let [route, inside, tool] = requests().try_into().unwrap();
        let is_route = |p: &PermissionUse| p.capability == Capability::Targets;
        assert!(route.permissions.iter().all(is_route));
        let path = ResourceId::path(&"build".parse().unwrap(), &outside);
        assert!(!tool.permissions.iter().any(is_route));
        assert!(!inside.permissions.iter().any(|p| p.resource == path));
        assert!(tool.permissions.iter().any(|p| p.resource == path));

        let image = crate::tests::png(b"remote source bytes").await;
        std::fs::write(workspace.join("image.png"), image.bytes()).unwrap();
        let arguments = serde_json::json!({"path": "image.png", "target": "build"});
        let read = executor.run_host(agent, "read", arguments).await.unwrap();
        let images = runtime.jobs.snapshot(read.job).await.unwrap().images;
        let [image_ref] = images.try_into().unwrap();
        let stored = (runtime.store)
            .read_blob(&image_ref.blob, skyhook_media::MAX_IMAGE_BYTES as usize)
            .await
            .unwrap();
        assert_eq!(stored, image.bytes());

        let binary: Vec<u8> = (0..CHUNK_BYTES * (WINDOW + 2) + 11)
            .map(|index| (index % 253) as u8)
            .collect();
        std::fs::write(workspace.join("remote.bin"), &binary).unwrap();
        let arguments = serde_json::json!({
            "path": "copy.bin", "source": {"path": "remote.bin", "target": "build"},
        });
        executor.run_host(agent, "write", arguments).await.unwrap();
        assert_eq!(
            std::fs::read(runtime.root.path().join("copy.bin")).unwrap(),
            binary
        );

        router.shutdown().await;
    }

    /// What a job's target holds of its output is read on that target with the
    /// session's readers let go, a page of it and a script's whole view alike: while
    /// the target leaves the read unanswered, other saved output is read, and
    /// cancelling the call ends its read there too. A caller that cannot reach the
    /// target is told the output is unavailable, and so is every caller once the target
    /// is redefined: the file is on the machine its run reached.
    #[tokio::test]
    async fn a_read_on_a_jobs_target_holds_no_session_reader_and_stops_when_cancelled() {
        use crate::tests::bounded;
        let runtime = crate::tests::TestRuntime::new().await.spilling_past(4096);
        let scratch = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(scratch.path()).unwrap();
        let text: String = (1..=12_000).map(|line| format!("line {line}\n")).collect();
        std::fs::write(workspace.join("big.log"), &text).unwrap();
        let tools = || {
            let mut builder = ToolRegistryBuilder::default();
            crate::tool::target_tools::register(&mut builder).unwrap();
            crate::tool::builtins::jobs::register(&mut builder, runtime.jobs.clone()).unwrap();
            builder
        };
        let definition =
            crate::target::tests::definition("build", crate::tests::text(&workspace), None);
        let policy = Arc::new(crate::tool::policy::AllowAll);
        let defined = [definition.clone()];
        let router = TargetRouter::test(&runtime.store, defined, policy.clone()).await;
        let factory = Arc::new(crate::remote::ServingFactory::default());
        let router = router.with_connection_factory(factory.clone());
        let executor = remote_executor(&runtime, tools(), &router);
        let agent = &runtime.agent;
        let read = serde_json::json!({"path": "big.log", "target": "build"});
        let read = executor.run_model(agent, "read", read).await.unwrap();
        let page = serde_json::json!({
            "job": read.job, "field": "/result/content", "start": 11_000, "limit": 2,
        });
        let paged = executor.run_model(agent, "jobs", page.clone()).await;
        let paged = paged.unwrap();
        let lines = &paged.output.value["presentation"]["preview"]["lines"];
        assert_eq!(*lines, serde_json::json!(["line 11000", "line 11001"]));

        factory.hold_reads();
        let spec = JobSpec {
            background: true,
            role: crate::job::JobRole::Script,
            ..JobSpec::test(agent.clone(), "script")
        };
        let script = runtime.jobs.create(spec).await.unwrap().id();
        let whole = serde_json::json!({"job": read.job});
        let mut reading = script;
        for (kind, arguments, parent) in [
            (InvocationKind::Model, page.clone(), None),
            (InvocationKind::Script, whole, Some(script)),
        ] {
            let call = {
                let (executor, agent) = (executor.clone(), agent.clone());
                let call = async move {
                    executor
                        .execute_as(kind, agent, "jobs", arguments, parent)
                        .await
                };
                tokio::spawn(call)
            };
            factory.read_held().await;
            let shown = crate::job::output::OutputArgs::new(read.job);
            let capabilities = Default::default();
            let shown = bounded(runtime.jobs.present_output(shown, &capabilities)).await;
            let shown = shown.unwrap()["result"]["content"].to_string();
            assert!(shown.starts_with("\"line 1\\nline 2\\n"), "{shown}");
            reading = JobId::new(reading.get() + 1).unwrap();
            runtime.jobs.cancel(reading).await.unwrap();
            let cancelled = bounded(call).await.unwrap().unwrap();
            assert_eq!(cancelled.output.value["state"], "cancelled");
            factory.call_cancelled().await;
        }

        let unrouted = remote_executor(
            &runtime,
            tools(),
            &TargetRouter::over(&runtime.store, policy),
        );
        let unreachable = unrouted
            .run_model(agent, "jobs", page.clone())
            .await
            .unwrap();
        let unreachable = unreachable.output.value["error"].as_str().unwrap();
        let plain = "saved output for /result/content is no longer available";
        assert!(unreachable.ends_with(plain), "{unreachable}");
        let subject = runtime.subject(script, crate::job::CancellationToken::new());
        router.add(definition, &subject).await.unwrap();
        let redefined = bounded(executor.run_model(agent, "jobs", page))
            .await
            .unwrap();
        let redefined = redefined.output.value["error"].as_str().unwrap();
        assert!(redefined.ends_with(plain), "{redefined}");
        router.shutdown().await;
    }

    /// A remote job's workspace is journaled as written and resolved where the job
    /// runs; one missing there refuses the connection.
    #[tokio::test]
    async fn remote_job_workspaces_resolve_where_they_run() {
        let runtime = crate::tests::TestRuntime::new().await;
        let scratch = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(scratch.path()).unwrap();
        let policy = crate::tests::RecordingPolicy::allowing();
        let definition =
            crate::target::tests::definition("build", crate::tests::text(&workspace), None);
        let router = TargetRouter::test(&runtime.store, [definition], policy.clone()).await;
        let router =
            router.with_connection_factory(Arc::new(crate::remote::ServingFactory::default()));
        let mut builder = ToolRegistryBuilder::default();
        let placed = |arguments: &Arguments| crate::tool::catalog::JobLocation {
            target: Some("build".parse().unwrap()),
            workspace: arguments
                .get("workspace")
                .and_then(Value::as_str)
                .map(Into::into),
        };
        builder
            .register_product::<Arguments, Value, _, _>(
                "place",
                "test",
                ToolOptions::default().job_location(placed),
                |context, _| async move {
                    let runs_in = &context.job_location().workspace;
                    Ok(ToolOutput::new(serde_json::json!(runs_in)))
                },
            )
            .unwrap();
        let executor = remote_executor(&runtime, builder, &router);
        let arguments = serde_json::json!({"workspace": "sub"});
        let refused = (executor.run_host(&runtime.agent, "place", arguments.clone())).await;
        let refused = refused.unwrap_err().diagnostic().cause;
        let fault = skyhook_tools::diagnostic::ConnectionFault::Refused;
        assert!(
            matches!(refused, Cause::Connection { fault: kept, .. } if kept == fault),
            "{refused:?}"
        );
        std::fs::create_dir(workspace.join("sub")).unwrap();
        let placed = (executor.run_host(&runtime.agent, "place", arguments))
            .await
            .unwrap();
        assert_eq!(
            placed.output.value,
            serde_json::json!(workspace.join("sub"))
        );
        let journaled = runtime.jobs.snapshot(placed.job).await.unwrap().location;
        assert_eq!(journaled.workspace, "sub".into());
        router.shutdown().await;
    }

    /// Remote output is live, and cancelling the call keeps what was cut. Its
    /// capability is scoped to the workspace as the target reports it, not as
    /// the target's definition spells it.
    #[tokio::test]
    async fn remote_output_is_live_and_cancellation_preserves_cut_streams() {
        use crate::tool::policy::ResourceId;
        let runtime = crate::tests::TestRuntime::new().await;
        let scratch = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(scratch.path()).unwrap();
        let spelled = workspace.join(".");
        let policy = crate::tests::RecordingPolicy::allowing();
        let (executor, router) = served(&runtime, &spelled, policy.clone()).await;
        let command = ["/bin/sh", "-c", "printf before; exec sleep 3600"];
        let arguments = serde_json::json!({"command": command, "target": "build"});
        let (calling, agent) = (executor.clone(), runtime.agent.clone());
        let call = tokio::spawn(async move { calling.run_host(&agent, "exec", arguments).await });
        // Files and saved output have no change notification, so this waits by polling.
        let poll = std::time::Duration::from_millis(10);
        let job = bounded(async {
            loop {
                if let Some(job) = runtime.jobs.list(&runtime.agent).await.unwrap().first() {
                    break job.id;
                }
                tokio::time::sleep(poll).await;
            }
        })
        .await;
        let saved = runtime.jobs.output(job).await;
        bounded(async {
            while saved.test_bytes("/result/stdout").as_deref() != Some(b"before") {
                tokio::time::sleep(poll).await;
            }
        })
        .await;
        assert!(
            !runtime
                .jobs
                .snapshot(job)
                .await
                .unwrap()
                .state
                .is_terminal()
        );
        runtime.jobs.cancel(job).await.unwrap();
        drop(bounded(call).await.unwrap());
        bounded(runtime.jobs.wait_settled(job)).await.unwrap();
        assert_eq!(
            runtime.jobs.snapshot(job).await.unwrap().state,
            JobState::Cancelled
        );
        assert_eq!(saved.test_captures_complete(), Some(false));
        assert_eq!(saved.test_bytes("/result/stdout").unwrap(), b"before");
        let reported = crate::tests::text(&workspace);
        let scope = ResourceId::workspace(&"build".parse().unwrap(), &reported);
        let requests = policy.requests.lock().unwrap().clone();
        let exec = |p: &PermissionUse| p.capability == Capability::Exec;
        let scoped = (requests.iter())
            .flat_map(|request| &request.permissions)
            .filter(|p| exec(p));
        assert!(scoped.map(|p| &p.resource).eq([&scope]), "{requests:?}");
        router.shutdown().await;
    }

    #[test]
    fn cancellation_retains_output_and_failure_context() {
        use skyhook_tools::diagnostic::{Cause, RequestEffects};
        use skyhook_worker::diagnostic::{Operation, Subject};

        for error in [
            ToolError::cancelled(),
            ToolError::failed("transport closed"),
        ] {
            let error = error
                .operation(Operation::Receive, Subject::Process)
                .request(RequestEffects::MayHaveExecuted)
                .with_result(ToolOutput::new(serde_json::json!({"partial":true})));
            let context = error.diagnostic().context;
            let (diagnostic, output) = cancellation_result::<ToolOutput>(Err(error), true)
                .unwrap_err()
                .into_parts();
            assert_eq!(diagnostic.cause, Cause::Cancelled);
            assert_eq!(diagnostic.context, context);
            assert_eq!(output.unwrap().value, serde_json::json!({"partial":true}));
        }
        let (diagnostic, output) = cancellation_result(
            Ok(ToolOutput::new(serde_json::json!({"partial":true}))),
            true,
        )
        .unwrap_err()
        .into_parts();
        assert_eq!(diagnostic.cause, Cause::Cancelled);
        // Only a sender claims that its request may have executed.
        assert_eq!(diagnostic.context.request, RequestEffects::Unknown);
        assert_eq!(output.unwrap().value, serde_json::json!({"partial":true}));
    }

    /// A unit tool has no result, even when cancelled after completing or when
    /// its failure carries output.
    #[tokio::test]
    async fn unit_calls_never_have_a_result() {
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        struct Unit {
            fail: bool,
        }
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        struct Empty {}
        let runtime = crate::tests::TestRuntime::new().await;
        let (entered, mut started) = tokio::sync::mpsc::unbounded_channel();
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_unit::<Unit, _, _>(
                "unit",
                "gated",
                ToolOptions::default(),
                move |call, input| {
                    let entered = entered.clone();
                    async move {
                        if input.fail {
                            let partial = ToolOutput::new(serde_json::json!({"partial":true}));
                            return Err(ToolError::failed("failed").with_result(partial));
                        }
                        entered.send(call.job()).unwrap();
                        call.cancellation_token().cancelled().await;
                        Ok(())
                    }
                },
            )
            .unwrap();
        builder
            .register_unit::<Empty, _, _>(
                "noop",
                "Do nothing.",
                ToolOptions::default(),
                |_, _| async { Ok(()) },
            )
            .unwrap();
        let executor = runtime.executor(builder);
        for fail in [false, true] {
            let (executor, agent) = (executor.clone(), runtime.agent.clone());
            let arguments = serde_json::json!({"fail": fail});
            let running =
                tokio::spawn(async move { executor.run_host(&agent, "unit", arguments).await });
            if !fail {
                let job = bounded(started.recv()).await.unwrap();
                runtime.jobs.cancel(job).await.unwrap();
            }
            let (_, output) = bounded(running).await.unwrap().unwrap_err().into_parts();
            assert!(output.is_none());
        }
        let jobs = runtime.jobs.list(&runtime.agent).await.unwrap();
        assert_eq!(jobs.len(), 2);
        for job in jobs {
            assert_eq!(
                runtime.jobs.test_snapshot(job.id).await.unwrap().output,
                None
            );
        }
        // A script's unit call completes with nothing in place of a result.
        let response = executor
            .execute_as(
                InvocationKind::Script,
                runtime.agent.clone(),
                "noop",
                serde_json::json!({}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(response.output.value, serde_json::json!({}));
    }

    /// A host call that places its job on a remote opens that route only once it is
    /// approved with the call: denied, the call fails before anything connects;
    /// approved, it runs with the facts the connection's handshake reported.
    #[tokio::test]
    async fn a_host_call_connects_where_it_places_its_job_once_approved() {
        use crate::remote::ShimFactory;
        use crate::tool::catalog::JobLocation;
        use crate::tool::policy::PolicyDecision;
        use std::sync::atomic::Ordering;

        for approved in [false, true] {
            let runtime = crate::tests::TestRuntime::new().await;
            let policy = crate::tests::RecordingPolicy::deciding(move |request| {
                let route = |p: &PermissionUse| p.capability == Capability::Targets;
                match approved || !request.permissions.iter().any(route) {
                    true => PolicyDecision::allow(),
                    false => PolicyDecision::refused(),
                }
            });
            let factory = ShimFactory::answering();
            let router = TargetRouter::test(&runtime.store, build(), policy.clone()).await;
            let router = router.with_connection_factory(factory.clone());
            let mut builder = ToolRegistryBuilder::default();
            let placed = |arguments: &Arguments| JobLocation {
                target: arguments
                    .get("target")
                    .and_then(|t| t.as_str()?.parse().ok()),
                workspace: None,
            };
            builder
                .register_product::<Arguments, serde_json::Value, _, _>(
                    "place",
                    "test",
                    ToolOptions::default().job_location(placed),
                    |context, _| async move {
                        let zone = context.job_target()?.facts().zone.name;
                        Ok(ToolOutput::new(serde_json::json!(zone)))
                    },
                )
                .unwrap();
            let executor = remote_executor(&runtime, builder, &router);
            let arguments = serde_json::json!({"target": "build"});
            let placed = executor.run_host(&runtime.agent, "place", arguments).await;
            match approved {
                false => {
                    let diagnostic = placed.unwrap_err().diagnostic();
                    assert!(
                        matches!(diagnostic.cause, Cause::Denied(_)),
                        "{diagnostic:?}"
                    );
                    assert_eq!(factory.starts.load(Ordering::SeqCst), 0);
                }
                true => {
                    assert_eq!(placed.unwrap().output.value, "Europe/Berlin");
                    assert_eq!(factory.starts.load(Ordering::SeqCst), 1);
                }
            }
            router.shutdown().await;
        }
    }

    #[tokio::test]
    async fn approved_remote_jobs_are_running_during_shared_connection_startup() {
        use crate::remote::ShimFactory;
        use std::sync::atomic::Ordering;

        let runtime = crate::tests::TestRuntime::new().await;
        let factory = ShimFactory::new();
        let policy = Arc::new(crate::tool::policy::AllowAll);
        let router = TargetRouter::test(&runtime.store, build(), policy).await;
        let router = router.with_connection_factory(factory.clone());
        let executor = remote_executor(&runtime, remote_tool(vec![Capability::Exec]), &router);
        let mut updates = runtime.store.observe().updates;
        let tasks: Vec<_> = (0..5)
            .map(|_| spawn_remote(&executor, &runtime.agent))
            .collect();
        // The factory never completes a handshake, so every job journaled as
        // running got there during the shared connection's startup.
        let mut running = std::collections::HashSet::new();
        while running.len() < 5 {
            let update = bounded(updates.recv()).await.unwrap();
            if let crate::session::RuntimeEvent::Record(record) = update.event
                && let SessionEvent::JobStateChanged {
                    job,
                    state: JobTransition::Running,
                } = record.event
            {
                running.insert(job);
            }
        }
        factory.wait_for_hello().await;
        assert_eq!(factory.starts.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime
                .jobs
                .cancel_all(&runtime.agent, crate::job::CancelScope::Outcome)
                .await,
            5
        );
        // Cancelled while connecting, none of them started.
        for task in tasks {
            let diagnostic = task.await.unwrap().unwrap_err().diagnostic();
            assert_eq!(diagnostic.cause, Cause::Cancelled);
            assert_eq!(diagnostic.context.effects, Effects::NotStarted);
        }
        router.shutdown().await;
    }

    /// A call cancelled while its target resolves its paths, in the flight that would
    /// start its operation with them, ends at once, cancelled, however long the target
    /// takes to answer: one that may only read changed nothing, any other may have acted.
    #[tokio::test]
    async fn a_call_cancelled_while_its_target_resolves_ends_at_once() {
        use crate::remote::ShimFactory;
        use skyhook_tools::diagnostic::RequestEffects;

        let runtime = crate::tests::TestRuntime::new().await;
        let factory = ShimFactory::answering();
        let policy = Arc::new(crate::tool::policy::AllowAll);
        let router = TargetRouter::test(&runtime.store, build(), policy).await;
        let router = router.with_connection_factory(factory.clone());
        let mut builder = ToolRegistryBuilder::default();
        crate::tool::target_tools::register(&mut builder).unwrap();
        let on_build = ExecutionLocation::named("build".parse().unwrap(), "/build".into());
        let executor = remote_executor(&runtime, builder, &router).with_location(on_build);
        let read = serde_json::json!({"path": "file"});
        let write = serde_json::json!({"path": "file", "content": "written"});
        for (tool, arguments, effects) in [
            ("read", read, Effects::NotStarted),
            ("write", write, Effects::Unknown),
        ] {
            let (calling, agent) = (executor.clone(), runtime.agent.clone());
            let call = tokio::spawn(async move { calling.run_host(&agent, tool, arguments).await });
            // The shim answers no call, so the flight never lands.
            factory.wait_for_call().await;
            let jobs = runtime.jobs.list(&runtime.agent).await.unwrap();
            let job = jobs.iter().find(|job| !job.state.is_terminal()).unwrap().id;
            runtime.jobs.cancel(job).await.unwrap();
            let diagnostic = bounded(call).await.unwrap().unwrap_err().diagnostic();
            assert_eq!(diagnostic.cause, Cause::Cancelled, "{tool}");
            assert_eq!(diagnostic.context.effects, effects, "{tool}");
            let acted = diagnostic.context.request == RequestEffects::MayHaveExecuted;
            assert_eq!(acted, tool == "write", "{tool}");
        }
        router.shutdown().await;
    }

    /// A remote call approved unasked for its paths resolving inside the workspace
    /// runs in the flight that resolves them, approved before it flew. One whose path
    /// resolves outside, as spelled or through a link, is held there, asks for that
    /// path as before, and performs where it was held once approved; one whose policy
    /// would ask resolves first.
    #[tokio::test]
    async fn remote_calls_inside_their_workspace_run_in_one_flight() {
        use crate::tool::policy::ResourceId;
        let runtime = crate::tests::TestRuntime::new().await;
        let scratch = tempfile::tempdir().unwrap();
        let scratch = std::fs::canonicalize(scratch.path()).unwrap();
        let workspace = scratch.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("inside.txt"), "inside").unwrap();
        std::fs::write(scratch.join("outside.txt"), "outside").unwrap();
        std::os::unix::fs::symlink(&scratch, workspace.join("link")).unwrap();
        let asks = |p: &PermissionUse| p.capability == Capability::Write;
        let policy = crate::tests::RecordingPolicy::asking(asks);
        let (executor, router, factory) = serving(&runtime, &workspace, policy.clone()).await;
        let on_build =
            ExecutionLocation::named("build".parse().unwrap(), crate::tests::text(&workspace));
        let remote = executor.clone().with_location(on_build);
        let read = |path: &str| serde_json::json!({"path": path, "target": "build"});
        let outside = crate::tests::text(scratch.join("outside.txt"));
        let (inside, outside, linked) = (
            read("inside.txt"),
            read(outside.as_str()),
            read("link/outside.txt"),
        );
        let write = serde_json::json!({"path": "written.txt", "content": "written"});
        let flown = &["ContainedRead"][..];
        // The route's request, then the call's, or the one it made before it flew and
        // then its own.
        for (executor, tool, arguments, served, text, requested) in [
            (&executor, "read", inside, flown, "inside", 2),
            (&executor, "read", outside, flown, "outside", 3),
            (&executor, "read", linked, flown, "outside", 3),
            (&remote, "write", write, &["Resolve", "Write"], "", 3),
        ] {
            let called = executor.run_host(&runtime.agent, tool, arguments).await;
            assert_eq!(factory.served(), served);
            let output = runtime.jobs.output(called.unwrap().job).await;
            let content = output.test_bytes("/result/content").unwrap_or_default();
            assert_eq!(content, text.as_bytes());
            let requests = std::mem::take(&mut *policy.requests.lock().unwrap());
            assert_eq!(requests.len(), requested);
            let asked = requests.last().unwrap();
            let path = |p: &PermissionUse| matches!(p.resource, ResourceId::Path { .. });
            assert_eq!(asked.permissions.iter().any(path), text == "outside");
        }
        let written = std::fs::read_to_string(workspace.join("written.txt")).unwrap();
        assert_eq!(written, "written");
        router.shutdown().await;
    }

    /// Under the host's own policy a write inside a remote target's workspace is
    /// asked about no more than one inside the root's: once its route is approved, it
    /// runs in the flight that resolves its path.
    #[tokio::test]
    async fn a_write_inside_an_approved_remote_workspace_flies_unasked() {
        use crate::interaction::{
            ApprovalReply, PromptKind, UiInteraction, policy::HostApprovalPolicy,
        };
        let runtime = crate::tests::TestRuntime::new().await;
        let scratch = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(scratch.path()).unwrap();
        let (ui, mut prompts) = UiInteraction::new();
        let policy = Arc::new(HostApprovalPolicy::Attended(ui));
        let (executor, router, factory) = serving(&runtime, &workspace, policy).await;
        let on_build =
            ExecutionLocation::named("build".parse().unwrap(), crate::tests::text(&workspace));
        let (calling, agent) = (executor.with_location(on_build), runtime.agent.clone());
        let write = serde_json::json!({"path": "written.txt", "content": "written"});
        let mut call = tokio::spawn(async move { calling.run_host(&agent, "write", write).await });
        let PromptKind::Approval { request, reply } = bounded(prompts.recv()).await.unwrap().kind
        else {
            panic!("expected the route's approval");
        };
        let route = |p: &PermissionUse| p.capability == Capability::Targets;
        assert!(request.permissions.iter().all(route));
        reply.send(Ok(ApprovalReply::Allow)).unwrap();
        let written = bounded(async {
            tokio::select! {
                written = &mut call => written.unwrap(),
                _ = prompts.recv() => panic!("the write was asked about"),
            }
        });
        written.await.unwrap();
        assert_eq!(factory.served(), ["ContainedWrite"]);
        let written = std::fs::read_to_string(workspace.join("written.txt")).unwrap();
        assert_eq!(written, "written");
        router.shutdown().await;
    }

    /// A target whose definition spells its workspace through a link runs its calls
    /// in the workspace it resolved: a path inside is inside, so the call flies and a
    /// source there needs no permission of its own.
    #[tokio::test]
    async fn a_workspace_spelled_through_a_link_holds_its_paths() {
        use crate::tool::policy::ResourceId;
        let runtime = crate::tests::TestRuntime::new().await;
        let scratch = tempfile::tempdir().unwrap();
        let scratch = std::fs::canonicalize(scratch.path()).unwrap();
        std::fs::create_dir_all(scratch.join("real/workspace")).unwrap();
        std::fs::write(scratch.join("real/workspace/inside.txt"), "inside").unwrap();
        std::os::unix::fs::symlink(scratch.join("real"), scratch.join("alias")).unwrap();
        let policy = crate::tests::RecordingPolicy::allowing();
        let spelled = scratch.join("alias/workspace");
        let (executor, router, factory) = serving(&runtime, &spelled, policy.clone()).await;
        let agent = &runtime.agent;
        let source = serde_json::json!({"path": "inside.txt", "target": "build"});
        let read = executor.run_host(agent, "read", source.clone()).await;
        assert_eq!(factory.served(), ["ContainedRead"]);
        let output = runtime.jobs.output(read.unwrap().job).await;
        assert_eq!(output.test_bytes("/result/content").unwrap(), b"inside");
        let write = serde_json::json!({"path": "copy.txt", "source": source});
        executor.run_host(agent, "write", write).await.unwrap();
        let requests = std::mem::take(&mut *policy.requests.lock().unwrap());
        let path = |p: &PermissionUse| matches!(p.resource, ResourceId::Path { .. });
        let mut requested = requests.iter().flat_map(|request| &request.permissions);
        assert!(!requested.any(path), "{requests:?}");
        router.shutdown().await;
    }

    /// A model's call its turn moves while it is still connecting returns the moved job
    /// at once; its start goes on, owning the job, and a cancellation still ends it.
    #[tokio::test]
    async fn a_call_moved_while_connecting_returns_at_once() {
        use crate::remote::ShimFactory;

        let runtime = crate::tests::TestRuntime::new().await;
        let factory = ShimFactory::new();
        let policy = Arc::new(crate::tool::policy::AllowAll);
        let router = TargetRouter::test(&runtime.store, build(), policy).await;
        let router = router.with_connection_factory(factory.clone());
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_product::<Arguments, serde_json::Value, _, _>(
                "custom_remote",
                "test",
                ToolOptions::new(vec![Capability::Exec])
                    .placement(ToolPlacement::TargetedWorkspace)
                    .background(),
                |_, _| async { panic!("remote job must not execute locally") },
            )
            .unwrap();
        let executor = remote_executor(&runtime, builder, &router);
        let (calling, agent) = (executor.clone(), runtime.agent.clone());
        let arguments = serde_json::json!({"target": "build"});
        let call = tokio::spawn(async move {
            let model = InvocationKind::Model;
            calling
                .execute_as(model, agent, "custom_remote", arguments, None)
                .await
        });
        // The factory never completes a handshake, so the call is still connecting.
        factory.wait_for_hello().await;
        let job = runtime.jobs.list(&runtime.agent).await.unwrap()[0].id;
        runtime
            .jobs
            .move_calls(vec![job], crate::job::MoveTrigger::Event)
            .await
            .unwrap();
        let moved = bounded(call).await.unwrap().unwrap();
        let view = moved.output.value;
        assert_eq!(view["state"], "running");
        let notice = crate::job::Notice::MovedOnEvent.to_string();
        assert_eq!(view["presentation"]["notice"], notice.as_str());
        runtime.jobs.cancel(job).await.unwrap();
        bounded(runtime.jobs.wait_settled(job)).await.unwrap();
        let state = runtime.jobs.snapshot(job).await.unwrap().state;
        assert_eq!(state, JobState::Cancelled);
        router.shutdown().await;
    }

    #[tokio::test]
    async fn route_approval_uses_the_job_subject_while_job_awaits_approval() {
        let runtime = crate::tests::TestRuntime::new().await;
        let (prompts, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let policy = Arc::new(crate::tests::PromptingPolicy(prompts));
        let router = TargetRouter::test(&runtime.store, build(), policy.clone()).await;
        let executor = remote_executor(&runtime, remote_tool(Vec::new()), &router);
        let running = spawn_remote(&executor, &runtime.agent);
        let (tool_request, _reply) = bounded(asked.recv()).await.unwrap();
        assert_eq!(tool_request.agent, runtime.agent);
        assert!(
            tool_request
                .permissions
                .iter()
                .any(|p| p.capability == Capability::Targets)
        );
        let arguments = &tool_request.arguments;
        assert_eq!(arguments["tool"]["target"], "build");
        assert_eq!(
            arguments["route"],
            serde_json::json!({"destination": "build", "origin": null,
                "route": [{"name": "build", "host": "build.example.com"}]})
        );
        let state = runtime.jobs.snapshot(tool_request.job).await.unwrap().state;
        assert_eq!(state, JobState::AwaitingApproval);
        runtime.jobs.cancel(tool_request.job).await.unwrap();
        let failed = bounded(running).await.unwrap().unwrap_err();
        assert_eq!(failed.diagnostic().cause, Cause::Cancelled);
        let state = runtime.jobs.snapshot(tool_request.job).await.unwrap().state;
        assert_eq!(state, JobState::Cancelled);
    }

    /// A redefinition of the target a call would connect to plans the call again:
    /// it is asked once more, about the whole call on the new definition. One
    /// landing while its approval is pending withdraws the prompt, an answer that
    /// arrives after one is refused, and one landing while it connects stops the
    /// startup. One that moves the job's journaled workspace fails, not started.
    #[tokio::test]
    async fn a_call_whose_target_is_redefined_before_it_connects_asks_again() {
        enum Redefined {
            Asking,
            /// Journaled without the router, so nothing revokes the decision.
            Answering,
            Connecting,
        }
        for (redefined, moved) in [
            (Redefined::Asking, false),
            (Redefined::Asking, true),
            (Redefined::Answering, false),
            (Redefined::Connecting, false),
            (Redefined::Connecting, true),
        ] {
            let runtime = crate::tests::TestRuntime::new().await;
            let (prompts, mut asked) = tokio::sync::mpsc::unbounded_channel();
            let policy = Arc::new(crate::tests::PromptingPolicy(prompts));
            let factory = crate::remote::ShimFactory::new();
            let router = TargetRouter::test(&runtime.store, build(), policy)
                .await
                .with_connection_factory(factory.clone());
            let executor = remote_executor(&runtime, remote_tool(Vec::new()), &router);
            let running = spawn_remote(&executor, &runtime.agent);
            let (stale, reply) = bounded(asked.recv()).await.unwrap();
            let host = |asked: &crate::tool::policy::AuthorizationRequest| {
                asked.arguments["route"]["route"][0]["host"].clone()
            };
            assert_eq!(host(&stale), "build.example.com");
            let workspace = if moved { "/moved" } else { "/build" };
            let mut replacement = crate::target::tests::definition("build", workspace, None);
            replacement.host = "replacement.example.test".into();
            let subject = runtime.subject(stale.job, crate::job::CancellationToken::new());
            match redefined {
                Redefined::Asking => {
                    router.add(replacement, &subject).await.unwrap();
                    let mut reply = reply;
                    bounded(reply.closed()).await;
                }
                Redefined::Answering => {
                    let targets = router.targets(|targets| targets.upsert(replacement));
                    let upserted = SessionEvent::TargetsUpserted {
                        targets: vec![targets.unwrap()],
                    };
                    runtime
                        .store
                        .append(runtime.agent.clone(), upserted)
                        .await
                        .unwrap();
                    reply.send(()).unwrap();
                }
                Redefined::Connecting => {
                    reply.send(()).unwrap();
                    factory.wait_for_hello().await;
                    router.add(replacement, &subject).await.unwrap();
                }
            }
            if moved {
                let error = bounded(running).await.unwrap().unwrap_err().diagnostic();
                let redefined =
                    skyhook_tools::diagnostic::Cause::Message("target was redefined".into());
                assert_eq!(error.cause, redefined);
                assert_eq!(error.context.effects, Effects::NotStarted);
                router.shutdown().await;
                continue;
            }
            let (current, reply) = bounded(asked.recv()).await.unwrap();
            assert_eq!((current.job, &current.tool), (stale.job, &stale.tool));
            assert_eq!(host(&current), "replacement.example.test");
            assert_ne!(current.permissions, stale.permissions, "a new revision");
            // It asks again awaiting approval, having run only if it was admitted
            // to connect.
            let state = runtime.jobs.snapshot(current.job).await.unwrap().state;
            assert_eq!(state, JobState::AwaitingApproval);
            let ran = runtime.store.records().into_iter().filter(|record| {
                let running = crate::job::JobTransition::Running;
                matches!(record.event, SessionEvent::JobStateChanged { state, .. } if state == running)
            });
            let connecting = matches!(redefined, Redefined::Connecting);
            assert_eq!(ran.count(), usize::from(connecting));
            reply.send(()).unwrap();
            factory.wait_for_hello().await;
            runtime.jobs.cancel(current.job).await.unwrap();
            assert!(bounded(running).await.unwrap().is_err());
            router.shutdown().await;
        }
    }

    /// Without Targets, exec keeps the SSH agent it inherited instead of the managed one.
    #[tokio::test]
    async fn exec_without_targets_keeps_the_inherited_agent() {
        let runtime = crate::tests::TestRuntime::new().await;
        let policy = Arc::new(crate::tool::policy::AllowAll);
        let router = TargetRouter::test(&runtime.store, [], policy).await;
        let mut builder = ToolRegistryBuilder::default();
        crate::tool::target_tools::exec(&mut builder).unwrap();
        let executor = runtime.routed_executor(builder, router.clone());
        let command = ["/bin/sh", "-c", r#"printf '%s' "$SSH_AUTH_SOCK""#];
        let output = executor
            .run_host(
                &runtime.agent,
                "exec",
                serde_json::json!({"command":command}),
            )
            .await
            .unwrap();
        let stdout = output.output.value["stdout"].as_str().unwrap_or_default();
        assert_eq!(stdout, std::env::var("SSH_AUTH_SOCK").unwrap_or_default());
        router.shutdown().await;
    }
}
