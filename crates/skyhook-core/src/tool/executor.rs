use serde_json::Value;
use std::{future::Future, path::PathBuf, sync::Arc};
use thiserror::Error;

use crate::{
    identity::{AgentId, JobId},
    job::{JobError, JobManager, JobState},
    target::ROOT_TARGET,
    tool::{
        builtins::workspace::resolve_for_authorization,
        policy::{AuthorizationRequest, PathAccess, Policy, PolicyDecision, ToolEffect},
    },
};

use super::{
    ScriptBinding, ToolContext, ToolError, ToolExposure, ToolOutput, ToolRegistry,
    ToolVisibilityContext,
};

#[derive(Clone, Copy)]
enum InvocationKind {
    Host,
    Model,
    Script,
}

#[derive(Clone)]
pub struct ToolExecutor {
    registry: ToolRegistry,
    policy: Arc<dyn Policy>,
    jobs: JobManager,
    workspace: PathBuf,
    authorization_root: PathBuf,
    target: String,
}

impl ToolExecutor {
    #[must_use]
    pub fn new(
        registry: ToolRegistry,
        policy: Arc<dyn Policy>,
        jobs: JobManager,
        workspace: PathBuf,
    ) -> Self {
        Self {
            registry,
            policy,
            jobs,
            authorization_root: workspace.clone(),
            workspace,
            target: ROOT_TARGET.to_owned(),
        }
    }

    #[must_use]
    pub(crate) fn with_workspace(mut self, workspace: PathBuf) -> Self {
        self.workspace = workspace;
        self
    }

    #[must_use]
    pub(crate) fn with_authorization_root(mut self, root: PathBuf) -> Self {
        self.authorization_root = root;
        self
    }

    #[must_use]
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    #[must_use]
    pub fn jobs(&self) -> &JobManager {
        &self.jobs
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
        let tool = self
            .registry
            .get(name)
            .ok_or_else(|| ExecutionError::UnknownTool(name.to_owned()))?;
        let started = self
            .start_with(
                kind,
                agent,
                tool.clone(),
                arguments,
                parent,
                Vec::new(),
                true,
                None,
                move |context, arguments| async move { tool.call(context, arguments).await },
            )
            .await?;
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
        let tool = self
            .registry
            .get(name)
            .ok_or_else(|| ExecutionError::UnknownTool(name.to_owned()))?;
        self.start_with(
            InvocationKind::Host,
            agent,
            tool.clone(),
            arguments,
            parent,
            Vec::new(),
            true,
            Some(authorization_scope),
            move |context, arguments| async move { tool.call(context, arguments).await },
        )
        .await
    }

    /// Supervise a tool invocation whose implementation executes in another runtime.
    pub async fn execute_external<F, Fut>(
        &self,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
        extra_effects: Vec<crate::tool::policy::ToolEffect>,
        run: F,
    ) -> Result<ExecutionResult, ExecutionError>
    where
        F: FnOnce(Value) -> Fut + Send + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        let tool = self
            .registry
            .get(name)
            .ok_or_else(|| ExecutionError::UnknownTool(name.to_owned()))?;
        let started = self
            .start_with(
                InvocationKind::Model,
                agent,
                tool,
                arguments,
                parent,
                extra_effects,
                false,
                None,
                move |_context, arguments| run(arguments),
            )
            .await?;
        self.collect_started(started).await
    }

    pub(crate) async fn execute_external_with_context<F, Fut>(
        &self,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
        extra_effects: Vec<crate::tool::policy::ToolEffect>,
        run: F,
    ) -> Result<ExecutionResult, ExecutionError>
    where
        F: FnOnce(ToolContext, Value) -> Fut + Send + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        let tool = self
            .registry
            .get(name)
            .ok_or_else(|| ExecutionError::UnknownTool(name.to_owned()))?;
        let started = self
            .start_with(
                InvocationKind::Model,
                agent,
                tool,
                arguments,
                parent,
                extra_effects,
                false,
                None,
                run,
            )
            .await?;
        self.collect_started(started).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_with<F, Fut>(
        &self,
        kind: InvocationKind,
        agent: AgentId,
        tool: Arc<super::RegisteredTool>,
        arguments: Value,
        parent: Option<JobId>,
        extra_effects: Vec<crate::tool::policy::ToolEffect>,
        preflight_paths: bool,
        authorization_scope: Option<u64>,
        run: F,
    ) -> Result<StartedExecution, ExecutionError>
    where
        F: FnOnce(ToolContext, Value) -> Fut + Send + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        validate_invocation(&tool, &agent, kind)?;
        let original = arguments.clone();
        let (mut arguments, background) = self.registry.split_execution(&tool, arguments)?;
        let path_effects = if preflight_paths {
            preflight_path_arguments(
                &tool,
                &self.workspace,
                &self.authorization_root,
                &mut arguments,
            )
            .await?
        } else {
            Vec::new()
        };
        let mut effects = tool.effects_for(&arguments)?;
        for effect in &path_effects {
            if let ToolEffect::ExternalPath { access, .. } = effect {
                effects.retain(|candidate| {
                    !matches!(
                        (access, candidate),
                        (PathAccess::Read, ToolEffect::ReadWorkspace)
                            | (PathAccess::Write, ToolEffect::WriteWorkspace)
                    )
                });
            }
        }
        effects.extend(path_effects);
        effects.extend(extra_effects);
        let authorization_scope = match (authorization_scope, parent) {
            (Some(scope), _) => Some(scope),
            (None, Some(parent)) => self.jobs.authorization_scope(parent).await?,
            (None, None) => None,
        };
        let lease = self
            .jobs
            .create(
                agent.clone(),
                parent,
                tool.name.clone(),
                original.clone(),
                tool.accepts_input,
                background,
                authorization_scope,
            )
            .await?;
        self.jobs
            .transition(lease.id, JobState::AwaitingApproval)
            .await?;
        let decision = self
            .authorize(
                &lease.cancellation,
                &lease.cancellation_notify,
                AuthorizationRequest {
                    agent: agent.clone(),
                    job: lease.id,
                    tool: tool.name.clone(),
                    target: self.target.clone(),
                    parent,
                    scope: authorization_scope,
                    effects,
                    arguments: original,
                },
            )
            .await;
        let Some(decision) = decision else {
            self.finish_cancelled(lease.id).await;
            return Err(ExecutionError::Failed {
                message: "tool was cancelled".to_owned(),
                output: None,
            });
        };
        if let PolicyDecision::Deny { reason } = decision {
            self.jobs
                .finish(lease.id, Err(reason.clone()), None)
                .await?;
            return Err(ExecutionError::Denied(reason));
        }
        self.jobs.transition(lease.id, JobState::Running).await?;
        let context = ToolContext::new(
            agent,
            lease.id,
            self.workspace.clone(),
            lease.cancellation.clone(),
            lease.cancellation_notify,
            lease.input,
            self.jobs.progress_sink(lease.id),
        );
        let jobs = self.jobs.clone();
        let job = lease.id;
        let worker = tokio::spawn(run(context, arguments));
        self.jobs.attach_task(job, worker.abort_handle()).await?;
        tokio::spawn(async move {
            let completion = match worker.await {
                Ok(Ok(output)) => JobResult::Completed(output),
                Ok(Err(ToolError::Cancelled)) => JobResult::Cancelled,
                Ok(Err(ToolError::FailedWithOutput { message, output })) => {
                    JobResult::FailedWithOutput { message, output }
                }
                Ok(Err(error)) => JobResult::Failed(error.concise_message()),
                Err(error) if error.is_cancelled() => JobResult::Cancelled,
                Err(_) => JobResult::Failed("tool handler panicked".to_owned()),
            };
            persist_completion(&jobs, job, completion).await;
        });
        Ok(StartedExecution { job, background })
    }

    pub(crate) async fn collect_started(
        &self,
        started: StartedExecution,
    ) -> Result<ExecutionResult, ExecutionError> {
        let StartedExecution { job, background } = started;
        if background {
            let envelope = self.jobs.snapshot(job).await?;
            return Ok(ExecutionResult {
                job,
                background: true,
                output: ToolOutput::new(serde_json::to_value(envelope)?),
            });
        }
        let envelope = self.jobs.wait_foreground(job).await?;
        if !envelope.state.is_terminal() {
            return Ok(ExecutionResult {
                job,
                background: true,
                output: ToolOutput::new(serde_json::to_value(envelope)?),
            });
        }
        self.jobs.claim(job).await?;
        let images = self.jobs.images(job).await?;
        if envelope.state == JobState::Completed {
            Ok(ExecutionResult {
                job,
                background: false,
                output: ToolOutput {
                    value: envelope.output.unwrap_or(Value::Null),
                    images,
                },
            })
        } else {
            Err(ExecutionError::Failed {
                message: envelope
                    .error
                    .unwrap_or_else(|| format!("job ended as {:?}", envelope.state)),
                output: envelope.output.map(|value| ToolOutput { value, images }),
            })
        }
    }

    async fn authorize(
        &self,
        cancellation: &Arc<std::sync::atomic::AtomicBool>,
        cancellation_notify: &Arc<tokio::sync::Notify>,
        request: AuthorizationRequest,
    ) -> Option<PolicyDecision> {
        let authorization = self.policy.authorize(request);
        tokio::pin!(authorization);
        let cancelled = wait_for_cancellation(cancellation, cancellation_notify);
        tokio::pin!(cancelled);
        tokio::select! {
            decision = &mut authorization => Some(decision),
            () = &mut cancelled => None,
        }
    }

    async fn finish_cancelled(&self, job: JobId) {
        if let Err(error) = self
            .jobs
            .finish(
                job,
                Err("tool was cancelled".to_owned()),
                Some(JobState::Cancelled),
            )
            .await
            && !matches!(error, JobError::AlreadyTerminal(_))
        {
            self.jobs
                .fail_volatile(
                    job,
                    format!("job cancellation could not be persisted: {error}"),
                )
                .await;
        }
    }
}

async fn preflight_path_arguments(
    tool: &super::RegisteredTool,
    workspace: &std::path::Path,
    authorization_root: &std::path::Path,
    arguments: &mut Value,
) -> Result<Vec<ToolEffect>, ToolError> {
    let object = arguments
        .as_object_mut()
        .ok_or(ToolError::ArgumentsMustBeObject)?;
    let remote_target = object
        .get("target")
        .and_then(Value::as_str)
        .is_some_and(|target| target != ROOT_TARGET);
    let mut effects = Vec::new();
    for spec in tool.path_arguments() {
        if spec.skip_for_remote_target && remote_target {
            continue;
        }
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
            effects.push(ToolEffect::ExternalPath {
                path: resolved.path,
                access: spec.access,
                directory: resolved.directory,
            });
        }
    }
    Ok(effects)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct StartedExecution {
    pub job: JobId,
    background: bool,
}

enum JobResult {
    Completed(ToolOutput),
    Failed(String),
    FailedWithOutput { message: String, output: ToolOutput },
    Cancelled,
}

async fn persist_completion(jobs: &JobManager, job: JobId, completion: JobResult) {
    let result = match completion {
        JobResult::Completed(output) => jobs.finish(job, Ok(output), None).await,
        JobResult::Failed(message) => jobs.finish(job, Err(message), None).await,
        JobResult::FailedWithOutput { message, output } => {
            jobs.finish_failed(job, message, Some(output), None).await
        }
        JobResult::Cancelled => {
            jobs.finish(
                job,
                Err("tool was cancelled".to_owned()),
                Some(JobState::Cancelled),
            )
            .await
        }
    };
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

async fn wait_for_cancellation(
    cancellation: &std::sync::atomic::AtomicBool,
    cancellation_notify: &Arc<tokio::sync::Notify>,
) {
    loop {
        let notified = cancellation_notify.clone().notified_owned();
        if cancellation.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        notified.await;
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
    pub(crate) fn concise_message(&self) -> String {
        match self {
            Self::Tool(error) => error.concise_message(),
            Self::Failed { message, .. } => message.clone(),
            _ => self.to_string(),
        }
    }
}

fn validate_invocation(
    tool: &super::RegisteredTool,
    agent: &AgentId,
    kind: InvocationKind,
) -> Result<(), ExecutionError> {
    if !tool.is_available(&ToolVisibilityContext::new(agent.clone())) {
        return Err(ExecutionError::UnavailableTool(tool.name.clone()));
    }
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
