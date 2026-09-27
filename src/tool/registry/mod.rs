//! Tool registration, execution metadata, and capability-scoped surfaces.

mod docs;
mod envelope;
mod options;
mod registration;
mod schema;
mod surface;

pub(crate) use docs::{ScriptManifest, job_view_type};
pub(crate) use envelope::{BACKGROUND, split_envelope};
pub use envelope::{ExecutionEnvelope, JobLaunch, JobName, TARGET};
pub use options::ToolOptions;
pub use registration::CatalogBuilder;
pub(crate) use registration::result_schema;
pub(crate) use schema::{MAX_TOOL_NAME_BYTES, is_tool_name_char, is_valid_tool_name};
pub(crate) use surface::AgentLevel;
pub use surface::{Catalog, ResultSchema, ToolSpec, ToolSurface};

use std::{future::Future, sync::Arc};

use futures_util::future::BoxFuture;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{
    execution::ExecutionLocation,
    tool::policy::{Capability, CapabilitySet, PathAccess, PermissionUse, ResourceId},
};

use super::{ToolContext, ToolError, ToolOutput};
use crate::tool::invocation::{AdmissionError, OperationError};

pub type RegisteredTool = CatalogEntry<ToolContext, ToolOutput>;
pub type ToolRegistry = Catalog<ToolContext, ToolOutput>;
pub type ToolRegistryBuilder = CatalogBuilder<ToolContext, ToolOutput>;
pub(crate) type AdmittedInvocation = Invocation<ToolContext, ToolOutput>;

/// A call's arguments once validated: always a JSON object.
pub type Arguments = Map<String, Value>;

pub trait OutputValue: std::fmt::Debug + Send + 'static {
    fn from_value(value: Value) -> Self;
    fn with_diagnostic(self, diagnostic: super::diagnostic::PartialDiagnostic) -> Self;
}

impl<K: std::fmt::Debug + Send + 'static> OutputValue for super::output::Output<K> {
    fn from_value(value: Value) -> Self {
        Self::new(value)
    }
    fn with_diagnostic(self, diagnostic: super::diagnostic::PartialDiagnostic) -> Self {
        self.with_diagnostic(diagnostic)
    }
}

/// A native value serialized as a tool's output.
pub(crate) fn serialized<O: OutputValue>(
    value: impl serde::Serialize,
) -> Result<O, OperationError<O>> {
    Ok(O::from_value(serde_json::to_value(value)?))
}

pub struct CatalogEntry<C, O: OutputValue> {
    definition: surface::GeneratedToolDefinition,
    execution: ToolExecution,
    admit: ArgumentAdmission<C, O>,
}

/// A one-shot handler with its admitted input retained, not reconstructed from JSON.
/// The closure erases the input type without a downcast or a mismatched tool/input pair.
type InvocationFuture<O> = BoxFuture<'static, Result<O, OperationError<O>>>;

pub(crate) struct Invocation<C, O> {
    run: Box<dyn FnOnce(C) -> InvocationFuture<O> + Send>,
    result_policy: ToolResultPolicy,
}

impl<C, O> Invocation<C, O> {
    pub(crate) fn new<Fut>(run: impl FnOnce(C) -> Fut + Send + 'static) -> Self
    where
        Fut: Future<Output = Result<O, OperationError<O>>> + Send + 'static,
    {
        Self {
            run: Box::new(move |context| Box::pin(run(context))),
            result_policy: ToolResultPolicy::Value,
        }
    }

    pub(crate) const fn result_policy(&self) -> ToolResultPolicy {
        self.result_policy
    }

    pub(crate) fn call(self, context: C) -> InvocationFuture<O> {
        (self.run)(context)
    }
}

impl<C, O: OutputValue> Invocation<C, O> {
    /// An invocation whose job completes without a result.
    pub(crate) fn unit<Fut>(run: impl FnOnce(C) -> Fut + Send + 'static) -> Self
    where
        Fut: Future<Output = Result<(), OperationError<O>>> + Send + 'static,
    {
        Self {
            result_policy: ToolResultPolicy::Nothing,
            ..Self::new(move |context| {
                let run = run(context);
                async move { run.await.map(|()| O::from_value(Value::Null)) }
            })
        }
    }
}

impl AdmittedInvocation {
    /// An invocation whose response is another job's presented view rather than
    /// a native value wrapped in this call's own envelope.
    pub(crate) fn job_view<Fut>(run: impl FnOnce(ToolContext) -> Fut + Send + 'static) -> Self
    where
        Fut: Future<Output = Result<crate::job::output::PresentedOutput, ToolError>>
            + Send
            + 'static,
    {
        Self {
            result_policy: ToolResultPolicy::JobView,
            ..Self::new(move |context| async move {
                let (_, view, images) = run(context).await?.into_parts();
                Ok(ToolOutput::new(view).with_images(images))
            })
        }
    }
}

type ArgumentAdmission<C, O> =
    Arc<dyn Fn(&Arguments) -> Result<Admitted<C, O>, AdmissionError> + Send + Sync>;

/// A call's handler input, admitted once before authorization. Its paths are
/// resolved in place, then it becomes the invocation.
pub(crate) struct Admitted<C, O>(Box<dyn AdmittedInput<C, O>>);

pub(super) trait AdmittedInput<C, O>: Send {
    fn paths(&mut self) -> Vec<PathArgument<'_>>;
    fn permissions(
        &self,
        location: &ExecutionLocation,
    ) -> Result<Vec<PermissionUse>, AdmissionError>;
    fn job_location(&self) -> Option<JobLocation>;
    fn invoke(self: Box<Self>) -> Invocation<C, O>;
}

/// Where a host tool's input places its job, as the caller would select it: a
/// target, and a workspace absolute or relative to the target's.
pub(crate) struct JobLocation {
    pub(crate) target: Option<crate::target::TargetRef>,
    pub(crate) workspace: Option<std::path::PathBuf>,
}

impl<C, O> Admitted<C, O> {
    pub(super) fn new(input: impl AdmittedInput<C, O> + 'static) -> Self {
        Self(Box::new(input))
    }

    /// Filesystem inputs/outputs the admitted input names.
    pub(crate) fn paths(&mut self) -> Vec<PathArgument<'_>> {
        self.0.paths()
    }

    /// Permissions derived from the admitted input.
    pub(crate) fn permissions(
        &self,
        location: &ExecutionLocation,
    ) -> Result<Vec<PermissionUse>, AdmissionError> {
        self.0.permissions(location)
    }

    /// Where the admitted input places its job, if not where the tool runs.
    pub(crate) fn job_location(&self) -> Option<JobLocation> {
        self.0.job_location()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolExposure {
    ModelVisible,
    ScriptOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "binding", rename_all = "snake_case")]
pub enum ScriptBinding {
    TopLevel,
    JobMethod {
        method: &'static str,
        job_argument: &'static str,
    },
    Unavailable,
}

/// Whether an admitted call returns a native value, a manager-produced job view,
/// or nothing. Only `Invocation::job_view` selects `JobView`, and only
/// `Invocation::unit` selects `Nothing`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolResultPolicy {
    Value,
    JobView,
    Nothing,
}

/// Job views a tool returns as its native result. Scripts read them in full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobViewResult {
    One,
    Many,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolPlacement {
    #[default]
    Host,
    InheritWorkspace,
    TargetedWorkspace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    Existing,
    Writable,
    /// A writable path whose parent directories may not exist yet.
    WritableWithParents,
    Removable,
    /// An existing directory a process runs in.
    WorkingDirectory,
}

/// The workspace itself, as a path relative to it: what an omitted path
/// argument names.
pub(crate) const DEFAULT_PATH: &str = ".";

/// Whether a path inside the authorization root needs its own permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathScope {
    /// The tool's workspace scope covers it; outside the root it needs its own.
    Workspace,
    /// It always needs its own, as the tool's capabilities do not cover its access.
    Exact,
}

/// A path the admitted input names. It is resolved where the call runs, and the
/// handler receives its resolved spelling.
pub struct PathArgument<'a> {
    pub(crate) path: &'a mut String,
    pub(crate) access: PathAccess,
    pub(crate) kind: PathKind,
    pub(crate) scope: PathScope,
}

impl<'a> PathArgument<'a> {
    #[must_use]
    pub fn new(path: &'a mut String, access: PathAccess, kind: PathKind) -> Self {
        Self {
            path,
            access,
            kind,
            scope: PathScope::Workspace,
        }
    }

    /// The path needs its own permission even inside the authorization root.
    #[must_use]
    pub const fn exact(mut self) -> Self {
        self.scope = PathScope::Exact;
        self
    }
}

#[derive(Clone, Default)]
struct ToolExecution {
    job_role: crate::job::JobRole,
    capabilities: Vec<Capability>,
    accepts_input: bool,
    supports_name: bool,
    placement: ToolPlacement,
    permission_resource: Option<ResourceId>,
    read_error_output: Option<fn(&super::diagnostic::Diagnostic) -> Option<Value>>,
    target_authentication: bool,
    reads_source: bool,
}

/// The `{path, target?}` argument a source-reading tool declares.
pub(crate) const SOURCE_ARGUMENT: &str = "source";

impl<C: Send + 'static, O: OutputValue> CatalogEntry<C, O> {
    #[must_use]
    pub const fn job_role(&self) -> crate::job::JobRole {
        self.execution.job_role
    }

    pub(crate) fn output_schema(&self, capabilities: &CapabilitySet) -> Option<Value> {
        self.definition
            .output_schema
            .as_ref()
            .map(|schema| schema.generate(capabilities))
    }

    /// Admit handler input before allocating a job or requesting approval.
    pub(crate) fn admit(&self, arguments: &Arguments) -> Result<Admitted<C, O>, AdmissionError> {
        (self.admit)(arguments)
    }

    /// The admitted input's invocation, once its paths are resolved.
    pub(crate) fn invoke(&self, admitted: Admitted<C, O>) -> Invocation<C, O> {
        let invocation = admitted.0.invoke();
        let Some(convert) = self.execution.read_error_output else {
            return invocation;
        };
        Invocation {
            result_policy: invocation.result_policy(),
            ..Invocation::new(move |context| async move {
                invocation.call(context).await.or_else(|error| {
                    let output = (error.is_source_filesystem_io())
                        .then(|| convert(&error.diagnostic()))
                        .flatten();
                    match output {
                        Some(output) => {
                            Ok(O::from_value(output).with_diagnostic(error.into_facts().0))
                        }
                        None => Err(error),
                    }
                })
            })
        }
    }

    pub(crate) fn capabilities(&self) -> Vec<Capability> {
        self.execution.capabilities.clone()
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.definition.name
    }

    #[must_use]
    pub(crate) const fn placement(&self) -> ToolPlacement {
        self.execution.placement
    }

    /// Convert an error at a known source-filesystem boundary, such as path
    /// preflight. Handler failures must first establish their source provenance.
    pub(crate) fn read_error_output(
        &self,
        diagnostic: &super::diagnostic::Diagnostic,
    ) -> Option<Value> {
        self.execution.read_error_output?(diagnostic)
    }

    pub(crate) const fn accepts_input(&self) -> bool {
        self.execution.accepts_input
    }

    pub(crate) fn permission_resource(&self) -> Option<&ResourceId> {
        self.execution.permission_resource.as_ref()
    }

    pub(crate) const fn target_authentication(&self) -> bool {
        self.execution.target_authentication
    }

    pub(crate) const fn reads_source(&self) -> bool {
        self.execution.reads_source
    }
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("invalid tool name `{0}`")]
    InvalidName(String),
    #[error("duplicate tool `{0}`")]
    Duplicate(String),
    #[error(
        "`{0}` is reserved by the JavaScript tool API; use a different name or disable its script binding"
    )]
    ReservedScriptName(String),
    #[error("tool schema is invalid: {0}")]
    Schema(String),
    #[error("`bg` is reserved by the harness")]
    ReservedBackground,
    #[error("`target` is reserved for structurally targeted tools")]
    ReservedTarget,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn completed_read_errors_require_source_provenance_and_registered_policy() {
        use crate::tool::diagnostic::{Operation, Subject};

        for (opt_in, source) in [(true, true), (true, false), (false, true)] {
            let mut options = ToolOptions::default();
            if opt_in {
                options = options.read_error_output(|_| Some(json!({"kind":"error"})));
            }
            let mut builder = CatalogBuilder::<(), ToolOutput>::default();
            builder
                .register_dynamic(
                    "read",
                    "read policy fixture",
                    json!({"type":"object", "properties":{"path":{"type":"string"}}}),
                    options,
                    move |_, _| async move {
                        // Labels describe failures; only source provenance and opt-in
                        // authorize completed read errors, not the tool name or operation.
                        Err(if source {
                            ToolError::source_filesystem_io(
                                std::io::ErrorKind::PermissionDenied.into(),
                            )
                            .operation(Operation::StoreImage, Subject::Label("source".into()))
                        } else {
                            ToolError::io(std::io::ErrorKind::PermissionDenied.into())
                                .operation(Operation::Read, Subject::path("source"))
                        })
                    },
                )
                .unwrap();
            let registry = builder.build();
            let tool = registry.get("read").unwrap();
            let arguments = json!({"path": "/resolved/source"});
            let admitted = tool.admit(arguments.as_object().unwrap()).unwrap();
            let result = tool.invoke(admitted).call(()).await;
            assert_eq!(result.is_ok(), opt_in && source);
            if let Ok(output) = result {
                assert_eq!(output.value, json!({"kind":"error"}));
                assert!(output.diagnostic.is_some());
            }
        }
    }
}
