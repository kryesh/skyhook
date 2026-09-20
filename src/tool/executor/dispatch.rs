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
                AuthorizationError::InvalidGrant(_) => ExecutionError::Failed {
                    message: "operation could not be started".to_owned(),
                    output: None,
                },
                AuthorizationError::Unavailable => {
                    ExecutionError::UnavailableTool(plan.tool.name().to_owned())
                }
            };
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
                        let error = match error {
                            RemoteError::ApprovalDenied(reason) => ExecutionError::Denied(reason),
                            RemoteError::ApprovalInvalidGrant(_)
                            | RemoteError::ApprovalUnavailable => ExecutionError::Failed {
                                message: "target is unavailable in this context".to_owned(),
                                output: None,
                            },
                            error => ExecutionError::Failed {
                                message: error.to_string(),
                                output: None,
                            },
                        };
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
                if let Some(router) = authentication {
                    context.process_environment.extend(
                        router
                            .environment()
                            .await
                            .map_err(|e| e.into_tool_error())?,
                    );
                }
                if context.is_cancelled() {
                    return Err(ToolError::Cancelled);
                }
                let cancellation = context.cancellation_token();
                let result = match dispatch {
                    InvocationDispatch::Local(admitted) => admitted?.call(context).await,
                    InvocationDispatch::ReadError(output) => Ok(output),
                    InvocationDispatch::Remote {
                        remote: connection,
                        arguments,
                    } => {
                        let result = connection
                            .execute(tool.name().to_owned(), arguments, &context)
                            .await;
                        if context.is_cancelled() {
                            Err(ToolError::Cancelled)
                        } else {
                            result.map_err(crate::remote::RemoteError::into_tool_error)
                        }
                    }
                };
                if cancellation.is_cancelled() {
                    Err(ToolError::Cancelled)
                } else {
                    result
                }
            })
            .await?;
        Ok(StartedExecution {
            job,
            background: plan.background,
        })
    }

    async fn cancelled(&self, lease: crate::job::JobLease) -> ExecutionError {
        lease.fail(JobOutcome::Cancelled).await;
        ExecutionError::Failed {
            message: "tool was cancelled".to_owned(),
            output: None,
        }
    }

    async fn fail_start(
        &self,
        lease: crate::job::JobLease,
        error: ExecutionError,
    ) -> ExecutionError {
        let message = match &error {
            ExecutionError::Denied(reason)
            | ExecutionError::Failed {
                message: reason, ..
            } => reason.clone(),
            ExecutionError::UnavailableTool(name) => {
                format!("tool `{name}` is unavailable in this context")
            }
            error => error.to_string(),
        };
        let outcome = if matches!(error, ExecutionError::Denied(_)) {
            ToolError::Denied(message).into()
        } else {
            ToolError::Failed(message).into()
        };
        lease.fail(outcome).await;
        error
    }
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

    #[tokio::test]
    async fn approved_remote_jobs_are_running_during_shared_connection_startup() {
        use crate::remote::{ConnectionFactory, ConnectionRequest, backend::Transport};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct PendingConnection(AtomicUsize);
        impl ConnectionFactory for PendingConnection {
            fn connect(
                &self,
                _: ConnectionRequest,
            ) -> futures_util::future::BoxFuture<'static, Result<Transport, RemoteError>>
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(std::future::pending())
            }
        }

        let runtime = crate::tests::TestRuntime::new().await;
        let authorization = AuthorizationCoordinator::new(Arc::new(crate::tool::policy::AllowAll));
        let factory = Arc::new(PendingConnection(AtomicUsize::new(0)));
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
                    && factory.0.load(Ordering::SeqCst) == 1
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
        let root = runtime.root.path().to_path_buf();
        let executor = ToolExecutor::new(
            remote_tool(Vec::new()).build(),
            policy.clone(),
            runtime.jobs.clone(),
            root,
        );
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
