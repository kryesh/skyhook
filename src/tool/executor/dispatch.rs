//! Start local or remote jobs after planning and authorization.

use super::*;

impl ToolExecutor {
    /// Authorize, prepare and launch one owned invocation. Nothing here is
    /// Clone: reusable transport connections do not make authority reusable.
    pub(super) async fn start(
        &self,
        plan: InvocationPlan,
    ) -> Result<StartedExecution, ExecutionError> {
        let spec = JobSpec {
            role: plan.tool.job_role(),
            origin: plan.origin.clone(),
            agent: plan.agent.clone(),
            parent: plan.parent,
            tool: plan.tool.name().to_owned(),
            name: plan.job_name.clone(),
            arguments: plan.original_arguments.clone(),
            output_schema: plan.tool.output_schema(&self.capabilities),
            accepts_input: plan.tool.accepts_input(),
            background: plan.background,
            authorization_scope: plan.authorization_scope,
            location: plan.execution_location.clone(),
        };
        let mut lease = self.shared.jobs.create(spec).await?;
        if lease.cancellation_token().is_cancelled() {
            return Err(self.cancelled(lease).await);
        }
        self.shared
            .jobs
            .transition(lease.id(), JobState::AwaitingApproval)
            .await?;
        let subject = AuthorizationSubject {
            agent: plan.agent.clone(),
            job: lease.id(),
            parent: plan.parent,
            capabilities: self.capabilities.clone(),
            cancellation: lease.cancellation_token(),
        };
        // Admission-time authorization snapshot: approval belongs to this
        // exact owned subject/arguments/permission plan, not later revocations.
        if let Err(error) = self
            .shared
            .authorization
            .authorize(
                &subject,
                plan.tool.name().to_owned(),
                plan.permissions.clone(),
                plan.authorization_arguments.clone(),
            )
            .await
        {
            let error = match error {
                AuthorizationError::Cancelled => return Err(self.cancelled(lease).await),
                AuthorizationError::Denied(reason) => ExecutionError::Denied(reason),
                AuthorizationError::InvalidGrant(_) => {
                    ToolError::Failed("operation could not be started".to_owned()).into()
                }
                AuthorizationError::Unavailable => {
                    ExecutionError::UnavailableTool(plan.tool.name().to_owned())
                }
            }
            .contextualize(
                DiagnosticContext::new(
                    Operation::Authorize,
                    Subject::Tool(plan.tool.name().to_owned()),
                )
                .at(FailureSite::Host)
                .effects(Effects::NotStarted),
                &self.capabilities,
            );
            return Err(self.fail_start(lease, error).await);
        }
        // Route preparation may reauthorize a changed route, and seals its own
        // admission snapshot under the router mutation gate. No gate is held
        // across physical tool IO or promises execution-time freshness.
        self.shared
            .jobs
            .transition(lease.id(), JobState::Running)
            .await?;
        let dispatch = match plan.dispatch {
            InvocationDispatch::Local(admitted) => InvocationDispatch::Local(admitted),
            InvocationDispatch::ReadError(output) => InvocationDispatch::ReadError(output),
            InvocationDispatch::Remote { remote, arguments } => {
                match remote
                    .router
                    .prepare(remote.route, &plan.execution_location.workspace, &subject)
                    .await
                {
                    Ok(connection) => InvocationDispatch::Remote {
                        remote: connection,
                        arguments,
                    },
                    Err(RemoteError::Cancelled) => return Err(self.cancelled(lease).await),
                    Err(error) => {
                        let error = ExecutionError::Tool(error.into_tool_error()).contextualize(
                            DiagnosticContext::new(
                                Operation::Connect,
                                Subject::Tool(plan.tool.name().to_owned()),
                            )
                            .at(FailureSite::Host),
                            &self.capabilities,
                        );
                        return Err(self.fail_start(lease, error).await);
                    }
                }
            }
        };
        if lease.cancellation_token().is_cancelled() {
            return Err(self.cancelled(lease).await);
        }
        let tool = plan.tool;
        let mut context = ToolContext::new(
            subject,
            plan.execution_location,
            plan.caller_location,
            lease.take_input(),
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
        lease
            .start_supervised(async move {
                let cancellation = context.cancellation_token();
                let mut fallback = DiagnosticContext::new(
                    Operation::Execute,
                    Subject::Tool(tool.name().to_owned()),
                )
                .at(FailureSite::bound(
                    context.execution_location(),
                    tool.placement() == ToolPlacement::Host,
                ));
                fallback.paths = plan.path_facts;
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
                        return Err(ToolError::Cancelled.effects(Effects::NotStarted));
                    }
                    match dispatch {
                        InvocationDispatch::Local(admitted) => match admitted {
                            Ok(admitted) => admitted.call(context).await,
                            Err(error) => Err(error.into()),
                        },
                        InvocationDispatch::ReadError(output) => Ok(*output),
                        InvocationDispatch::Remote {
                            remote: connection,
                            arguments,
                        } => connection
                            .execute(tool.name().to_owned(), arguments, &context)
                            .await
                            .map_err(crate::remote::RemoteError::into_tool_error),
                    }
                }
                .await;
                let result = result.map(|mut output| {
                    if let Some(diagnostic) = &mut output.diagnostic {
                        diagnostic.context.fallback(fallback.clone());
                    }
                    output
                });
                cancellation_result(result, cancellation.is_cancelled())
                    .map_err(|error| error.fallback_context(fallback))
            })
            .await?;
        Ok(StartedExecution {
            job,
            background: plan.background,
        })
    }

    async fn cancelled(&self, lease: crate::job::JobLease) -> ExecutionError {
        lease.fail(ToolError::Cancelled.into()).await;
        ExecutionError::Tool(ToolError::Cancelled)
    }

    async fn fail_start(
        &self,
        lease: crate::job::JobLease,
        error: ExecutionError,
    ) -> ExecutionError {
        let outcome = ToolError::from_diagnostic(error.diagnostic(), None).into();
        lease.fail(outcome).await;
        error
    }
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
        Err(error) => error.into_parts(),
        Ok(output) => {
            // A completed read error remains data within the cancelled outcome.
            // Its marker renders that payload; the outer cause describes cancellation.
            let context = output
                .diagnostic
                .as_ref()
                .map_or_else(DiagnosticContext::default, |diagnostic| {
                    diagnostic.context.clone()
                });
            (Diagnostic::new(context, Cause::Cancelled), Some(output))
        }
    };
    diagnostic.cause = Cause::Cancelled;
    if diagnostic.context.effects == Effects::Unknown {
        diagnostic.context.effects = Effects::MayHaveExecuted;
    }
    Err(ToolError::from_diagnostic(diagnostic, output.map(Box::new)))
}

#[cfg(test)]
mod tests {
    use super::super::planning::tests::router;
    use super::*;
    use crate::{
        target::{TargetDefinition, TargetRegistry},
        tool::{
            ToolOptions, ToolRegistryBuilder,
            policy::{AuthorizationRequest, PolicyDecision, PolicyFuture},
        },
    };

    struct BlockingRoutePolicy {
        requests: std::sync::Mutex<Vec<AuthorizationRequest>>,
        release: tokio::sync::Notify,
    }

    impl Policy for BlockingRoutePolicy {
        fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
            let route = request
                .permissions
                .iter()
                .any(|p| p.capability == Capability::Targets);
            let grants = request
                .permissions
                .iter()
                .filter_map(|p| p.proposed_grant.clone())
                .collect();
            self.requests.lock().unwrap().push(request);
            Box::pin(async move {
                if route {
                    self.release.notified().await;
                }
                PolicyDecision::Allow { grants }
            })
        }
    }

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

    fn targets() -> TargetRegistry {
        TargetRegistry::from_definitions([TargetDefinition::test("build", "/build", None)]).unwrap()
    }

    fn spawn_remote(
        executor: &ToolExecutor,
        agent: &AgentId,
    ) -> tokio::task::JoinHandle<Result<ExecutionResult, ExecutionError>> {
        let (executor, agent) = (executor.clone(), agent.clone());
        let arguments = serde_json::json!({"target":"build"});
        tokio::spawn(async move { executor.run_host(&agent, "custom_remote", arguments).await })
    }

    #[test]
    fn cancellation_retains_partial_output_and_failure_context() {
        use crate::tool::diagnostic::{Cause, Effects, Operation, Subject};

        for error in [
            ToolError::Cancelled,
            ToolError::Failed("transport closed".into()),
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
        use std::time::Duration;

        let runtime = crate::tests::TestRuntime::new().await;
        let (entered, mut started) = tokio::sync::mpsc::unbounded_channel();
        let mut read_diagnostic =
            ToolError::source_filesystem_io(std::io::ErrorKind::NotFound.into())
                .operation(Operation::Read, Subject::path("missing"))
                .effects(Effects::Unchanged)
                .diagnostic();
        let output = ToolOutput::new(serde_json::json!({
            "kind":"error", "path":"missing", "error":{"code":"not_found", "message":""}
        }))
        .with_diagnostic(read_diagnostic.clone());
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
        let job = tokio::time::timeout(Duration::from_secs(5), started.recv())
            .await
            .expect("handler must enter actual dispatch")
            .unwrap();
        runtime.jobs.cancel(job).await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("cooperative cancellation must finish")
            .unwrap()
            .unwrap_err();
        let (diagnostic, output) = error.into_tool_error().into_parts();
        read_diagnostic.context.site =
            FailureSite::Execution(ExecutionLocation::root(runtime.root.path().to_owned()));
        assert_eq!(diagnostic.cause, Cause::Cancelled);
        assert_eq!(diagnostic.context, read_diagnostic.context);

        // The completed read error stays data inside the cancelled outcome.
        let saved = runtime.jobs.snapshot(job).await.unwrap();
        assert_eq!(saved.state, JobState::Cancelled);
        assert_eq!(saved.diagnostic.as_ref(), Some(&diagnostic));
        assert_eq!(saved.output_diagnostic.as_ref(), Some(&read_diagnostic));
        assert!(output.is_some());
    }

    #[tokio::test]
    async fn approved_remote_jobs_are_running_during_shared_connection_startup() {
        use crate::remote::PendingHandshakeFactory;
        use std::sync::atomic::Ordering;

        let runtime = crate::tests::TestRuntime::new().await;
        let authorization = AuthorizationCoordinator::new(Arc::new(crate::tool::policy::AllowAll));
        let factory = PendingHandshakeFactory::new();
        let remote = crate::remote::RemoteManager::new(
            crate::remote::EmbeddedShimCatalog::default(),
            Arc::new(crate::remote::RejectSensitivePrompts),
            authorization.clone(),
        )
        .with_connection_factory(factory.clone());
        let router = TargetRouter::new(targets(), remote.clone(), authorization);
        let executor = remote_executor(
            runtime.executor(remote_tool(vec![Capability::Exec])),
            router,
        );
        let tasks: Vec<_> = (0..5)
            .map(|_| spawn_remote(&executor, &runtime.agent))
            .collect();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let jobs = runtime.jobs.list(&runtime.agent).await;
                if jobs.len() == 5
                    && jobs.iter().all(|job| job.state == JobState::Running)
                    && factory.starts.load(Ordering::SeqCst) == 1
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("approved jobs must not appear to await approval during connection startup");
        assert_eq!(runtime.jobs.cancel_all(&runtime.agent).await, 5);
        for task in tasks {
            assert!(task.await.unwrap().is_err());
        }
        remote.shutdown().await;
    }

    #[tokio::test]
    async fn route_approval_uses_the_job_subject_while_job_awaits_approval() {
        let runtime = crate::tests::TestRuntime::new().await;
        let policy = Arc::new(BlockingRoutePolicy {
            requests: std::sync::Mutex::new(Vec::new()),
            release: tokio::sync::Notify::new(),
        });
        let executor = runtime.executor_with_policy(remote_tool(Vec::new()), policy.clone());
        let executor = remote_executor(executor, router(targets(), policy.clone()));
        let running = spawn_remote(&executor, &runtime.agent);
        while policy.requests.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        let tool_request = policy.requests.lock().unwrap()[0].clone();
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
        let state = runtime.jobs.snapshot(tool_request.job).await.unwrap().state;
        assert_eq!(state, JobState::AwaitingApproval);
        runtime.jobs.cancel(tool_request.job).await.unwrap();
        assert!(running.await.unwrap().is_err());
    }
}
