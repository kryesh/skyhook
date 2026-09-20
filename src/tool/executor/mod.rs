//! Tool invocation entry points and shared execution services.

mod dispatch;
mod planning;
mod results;

pub use results::{ExecutionError, ExecutionResult};
pub(crate) use results::{ExecutionFailure, StartedExecution, persist_completion};

use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    job::{JobError, JobManager, JobOutcome, JobSpec, JobState},
    remote::RemoteError,
    target::{ROOT_TARGET, ResolvedRoute, TargetRouter},
    tool::{
        ToolPlacement,
        authorization::{AuthorizationCoordinator, AuthorizationError, AuthorizationSubject},
        policy::{Capability, CapabilitySet, PermissionUse, Policy, ResourceId},
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
    background: bool,
    job_name: Option<String>,
    authorization_scope: Option<u64>,
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
    parent: Option<JobId>,
    authorization_scope: Option<u64>,
    background: bool,
    job_name: Option<String>,
    dispatch: InvocationDispatch<PlannedRemote>,
}

/// The remote payload is a planned route before authorization and a prepared
/// connection afterwards; local dispatch is unchanged by that transition.
enum InvocationDispatch<R> {
    /// Admission failures are reported by the job, after approval, as a handler would.
    Local(Result<super::registry::AdmittedInvocation, super::AdmissionError>),
    ReadError(ToolOutput),
    Remote {
        remote: R,
        arguments: Value,
    },
}

struct PlannedRemote {
    route: ResolvedRoute,
    router: TargetRouter,
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
            .plan_registered(kind, agent, name, arguments, parent)
            .await?;
        let result_policy = plan.tool.result_policy();
        let started = self.start(plan).await?;
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
