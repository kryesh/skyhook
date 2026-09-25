//! Tool invocation entry points and shared execution services.

mod dispatch;
mod planning;
mod results;

pub use results::{ExecutionError, ExecutionResult};
pub(crate) use results::{StartedExecution, persist_completion};

use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    job::{JobError, JobManager, JobOutcome, JobSpec, JobState},
    remote::RemoteError,
    target::{ResolvedRoute, TargetRef, TargetRouter},
    tool::{
        ToolPlacement,
        authorization::{AuthorizationCoordinator, AuthorizationError, AuthorizationSubject},
        diagnostic::{
            Cause, Effects, FailureSite, Operation, PartialContext, PartialDiagnostic, PathFact,
            Subject,
        },
        policy::{Capability, CapabilitySet, PermissionUse, Policy},
        registry::{ExecutionEnvelope, JobLaunch, JobName},
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
    tool: Arc<crate::tool::RegisteredTool>,
    original_arguments: Value,
    handler_arguments: Value,
    envelope: ExecutionEnvelope,
}

struct InvocationPlan {
    origin: Option<crate::session::ModelCallOrigin>,
    agent: AgentId,
    tool: Arc<crate::tool::RegisteredTool>,
    original_arguments: Value,
    authorization_arguments: Value,
    caller_location: ExecutionLocation,
    execution_location: ExecutionLocation,
    permissions: Vec<PermissionUse>,
    path_facts: Vec<PathFact>,
    parent: Option<JobId>,
    launch: JobLaunch,
    dispatch: InvocationDispatch<PlannedRemote>,
    source: Option<SourcePlan<PlannedRemote>>,
}

/// The read of a tool's source argument, planned and authorized with the call it
/// feeds. A remote path is resolved and checked by the worker that reads it.
enum SourcePlan<R> {
    Local {
        path: PathBuf,
        location: ExecutionLocation,
    },
    Remote {
        remote: R,
        workspace: PathBuf,
        path: String,
    },
}

impl InvocationPlan {
    /// Only a locally admitted call can present another job's view.
    fn result_policy(&self) -> super::ToolResultPolicy {
        match &self.dispatch {
            InvocationDispatch::Local(Ok(admitted)) => admitted.result_policy(),
            _ => super::ToolResultPolicy::Value,
        }
    }
}

/// The remote payload is a planned route before authorization and a prepared
/// connection afterwards; local dispatch is unchanged by that transition.
enum InvocationDispatch<R> {
    /// Admission failures are reported by the job, after approval, as a handler would.
    Local(Result<super::registry::AdmittedInvocation, super::AdmissionError>),
    ReadError(Box<ToolOutput>),
    Remote {
        remote: R,
        arguments: Value,
    },
}

struct PlannedRemote {
    route: ResolvedRoute,
    router: TargetRouter,
}

/// A planned invocation whose job is published: the caller's outstanding work
/// from here on, before anything runs. Dropping it cancels the job.
pub(crate) struct CreatedInvocation {
    kind: InvocationKind,
    plan: InvocationPlan,
    lease: crate::job::JobLease,
}

impl CreatedInvocation {
    /// The name the job was published under.
    pub(crate) fn job_name(&self) -> Option<&JobName> {
        self.plan.launch.name.as_ref()
    }

    pub(crate) fn result_policy(&self) -> super::ToolResultPolicy {
        self.plan.result_policy()
    }
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
    pub fn surface(&self) -> crate::tool::ToolSurface {
        self.shared.registry.surface(&self.capabilities)
    }

    /// Generate model and JavaScript tools for the receiving agent.
    #[must_use]
    pub fn surface_for_agent(&self, agent: &AgentId) -> crate::tool::ToolSurface {
        self.shared
            .registry
            .surface_for_agent(&self.capabilities, agent)
    }

    #[must_use]
    pub(crate) fn with_target_router(mut self, router: TargetRouter) -> Self {
        Arc::make_mut(&mut self.shared).router = Some(router);
        self
    }

    #[must_use]
    pub fn registry(&self) -> &ToolRegistry {
        &self.shared.registry
    }

    pub(crate) fn diagnostic_viewer(&self) -> super::diagnostic::DiagnosticViewer<'_> {
        super::diagnostic::DiagnosticViewer::new(&self.capabilities, &self.caller_location)
    }

    #[must_use]
    pub fn jobs(&self) -> &JobManager {
        &self.shared.jobs
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

    #[cfg(test)]
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

    #[cfg(test)]
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
        let created = self.create(kind, agent, name, arguments, parent).await?;
        self.run(created).await
    }

    /// Plan a model call and publish its job without running it, so the calls of
    /// one response can all be outstanding before any of them runs.
    pub(crate) async fn create_model(
        &self,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> Result<CreatedInvocation, ExecutionError> {
        self.create(InvocationKind::Model, agent, name, arguments, parent)
            .await
    }

    /// Plan a script call and publish its job without running it.
    pub(crate) async fn create_script(
        &self,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> Result<CreatedInvocation, ExecutionError> {
        self.create(InvocationKind::Script, agent, name, arguments, parent)
            .await
    }

    async fn create(
        &self,
        kind: InvocationKind,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> Result<CreatedInvocation, ExecutionError> {
        let plan = self
            .plan_registered(kind, agent, name, arguments, parent)
            .await
            .map_err(|error| {
                let host = self
                    .shared
                    .registry
                    .get(name)
                    .is_none_or(|tool| tool.placement() == ToolPlacement::Host);
                let fallback =
                    PartialContext::new(Operation::Validate, Subject::Tool(name.to_owned()))
                        .at(FailureSite::bound(&self.caller_location, host));
                error.or(fallback, &self.capabilities)
            })?;
        // The plan's borrow ends before the append: admitted handlers are not `Sync`.
        let (spec, fallback) = (self.job_spec(&plan), prepare_fallback(&plan));
        let lease = self
            .shared
            .jobs
            .create(spec)
            .await
            .map_err(|error| ExecutionError::from(error).or(fallback, &self.capabilities))?;
        Ok(CreatedInvocation { kind, plan, lease })
    }

    /// Authorize, launch and collect a created invocation.
    pub(crate) async fn run(
        &self,
        created: CreatedInvocation,
    ) -> Result<ExecutionResult, ExecutionError> {
        let CreatedInvocation { kind, plan, lease } = created;
        let result_policy = plan.result_policy();
        let fallback = prepare_fallback(&plan);
        let started = self
            .start(plan, lease)
            .await
            .map_err(|error| error.or(fallback, &self.capabilities))?;
        match kind {
            InvocationKind::Model if result_policy != super::ToolResultPolicy::JobView => {
                self.collect_model_started(started).await
            }
            InvocationKind::Model | InvocationKind::Script => {
                self.collect_full_view_started(started, result_policy).await
            }
            InvocationKind::Host => self.collect_started(started).await,
        }
    }
}

fn prepare_fallback(plan: &InvocationPlan) -> PartialContext {
    PartialContext::new(
        Operation::Prepare,
        Subject::Tool(plan.tool.name().to_owned()),
    )
    .at(FailureSite::bound(
        &plan.execution_location,
        plan.tool.placement() == ToolPlacement::Host,
    ))
}
