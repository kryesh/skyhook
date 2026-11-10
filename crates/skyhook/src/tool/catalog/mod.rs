//! The host's tool registry: registration, execution metadata, capability-scoped
//! surfaces, and what models see of it.

mod docs;
mod envelope;
mod options;
mod registration;
mod schema;
mod surface;

pub(crate) use docs::{DOCUMENTED, definitions, job_view_type};
pub use envelope::{BACKGROUND, split_envelope};
pub use envelope::{ExecutionEnvelope, JobLaunch, TARGET};
pub use options::ToolOptions;
pub(crate) use options::{Flight, FlownInvocation};
pub use registration::ToolRegistryBuilder;
pub use schema::{MAX_TOOL_NAME_BYTES, is_tool_name_char, is_valid_tool_name};
pub use surface::AgentLevel;
pub use surface::{ToolDefinition, ToolRegistry, ToolSpec, ToolSurface};

use std::{future::Future, sync::Arc};

use skyhook_tools::{arguments::Arguments, path::PathArgument};

use futures_util::future::BoxFuture;
use serde_json::Value;
use thiserror::Error;

use crate::job::{Completion, JobRole, output::PresentedOutput};
use crate::remote::{Landed, PreparedConnection};
use crate::tool::policy::{Capability, CapabilitySet, PermissionUse, ResourceId};
use crate::tool::{ToolContext, ToolError, ToolOutput};
use skyhook_tools::diagnostic::FailureSite;
use skyhook_tools::target::ExecutionLocation;

use skyhook_tools::error::AdmissionError;

/// Whether an admitted call returns a native value, a manager-produced job view,
/// or nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolResultPolicy {
    Value,
    JobView,
    Nothing,
}

/// Where a tool runs: on the host, or where its caller works, or where its `target`
/// selects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolPlacement {
    #[default]
    Host,
    InheritWorkspace,
    TargetedWorkspace,
}

impl ToolPlacement {
    /// Where a tool so placed fails: the host, or `location`, where it runs.
    pub fn site(self, location: &ExecutionLocation) -> FailureSite {
        match self {
            Self::Host => FailureSite::Host,
            Self::InheritWorkspace | Self::TargetedWorkspace => {
                FailureSite::Execution(location.clone())
            }
        }
    }
}

/// A native value serialized as a tool's output.
pub fn serialized(value: impl serde::Serialize) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput::new(serde_json::to_value(value)?))
}

pub struct RegisteredTool {
    definition: surface::GeneratedToolDefinition,
    execution: ToolExecution,
    admit: ArgumentAdmission,
}

type InvocationFuture = BoxFuture<'static, Result<Completion, ToolError>>;

/// A one-shot handler with its admitted input retained, not reconstructed from JSON.
/// The closure erases the input type without a downcast or a mismatched tool/input pair.
/// The constructor that makes an invocation decides its result policy, so a handler
/// states its result's kind once.
pub struct Invocation {
    run: Box<dyn FnOnce(ToolContext) -> InvocationFuture + Send>,
    policy: ToolResultPolicy,
}

impl Invocation {
    pub fn new<Fut>(run: impl FnOnce(ToolContext) -> Fut + Send + 'static) -> Self
    where
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        Self::completing(move |context| {
            let run = run(context);
            async move { run.await.map(Into::into) }
        })
    }

    /// An invocation completing with what `run` completes with.
    pub fn completing<Fut>(run: impl FnOnce(ToolContext) -> Fut + Send + 'static) -> Self
    where
        Fut: Future<Output = Result<Completion, ToolError>> + Send + 'static,
    {
        Self {
            run: Box::new(move |context| Box::pin(run(context))),
            policy: ToolResultPolicy::Value,
        }
    }

    pub(crate) fn result_policy(&self) -> ToolResultPolicy {
        self.policy
    }

    /// A child agent call's invocation, which completes with its child's final reply.
    pub(crate) fn reply<Fut>(run: impl FnOnce(ToolContext) -> Fut + Send + 'static) -> Self
    where
        Fut: Future<Output = Result<String, ToolError>> + Send + 'static,
    {
        Self::completing(move |context| {
            let run = run(context);
            async move { run.await.map(Completion::Reply) }
        })
    }

    pub fn call(self, context: ToolContext) -> InvocationFuture {
        (self.run)(context)
    }

    /// An invocation responding with another job's presented view.
    pub(crate) fn job_view<Fut>(run: impl FnOnce(ToolContext) -> Fut + Send + 'static) -> Self
    where
        Fut: Future<Output = Result<PresentedOutput, ToolError>> + Send + 'static,
    {
        let invocation =
            Self::new(move |context| async move { Ok(run(context).await?.into_output()) });
        invocation.with_policy(ToolResultPolicy::JobView)
    }

    /// An invocation producing nothing: its job completes without a result.
    pub fn unit<Fut>(run: impl FnOnce(ToolContext) -> Fut + Send + 'static) -> Self
    where
        Fut: Future<Output = Result<(), ToolError>> + Send + 'static,
    {
        let invocation = Self::new(move |context| {
            let run = run(context);
            async move { run.await.map(|()| ToolOutput::new(Value::Null)) }
        });
        invocation.with_policy(ToolResultPolicy::Nothing)
    }

    fn with_policy(self, policy: ToolResultPolicy) -> Self {
        Self { policy, ..self }
    }
}

type ArgumentAdmission = Arc<dyn Fn(&Arguments) -> Result<Admitted, AdmissionError> + Send + Sync>;

/// A call's handler input, admitted once before authorization. Its paths are
/// resolved in place, then it becomes the invocation.
pub(crate) type Admitted = Box<dyn AdmittedInput>;

pub(crate) trait AdmittedInput: Send {
    /// Filesystem inputs/outputs the admitted input names.
    fn paths(&mut self) -> Vec<PathArgument<'_>>;
    /// Permissions derived from the admitted input.
    fn permissions(
        &self,
        location: &ExecutionLocation,
    ) -> Result<Vec<PermissionUse>, AdmissionError>;
    /// Where the admitted input places its job, if not where the tool runs.
    fn job_location(&self) -> Option<JobLocation>;
    /// The schema the call's result follows when its tool chooses one per call.
    fn result_schema(&self) -> Option<Value>;
    /// The source file whose bytes the call reads, on whichever target holds it.
    fn source(&self) -> Option<&skyhook_tools::target::TargetedPath>;
    /// Start the call's operation on `connection` in the flight that resolves its
    /// paths there, where they landed: none starts when its tool has no flight or it
    /// names no path. A flown call's invocation runs that operation, as it performs or
    /// resumed once held there.
    fn fly(
        &mut self,
        connection: &PreparedConnection,
        location: &ExecutionLocation,
        spills: skyhook_tools::spec::Spills,
    ) -> Option<BoxFuture<'static, Result<Landed, AdmissionError>>>;
    fn invoke(self: Box<Self>) -> Invocation;
}

/// Where a host tool's input places its job, as the caller would select it: a
/// target, and a workspace absolute or relative to the target's.
pub struct JobLocation {
    pub target: Option<skyhook_tools::target::TargetRef>,
    pub workspace: Option<skyhook_worker::op::TargetPath>,
}

/// Who an invocation comes from: the host, a model's tool call, or a script.
#[derive(Clone, Copy)]
pub enum InvocationKind {
    Host,
    Model,
    Script,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptBinding {
    TopLevel,
    Unavailable,
}

#[derive(Clone, Default)]
struct ToolExecution {
    job_role: JobRole,
    capabilities: Vec<Capability>,
    accepts_input: bool,
    supports_name: bool,
    placement: ToolPlacement,
    permission_resource: Option<ResourceId>,
    managed_agent: bool,
}

impl RegisteredTool {
    pub const fn job_role(&self) -> JobRole {
        self.execution.job_role
    }

    pub fn output_schema(&self, capabilities: &CapabilitySet) -> Option<Value> {
        self.definition
            .output_schema
            .as_ref()
            .map(|schema| schema.generate(capabilities))
    }

    /// Admit handler input before allocating a job or requesting approval.
    pub(crate) fn admit(&self, arguments: &Arguments) -> Result<Admitted, AdmissionError> {
        (self.admit)(arguments)
    }

    pub fn capabilities(&self) -> Vec<Capability> {
        self.execution.capabilities.clone()
    }

    pub fn name(&self) -> &str {
        &self.definition.name
    }

    pub const fn placement(&self) -> ToolPlacement {
        self.execution.placement
    }

    pub const fn supports_background(&self) -> bool {
        self.definition.supports_background
    }

    pub const fn accepts_input(&self) -> bool {
        self.execution.accepts_input
    }

    pub(crate) fn permission_resource(&self) -> Option<&ResourceId> {
        self.execution.permission_resource.as_ref()
    }

    pub const fn managed_agent(&self) -> bool {
        self.execution.managed_agent
    }
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("invalid tool name `{0}`")]
    InvalidName(String),
    #[error("duplicate tool `{0}`")]
    Duplicate(String),
    #[error("tool schema is invalid: {0}")]
    Schema(String),
    #[error("`bg` is reserved by the harness")]
    ReservedBackground,
    #[error("`target` is reserved for structurally targeted tools")]
    ReservedTarget,
}
