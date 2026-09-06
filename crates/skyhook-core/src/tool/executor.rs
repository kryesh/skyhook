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
        builtins::workspace::resolve_for_authorization,
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
    Remote(ResolvedRoute),
}

#[derive(Clone)]
pub struct ToolExecutor {
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
    process_environment: crate::remote::authentication::ProcessEnvironment,
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
        environment: crate::remote::authentication::ProcessEnvironment,
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
            let job = started.job;
            let background = started.background;
            if !background {
                self.shared.jobs.wait_foreground(job).await?;
            }
            let output = self
                .shared
                .jobs
                .present_output(crate::job::output::OutputArgs::new(job), &self.capabilities)
                .await?;
            return Ok(ExecutionResult {
                job,
                background,
                output: ToolOutput::new(output).with_images(self.shared.jobs.images(job).await?),
            });
        }
        self.collect_started(started).await
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
        let surface = self.surface();
        surface.validate_arguments(name, &arguments)?;
        let spec = surface
            .get(name)
            .expect("validated tools are present on the surface");
        validate_invocation(spec, kind)?;
        let original_arguments = arguments.clone();
        let (mut handler_arguments, background) =
            self.shared.registry.split_execution(spec, arguments)?;
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
        let path_permissions = if selected.route.is_none() {
            preflight_path_arguments(
                &tool,
                &selected.location.target,
                &selected.location.workspace,
                &self.shared.root_location.workspace,
                &mut arguments,
            )
            .await?
        } else {
            Vec::new()
        };
        let mut capabilities = tool.capabilities_for(&arguments)?;
        for permission in &path_permissions {
            capabilities.retain(|candidate| *candidate != permission.capability);
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
            dispatch: selected
                .route
                .map_or(InvocationDispatch::Local, InvocationDispatch::Remote),
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
        self.shared
            .jobs
            .transition(lease.id, JobState::Running)
            .await?;
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
            let value = envelope.presented(&self.capabilities)?;
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
                output: ToolOutput::new(envelope.presented(&self.capabilities)?),
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
                    console_output: envelope.console_output,
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
                output: envelope.output.map(|value| ToolOutput {
                    value,
                    images,
                    console_output: envelope.console_output,
                }),
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
            import_remote_output(store, output).await?,
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
) -> Result<Vec<PermissionUse>, ToolError> {
    let object = arguments
        .as_object_mut()
        .ok_or(ToolError::ArgumentsMustBeObject)?;
    let mut permissions = Vec::new();
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
        let resolved = resolve_for_authorization(workspace, &input, spec.kind).await?;
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
    Ok(permissions)
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

async fn persist_completion(jobs: &JobManager, job: JobId, completion: JobOutcome) {
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
        output: Option<ToolOutput>,
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
            Self::Failed { output, .. } => output,
            Self::Tool(ToolError::FailedWithOutput { output, .. }) => Some(output),
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
            policy::{AllowAll, AuthorizationRequest, PolicyDecision, PolicyFuture},
        },
    };

    #[derive(Deserialize, JsonSchema)]
    struct PathArgs {
        path: String,
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
    async fn target_named_application_data_is_not_globally_filtered() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create_ephemeral(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store);
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_dynamic(
                "adapter",
                "Dynamic adapter",
                serde_json::json!({
                    "type": "object",
                    "properties": {"target": {"type": "string"}}
                }),
                ToolOptions::default(),
                |_context, arguments| async move {
                    Ok(ToolOutput::new(serde_json::json!({
                        "payload": {"target": arguments["target"]}
                    })))
                },
            )
            .unwrap();
        let executor = ToolExecutor::new(
            builder.build(),
            Arc::new(AllowAll),
            jobs,
            root.path().to_path_buf(),
        );

        let schema = &executor.surface().definitions()[0].input_schema;
        assert!(schema["properties"]["target"].is_object());
        let result = executor
            .execute(
                agent,
                "adapter",
                serde_json::json!({"target": "application-value"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            result.output.value["payload"]["target"],
            "application-value"
        );
    }

    #[tokio::test]
    async fn workspace_target_resolution_obeys_root_current_and_other_rules() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create_ephemeral(root.path()).await.unwrap();
        let jobs = JobManager::new(store);
        let mut builder = ToolRegistryBuilder::default();
        let seen = Arc::new(std::sync::Mutex::new(None));
        builder
            .register::<PathArgs, String, _, _>(
                "arbitrary_name",
                "test read",
                ToolOptions::new(Vec::new())
                    .placement(crate::tool::ToolPlacement::TargetedWorkspace),
                |_context, input| async move { Ok(input.path) },
            )
            .unwrap();
        builder
            .register::<PathArgs, String, _, _>(
                "write_like",
                "inherit only",
                ToolOptions::new(Vec::new())
                    .placement(crate::tool::ToolPlacement::InheritWorkspace),
                |_context, input| async move { Ok(input.path) },
            )
            .unwrap();
        let dynamic_seen = seen.clone();
        builder
            .register_dynamic(
                "totally_custom",
                "dynamic targeted",
                serde_json::json!({"type":"object","properties":{"value":{"type":"string"}}}),
                ToolOptions::new(Vec::new())
                    .placement(crate::tool::ToolPlacement::TargetedWorkspace),
                move |_context, arguments| {
                    *dynamic_seen.lock().unwrap() = Some(arguments.clone());
                    async move { Ok(ToolOutput::new(arguments)) }
                },
            )
            .unwrap();
        let targets = TargetRegistry::from_definitions([
            target("gateway", "/gateway", None),
            target("current", "/configured-current", Some("gateway")),
            target("other", "/configured-other", Some("gateway")),
        ])
        .unwrap();
        let registry = builder.build();
        let policy = Arc::new(AllowAll);
        let router = router(targets, policy.clone());
        let executor = ToolExecutor::new(registry.clone(), policy, jobs, root.path().to_path_buf())
            .with_target_router(router)
            .with_capabilities({
                let mut capabilities = CapabilitySet::default();
                capabilities.insert(Capability::Targets);
                capabilities
            })
            .with_location(ExecutionLocation::named(
                "current",
                PathBuf::from("/override-current"),
            ));
        let tool = registry.get("arbitrary_name").unwrap();
        assert!(
            registry
                .surface(&{
                    let mut capabilities = CapabilitySet::default();
                    capabilities.insert(Capability::Targets);
                    capabilities
                })
                .get("totally_custom")
                .unwrap()
                .input_schema["properties"]["target"]
                .is_object()
        );

        for (target, expected) in [
            (
                None,
                ExecutionLocation::named("current", "/override-current".into()),
            ),
            (
                Some("current"),
                ExecutionLocation::named("current", "/override-current".into()),
            ),
            (
                Some("other"),
                ExecutionLocation::named("other", "/configured-other".into()),
            ),
            (
                Some(ROOT_TARGET),
                ExecutionLocation::root(root.path().to_path_buf()),
            ),
        ] {
            let mut arguments = serde_json::json!({"path":"file"});
            if let Some(target) = target {
                arguments["target"] = target.into();
            }
            let selected = executor
                .resolve_workspace_invocation(&tool, &arguments)
                .await
                .unwrap();
            assert_eq!(selected.location, expected);
        }

        let error = executor
            .resolve_workspace_invocation(
                &tool,
                &serde_json::json!({"target":"missing", "path":"file"}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown target `missing`"));

        let error = executor
            .clone()
            .with_capabilities(CapabilitySet::default())
            .resolve_workspace_invocation(
                &tool,
                &serde_json::json!({"target":"current", "path":"file"}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("targets capability"));

        let inherit = registry.get("write_like").unwrap();
        assert_eq!(
            executor
                .resolve_workspace_invocation(&inherit, &serde_json::json!({"path":"file"}))
                .await
                .unwrap()
                .location,
            ExecutionLocation::named("current", "/override-current".into())
        );
        let error = executor
            .resolve_workspace_invocation(
                &inherit,
                &serde_json::json!({"target":"root", "path":"file"}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("does not accept a target"));

        executor
            .execute(
                AgentId::root(executor.jobs().store().id()),
                "totally_custom",
                serde_json::json!({"target":"root", "value":"kept"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            Some(serde_json::json!({"value":"kept"}))
        );
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
