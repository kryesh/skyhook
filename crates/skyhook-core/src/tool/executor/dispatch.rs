//! Start local or remote jobs after planning and authorization.

use super::results::import_remote_result;
use super::*;

impl ToolExecutor {
    pub(super) async fn start(
        &self,
        plan: InvocationPlan,
    ) -> Result<StartedExecution, ExecutionError> {
        let lease = self
            .shared
            .jobs
            .create(JobSpec {
                origin: plan.origin,
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
            })
            .await?;
        if lease.cancellation.is_cancelled() {
            return Err(self.cancelled(lease.id).await);
        }
        self.shared
            .jobs
            .transition(lease.id, JobState::AwaitingApproval)
            .await?;
        let subject = AuthorizationSubject {
            agent: plan.agent,
            job: lease.id,
            parent: plan.parent,
            scope: plan.authorization_scope,
            capabilities: self.capabilities.clone(),
            cancellation: lease.cancellation.clone(),
        };
        if let Err(error) = self
            .shared
            .authorization
            .authorize(
                &subject,
                plan.tool.name().to_owned(),
                plan.permissions,
                plan.authorization_arguments.clone(),
            )
            .await
        {
            let error = match error {
                AuthorizationError::Cancelled => return Err(self.cancelled(lease.id).await),
                AuthorizationError::Denied(reason) => ExecutionError::Denied(reason),
                AuthorizationError::InvalidGrant(_) => ExecutionError::Failed {
                    message: "operation could not be started".to_owned(),
                    output: None,
                },
                AuthorizationError::Unavailable => {
                    ExecutionError::UnavailableTool(plan.tool.name().to_owned())
                }
            };
            return Err(self.fail_start(lease.id, error).await?);
        }
        // The policy has approved this invocation. Remote connection/bootstrap can
        // still wait for network I/O or SSH authentication; that is execution, not
        // a pending tool approval. Keep route reauthorization inside prepare.
        self.shared
            .jobs
            .transition(lease.id, JobState::Running)
            .await?;
        let prepared = if let InvocationDispatch::Remote(route) = &plan.dispatch {
            let router = self
                .shared
                .router
                .as_ref()
                .expect("remote plans require a target router");
            match router
                .prepare(route.clone(), &plan.execution_location.workspace, &subject)
                .await
            {
                Ok(prepared) => Some(prepared),
                Err(RemoteError::Cancelled) => {
                    return Err(self.cancelled(lease.id).await);
                }
                Err(error) => {
                    let error = match error {
                        RemoteError::ApprovalDenied(reason) => ExecutionError::Denied(reason),
                        RemoteError::ApprovalInvalidGrant(_) | RemoteError::ApprovalUnavailable => {
                            ExecutionError::Failed {
                                message: "target is unavailable in this context".to_owned(),
                                output: None,
                            }
                        }
                        error => ExecutionError::Failed {
                            message: error.to_string(),
                            output: None,
                        },
                    };
                    return Err(self.fail_start(lease.id, error).await?);
                }
            }
        } else {
            None
        };
        if lease.cancellation.is_cancelled() {
            return Err(self.cancelled(lease.id).await);
        }
        let mut context = ToolContext::new(
            subject,
            plan.execution_location,
            plan.caller_location,
            lease.input,
            self.shared.jobs.clone(),
        );
        context.process_environment = self.shared.process_environment.clone();
        context.authorizer = Some((
            self.shared.authorization.clone(),
            plan.tool.name().to_owned(),
            plan.authorization_arguments.clone(),
        ));
        let authentication = if context.capabilities.contains(Capability::Targets)
            && context.execution_location.is_root()
            && matches!(plan.tool.name(), "exec" | "shell")
        {
            self.shared.router.clone()
        } else {
            None
        };
        let jobs = self.shared.jobs.clone();
        let store = jobs.store().clone();
        let job = lease.id;
        let background = plan.background;
        let worker = tokio::spawn(async move {
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
            let cancellation = context.authorization.cancellation.clone();
            let result = match plan.dispatch {
                InvocationDispatch::Local => plan.tool.call(context, plan.handler_arguments).await,
                InvocationDispatch::ReadError(output) => Ok(output),
                InvocationDispatch::Remote(_) => {
                    let result = prepared
                        .expect("remote plans have prepared connections")
                        .execute(
                            plan.tool.name().to_owned(),
                            plan.handler_arguments,
                            &context,
                        )
                        .await;
                    if context.is_cancelled() {
                        Err(ToolError::Cancelled)
                    } else {
                        import_remote_result(&store, result).await
                    }
                }
            };
            if cancellation.is_cancelled() {
                Err(ToolError::Cancelled)
            } else {
                result
            }
        });
        self.shared
            .jobs
            .attach_task(job, worker.abort_handle())
            .await?;
        tokio::spawn(async move {
            let completion = match worker.await {
                Ok(Ok(output)) => JobOutcome::Completed(output),
                Ok(Err(error)) => error.into(),
                Err(error) if error.is_cancelled() => JobOutcome::Cancelled,
                Err(_) => ToolError::Failed("tool handler panicked".to_owned()).into(),
            };
            persist_completion(&jobs, job, completion).await;
        });
        Ok(StartedExecution { job, background })
    }

    async fn cancelled(&self, job: JobId) -> ExecutionError {
        persist_completion(&self.shared.jobs, job, JobOutcome::Cancelled).await;
        ExecutionError::Failed {
            message: "tool was cancelled".to_owned(),
            output: None,
        }
    }

    async fn fail_start(
        &self,
        job: JobId,
        error: ExecutionError,
    ) -> Result<ExecutionError, JobError> {
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
        if matches!(error, ExecutionError::Denied(_)) {
            self.shared
                .jobs
                .finish(job, ToolError::Denied(message).into())
                .await?;
        } else {
            self.shared
                .jobs
                .finish(job, ToolError::Failed(message).into())
                .await?;
        }
        Ok(error)
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
                .any(|permission| permission.capability == Capability::Targets);
            let grants = request
                .permissions
                .iter()
                .filter_map(|permission| permission.proposed_grant.clone())
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
        let policy = Arc::new(crate::tool::policy::AllowAll);
        let authorization = AuthorizationCoordinator::new(policy.clone());
        let factory = Arc::new(PendingConnection(AtomicUsize::new(0)));
        let remote = crate::remote::RemoteManager::new(
            crate::remote::EmbeddedShimCatalog::default(),
            Arc::new(crate::remote::RejectSensitivePrompts),
            authorization.clone(),
        )
        .with_connection_factory(factory.clone());
        let router = TargetRouter::new(
            TargetRegistry::from_definitions([TargetDefinition::test("build", "/build", None)])
                .unwrap(),
            remote.clone(),
            authorization,
        );
        let builder = remote_tool(vec![Capability::Exec]);
        let executor = runtime
            .executor(builder)
            .with_target_router(router)
            .with_capabilities({
                let mut capabilities = CapabilitySet::default();
                capabilities.insert(Capability::Targets);
                capabilities
            });
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..5 {
            let executor = executor.clone();
            let agent = runtime.agent.clone();
            tasks.spawn(async move {
                executor
                    .execute(
                        agent,
                        "custom_remote",
                        serde_json::json!({"target":"build"}),
                        None,
                    )
                    .await
            });
        }
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
        while let Some(result) = tasks.join_next().await {
            assert!(result.unwrap().is_err());
        }
        remote.shutdown().await;
    }

    #[tokio::test]
    async fn route_approval_uses_the_job_subject_while_job_awaits_approval() {
        let runtime = crate::tests::TestRuntime::new().await;
        let builder = remote_tool(Vec::new());
        let targets =
            TargetRegistry::from_definitions([TargetDefinition::test("build", "/build", None)])
                .unwrap();
        let policy = Arc::new(BlockingRoutePolicy {
            requests: std::sync::Mutex::new(Vec::new()),
            release: tokio::sync::Notify::new(),
        });
        let router = router(targets, policy.clone());
        let executor = ToolExecutor::new(
            builder.build(),
            policy.clone(),
            runtime.jobs.clone(),
            runtime.root.path().to_path_buf(),
        )
        .with_target_router(router)
        .with_capabilities({
            let mut capabilities = CapabilitySet::default();
            capabilities.insert(Capability::Targets);
            capabilities
        });
        let running = {
            let executor = executor.clone();
            let agent = runtime.agent.clone();
            tokio::spawn(async move {
                executor
                    .execute(
                        agent,
                        "custom_remote",
                        serde_json::json!({"target":"build"}),
                        None,
                    )
                    .await
            })
        };
        while policy.requests.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        let tool_request = policy.requests.lock().unwrap()[0].clone();
        assert_eq!(tool_request.agent, runtime.agent);
        assert!(
            tool_request
                .permissions
                .iter()
                .any(|permission| permission.capability == Capability::Targets)
        );
        assert_eq!(tool_request.arguments["tool"]["target"], "build");
        assert_eq!(tool_request.arguments["route"]["destination"], "build");
        assert_eq!(tool_request.arguments["route"]["route"][0], "build");
        assert!(tool_request.arguments.get("workspace").is_none());
        assert_eq!(
            runtime.jobs.snapshot(tool_request.job).await.unwrap().state,
            JobState::AwaitingApproval
        );
        runtime.jobs.cancel(tool_request.job).await.unwrap();
        assert!(running.await.unwrap().is_err());
    }
}
