use base64::Engine as _;
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};
use thiserror::Error;

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    job::{JobError, JobManager, JobOutcome, JobSpec, JobState},
    remote::RemoteError,
    target::{ROOT_TARGET, ResolvedRoute, TargetRouter},
    tool::{
        ToolPlacement,
        authorization::{AuthorizationCoordinator, AuthorizationError, AuthorizationSubject},
        builtins::workspace::{lexical_path, resolve_for_authorization},
        policy::{ApprovalGrant, Capability, CapabilitySet, PermissionUse, Policy, ResourceId},
    },
};

use super::{ScriptBinding, ToolContext, ToolError, ToolExposure, ToolOutput, ToolRegistry};

#[derive(Clone, Copy)]
enum InvocationKind {
    Host,
    Model,
    Script,
}

struct PreparedInvocation {
    tool: Arc<super::RegisteredTool>,
    original_arguments: Value,
    handler_arguments: Value,
    background: bool,
    job_name: Option<String>,
    authorization_scope: Option<u64>,
}

struct InvocationPlan {
    origin: Option<crate::session::ModelCallOrigin>,
    agent: AgentId,
    tool: Arc<super::RegisteredTool>,
    original_arguments: Value,
    authorization_arguments: Value,
    handler_arguments: Value,
    caller_location: ExecutionLocation,
    execution_location: ExecutionLocation,
    permissions: Vec<PermissionUse>,
    parent: Option<JobId>,
    authorization_scope: Option<u64>,
    background: bool,
    job_name: Option<String>,
    dispatch: InvocationDispatch,
}

enum InvocationDispatch {
    Local,
    ReadError(ToolOutput),
    Remote(ResolvedRoute),
}

#[derive(Clone)]
pub struct ToolExecutor {
    model_origin: Option<crate::session::ModelCallOrigin>,
    shared: Arc<ExecutorServices>,
    caller_location: ExecutionLocation,
    capabilities: CapabilitySet,
}

#[derive(Clone)]
struct ExecutorServices {
    registry: ToolRegistry,
    authorization: AuthorizationCoordinator,
    jobs: JobManager,
    root_location: ExecutionLocation,
    router: Option<TargetRouter>,
    process_environment: crate::remote::backend::ProcessEnvironment,
}

impl ToolExecutor {
    #[must_use]
    pub fn new(
        registry: ToolRegistry,
        policy: Arc<dyn Policy>,
        jobs: JobManager,
        workspace: PathBuf,
    ) -> Self {
        Self::with_authorization(
            registry,
            AuthorizationCoordinator::new(policy),
            jobs,
            workspace,
        )
    }

    #[must_use]
    pub(crate) fn with_authorization(
        registry: ToolRegistry,
        authorization: AuthorizationCoordinator,
        jobs: JobManager,
        workspace: PathBuf,
    ) -> Self {
        let caller_location = ExecutionLocation::root(workspace.clone());
        Self {
            model_origin: None,
            shared: Arc::new(ExecutorServices {
                registry,
                authorization,
                jobs,
                root_location: ExecutionLocation::root(workspace),
                router: None,
                process_environment: Default::default(),
            }),
            caller_location,
            capabilities: CapabilitySet::default(),
        }
    }

    #[must_use]
    pub(crate) fn with_model_origin(mut self, origin: crate::session::ModelCallOrigin) -> Self {
        self.model_origin = Some(origin);
        self
    }

    #[must_use]
    pub(crate) fn with_location(mut self, location: ExecutionLocation) -> Self {
        self.caller_location = location;
        self
    }

    #[must_use]
    pub(crate) fn with_capabilities(mut self, capabilities: CapabilitySet) -> Self {
        self.capabilities = capabilities;
        self
    }

    #[must_use]
    pub fn surface(&self) -> super::ToolSurface {
        self.shared.registry.surface(&self.capabilities)
    }

    #[must_use]
    pub(crate) fn with_target_router(mut self, router: TargetRouter) -> Self {
        Arc::make_mut(&mut self.shared).router = Some(router);
        self
    }

    #[must_use]
    pub(crate) fn with_authorization_root(mut self, root: PathBuf) -> Self {
        Arc::make_mut(&mut self.shared).root_location = ExecutionLocation::root(root);
        self
    }

    #[must_use]
    pub fn registry(&self) -> &ToolRegistry {
        &self.shared.registry
    }

    #[must_use]
    pub fn jobs(&self) -> &JobManager {
        &self.shared.jobs
    }

    async fn resolve_workspace_invocation(
        &self,
        tool: &super::RegisteredTool,
        arguments: &Value,
    ) -> Result<SelectedLocation, ExecutionError> {
        if tool.placement() == ToolPlacement::Host {
            return Ok(SelectedLocation {
                location: self.shared.root_location.clone(),
                route: None,
            });
        }
        let explicit = if tool.placement() == ToolPlacement::TargetedWorkspace {
            if arguments.get("target").is_some() && !self.capabilities.contains(Capability::Targets)
            {
                return Err(ToolError::InvalidArguments(
                    "target selection requires the targets capability".to_owned(),
                )
                .into());
            }
            match arguments.get("target") {
                Some(Value::String(target)) => Some(target.as_str()),
                Some(Value::Null) | None => None,
                Some(_) => {
                    return Err(
                        ToolError::InvalidArguments("target must be a string".to_owned()).into(),
                    );
                }
            }
        } else if arguments.get("target").is_some() {
            return Err(ToolError::InvalidArguments(format!(
                "tool `{}` does not accept a target",
                tool.name()
            ))
            .into());
        } else {
            None
        };
        let selected = explicit.unwrap_or(&self.caller_location.target);
        if selected == ROOT_TARGET {
            return Ok(SelectedLocation {
                location: ExecutionLocation::select(
                    &self.caller_location,
                    &self.shared.root_location.workspace,
                    explicit,
                    None,
                ),
                route: None,
            });
        }
        let router = self.shared.router.as_ref().ok_or_else(|| {
            ToolError::InvalidArguments(
                "remote targets are unavailable in this tool runtime".to_owned(),
            )
        })?;
        let route = router
            .resolve(selected)
            .await
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let definition = route
            .definitions
            .last()
            .expect("validated target routes are nonempty");
        Ok(SelectedLocation {
            location: ExecutionLocation::select(
                &self.caller_location,
                &self.shared.root_location.workspace,
                explicit,
                Some(&definition.workspace),
            ),
            route: Some(route),
        })
    }

    pub(crate) fn with_process_environment(
        mut self,
        environment: crate::remote::backend::ProcessEnvironment,
    ) -> Self {
        Arc::make_mut(&mut self.shared).process_environment = environment;
        self
    }

    pub async fn execute(
        &self,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> Result<ExecutionResult, ExecutionError> {
        self.execute_as(InvocationKind::Host, agent, name, arguments, parent)
            .await
    }

    pub(crate) async fn execute_model(
        &self,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> Result<ExecutionResult, ExecutionError> {
        self.execute_as(InvocationKind::Model, agent, name, arguments, parent)
            .await
    }

    pub(crate) async fn execute_script(
        &self,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> Result<ExecutionResult, ExecutionError> {
        self.execute_as(InvocationKind::Script, agent, name, arguments, parent)
            .await
    }

    async fn execute_as(
        &self,
        kind: InvocationKind,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> Result<ExecutionResult, ExecutionError> {
        let plan = self
            .plan_registered(kind, agent, name, arguments, parent, None)
            .await?;
        let started = self.start(plan).await?;
        if matches!(kind, InvocationKind::Model) && name != "job_output" {
            return self.collect_model_started(name, started).await;
        }
        self.collect_started(started).await
    }

    async fn collect_model_started(
        &self,
        name: &str,
        started: StartedExecution,
    ) -> Result<ExecutionResult, ExecutionError> {
        let job = started.job;
        let background = started.background;
        if !background {
            self.shared.jobs.wait_foreground(job).await?;
        }
        let mut output = self
            .shared
            .jobs
            .present_output_for(
                crate::job::output::OutputArgs::new(job),
                &self.capabilities,
                &self.caller_location,
                background,
            )
            .await?;
        // Automatic model responses reference independently published child
        // replies instead of returning their text again. This also handles
        // a background child that finishes before its launch is presented.
        // Explicit job_output and native script/host results are unchanged.
        if name == "agent"
            && output["state"] == "completed"
            && let Some(sequence) = self.shared.jobs.last_agent_message(job).await?
            && let Some(view) = output.as_object_mut()
        {
            view.remove("result");
            view.remove("preview");
            view.remove("truncated");
            view.insert("last_message".into(), serde_json::json!(sequence));
        }
        Ok(ExecutionResult {
            job,
            background,
            output: ToolOutput::new(output).with_images(self.shared.jobs.images(job).await?),
        })
    }

    pub(crate) async fn start_scoped(
        &self,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
        authorization_scope: u64,
    ) -> Result<StartedExecution, ExecutionError> {
        let plan = self
            .plan_registered(
                InvocationKind::Host,
                agent,
                name,
                arguments,
                parent,
                Some(authorization_scope),
            )
            .await?;
        self.start(plan).await
    }

    async fn prepare_invocation(
        &self,
        kind: InvocationKind,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
        authorization_scope: Option<u64>,
    ) -> Result<PreparedInvocation, ExecutionError> {
        let tool = self
            .shared
            .registry
            .get(name)
            .ok_or_else(|| ExecutionError::UnknownTool(name.to_owned()))?;
        let spec = tool
            .spec(&self.capabilities)
            .ok_or_else(|| ToolError::InvalidArguments(format!("tool `{name}` is unavailable")))?;
        spec.validate_arguments(&arguments)?;
        validate_invocation(&spec, kind)?;
        let original_arguments = arguments.clone();
        let (mut handler_arguments, background) =
            self.shared.registry.split_execution(&spec, arguments)?;
        let job_name = tool.take_job_name(&mut handler_arguments)?;
        Ok(PreparedInvocation {
            tool,
            original_arguments,
            handler_arguments,
            background,
            job_name,
            authorization_scope: self
                .authorization_scope(parent, authorization_scope)
                .await?,
        })
    }

    async fn plan_registered(
        &self,
        kind: InvocationKind,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
        authorization_scope: Option<u64>,
    ) -> Result<InvocationPlan, ExecutionError> {
        let PreparedInvocation {
            tool,
            original_arguments,
            handler_arguments: mut arguments,
            background,
            job_name,
            authorization_scope,
        } = self
            .prepare_invocation(kind, name, arguments, parent, authorization_scope)
            .await?;
        let selected = self
            .resolve_workspace_invocation(&tool, &original_arguments)
            .await?;
        if tool.placement() == ToolPlacement::TargetedWorkspace {
            arguments
                .as_object_mut()
                .ok_or(ToolError::ArgumentsMustBeObject)?
                .remove("target");
        }
        let (path_permissions, read_error) = if selected.route.is_none() {
            preflight_path_arguments(
                &tool,
                &selected.location.target,
                &selected.location.workspace,
                &self.shared.root_location.workspace,
                &mut arguments,
            )
            .await?
        } else {
            (Vec::new(), None)
        };
        tool.validate_arguments(&arguments)?;
        let mut capabilities = tool.capabilities();
        // An unresolved read still requires the ordinary workspace authorization,
        // as well as approval for the unresolved path below.
        if read_error.is_none() {
            for permission in &path_permissions {
                capabilities.retain(|candidate| *candidate != permission.capability);
            }
        }
        let mut permissions =
            scope_capabilities(capabilities, &selected.location, tool.permission_resource());
        permissions.extend(path_permissions);
        let authorization_arguments = if let Some(route) = &selected.route {
            permissions.push(route.permission());
            serde_json::json!({
                "tool": original_arguments,
                "route": route.authorization_arguments(),
            })
        } else {
            original_arguments.clone()
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
            handler_arguments: arguments,
            caller_location: self.caller_location.clone(),
            execution_location: selected.location,
            permissions,
            parent,
            authorization_scope,
            background,
            job_name,
            dispatch: read_error.map_or_else(
                || {
                    selected
                        .route
                        .map_or(InvocationDispatch::Local, InvocationDispatch::Remote)
                },
                InvocationDispatch::ReadError,
            ),
        })
    }

    async fn authorization_scope(
        &self,
        parent: Option<JobId>,
        requested: Option<u64>,
    ) -> Result<Option<u64>, JobError> {
        Ok(match (requested, parent) {
            (Some(scope), _) => Some(scope),
            (None, Some(parent)) => self.shared.jobs.authorization_scope(parent).await?,
            (None, None) => None,
        })
    }

    async fn start(&self, plan: InvocationPlan) -> Result<StartedExecution, ExecutionError> {
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
                plan.authorization_arguments,
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

    pub(crate) async fn collect_started(
        &self,
        started: StartedExecution,
    ) -> Result<ExecutionResult, ExecutionError> {
        self.collect_up_to(started, u64::MAX).await
    }

    pub(crate) async fn collect_for_transfer(
        &self,
        started: StartedExecution,
    ) -> Result<ExecutionResult, ExecutionError> {
        self.collect_up_to(started, crate::job::output::PAGE_BYTES as u64)
            .await
    }

    async fn collect_up_to(
        &self,
        started: StartedExecution,
        maximum: u64,
    ) -> Result<ExecutionResult, ExecutionError> {
        let StartedExecution { job, background } = started;
        if background {
            let envelope = self.shared.jobs.metadata(job).await?;
            let value =
                envelope.presented_for(&self.capabilities, Some(&self.caller_location), true)?;
            return Ok(ExecutionResult {
                job,
                background: true,
                output: ToolOutput::new(value),
            });
        }
        let mut envelope = self.shared.jobs.wait_foreground(job).await?;
        self.shared
            .jobs
            .hydrate_envelope_up_to(&mut envelope, maximum)
            .await?;
        if !envelope.state.is_terminal() {
            return Ok(ExecutionResult {
                job,
                background: true,
                output: ToolOutput::new(envelope.presented_for(
                    &self.capabilities,
                    Some(&self.caller_location),
                    true,
                )?),
            });
        }
        if maximum == u64::MAX {
            self.shared.jobs.claim(job).await?;
        }
        let images = self.shared.jobs.images(job).await?;
        if envelope.state == JobState::Completed {
            Ok(ExecutionResult {
                job,
                background: false,
                output: ToolOutput {
                    value: envelope.output.unwrap_or(Value::Null),
                    images,
                },
            })
        } else if envelope.denial.is_some() {
            Err(ExecutionError::Denied(
                envelope
                    .error
                    .unwrap_or_else(|| "operation denied".to_owned()),
            ))
        } else {
            Err(ExecutionError::Failed {
                message: envelope
                    .error
                    .unwrap_or_else(|| format!("job ended as {:?}", envelope.state)),
                output: envelope
                    .output
                    .map(|value| ToolOutput { value, images })
                    .map(Box::new),
            })
        }
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

#[derive(Debug)]
struct SelectedLocation {
    location: ExecutionLocation,
    route: Option<ResolvedRoute>,
}

async fn import_remote_result(
    store: &crate::session::SessionStore,
    result: Result<ToolOutput, RemoteError>,
) -> Result<ToolOutput, ToolError> {
    match result {
        Ok(output) => import_remote_output(store, output).await,
        Err(RemoteError::Remote {
            message,
            output: Some(output),
        }) => Err(ToolError::with_output(
            message,
            import_remote_output(store, *output).await?,
        )),
        Err(error) => Err(error.into_tool_error()),
    }
}

async fn import_remote_output(
    store: &crate::session::SessionStore,
    mut output: ToolOutput,
) -> Result<ToolOutput, ToolError> {
    let mut imported = Vec::with_capacity(output.images.len());
    for image in output.images {
        let encoded = image
            .data_base64
            .as_deref()
            .ok_or_else(|| ToolError::Failed("remote image payload is missing".to_owned()))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        let reference = store
            .import_blob(&bytes, image.name, image.media_type)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        if reference.sha256 != image.sha256 {
            return Err(ToolError::Failed(
                "remote image hash did not match its payload".to_owned(),
            ));
        }
        imported.push(reference);
    }
    output.images = imported;
    Ok(output)
}

async fn preflight_path_arguments(
    tool: &super::RegisteredTool,
    target: &str,
    workspace: &std::path::Path,
    authorization_root: &std::path::Path,
    arguments: &mut Value,
) -> Result<(Vec<PermissionUse>, Option<ToolOutput>), ToolError> {
    let object = arguments
        .as_object_mut()
        .ok_or(ToolError::ArgumentsMustBeObject)?;
    let mut permissions = Vec::new();
    let mut read_error = None;
    for spec in tool.path_arguments() {
        let input = match object.get(&spec.name) {
            Some(Value::String(path)) => path.clone(),
            Some(_) => {
                return Err(ToolError::InvalidArguments(format!(
                    "{} must be a string",
                    spec.name
                )));
            }
            None => match &spec.default {
                Some(default) => default.clone(),
                None => continue,
            },
        };
        let resolved = match resolve_for_authorization(workspace, &input, spec.kind).await {
            Ok(resolved) => resolved,
            Err(error) => {
                let Some(output) = tool.read_error_output(&input, &error) else {
                    return Err(error);
                };
                // Canonicalization failed, so this path is not proven to be within
                // the authorization root. Require exact path authorization even for
                // apparently local paths, then return the captured failure without
                // retrying the handler (which could now access a changed target).
                let path = lexical_path(workspace, &input)?;
                let capability = spec.access.capability();
                let resource = ResourceId::path(target, &path);
                permissions.push(
                    PermissionUse::new(capability, resource.clone())
                        .with_grant(ApprovalGrant::exact(capability, resource)),
                );
                read_error = Some(output);
                continue;
            }
        };
        object.insert(
            spec.name.clone(),
            Value::String(resolved.path.to_string_lossy().into_owned()),
        );
        if !resolved.path.starts_with(authorization_root) {
            let capability = spec.access.capability();
            let resource = ResourceId::path(target, &resolved.path);
            let grant = if resolved.directory {
                ApprovalGrant::descendants(capability, resource.clone())
            } else {
                ApprovalGrant::exact(capability, resource.clone())
            };
            permissions.push(PermissionUse::new(capability, resource).with_grant(grant));
        }
    }
    Ok((permissions, read_error))
}

fn scope_capabilities(
    capabilities: Vec<Capability>,
    location: &ExecutionLocation,
    override_resource: Option<&ResourceId>,
) -> Vec<PermissionUse> {
    let resource = override_resource
        .cloned()
        .unwrap_or_else(|| ResourceId::workspace(&location.target, &location.workspace));
    capabilities
        .into_iter()
        .map(|capability| PermissionUse::new(capability, resource.clone()))
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct StartedExecution {
    pub job: JobId,
    background: bool,
}

pub(crate) async fn persist_completion(jobs: &JobManager, job: JobId, completion: JobOutcome) {
    let result = jobs.finish(job, completion).await;
    if let Err(error) = result
        && !matches!(error, JobError::AlreadyTerminal(_))
    {
        jobs.fail_volatile(
            job,
            format!("job finalization could not be persisted: {error}"),
        )
        .await;
    }
}

#[derive(Debug)]
pub struct ExecutionResult {
    pub job: JobId,
    pub background: bool,
    pub output: ToolOutput,
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
    #[error("tool `{0}` is unavailable in this context")]
    UnavailableTool(String),
    #[error("tool `{0}` is not exposed for direct model calls")]
    ModelHidden(String),
    #[error("tool `{0}` is not available in scripts")]
    ScriptUnavailable(String),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Job(#[from] JobError),
    #[error("tool authorization denied: {0}")]
    Denied(String),
    #[error("tool execution failed: {message}")]
    Failed {
        message: String,
        output: Option<Box<ToolOutput>>,
    },
    #[error("could not serialize tool result: {0}")]
    Json(#[from] serde_json::Error),
}

impl ExecutionError {
    pub(crate) fn into_failure(self) -> ExecutionFailure {
        let message = self.concise_message();
        let denial = matches!(&self, Self::Denied(_) | Self::Tool(ToolError::Denied(_)))
            .then(super::Denial::permission_denied);
        let output = match self {
            Self::Failed { output, .. } => output.map(|output| *output),
            Self::Tool(ToolError::FailedWithOutput { output, .. }) => Some(*output),
            _ => None,
        };
        ExecutionFailure {
            message,
            output,
            denial,
        }
    }

    pub(crate) fn concise_message(&self) -> String {
        match self {
            Self::Tool(error) => error.concise_message(),
            Self::Failed { message, .. } => message.clone(),
            _ => self.to_string(),
        }
    }
}

pub(crate) struct ExecutionFailure {
    pub message: String,
    pub output: Option<ToolOutput>,
    pub denial: Option<super::Denial>,
}

fn validate_invocation(tool: &super::ToolSpec, kind: InvocationKind) -> Result<(), ExecutionError> {
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
mod tests {
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::*;
    use crate::{
        session::SessionStore,
        target::{TargetDefinition, TargetRegistry},
        tool::{
            ToolOptions, ToolRegistryBuilder,
            policy::{AuthorizationRequest, PolicyDecision, PolicyFuture},
        },
    };

    #[derive(Deserialize, JsonSchema)]
    struct PathArgs {
        #[serde(rename = "path")]
        _path: String,
    }

    fn target(name: &str, workspace: &str, via: Option<&str>) -> TargetDefinition {
        TargetDefinition::test(name, workspace, via)
    }

    fn router(targets: TargetRegistry, policy: Arc<dyn Policy>) -> TargetRouter {
        let authorization = AuthorizationCoordinator::new(policy);
        let remote = crate::remote::RemoteManager::new(
            crate::remote::EmbeddedShimCatalog::default(),
            Arc::new(crate::remote::RejectSensitivePrompts),
            authorization.clone(),
        );
        TargetRouter::new(targets, remote, authorization)
    }

    #[tokio::test]
    async fn automatic_completed_child_results_reference_independent_messages() {
        use crate::{
            job::JobSpec,
            provider::protocol::{AssistantContent, Message},
            session::SessionEvent,
        };

        // Cover foreground presentation and a background child that has already
        // finished before its launch response is collected, without a timing race.
        for background in [false, true] {
            let runtime = crate::test_support::TestRuntime::new().await;
            let executor = runtime.executor(ToolRegistryBuilder::default());
            let child = runtime.agent.child(1);
            let job = runtime
                .jobs
                .create(JobSpec {
                    background,
                    ..JobSpec::test(runtime.agent.clone(), "agent")
                })
                .await
                .unwrap()
                .id;
            runtime
                .store
                .append(
                    child.clone(),
                    SessionEvent::AgentStarted {
                        parent: Some(runtime.agent.clone()),
                        owner_job: Some(job),
                        model_profile: "test".into(),
                        max_context: None,
                        agent_profile: None,
                        location: ExecutionLocation::root(runtime.root.path().to_owned()),
                    },
                )
                .await
                .unwrap();
            runtime
                .jobs
                .transition(job, JobState::Running)
                .await
                .unwrap();
            let text = (0..500)
                .map(|line| format!("child answer line {line}\n"))
                .collect::<String>();
            let sequence = runtime
                .jobs
                .commit_child_message(
                    &child,
                    job,
                    Message::Assistant(vec![AssistantContent::text("answer", 0, text.clone())]),
                    text.clone(),
                )
                .await
                .unwrap();
            runtime
                .jobs
                .finish(
                    job,
                    JobOutcome::Completed(ToolOutput::new(serde_json::json!(text))),
                )
                .await
                .unwrap();

            let result = executor
                .collect_model_started("agent", StartedExecution { job, background })
                .await
                .unwrap();
            assert_eq!(result.output.value["state"], "completed");
            assert_eq!(result.output.value["last_message"], sequence);
            for field in ["result", "preview", "truncated"] {
                assert!(result.output.value.get(field).is_none(), "{field}");
            }
            // Claiming the model result must not claim the independently
            // deliverable reply, nor destroy the explicit saved final answer.
            let pending = runtime.jobs.pending_delivery(&runtime.agent).await.unwrap();
            assert_eq!(pending.messages().len(), 1);
            assert_eq!(pending.messages()[0].text, text);
            drop(pending);
            let saved = runtime.jobs.snapshot(job).await.unwrap();
            assert_eq!(saved.output, Some(serde_json::json!(text)));
        }
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
                        .any(|permission| permission.capability == Capability::Read)
                );
                Box::pin(async {
                    PolicyDecision::Deny {
                        reason: "read forbidden".into(),
                    }
                })
            }
        }
        let runtime = crate::test_support::TestRuntime::new().await;
        let mut builder = ToolRegistryBuilder::default();
        crate::tool::builtins::register_worker_tools(&mut builder, runtime.store.clone()).unwrap();
        let executor = ToolExecutor::new(
            builder.build(),
            Arc::new(DenyReads),
            runtime.jobs.clone(),
            runtime.root.path().to_owned(),
        );
        for path in ["missing/nested/file", "../outside/missing"] {
            let error = executor
                .execute_model(
                    runtime.agent.clone(),
                    "read",
                    serde_json::json!({"path":path}),
                    None,
                )
                .await
                .unwrap_err();
            assert!(matches!(error, ExecutionError::Denied(_)));
        }
    }

    #[tokio::test]
    async fn a_registered_name_read_does_not_opt_into_io_recovery() {
        let runtime = crate::test_support::TestRuntime::new().await;
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
                    Err(ToolError::Io(std::io::Error::from(
                        std::io::ErrorKind::PermissionDenied,
                    )))
                },
            )
            .unwrap();
        let executor = runtime.executor(builder);
        let error = executor
            .execute(
                runtime.agent.clone(),
                "read",
                serde_json::json!({"path":"missing/nested"}),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, ExecutionError::Tool(ToolError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound)
        );
        let error = executor
            .execute(
                runtime.agent.clone(),
                "read",
                serde_json::json!({"path":"."}),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ExecutionError::Failed { .. }));
    }

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

        let runtime = crate::test_support::TestRuntime::new().await;
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
            TargetRegistry::from_definitions([target("build", "/build", None)]).unwrap(),
            remote.clone(),
            authorization,
        );
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_dynamic(
                "custom_remote",
                "test",
                serde_json::json!({"type":"object","properties":{}}),
                ToolOptions::new(vec![Capability::Exec])
                    .placement(crate::tool::ToolPlacement::TargetedWorkspace),
                |_, _| async { panic!("connection has not completed") },
            )
            .unwrap();
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
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create_ephemeral(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store);
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_dynamic(
                "custom_remote",
                "test",
                serde_json::json!({"type":"object","properties":{}}),
                ToolOptions::new(Vec::new())
                    .placement(crate::tool::ToolPlacement::TargetedWorkspace),
                |_context, _arguments| async { Ok(ToolOutput::new(Value::Null)) },
            )
            .unwrap();
        let targets = TargetRegistry::from_definitions([target("build", "/build", None)]).unwrap();
        let policy = Arc::new(BlockingRoutePolicy {
            requests: std::sync::Mutex::new(Vec::new()),
            release: tokio::sync::Notify::new(),
        });
        let router = router(targets, policy.clone());
        let executor = ToolExecutor::new(
            builder.build(),
            policy.clone(),
            jobs.clone(),
            root.path().to_path_buf(),
        )
        .with_target_router(router)
        .with_capabilities({
            let mut capabilities = CapabilitySet::default();
            capabilities.insert(Capability::Targets);
            capabilities
        });
        let running = {
            let executor = executor.clone();
            let agent = agent.clone();
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
        assert_eq!(tool_request.agent, agent);
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
            jobs.snapshot(tool_request.job).await.unwrap().state,
            JobState::AwaitingApproval
        );
        jobs.cancel(tool_request.job).await.unwrap();
        assert!(running.await.unwrap().is_err());
    }
}
