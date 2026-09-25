//! Start local or remote jobs after planning and authorization.

use futures_util::future::{BoxFuture, OptionFuture};

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
            arguments: plan.original_arguments.clone(),
            output_schema: match plan.result_policy() {
                crate::tool::ToolResultPolicy::Value => plan.tool.output_schema(&self.capabilities),
                crate::tool::ToolResultPolicy::JobView => {
                    Some(crate::job::presented_job_schema(false))
                }
            },
            accepts_input: plan.tool.accepts_input(),
            background: plan.launch.background,
            location: plan.execution_location.clone(),
        }
    }

    /// Authorize, prepare and launch one owned invocation. Nothing here is
    /// Clone: reusable transport connections do not make authority reusable.
    pub(super) async fn start(
        &self,
        plan: InvocationPlan,
        lease: crate::job::JobLease,
    ) -> Result<StartedExecution, ExecutionError> {
        if lease.cancellation_token().is_cancelled() {
            return Err(self.cancelled(lease).await);
        }
        let lease = lease.await_approval().await?;
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
            if let AuthorizationError::Cancelled = error {
                return Err(self.cancelled(lease).await);
            }
            let error = ExecutionError::from(crate::tool::AdmissionError::from(error)).or(
                PartialContext::new(
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
        let lease = lease.run().await?;
        // A failed source connection stops before the tool's own connection opens.
        let connected =
            match OptionFuture::from(plan.source.map(|source| connect_source(source, &subject)))
                .await
                .transpose()
            {
                Ok(source) => connect(plan.dispatch, &plan.execution_location.workspace, &subject)
                    .await
                    .map(|dispatch| (dispatch, source)),
                Err(error) => Err(error),
            };
        let (dispatch, source) = match connected {
            Ok(connected) => connected,
            Err(RemoteError::Cancelled) => return Err(self.cancelled(lease).await),
            Err(error) => {
                let error = ExecutionError::Tool(error.into_tool_error()).or(
                    PartialContext::new(
                        Operation::Connect,
                        Subject::Tool(plan.tool.name().to_owned()),
                    )
                    .at(FailureSite::Host),
                    &self.capabilities,
                );
                return Err(self.fail_start(lease, error).await);
            }
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
        worker
            .start_supervised(async move {
                let cancellation = context.cancellation_token();
                let fallback =
                    PartialContext::new(Operation::Execute, Subject::Tool(tool.name().to_owned()))
                        .at(FailureSite::bound(
                            context.execution_location(),
                            tool.placement() == ToolPlacement::Host,
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
                let result = result.map(|mut output| {
                    output.diagnostic = output.diagnostic.map(|d| d.or(fallback.clone()));
                    output
                });
                cancellation_result(result, cancellation.is_cancelled())
                    .map_err(|error| error.or(fallback))
            })
            .await?;
        Ok(StartedExecution {
            job,
            background: plan.launch.background,
        })
    }

    async fn cancelled<S>(&self, lease: crate::job::JobLease<S>) -> ExecutionError {
        lease.fail(ToolError::cancelled().into()).await;
        ExecutionError::Tool(ToolError::cancelled())
    }

    async fn fail_start<S>(
        &self,
        lease: crate::job::JobLease<S>,
        error: ExecutionError,
    ) -> ExecutionError {
        lease
            .fail(ToolError::from_facts(error.facts(), None).into())
            .await;
        error
    }
}

async fn connect(
    dispatch: InvocationDispatch<PlannedRemote>,
    workspace: &std::path::Path,
    subject: &AuthorizationSubject,
) -> Result<InvocationDispatch<PreparedConnection>, RemoteError> {
    Ok(match dispatch {
        InvocationDispatch::Local(admitted) => InvocationDispatch::Local(admitted),
        InvocationDispatch::ReadError(output) => InvocationDispatch::ReadError(output),
        InvocationDispatch::Remote { remote, arguments } => InvocationDispatch::Remote {
            remote: remote
                .router
                .prepare(remote.route, workspace, subject)
                .await?,
            arguments,
        },
    })
}

async fn connect_source(
    source: SourcePlan<PlannedRemote>,
    subject: &AuthorizationSubject,
) -> Result<SourcePlan<PreparedConnection>, RemoteError> {
    Ok(match source {
        SourcePlan::Local { path, location } => SourcePlan::Local { path, location },
        SourcePlan::Remote {
            remote,
            workspace,
            path,
        } => SourcePlan::Remote {
            remote: remote
                .router
                .prepare(remote.route, &workspace, subject)
                .await?,
            workspace,
            path,
        },
    })
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
            InvocationDispatch::Local(admitted) => admitted?.call(context).await,
            InvocationDispatch::ReadError(output) => Ok(*output),
            InvocationDispatch::Remote {
                remote: connection,
                arguments,
            } => connection
                .execute(tool, arguments, &context)
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
                .filter_map(PermissionUse::proposed_grant)
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
        use std::time::Duration;

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
        let executor = remote_executor(executor, TargetRouter::test(targets(), policy.clone()));
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
