//! Start local or remote jobs after planning and authorization.

use futures_util::future::BoxFuture;

use super::*;
use crate::{remote::PreparedConnection, tool::source::Source};

impl ToolExecutor {
    pub(super) fn job_spec(&self, plan: &InvocationPlan) -> JobSpec {
        JobSpec {
            role: plan.tool.job_role(),
            origin: plan.origin.clone(),
            agent: plan.agent.clone(),
            parent: plan.parent,
            tool: plan.tool.name().to_owned(),
            name: plan.launch.name.clone(),
            arguments: Value::Object(plan.original_arguments.clone()),
            output_schema: match plan.result_policy {
                crate::tool::ToolResultPolicy::Value => plan.tool.output_schema(&self.capabilities),
                crate::tool::ToolResultPolicy::Nothing => None,
                crate::tool::ToolResultPolicy::JobView => {
                    Some(crate::job::JOB_VIEW_SCHEMAS.one.clone())
                }
            },
            accepts_input: plan.tool.accepts_input(),
            background: plan.launch.background,
            location: plan.job_location.clone(),
        }
    }

    /// Authorize, prepare and launch one owned invocation. Nothing here is
    /// Clone: reusable transport connections do not make authority reusable.
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

    /// Approve, connect and launch `plan`, whose job awaits approval. A
    /// redefinition of a target the call names, revoking its decision or
    /// replacing its route before it connects, plans the whole call once more,
    /// so the policy is asked about the current definitions, never just to
    /// connect. Nothing is held across physical tool IO or promises
    /// execution-time freshness.
    async fn admit(
        &self,
        mut plan: InvocationPlan,
        mut lease: crate::job::JobLease<crate::job::stage::Approving>,
    ) -> Result<StartedExecution, ToolError> {
        let result_policy = plan.result_policy;
        let subject = AuthorizationSubject {
            agent: plan.agent.clone(),
            job: lease.id(),
            parent: plan.parent,
            capabilities: self.capabilities.clone(),
            cancellation: lease.cancellation_token(),
        };
        let mut replanned = false;
        let (lease, dispatch, source) = loop {
            // A redefinition before it connects hands back the approving lease.
            let approving = match self.approve(&subject, &plan).await {
                Ok(()) => {
                    let running = lease.run().await.map_err(job_error)?;
                    let workspace = &plan.execution_location.workspace;
                    match connect(plan.dispatch, plan.source, workspace, &subject).await {
                        Ok((dispatch, source)) => break (running, dispatch, source),
                        Err(RemoteError::Authorization(AuthorizationError::TargetRedefined))
                            if !replanned =>
                        {
                            running.reapprove().await.map_err(job_error)?
                        }
                        Err(RemoteError::Cancelled) => return Err(self.cancelled(running).await),
                        Err(error) => {
                            let error = error.into_tool_error().or(PartialContext::new(
                                Operation::Connect,
                                Subject::Tool(plan.tool.name().to_owned()),
                            )
                            .at(FailureSite::Host));
                            return Err(self.fail_start(running, error).await);
                        }
                    }
                }
                Err(AuthorizationError::TargetRedefined) if !replanned => lease,
                Err(error) => return Err(self.fail_start(lease, refusal(&plan, error)).await),
            };
            replanned = true;
            lease = approving;
            plan = match self.replan(subject.job).await {
                Ok(plan) => plan,
                Err(error) => return Err(self.fail_start(lease, error).await),
            };
        };
        if lease.cancellation_token().is_cancelled() {
            return Err(self.cancelled(lease).await);
        }
        let tool = plan.tool;
        let (input, worker) = lease.split();
        let mut context = ToolContext::new(
            subject,
            plan.execution_location,
            plan.caller_location,
            input,
            self.shared.jobs.clone(),
        )
        .with_invocation_authority(
            self.shared.authorization.clone(),
            tool.name().to_owned(),
            plan.authorization_arguments,
        );
        let authentication = if context.capabilities().contains(Capability::Targets)
            && context.execution_location().is_root()
            && tool.target_authentication()
        {
            self.shared.router.clone()
        } else {
            None
        };
        let job = context.job();
        // The handle shows the job as launched, before its worker can finish it.
        let started = if plan.launch.background {
            let launched = self.shared.jobs.metadata(job).map_err(job_error)?;
            StartedExecution::Background(Box::new(launched))
        } else {
            StartedExecution::Foreground(job)
        };
        worker.launch(
            async move {
                let cancellation = context.cancellation_token();
                let fallback =
                    PartialContext::new(Operation::Execute, Subject::Tool(tool.name().to_owned()))
                        .at(FailureSite::bound(
                            context.execution_location(),
                            tool.placement(),
                        ))
                        .paths(plan.path_facts);
                let result = async {
                    if let Some(router) = authentication {
                        context.process_environment.extend(
                            router
                                .environment()
                                .await
                                .map_err(|e| e.into_tool_error())?,
                        );
                    }
                    if context.is_cancelled() {
                        return Err(ToolError::cancelled().effects(Effects::NotStarted));
                    }
                    let source = open_source(source, tool.name(), &context).await?;
                    run(dispatch, tool.name(), context.with_source(source)).await
                }
                .await;
                let result = crate::tool::output::or_fallback(result, fallback.clone());
                let outcome = crate::job::JobOutcome::from(
                    cancellation_result(result, cancellation.is_cancelled())
                        .map_err(|error| error.or(fallback)),
                );
                match result_policy {
                    crate::tool::ToolResultPolicy::Nothing => outcome.without_result(),
                    _ => outcome,
                }
            },
            "tool handler panicked",
        );
        Ok(started)
    }

    /// Authorize `plan` as it stands.
    fn approve<'a>(
        &'a self,
        subject: &'a AuthorizationSubject,
        plan: &InvocationPlan,
    ) -> impl Future<Output = Result<(), AuthorizationError>> + Send + 'a {
        let arguments = plan.authorization_arguments.document(None);
        let (tool, permissions) = (plan.tool.name().to_owned(), plan.permissions.clone());
        (self.shared.authorization).authorize(subject, tool, permissions, arguments, &[])
    }

    /// Plan job `id`'s call again against the current targets, as it was
    /// journaled and admitted once, unless the location its job journaled no
    /// longer holds.
    async fn replan(&self, id: JobId) -> Result<InvocationPlan, ToolError> {
        let call = self.shared.jobs.metadata(id).map_err(job_error)?;
        let (agent, tool, parent) = (call.agent, call.tool, call.parent);
        let arguments = std::sync::Arc::unwrap_or_clone(call.arguments);
        let planned = self.plan_registered(InvocationKind::Host, agent, &tool, arguments, parent);
        let plan = planned
            .await
            .map_err(|error| error.effects(Effects::NotStarted))?;
        if plan.job_location != call.location {
            return Err(refusal(&plan, AuthorizationError::TargetRedefined));
        }
        Ok(plan)
    }

    async fn cancelled<S>(&self, lease: crate::job::JobLease<S>) -> ToolError {
        lease.fail(ToolError::cancelled().into()).await;
        ToolError::cancelled()
    }

    async fn fail_start<S>(&self, lease: crate::job::JobLease<S>, error: ToolError) -> ToolError {
        lease
            .fail(ToolError::from_facts(error.facts().clone(), None).into())
            .await;
        error
    }
}

/// How a call reports an authorization that refused it before it started.
fn refusal(plan: &InvocationPlan, error: AuthorizationError) -> ToolError {
    if let AuthorizationError::Cancelled = error {
        return ToolError::cancelled();
    }
    ToolError::from(crate::tool::AdmissionError::from(error)).or(PartialContext::new(
        Operation::Authorize,
        Subject::Tool(plan.tool.name().to_owned()),
    )
    .at(FailureSite::Host)
    .effects(Effects::NotStarted))
}

/// Connect the call's source, then its own dispatch: a failed source connection
/// stops before the tool's own connection opens.
async fn connect(
    dispatch: InvocationDispatch<PlannedRemote>,
    source: Option<SourcePlan<PlannedRemote>>,
    workspace: &std::path::Path,
    subject: &AuthorizationSubject,
) -> Result<
    (
        InvocationDispatch<PreparedConnection>,
        Option<SourcePlan<PreparedConnection>>,
    ),
    RemoteError,
> {
    let source = match source {
        None => None,
        Some(SourcePlan::Local { path, location }) => Some(SourcePlan::Local { path, location }),
        Some(SourcePlan::Remote {
            remote,
            workspace,
            path,
        }) => Some(SourcePlan::Remote {
            remote: remote
                .router
                .prepare(remote.route, &workspace, subject)
                .await?,
            workspace,
            path,
        }),
    };
    let dispatch = match dispatch {
        InvocationDispatch::Local(admitted) => InvocationDispatch::Local(admitted),
        InvocationDispatch::ReadError(output) => InvocationDispatch::ReadError(output),
        InvocationDispatch::Remote { remote, arguments } => InvocationDispatch::Remote {
            remote: remote
                .router
                .prepare(remote.route, workspace, subject)
                .await?,
            arguments,
        },
    };
    Ok((dispatch, source))
}

/// Boxed, like `run`, to keep the executor's future types shallow.
fn open_source<'a>(
    source: Option<SourcePlan<PreparedConnection>>,
    tool: &str,
    context: &'a ToolContext,
) -> BoxFuture<'a, Result<Option<Source>, ToolError>> {
    let tool = tool.to_owned();
    Box::pin(async move {
        Ok(Some(match source {
            None => return Ok(None),
            Some(SourcePlan::Local { path, location }) => {
                Source::open(&path).await.map_err(|error| {
                    ToolError::from(error)
                        .or(PartialContext::default().at(FailureSite::Execution(location)))
                })?
            }
            Some(SourcePlan::Remote { remote, path, .. }) => remote
                .read_source(tool, path, context)
                .await
                .map_err(RemoteError::into_tool_error)?,
        }))
    })
}

/// Boxed so the executor's future types stay shallow enough for Send inference.
fn run(
    dispatch: InvocationDispatch<PreparedConnection>,
    tool: &str,
    context: ToolContext,
) -> BoxFuture<'static, Result<ToolOutput, ToolError>> {
    let tool = tool.to_owned();
    Box::pin(async move {
        match dispatch {
            InvocationDispatch::Local(admitted) => admitted.call(context).await,
            InvocationDispatch::ReadError(output) => Ok(*output),
            InvocationDispatch::Remote {
                remote: connection,
                arguments,
            } => connection
                .execute(tool, Value::Object(arguments), &context)
                .await
                .map_err(RemoteError::into_tool_error),
        }
    })
}

/// Cancellation determines the outcome, not whether already observed effects or
/// output happened. Apply its precedence once, before binding failure facts.
fn cancellation_result(
    result: Result<ToolOutput, ToolError>,
    cancelled: bool,
) -> Result<ToolOutput, ToolError> {
    if !cancelled {
        return result;
    }
    let (mut diagnostic, output) = match result {
        Err(error) => error.into_facts(),
        Ok(output) => {
            // A completed read error remains data within the cancelled outcome.
            // Its marker renders that payload; the outer cause describes cancellation.
            let context = output
                .diagnostic
                .as_ref()
                .map_or_else(PartialContext::default, |diagnostic| {
                    diagnostic.context.clone()
                });
            (
                PartialDiagnostic::new(context, Cause::Cancelled),
                Some(output),
            )
        }
    };
    diagnostic.cause = Cause::Cancelled;
    let diagnostic = diagnostic.or(PartialContext::default().effects(Effects::MayHaveExecuted));
    Err(ToolError::from_facts(diagnostic, output))
}

#[cfg(test)]
mod tests {
    use super::super::planning::tests::build;
    use super::*;
    use crate::{
        job::JobTransition,
        session::SessionEvent,
        target::TargetDefinition,
        tests::bounded,
        tool::{ToolOptions, ToolRegistryBuilder},
    };

    fn remote_executor(executor: ToolExecutor, router: TargetRouter) -> ToolExecutor {
        let mut capabilities = CapabilitySet::default();
        capabilities.insert(Capability::Targets);
        executor
            .with_target_router(router)
            .with_capabilities(capabilities)
    }

    fn remote_tool(capabilities: Vec<Capability>) -> ToolRegistryBuilder {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_dynamic(
                "custom_remote",
                "test",
                serde_json::json!({"type":"object","properties":{}}),
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

    #[test]
    fn cancellation_retains_partial_output_and_failure_context() {
        use crate::tool::diagnostic::{Cause, Effects, Operation, Subject};

        for error in [
            ToolError::cancelled(),
            ToolError::failed("transport closed"),
        ] {
            let error = error
                .operation(Operation::Receive, Subject::Process)
                .effects(Effects::MayHaveExecuted)
                .with_result(ToolOutput::new(serde_json::json!({"partial":true})));
            let context = error.diagnostic().context;
            let (diagnostic, output) = cancellation_result(Err(error), true)
                .unwrap_err()
                .into_parts();
            assert_eq!(diagnostic.cause, Cause::Cancelled);
            assert_eq!(diagnostic.context, context);
            assert_eq!(output.unwrap().value, serde_json::json!({"partial":true}));
        }
    }

    #[tokio::test]
    async fn cancelled_dispatch_keeps_a_completed_read_error_as_output() {
        use crate::tool::diagnostic::{Cause, Effects, Operation, Subject};

        let runtime = crate::tests::TestRuntime::new().await;
        let (entered, mut started) = tokio::sync::mpsc::unbounded_channel();
        let read_facts = ToolError::source_filesystem_io(std::io::ErrorKind::NotFound.into())
            .operation(Operation::Read, Subject::path("missing"))
            .effects(Effects::Unchanged)
            .into_facts()
            .0;
        let output = ToolOutput::new(serde_json::json!({
            "kind":"error", "path":"missing", "error":{"code":"not_found", "message":""}
        }))
        .with_diagnostic(read_facts.clone());
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_dynamic(
                "read_error",
                "gated completion",
                serde_json::json!({"type":"object","properties":{}}),
                ToolOptions::default().placement(ToolPlacement::InheritWorkspace),
                move |invocation, _| {
                    let entered = entered.clone();
                    let output = output.clone();
                    async move {
                        entered.send(invocation.job()).unwrap();
                        invocation.cancellation_token().cancelled().await;
                        Ok(output)
                    }
                },
            )
            .unwrap();
        let executor = runtime.executor(builder);
        let agent = runtime.agent.clone();
        let running = tokio::spawn(async move {
            executor
                .run_host(&agent, "read_error", serde_json::json!({}))
                .await
        });
        let job = bounded(started.recv()).await.unwrap();
        runtime.jobs.cancel(job).await.unwrap();
        let error = bounded(running).await.unwrap().unwrap_err();
        let (diagnostic, output) = error.into_parts();
        // The dispatch boundary binds the site the read left unset.
        let read_diagnostic = read_facts
            .or(
                PartialContext::default().at(FailureSite::Execution(ExecutionLocation::root(
                    runtime.root.path().to_owned(),
                ))),
            )
            .resolve();
        assert_eq!(diagnostic.cause, Cause::Cancelled);
        assert_eq!(diagnostic.context, read_diagnostic.context);

        // The completed read error stays data inside the cancelled outcome.
        let saved = runtime.jobs.snapshot(job).unwrap();
        assert_eq!(saved.state, JobState::Cancelled);
        assert_eq!(saved.diagnostic.as_ref(), Some(&diagnostic));
        assert_eq!(saved.output_diagnostic.as_ref(), Some(&read_diagnostic));
        assert!(output.is_some());
    }

    /// A unit tool has no result, even when cancelled after completing or when
    /// its failure carries output.
    #[tokio::test]
    async fn unit_calls_never_have_a_result() {
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        struct Unit {
            fail: bool,
        }
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
        let jobs = runtime.jobs.list(&runtime.agent);
        assert_eq!(jobs.len(), 2);
        for job in jobs {
            assert_eq!(
                runtime.jobs.test_snapshot(job.id).await.unwrap().output,
                None
            );
        }
    }

    #[tokio::test]
    async fn approved_remote_jobs_are_running_during_shared_connection_startup() {
        use crate::remote::PendingHandshakeFactory;
        use std::sync::atomic::Ordering;

        let runtime = crate::tests::TestRuntime::new().await;
        let factory = PendingHandshakeFactory::new();
        let policy = Arc::new(crate::tool::policy::AllowAll);
        let router = TargetRouter::test(&runtime.store, build(), policy).await;
        let router = router.with_connection_factory(factory.clone());
        let executor = remote_executor(
            runtime.executor(remote_tool(vec![Capability::Exec])),
            router.clone(),
        );
        let mut updates = runtime.store.observe().updates;
        let tasks: Vec<_> = (0..5)
            .map(|_| spawn_remote(&executor, &runtime.agent))
            .collect();
        // The factory never completes a handshake, so every job journaled as
        // running got there during the shared connection's startup.
        let mut running = std::collections::HashSet::new();
        while running.len() < 5 {
            let update = bounded(updates.recv()).await.unwrap();
            if let crate::agent::RuntimeEvent::Record(record) = update.event
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
        for task in tasks {
            assert!(task.await.unwrap().is_err());
        }
        router.shutdown().await;
    }

    #[tokio::test]
    async fn route_approval_uses_the_job_subject_while_job_awaits_approval() {
        let runtime = crate::tests::TestRuntime::new().await;
        let (prompts, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let policy = Arc::new(crate::tests::PromptingPolicy(prompts));
        let executor = runtime.executor_with_policy(remote_tool(Vec::new()), policy.clone());
        let router = TargetRouter::test(&runtime.store, build(), policy.clone()).await;
        let executor = remote_executor(executor, router);
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
            (
                &arguments["route"]["destination"],
                &arguments["route"]["route"][0]
            ),
            (&"build".into(), &"build".into())
        );
        assert!(arguments.get("workspace").is_none());
        let state = runtime.jobs.snapshot(tool_request.job).unwrap().state;
        assert_eq!(state, JobState::AwaitingApproval);
        runtime.jobs.cancel(tool_request.job).await.unwrap();
        assert!(running.await.unwrap().is_err());
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
            let factory = crate::remote::PendingHandshakeFactory::new();
            let router = TargetRouter::test(&runtime.store, build(), policy)
                .await
                .with_connection_factory(factory.clone());
            let registry = remote_tool(Vec::new()).build();
            let (jobs, workspace) = (runtime.jobs.clone(), runtime.root.path().to_owned());
            let authorization = router.authorization().clone();
            let executor =
                ToolExecutor::with_authorization(registry, authorization, jobs, workspace);
            let running = spawn_remote(&remote_executor(executor, router.clone()), &runtime.agent);
            let (stale, reply) = bounded(asked.recv()).await.unwrap();
            assert_eq!(stale.arguments["route"]["host"], "build.example.com");
            let workspace = if moved { "/moved" } else { "/build" };
            let mut replacement = TargetDefinition::test("build", workspace, None);
            replacement.host = "replacement.example.test".into();
            let subject = runtime.subject(stale.job, crate::job::CancellationToken::new());
            match redefined {
                Redefined::Asking => {
                    router.add(replacement, &subject).await.unwrap();
                    let mut reply = reply;
                    bounded(reply.closed()).await;
                }
                Redefined::Answering => {
                    let targets = router.targets(|targets| targets.upsert(vec![replacement]));
                    let upserted = SessionEvent::TargetsUpserted {
                        targets: targets.unwrap(),
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
                    crate::tool::diagnostic::Cause::Message("target was redefined".into());
                assert_eq!(error.cause, redefined);
                assert_eq!(error.context.effects, Effects::NotStarted);
                assert!(asked.try_recv().is_err());
                router.shutdown().await;
                continue;
            }
            let (current, reply) = bounded(asked.recv()).await.unwrap();
            assert_eq!((current.job, &current.tool), (stale.job, &stale.tool));
            let host = &current.arguments["route"]["host"];
            assert_eq!(host, "replacement.example.test");
            assert_ne!(current.permissions, stale.permissions, "a new revision");
            // It asks again awaiting approval, having run only if it was admitted
            // to connect.
            let state = runtime.jobs.snapshot(current.job).unwrap().state;
            assert_eq!(state, JobState::AwaitingApproval);
            let ran = runtime.store.records().into_iter().filter(|record| {
                let running = crate::job::JobTransition::Running;
                matches!(record.event, SessionEvent::JobStateChanged { state, .. } if state == running)
            });
            let connecting = matches!(redefined, Redefined::Connecting);
            assert_eq!(ran.count(), usize::from(connecting));
            reply.send(()).unwrap();
            factory.wait_for_hello().await;
            assert!(asked.try_recv().is_err());
            runtime.jobs.cancel(current.job).await.unwrap();
            assert!(bounded(running).await.unwrap().is_err());
            router.shutdown().await;
        }
    }
}
