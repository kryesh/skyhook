//! Tool registration, execution metadata, and capability-scoped surfaces.

mod docs;
mod envelope;
mod schema;

pub(crate) use docs::{ScriptManifest, job_view_type};
pub(crate) use envelope::split_envelope;
pub use envelope::{ExecutionEnvelope, JobLaunch, JobName};
use schema::{
    add_nested_schema_property, add_schema_property, ensure_no_target, tags_first,
    validate_object_schema, validate_output_schema, validate_schema,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::Arc,
};

use futures_util::future::BoxFuture;
use schemars::{JsonSchema, generate::SchemaSettings};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
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

pub trait OutputValue: std::fmt::Debug + Send + 'static {
    fn from_value(value: Value) -> Self;
    fn with_diagnostic(self, diagnostic: super::diagnostic::PartialDiagnostic) -> Self;
}

impl OutputValue for ToolOutput {
    fn from_value(value: Value) -> Self {
        Self::new(value)
    }
    fn with_diagnostic(self, diagnostic: super::diagnostic::PartialDiagnostic) -> Self {
        self.with_diagnostic(diagnostic)
    }
}

#[derive(Clone)]
pub struct CatalogEntry<C, O: OutputValue> {
    definition: GeneratedToolDefinition,
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
    Arc<dyn Fn(Value) -> Result<Invocation<C, O>, AdmissionError> + Send + Sync>;
type ArgumentValidator<I> = Arc<dyn Fn(&I) -> Result<(), AdmissionError> + Send + Sync>;
type ArgumentPaths<I> = Arc<dyn Fn(&I) -> Result<Vec<PathArgument>, AdmissionError> + Send + Sync>;
type ArgumentPermissions<I> =
    Arc<dyn Fn(&ExecutionLocation, &I) -> Result<Vec<PermissionUse>, AdmissionError> + Send + Sync>;
type ArgumentCheck = Arc<
    dyn Fn(&ExecutionLocation, &Value) -> Result<CheckedArguments, AdmissionError> + Send + Sync,
>;

/// Tool-specific checks over a tool's parsed input, run before authorization.
struct ArgumentHooks<I> {
    validator: Option<ArgumentValidator<I>>,
    paths: Option<ArgumentPaths<I>>,
    permissions: Option<ArgumentPermissions<I>>,
}

impl<I> Clone for ArgumentHooks<I> {
    fn clone(&self) -> Self {
        Self {
            validator: self.validator.clone(),
            paths: self.paths.clone(),
            permissions: self.permissions.clone(),
        }
    }
}

impl<I> Default for ArgumentHooks<I> {
    fn default() -> Self {
        Self {
            validator: None,
            paths: None,
            permissions: None,
        }
    }
}

impl<I: DeserializeOwned + JsonSchema + 'static> ArgumentHooks<I> {
    /// One check that parses the arguments once for every hook. A tool without
    /// hooks parses nothing before admission.
    fn check(self) -> Option<ArgumentCheck> {
        let Self {
            validator,
            paths,
            permissions,
        } = self;
        if validator.is_none() && paths.is_none() && permissions.is_none() {
            return None;
        }
        Some(Arc::new(move |location, arguments| {
            let input = super::diagnostic::deserialize_arguments::<I>(arguments)?;
            if let Some(validate) = &validator {
                validate(&input)?;
            }
            Ok(CheckedArguments {
                paths: paths
                    .as_ref()
                    .map_or(Ok(Vec::new()), |paths| paths(&input))?,
                permissions: (permissions.as_ref())
                    .map_or(Ok(Vec::new()), |permissions| permissions(location, &input))?,
            })
        }))
    }
}

/// What a tool's checks derived from one invocation's arguments.
pub(crate) struct CheckedArguments {
    /// Filesystem inputs/outputs, resolved and rewritten before authorization.
    pub(crate) paths: Vec<PathArgument>,
    pub(crate) permissions: Vec<PermissionUse>,
}

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub supports_background: bool,
    pub job_role: crate::job::JobRole,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Handler result, without the background job alternative.
    pub result_schema: Option<Value>,
    pub output_schema: Option<Value>,
    pub exposure: ToolExposure,
    pub script_binding: ScriptBinding,
}

impl ToolSpec {
    pub(crate) fn validate_arguments(&self, arguments: &Value) -> Result<(), AdmissionError> {
        let arguments = arguments
            .as_object()
            .ok_or(AdmissionError::arguments_must_be_object())?;
        if self.input_schema["additionalProperties"] == false
            && let Some(argument) = arguments.keys().find(|argument| {
                !self.input_schema["properties"]
                    .as_object()
                    .is_some_and(|properties| properties.contains_key(*argument))
            })
        {
            return Err(AdmissionError::invalid_arguments(format!(
                "unknown argument `{argument}`"
            )));
        }
        Ok(())
    }
}

type SchemaGenerator = Arc<dyn Fn(&CapabilitySet) -> Value + Send + Sync>;

#[derive(Clone)]
enum OutputSchema {
    Static(Value),
    Generated(SchemaGenerator),
}

#[derive(Clone)]
struct GeneratedToolDefinition {
    name: String,
    description: String,
    input_schema: SchemaGenerator,
    output_schema: Option<OutputSchema>,
    exposure: ToolExposure,
    script_binding: ScriptBinding,
    supports_background: bool,
    preserve_required: bool,
    preserve_schema_dialect: bool,
    required: BTreeSet<Capability>,
    root_required: BTreeSet<Capability>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolExposure {
    ModelVisible,
    ScriptOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptBinding {
    TopLevel,
    JobMethod {
        method: String,
        job_argument: String,
    },
    Unavailable,
}

/// Whether an admitted call returns a native value or a manager-produced job view.
/// Only `Invocation::job_view` selects `JobView`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolResultPolicy {
    Value,
    JobView,
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
}

#[derive(Clone, Debug)]
pub(crate) enum PathBinding {
    TopLevel {
        name: String,
        default: Option<String>,
    },
    /// Nested bindings always contribute permission, even inside the root.
    Pointer(String),
}

#[derive(Clone, Debug)]
pub struct PathArgument {
    pub(crate) binding: PathBinding,
    pub(crate) access: PathAccess,
    pub(crate) kind: PathKind,
}

fn unidentified_pointer(pointer: &str) -> AdmissionError {
    AdmissionError::invalid_arguments(format!(
        "path pointer `{pointer}` does not identify an argument"
    ))
}

impl PathArgument {
    #[must_use]
    pub fn top_level(
        name: impl Into<String>,
        default: Option<String>,
        access: PathAccess,
        kind: PathKind,
    ) -> Self {
        let name = name.into();
        Self {
            binding: PathBinding::TopLevel { name, default },
            access,
            kind,
        }
    }

    pub fn pointer(pointer: impl Into<String>, access: PathAccess, kind: PathKind) -> Self {
        Self {
            binding: PathBinding::Pointer(pointer.into()),
            access,
            kind,
        }
    }

    pub(crate) fn name(&self) -> &str {
        match &self.binding {
            PathBinding::TopLevel { name, .. } => name,
            PathBinding::Pointer(pointer) => pointer,
        }
    }

    pub(crate) fn input<'a>(
        &'a self,
        arguments: &'a Value,
    ) -> Result<Option<&'a str>, AdmissionError> {
        let value = match &self.binding {
            PathBinding::TopLevel { name, .. } => arguments.get(name),
            PathBinding::Pointer(pointer) => arguments.pointer(pointer),
        };
        match value {
            Some(Value::String(value)) => Ok(Some(value)),
            Some(_) => Err(AdmissionError::invalid_arguments(format!(
                "{} must be a string",
                self.name()
            ))),
            None => match &self.binding {
                PathBinding::TopLevel { default, .. } => Ok(default.as_deref()),
                PathBinding::Pointer(pointer) => Err(unidentified_pointer(pointer)),
            },
        }
    }

    pub(crate) fn rewrite(
        &self,
        arguments: &mut Value,
        value: Value,
    ) -> Result<(), AdmissionError> {
        match &self.binding {
            PathBinding::TopLevel { name, .. } => {
                arguments
                    .as_object_mut()
                    .ok_or(AdmissionError::arguments_must_be_object())?
                    .insert(name.clone(), value);
            }
            PathBinding::Pointer(pointer) => {
                *arguments
                    .pointer_mut(pointer)
                    .ok_or_else(|| unidentified_pointer(pointer))? = value;
            }
        }
        Ok(())
    }
}

/// Registration options for a tool whose handler input is `I`; dynamic tools
/// take their arguments as JSON.
pub struct ToolOptions<I = Value> {
    execution: ToolExecution,
    hooks: ArgumentHooks<I>,
    pub supports_background: bool,
    preserve_required: bool,
    preserve_schema_dialect: bool,
    exposure: ToolExposure,
    script_binding: ScriptBinding,
    required: BTreeSet<Capability>,
    root_required: BTreeSet<Capability>,
    conditional_inputs: Vec<ConditionalProperty>,
    /// Like `conditional_inputs`, for properties of the result schema.
    conditional_outputs: Vec<ConditionalProperty>,
    output_schema: Option<OutputSchema>,
}

impl<I> Clone for ToolOptions<I> {
    fn clone(&self) -> Self {
        Self {
            execution: self.execution.clone(),
            hooks: self.hooks.clone(),
            supports_background: self.supports_background,
            preserve_required: self.preserve_required,
            preserve_schema_dialect: self.preserve_schema_dialect,
            exposure: self.exposure,
            script_binding: self.script_binding.clone(),
            required: self.required.clone(),
            root_required: self.root_required.clone(),
            conditional_inputs: self.conditional_inputs.clone(),
            conditional_outputs: self.conditional_outputs.clone(),
            output_schema: self.output_schema.clone(),
        }
    }
}

/// (object pointer, property name, the property's schema under an agent's capabilities)
type ConditionalProperty = (String, String, ComputedProperty);
type ComputedProperty = Arc<dyn Fn(&CapabilitySet) -> Option<Value> + Send + Sync>;

fn gated(capability: Capability, schema: Value) -> ComputedProperty {
    Arc::new(move |capabilities: &CapabilitySet| {
        capabilities.contains(capability).then(|| schema.clone())
    })
}

fn check_conditional(
    schema: &Value,
    properties: &[ConditionalProperty],
    kind: &str,
) -> Result<(), RegistryError> {
    match (properties.iter())
        .find(|(pointer, ..)| !schema.pointer(pointer).is_some_and(Value::is_object))
    {
        Some((pointer, ..)) => Err(RegistryError::Schema(format!(
            "conditional {kind} location `{pointer}` is not an object schema"
        ))),
        None => Ok(()),
    }
}

fn add_conditional(
    schema: &mut Value,
    properties: &[ConditionalProperty],
    capabilities: &CapabilitySet,
) {
    for (pointer, name, property) in properties {
        if let Some(property) = property(capabilities) {
            add_nested_schema_property(schema, pointer, name, property);
        }
    }
}

#[derive(Clone, Default)]
struct ToolExecution {
    job_role: crate::job::JobRole,
    pub capabilities: Vec<Capability>,
    pub accepts_input: bool,
    supports_name: bool,
    placement: ToolPlacement,
    permission_resource: Option<ResourceId>,
    path_arguments: Vec<PathArgument>,
    read_error_output: Option<fn(&str, &super::diagnostic::Diagnostic) -> Option<Value>>,
    target_authentication: bool,
    /// An argument naming a file, on any target, whose bytes the handler consumes.
    source_argument: Option<String>,
    check: Option<ArgumentCheck>,
}

impl<I> ToolOptions<I> {
    #[must_use]
    pub const fn job_role(mut self, role: crate::job::JobRole) -> Self {
        self.execution.job_role = role;
        self
    }

    /// Built-in read only: expected OS read failures are successful structured results.
    pub(crate) fn read_error_output(
        mut self,
        convert: fn(&str, &super::diagnostic::Diagnostic) -> Option<Value>,
    ) -> Self {
        self.execution.read_error_output = Some(convert);
        self
    }

    #[must_use]
    pub const fn placement(mut self, placement: ToolPlacement) -> Self {
        self.execution.placement = placement;
        self
    }

    /// Expose an optional kebab-case job label, handled by the executor.
    #[must_use]
    pub const fn named(mut self) -> Self {
        self.execution.supports_name = true;
        self
    }

    /// The `{path, target?}` argument `name` is a source file. The executor
    /// authorizes and loads it with the call, and the handler reads its bytes
    /// from the context. Its `target` follows the tool-level target rules.
    #[must_use]
    pub(crate) fn source_argument(mut self, name: &str) -> Self {
        self.execution.source_argument = Some(name.to_owned());
        self.conditional_nested_input(
            crate::target::TargetPath::SCHEMA,
            "target",
            Capability::Targets,
            crate::target::TargetPath::target_schema(),
        )
    }

    /// The handler spawns processes that may authenticate to configured targets.
    #[must_use]
    pub(crate) const fn target_authentication(mut self) -> Self {
        self.execution.target_authentication = true;
        self
    }

    #[must_use]
    pub fn new(capabilities: Vec<Capability>) -> Self {
        let required = capabilities.iter().copied().collect();
        Self {
            supports_background: false,
            preserve_required: false,
            preserve_schema_dialect: false,
            execution: ToolExecution {
                capabilities,
                ..ToolExecution::default()
            },
            hooks: ArgumentHooks::default(),
            exposure: ToolExposure::ModelVisible,
            script_binding: ScriptBinding::TopLevel,
            required,
            root_required: BTreeSet::new(),
            conditional_inputs: Vec::new(),
            conditional_outputs: Vec::new(),
            output_schema: None,
        }
    }

    /// Require a capability only for root agents. Child-to-parent communication
    /// can remain available without granting the child host-interaction rights.
    /// Like `requires`, this is an availability gate, not an execution permission.
    #[must_use]
    pub fn requires_for_root(mut self, capability: Capability) -> Self {
        self.root_required.insert(capability);
        self
    }

    /// Preserve JSON Schema required fields even when they have defaults.
    /// External tools need this because defaults are annotations, not values
    /// supplied by Skyhook or a typed argument deserializer.
    #[must_use]
    pub fn preserve_required(mut self) -> Self {
        self.preserve_required = true;
        self
    }

    /// Retain declared JSON Schema dialects for externally supplied schemas.
    #[must_use]
    pub fn preserve_schema_dialect(mut self) -> Self {
        self.preserve_schema_dialect = true;
        self
    }

    #[must_use]
    pub fn background(mut self) -> Self {
        self.supports_background = true;
        self
    }

    #[must_use]
    pub const fn input(mut self) -> Self {
        self.execution.accepts_input = true;
        self
    }

    #[must_use]
    pub fn script_only(mut self) -> Self {
        self.exposure = ToolExposure::ScriptOnly;
        self
    }

    #[must_use]
    pub fn requires(mut self, capability: Capability) -> Self {
        self.required.insert(capability);
        self
    }

    #[must_use]
    pub fn conditional_input(
        self,
        name: impl Into<String>,
        capability: Capability,
        schema: Value,
    ) -> Self {
        self.conditional_nested_input("", name, capability, schema)
    }

    /// Like `conditional_input`, for a property of the object schema at a JSON
    /// pointer, such as a nested definition (`/$defs/Options`).
    #[must_use]
    pub fn conditional_nested_input(
        mut self,
        pointer: impl Into<String>,
        name: impl Into<String>,
        capability: Capability,
        schema: Value,
    ) -> Self {
        self.conditional_inputs
            .push((pointer.into(), name.into(), gated(capability, schema)));
        self
    }

    /// A result property present only with `capability`, at the object schema
    /// at `pointer`, such as a nested definition (`/$defs/Options`).
    #[must_use]
    pub(crate) fn conditional_output(
        mut self,
        pointer: impl Into<String>,
        name: impl Into<String>,
        capability: Capability,
        schema: Value,
    ) -> Self {
        self.conditional_outputs
            .push((pointer.into(), name.into(), gated(capability, schema)));
        self
    }

    /// An input whose schema, or absence, follows the agent's capabilities.
    #[must_use]
    pub fn computed_input(
        mut self,
        name: impl Into<String>,
        schema: impl Fn(&CapabilitySet) -> Option<Value> + Send + Sync + 'static,
    ) -> Self {
        self.conditional_inputs
            .push((String::new(), name.into(), Arc::new(schema)));
        self
    }

    #[must_use]
    pub fn job_method(
        mut self,
        method: impl Into<String>,
        job_argument: impl Into<String>,
    ) -> Self {
        self.script_binding = ScriptBinding::JobMethod {
            method: method.into(),
            job_argument: job_argument.into(),
        };
        self
    }

    #[must_use]
    pub fn script_unavailable(mut self) -> Self {
        self.script_binding = ScriptBinding::Unavailable;
        self
    }

    #[must_use]
    pub(crate) fn path_argument(
        mut self,
        name: impl Into<String>,
        access: PathAccess,
        kind: PathKind,
    ) -> Self {
        self.execution
            .path_arguments
            .push(PathArgument::top_level(name, None, access, kind));
        self
    }

    #[must_use]
    pub(crate) fn default_path_argument(
        mut self,
        name: impl Into<String>,
        default: impl Into<String>,
        access: PathAccess,
        kind: PathKind,
    ) -> Self {
        self.execution.path_arguments.push(PathArgument::top_level(
            name,
            Some(default.into()),
            access,
            kind,
        ));
        self
    }

    /// Derive invocation-specific permissions from the validated input as
    /// submitted. These replace static permissions of the same capability. Remote path and
    /// network permissions are forwarded from the destination for host approval;
    /// other namespaces are authorized by the host before dispatch.
    #[must_use]
    pub fn argument_permissions(
        mut self,
        extract: impl Fn(&ExecutionLocation, &I) -> Result<Vec<PermissionUse>, AdmissionError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.hooks.permissions = Some(Arc::new(extract));
        self
    }

    /// Extract concrete JSON pointers (including array indices) to filesystem
    /// inputs/outputs. Each path is resolved and rewritten before authorization.
    /// Dynamic paths require their capability even when inside the workspace.
    #[must_use]
    pub fn argument_paths(
        mut self,
        extract: impl Fn(&I) -> Result<Vec<PathArgument>, AdmissionError> + Send + Sync + 'static,
    ) -> Self {
        self.hooks.paths = Some(Arc::new(extract));
        self
    }

    /// Validate tool-specific arguments before requesting authorization.
    #[must_use]
    pub fn argument_validator(
        mut self,
        validate: impl Fn(&I) -> Result<(), AdmissionError> + Send + Sync + 'static,
    ) -> Self {
        self.hooks.validator = Some(Arc::new(validate));
        self
    }

    #[must_use]
    pub fn output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(OutputSchema::Static(schema));
        self
    }

    #[must_use]
    pub fn generated_output_schema(
        mut self,
        generator: impl Fn(&CapabilitySet) -> Value + Send + Sync + 'static,
    ) -> Self {
        self.output_schema = Some(OutputSchema::Generated(Arc::new(generator)));
        self
    }

    #[must_use]
    pub fn permission_resource(mut self, resource: ResourceId) -> Self {
        self.execution.permission_resource = Some(resource);
        self
    }
}

impl<I> Default for ToolOptions<I> {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

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
    /// Native-path consumers retain wire spelling before IO; other consumers
    /// retain path-rewritten input. Expected read errors retain the requested path
    /// separately so authorization rewrites cannot change their result path.
    pub(crate) fn admit(
        &self,
        arguments: Value,
        requested_arguments: &Value,
    ) -> Result<Invocation<C, O>, AdmissionError> {
        let convert = self.execution.read_error_output.and_then(|convert| {
            let path = self
                .execution
                .path_arguments
                .first()?
                .input(requested_arguments)
                .ok()
                .flatten()?
                .to_owned();
            Some((path, convert))
        });
        let admitted = (self.admit)(arguments)?;
        Ok(Invocation {
            result_policy: admitted.result_policy(),
            ..Invocation::new(move |context| async move {
                admitted.call(context).await.or_else(|error| {
                    let output = convert.and_then(|(path, convert)| {
                        error
                            .is_source_filesystem_io()
                            .then(|| convert(&path, &error.diagnostic()))
                            .flatten()
                    });
                    match output {
                        Some(output) => {
                            Ok(O::from_value(output).with_diagnostic(error.into_facts().0))
                        }
                        None => Err(error),
                    }
                })
            })
        })
    }

    /// Validate the submitted arguments and derive their path arguments
    /// (declared ones first) and permissions, before authorization.
    pub(crate) fn check_arguments(
        &self,
        location: &ExecutionLocation,
        arguments: &Value,
    ) -> Result<CheckedArguments, AdmissionError> {
        let checked = match &self.execution.check {
            Some(check) => check(location, arguments)?,
            None => CheckedArguments {
                paths: Vec::new(),
                permissions: Vec::new(),
            },
        };
        Ok(CheckedArguments {
            paths: (self.execution.path_arguments.iter().cloned())
                .chain(checked.paths)
                .collect(),
            ..checked
        })
    }

    pub(crate) fn capabilities(&self) -> Vec<Capability> {
        self.execution.capabilities.clone()
    }

    pub(crate) fn spec(
        &self,
        capabilities: &CapabilitySet,
        agent: &crate::identity::AgentId,
    ) -> Option<ToolSpec> {
        let child = agent.parent().is_some();
        self.definition
            .generate_scoped(capabilities, &self.execution, child)
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
        path: &str,
        diagnostic: &super::diagnostic::Diagnostic,
    ) -> Option<Value> {
        self.execution.read_error_output?(path, diagnostic)
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

    pub(crate) fn source_argument(&self) -> Option<&str> {
        self.execution.source_argument.as_deref()
    }
}

#[derive(Clone, Default)]
pub struct ToolSurface {
    tools: BTreeMap<String, ToolSpec>,
    /// Result schemas documented by name, since the prompt defines `JobView` once.
    job_views: docs::JobViewSchemas,
}

impl ToolSurface {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    pub fn validate_arguments(&self, name: &str, arguments: &Value) -> Result<(), AdmissionError> {
        let tool = self
            .get(name)
            .ok_or_else(|| AdmissionError::unavailable(name))?;
        tool.validate_arguments(arguments)
    }

    pub(crate) fn script_manifests(&self) -> Vec<ScriptManifest> {
        self.tools
            .values()
            .filter_map(ScriptManifest::from_tool)
            .collect()
    }
}

impl<C, O: OutputValue> Default for Catalog<C, O> {
    fn default() -> Self {
        Self {
            tools: Arc::default(),
        }
    }
}
impl<C, O: OutputValue> Default for CatalogBuilder<C, O> {
    fn default() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }
}

type CatalogTools<C, O> = BTreeMap<String, Arc<CatalogEntry<C, O>>>;

#[derive(Clone)]
pub struct Catalog<C, O: OutputValue> {
    tools: Arc<CatalogTools<C, O>>,
}

impl<C: Send + 'static, O: OutputValue> Catalog<C, O> {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<CatalogEntry<C, O>>> {
        self.tools.get(name).cloned()
    }

    pub fn tools(&self) -> impl Iterator<Item = &Arc<CatalogEntry<C, O>>> {
        self.tools.values()
    }

    #[must_use]
    pub fn surface(&self, capabilities: &CapabilitySet) -> ToolSurface {
        self.surface_scoped(capabilities, false)
    }

    /// Generate a surface for the receiving agent, preserving root-only requirements.
    #[must_use]
    pub fn surface_for_agent(
        &self,
        capabilities: &CapabilitySet,
        agent: &crate::identity::AgentId,
    ) -> ToolSurface {
        self.surface_scoped(capabilities, agent.parent().is_some())
    }

    fn surface_scoped(&self, capabilities: &CapabilitySet, child: bool) -> ToolSurface {
        let tools = self
            .tools
            .values()
            .filter_map(|tool| {
                tool.definition
                    .generate_scoped(capabilities, &tool.execution, child)
                    .map(|spec| (spec.name.clone(), spec))
            })
            .collect();
        ToolSurface {
            tools,
            job_views: docs::JobViewSchemas::new(),
        }
    }
}

pub struct CatalogBuilder<C, O: OutputValue> {
    tools: CatalogTools<C, O>,
}

impl<C: Send + 'static, P: OutputValue> CatalogBuilder<C, P> {
    /// Check names already claimed by builtin or host-supplied tools.
    pub(crate) fn contains_name(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn extend(&mut self, registry: &Catalog<C, P>) -> Result<&mut Self, RegistryError> {
        for (name, tool) in registry.tools.iter() {
            if self.tools.insert(name.clone(), tool.clone()).is_some() {
                return Err(RegistryError::Duplicate(name.clone()));
            }
        }
        Ok(self)
    }

    pub fn register_dynamic<F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        options: ToolOptions,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        F: Fn(C, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<P, OperationError<P>>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        self.register_admission(name, description, input_schema, options, move |arguments| {
            let handler = handler.clone();
            Ok(Invocation::new(move |context| handler(context, arguments)))
        })
    }

    pub(crate) fn register_admission(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        options: ToolOptions,
        admit: impl Fn(Value) -> Result<Invocation<C, P>, AdmissionError> + Send + Sync + 'static,
    ) -> Result<&mut Self, RegistryError> {
        self.register_checked(name, description, input_schema, options, admit)
    }

    fn register_checked<I: DeserializeOwned + JsonSchema + 'static>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        mut input_schema: Value,
        mut options: ToolOptions<I>,
        admit: impl Fn(Value) -> Result<Invocation<C, P>, AdmissionError> + Send + Sync + 'static,
    ) -> Result<&mut Self, RegistryError> {
        tags_first(&mut input_schema);
        if options.execution.placement == ToolPlacement::TargetedWorkspace {
            ensure_no_target(&input_schema)?;
            options = options.conditional_input(
                "target",
                Capability::Targets,
                crate::target::TargetRef::schema(),
            );
        }
        let name = name.into();
        validate_name(&name)?;
        if matches!(options.script_binding, ScriptBinding::TopLevel) && name == "job" {
            return Err(RegistryError::ReservedScriptName(name));
        }
        validate_schema(&input_schema)?;
        check_conditional(&input_schema, &options.conditional_inputs, "input")?;
        let all: CapabilitySet = Capability::ALL.into_iter().collect();
        let ToolOptions {
            mut execution,
            hooks,
            supports_background,
            preserve_required,
            preserve_schema_dialect,
            exposure,
            script_binding,
            required,
            root_required,
            conditional_inputs,
            conditional_outputs,
            output_schema,
        } = options;
        let output_schema = match output_schema {
            Some(base) if !conditional_outputs.is_empty() => {
                check_conditional(&base.generate(&all), &conditional_outputs, "output")?;
                Some(OutputSchema::Generated(Arc::new(move |capabilities| {
                    let mut schema = base.generate(capabilities);
                    add_conditional(&mut schema, &conditional_outputs, capabilities);
                    schema
                })))
            }
            output_schema => output_schema,
        };
        execution.check = hooks.check();
        let supports_name = execution.supports_name;
        if supports_name && input_schema["properties"].get("name").is_some() {
            return Err(RegistryError::Schema(
                "named tools reserve the name argument for job metadata".to_owned(),
            ));
        }
        let schema = move |capabilities: &CapabilitySet| {
            let mut schema = input_schema.clone();
            if supports_name {
                add_schema_property(
                    &mut schema,
                    "name",
                    serde_json::json!({
                        "type": ["string", "null"],
                        "pattern": "^[a-z][a-z0-9]*(-[a-z0-9]+)*$",
                        "description": "Lowercase kebab-case name shown in job state and notifications."
                    }),
                );
            }
            add_conditional(&mut schema, &conditional_inputs, capabilities);
            schema
        };
        let definition = GeneratedToolDefinition {
            name,
            description: description.into(),
            input_schema: Arc::new(schema),
            output_schema,
            exposure,
            script_binding,
            supports_background,
            preserve_required,
            preserve_schema_dialect,
            required,
            root_required,
        };
        if self.tools.contains_key(&definition.name) {
            return Err(RegistryError::Duplicate(definition.name));
        }
        // Schema roots do not vary with capabilities; validate the full surface once.
        if let Some(spec) = definition.generate_scoped(&all, &execution, false) {
            validate_object_schema(&spec.input_schema)?;
            if let Some(schema) = &spec.output_schema {
                validate_output_schema(schema)?;
            }
        }
        self.tools.insert(
            definition.name.clone(),
            Arc::new(CatalogEntry {
                definition,
                execution,
                admit: Arc::new(admit),
            }),
        );
        Ok(self)
    }

    pub fn register<I, O, F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        options: ToolOptions<I>,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: Serialize + JsonSchema + Send + 'static,
        F: Fn(C, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, OperationError<P>>> + Send + 'static,
    {
        self.register_product::<I, O, _, _>(name, description, options, move |context, input| {
            let future = handler(context, input);
            async move {
                let output = future.await?;
                Ok(P::from_value(serde_json::to_value(output)?))
            }
        })
    }

    /// Register a typed input with an already-constructed output product.
    /// `O` supplies only the advertised result schema. Unlike `register`, this
    /// adapter never serializes the product, so native capture ownership and
    /// images survive until the canonical completion owner consumes them.
    pub fn register_product<I, O, F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        mut options: ToolOptions<I>,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: JsonSchema,
        F: Fn(C, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<P, OperationError<P>>> + Send + 'static,
    {
        // Handler results are described by their serialization contract. In
        // particular, an `Option<T>` that is always emitted is required and
        // nullable, while `skip_serializing_if` still makes a field optional.
        let output_schema = serde_json::to_value(
            SchemaSettings::default()
                .for_serialize()
                .into_generator()
                .into_root_schema_for::<O>(),
        )
        .map_err(|error| RegistryError::Schema(error.to_string()))?;
        if options.output_schema.is_none() {
            options = options.output_schema(output_schema);
        }
        self.register_typed::<I, _, _>(name, description, options, handler)
    }

    fn register_typed<I, F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        options: ToolOptions<I>,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        F: Fn(C, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<P, OperationError<P>>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let input_schema = input_schema::<I>()?;
        self.register_checked(name, description, input_schema, options, move |arguments| {
            let input = super::diagnostic::deserialize_arguments::<I>(&arguments)?;
            let handler = handler.clone();
            Ok(Invocation::new(move |context| handler(context, input)))
        })
    }

    pub fn build(self) -> Catalog<C, P> {
        Catalog {
            tools: Arc::new(self.tools),
        }
    }
}

/// Typed arguments keep the deserialization contract: defaults and `Option`
/// fields remain omissible even when the output type would emit the same fields.
pub(crate) fn input_schema<I: JsonSchema>() -> Result<Value, RegistryError> {
    serde_json::to_value(
        SchemaSettings::default()
            .for_deserialize()
            .into_generator()
            .into_root_schema_for::<I>(),
    )
    .map_err(|error| RegistryError::Schema(error.to_string()))
}

fn validate_name(name: &str) -> Result<(), RegistryError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(RegistryError::InvalidName(name.to_owned()))
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

struct HostAuthorizer(ToolContext);

impl crate::tool::invocation::LocalAuthorizer for HostAuthorizer {
    fn authorize(
        &self,
        permissions: Vec<PermissionUse>,
        arguments: Value,
    ) -> BoxFuture<'static, Result<(), AdmissionError>> {
        let context = self.0.clone();
        Box::pin(async move { context.authorize(permissions, arguments).await })
    }
}

impl ToolRegistryBuilder {
    pub(crate) fn register_local(
        &mut self,
        register: impl FnOnce(
            &mut crate::tool::invocation::LocalCatalogBuilder,
        ) -> Result<(), RegistryError>,
    ) -> Result<(), RegistryError> {
        use crate::tool::invocation::LocalContext;
        let mut builder = crate::tool::invocation::LocalCatalogBuilder::default();
        register(&mut builder)?;
        for (name, tool) in builder.tools {
            if self.tools.contains_key(&name) {
                return Err(RegistryError::Duplicate(name));
            }
            let local = tool.clone();
            let host = CatalogEntry {
                definition: tool.definition.clone(),
                execution: tool.execution.clone(),
                admit: Arc::new(move |arguments: Value| {
                    let admitted = (local.admit)(arguments.clone())?;
                    Ok(Invocation::new(move |context: ToolContext| async move {
                        let output = crate::job::output::HostOutput::new(
                            context.store().clone(),
                            context.job(),
                        );
                        let local = LocalContext::new(
                            context.execution_location().clone(),
                            context.capabilities().clone(),
                            context.process_environment.clone(),
                            context.cancellation_token(),
                            output.context(),
                            Arc::new(HostAuthorizer(context.clone())),
                            arguments,
                        )
                        .with_source(context.source().cloned());
                        let result = admitted.call(local).await;
                        output.context().settle().await?;
                        match result {
                            Ok(value) => Ok(output.finish(value)?),
                            Err(error) => Err(error.try_map_output(|value| output.finish(value))?),
                        }
                    }))
                }),
            };
            self.tools.insert(name, Arc::new(host));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobSpec;
    use std::io::Write as _;

    #[derive(serde::Deserialize, JsonSchema)]
    struct Empty {}

    #[tokio::test]
    async fn completed_read_errors_require_source_provenance_and_registered_policy() {
        use crate::tool::diagnostic::{Operation, Subject};
        use serde_json::json;

        for (opt_in, source) in [(true, true), (true, false), (false, true)] {
            let mut options =
                ToolOptions::default().path_argument("path", PathAccess::Read, PathKind::Existing);
            if opt_in {
                options =
                    options.read_error_output(|path, _| Some(json!({"kind":"error", "path":path})));
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
            let result = registry
                .get("read")
                .unwrap()
                .admit(
                    json!({"path":"/resolved/source"}),
                    &json!({"path":"requested"}),
                )
                .unwrap()
                .call(())
                .await;
            assert_eq!(result.is_ok(), opt_in && source);
            if let Ok(output) = result {
                assert_eq!(output.value, json!({"kind":"error", "path":"requested"}));
                assert!(output.diagnostic.is_some());
            }
        }
    }

    #[test]
    fn javascript_job_namespace_is_reserved_but_unwrap_is_an_ordinary_tool_name() {
        let mut builder = ToolRegistryBuilder::default();
        let registered = builder.register::<Empty, Value, _, _>(
            "job",
            "reserved namespace",
            ToolOptions::default(),
            |_, _| async { Ok(Value::Null) },
        );
        assert!(matches!(
            registered,
            Err(RegistryError::ReservedScriptName(_))
        ));
        for (name, options) in [
            ("job", ToolOptions::default().script_unavailable()),
            ("unwrap", ToolOptions::default()),
        ] {
            builder
                .register::<Empty, Value, _, _>(name, "allowed binding", options, |_, _| async {
                    Ok(Value::Null)
                })
                .unwrap();
        }
    }

    #[test]
    fn typed_schemas_use_their_respective_serde_contracts() {
        #[derive(serde::Deserialize, JsonSchema)]
        struct Input {
            #[serde(default, rename = "defaulted")]
            _defaulted: bool,
            #[serde(rename = "optional")]
            _optional: Option<String>,
        }
        #[derive(serde::Serialize, JsonSchema)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Choice {
            Empty,
            Value { value: Option<u64> },
        }
        #[derive(serde::Serialize, JsonSchema)]
        struct Output {
            emitted: Option<String>,
            #[serde(default)]
            defaulted: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            skipped: Option<String>,
            choice: Choice,
        }

        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Input, Output, _, _>(
                "contracts",
                "serde contracts",
                ToolOptions::default(),
                |_, _| async { unreachable!("schema-only fixture") },
            )
            .unwrap();
        let spec = builder
            .build()
            .surface(&CapabilitySet::default())
            .get("contracts")
            .unwrap()
            .clone();

        let input = jsonschema::validator_for(&spec.input_schema).unwrap();
        assert!(input.is_valid(&serde_json::json!({})));
        assert!(input.is_valid(&serde_json::json!({"optional": null})));

        let schema = spec.output_schema.unwrap();
        let required = schema["required"].as_array().unwrap();
        for field in ["emitted", "defaulted", "choice"] {
            assert!(required.contains(&Value::String(field.into())), "{schema}");
        }
        assert!(!required.contains(&Value::String("skipped".into())));
        let output = jsonschema::validator_for(&schema).unwrap();
        for value in [
            serde_json::to_value(Output {
                emitted: None,
                defaulted: false,
                skipped: None,
                choice: Choice::Empty,
            })
            .unwrap(),
            serde_json::to_value(Output {
                emitted: Some("present".into()),
                defaulted: true,
                skipped: Some("present".into()),
                choice: Choice::Value { value: None },
            })
            .unwrap(),
        ] {
            assert!(output.is_valid(&value), "{value} rejected by {schema}");
        }
        assert!(!output.is_valid(&serde_json::json!({
            "defaulted": false, "choice": {"kind": "empty"}
        })));
    }

    #[tokio::test]
    async fn product_registration_preserves_capture_evidence_without_serialization() {
        #[derive(serde::Deserialize, JsonSchema)]
        struct Input {
            content: String,
        }
        // This is a schema-only declaration, deliberately not Serialize.
        #[derive(JsonSchema)]
        struct Output {
            #[serde(rename = "content")]
            _content: String,
        }
        let runtime = crate::tests::TestRuntime::new().await;
        let lease = runtime
            .jobs
            .create(JobSpec::test(runtime.agent.clone(), "product"));
        let lease = lease.await.unwrap().test_run().await;
        let job = lease.id();
        let (context, worker) = runtime.tool_context(lease);
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_product::<Input, Output, _, _>(
                "product",
                "Native output product",
                ToolOptions::default(),
                |context, input| async move {
                    let capture =
                        context.text_capture(crate::tool::output::TextCaptureField::Content);
                    let mut capture = capture.await?.open();
                    capture.write_all(input.content.as_bytes())?;
                    let completed = capture.finish()?;
                    Ok(ToolOutput::new(serde_json::json!({})).with_captures(vec![completed]))
                },
            )
            .unwrap();
        let registry = builder.build();
        let tool = registry.get("product").unwrap();
        let spec = tool
            .spec(&CapabilitySet::default(), &runtime.agent)
            .unwrap();
        assert_eq!(
            spec.result_schema.unwrap()["properties"]["content"]["type"],
            "string"
        );
        let arguments = serde_json::json!({"content":"native evidence"});
        let admitted = tool.admit(arguments.clone(), &arguments).unwrap();
        let mut output = admitted.call(context).await.unwrap();
        assert_eq!(output.value, serde_json::json!({}));
        let captures = output.take_captures();
        assert_eq!(captures.len(), 1);
        assert!(captures[0].matches(job, "/result/content"));
        assert!(output.take_captures().is_empty());
        worker.fail(ToolError::cancelled().into()).await;
    }

    #[test]
    fn conditional_nested_inputs_follow_capabilities_and_must_name_objects() {
        #[derive(serde::Deserialize, JsonSchema)]
        struct Inner {
            #[serde(rename = "value")]
            _value: String,
        }
        #[derive(serde::Deserialize, JsonSchema)]
        struct Input {
            #[serde(rename = "inner")]
            _inner: Inner,
        }
        let options = |pointer: &str| {
            let extra = serde_json::json!({"type": "boolean"});
            ToolOptions::default().conditional_nested_input(
                pointer,
                "extra",
                Capability::Targets,
                extra,
            )
        };
        let mut builder = ToolRegistryBuilder::default();
        let handler = |_, _: Input| async { Ok(String::new()) };
        builder
            .register::<Input, String, _, _>(
                "nested",
                "Nested input",
                options("/$defs/Inner"),
                handler,
            )
            .unwrap();
        let missing = options("/$defs/Missing");
        assert!(
            builder
                .register::<Input, String, _, _>("bad", "Bad", missing, handler)
                .is_err()
        );
        let registry = builder.build();
        let extra = |capabilities: &CapabilitySet| {
            let surface = registry.surface(capabilities);
            let spec = surface.get("nested").unwrap();
            spec.input_schema["$defs"]["Inner"]["properties"]
                .get("extra")
                .is_some()
        };
        let mut capabilities = CapabilitySet::default();
        assert!(!extra(&capabilities));
        capabilities.insert(Capability::Targets);
        assert!(extra(&capabilities));
    }

    /// schemars lists an internally tagged variant's fields before its tag; the
    /// registered schema leads with the tag so a schema-constrained decoder can
    /// still choose that variant after writing the tag.
    #[test]
    fn registered_schemas_lead_tagged_variants_with_their_tag() {
        #[derive(serde::Deserialize, JsonSchema)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Auth {
            Agent,
            Key {
                #[serde(rename = "path")]
                _path: String,
            },
        }
        #[derive(serde::Deserialize, JsonSchema)]
        struct Input {
            #[serde(rename = "auth")]
            _auth: Auth,
        }
        let mut builder = ToolRegistryBuilder::default();
        let handler = |_, _: Input| async { Ok(String::new()) };
        let options = ToolOptions::default();
        builder
            .register::<Input, String, _, _>("tagged", "Tagged", options, handler)
            .unwrap();
        let surface = builder.build().surface(&CapabilitySet::default());
        let spec = surface.get("tagged").unwrap();
        let variants = spec.input_schema["$defs"]["Auth"]["oneOf"]
            .as_array()
            .unwrap();
        let key = variants
            .iter()
            .find(|variant| variant["properties"]["kind"]["const"] == "key")
            .unwrap();
        let keys: Vec<_> = key["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["kind", "path"]);
    }
}
