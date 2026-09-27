//! Registration options: how a tool is exposed, bound, authorized and described.

use std::{collections::BTreeSet, sync::Arc};

use schemars::JsonSchema;
use serde_json::Value;

use super::{
    JobLocation, PathArgument, ScriptBinding, ToolExecution, ToolExposure, ToolPlacement,
    envelope::TARGET, registration::result_schema, surface::OutputSchema,
};
use crate::{
    execution::ExecutionLocation,
    tool::{
        invocation::AdmissionError,
        policy::{Capability, CapabilitySet, PermissionUse, ResourceId},
    },
};

type ArgumentPaths<A> = Arc<dyn Fn(&mut A) -> Vec<PathArgument<'_>> + Send + Sync>;
type ArgumentPermissions<A> =
    Arc<dyn Fn(&ExecutionLocation, &A) -> Result<Vec<PermissionUse>, AdmissionError> + Send + Sync>;
type ArgumentLocation<A> = Arc<dyn Fn(&A) -> JobLocation + Send + Sync>;
/// Tool-specific derivations from a call's admitted input, run before authorization.
pub(super) struct ArgumentHooks<A> {
    pub(super) paths: Option<ArgumentPaths<A>>,
    pub(super) permissions: Option<ArgumentPermissions<A>>,
    pub(super) location: Option<ArgumentLocation<A>>,
}

/// (object pointer, property name, the property's schema under an agent's capabilities)
pub(super) type ConditionalProperty = (String, String, ComputedProperty);
type ComputedProperty = Arc<dyn Fn(&CapabilitySet) -> Option<Value> + Send + Sync>;

fn gated(capability: Capability, schema: Value) -> ComputedProperty {
    Arc::new(move |capabilities: &CapabilitySet| {
        capabilities.contains(capability).then(|| schema.clone())
    })
}

/// Registration options for a tool whose handler input is `A`; dynamic tools
/// take their arguments as JSON.
pub struct ToolOptions<A = Value> {
    pub(super) execution: ToolExecution,
    pub(super) hooks: ArgumentHooks<A>,
    pub(super) supports_background: bool,
    pub(super) preserve_required: bool,
    pub(super) preserve_schema_dialect: bool,
    pub(super) exposure: ToolExposure,
    pub(super) script_binding: ScriptBinding,
    pub(super) required: BTreeSet<Capability>,
    pub(super) root_required: BTreeSet<Capability>,
    pub(super) conditional_inputs: Vec<ConditionalProperty>,
    /// Like `conditional_inputs`, for properties of the result schema.
    pub(super) conditional_outputs: Vec<ConditionalProperty>,
    pub(super) output_schema: Option<OutputSchema>,
}

impl<A> ToolOptions<A> {
    #[must_use]
    pub const fn job_role(mut self, role: crate::job::JobRole) -> Self {
        self.execution.job_role = role;
        self
    }

    /// Built-in read only: expected OS read failures are successful structured results.
    pub(crate) fn read_error_output(
        mut self,
        convert: fn(&crate::tool::diagnostic::Diagnostic) -> Option<Value>,
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

    /// The `{path, target?}` [`SOURCE_ARGUMENT`] is a source file. The executor
    /// authorizes and loads it with the call, and the handler reads its bytes
    /// from the context. Its `target` follows the tool-level target rules.
    #[must_use]
    pub(crate) fn reads_source(mut self) -> Self {
        self.execution.reads_source = true;
        self.conditional_nested_input(
            crate::target::TargetPath::SCHEMA,
            TARGET,
            Capability::Targets,
            crate::target::TargetPath::target_schema(&schemars::generate::Contract::Deserialize),
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
            hooks: ArgumentHooks {
                paths: None,
                permissions: None,
                location: None,
            },
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
    pub fn job_method(mut self, method: &'static str, job_argument: &'static str) -> Self {
        self.script_binding = ScriptBinding::JobMethod {
            method,
            job_argument,
        };
        self
    }

    #[must_use]
    pub fn script_unavailable(mut self) -> Self {
        self.script_binding = ScriptBinding::Unavailable;
        self
    }

    /// Derive invocation-specific permissions from the admitted input. These
    /// replace static permissions of the same capability. Remote path and
    /// network permissions are forwarded from the destination for host approval;
    /// other namespaces are authorized by the host before dispatch.
    #[must_use]
    pub fn argument_permissions(
        mut self,
        extract: impl Fn(&ExecutionLocation, &A) -> Result<Vec<PermissionUse>, AdmissionError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.hooks.permissions = Some(Arc::new(extract));
        self
    }

    /// The filesystem inputs/outputs the admitted input names. Each is resolved
    /// and replaced with its resolved spelling before authorization.
    #[must_use]
    pub fn argument_paths(
        mut self,
        extract: impl Fn(&mut A) -> Vec<PathArgument<'_>> + Send + Sync + 'static,
    ) -> Self {
        self.hooks.paths = Some(Arc::new(extract));
        self
    }

    /// Where the admitted input places a host tool's job. The executor selects
    /// that location before the job exists, so even a background handle reports it.
    #[must_use]
    pub(crate) fn job_location(
        mut self,
        extract: impl Fn(&A) -> JobLocation + Send + Sync + 'static,
    ) -> Self {
        self.hooks.location = Some(Arc::new(extract));
        self
    }

    #[must_use]
    pub fn output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(OutputSchema::Static(schema));
        self
    }

    #[must_use]
    pub(crate) fn job_views(mut self, views: super::JobViewResult) -> Self {
        self.output_schema = Some(OutputSchema::JobViews(views));
        self
    }

    /// Describe results by `O`'s serialization contract: an absent field is a
    /// skipped `Option<T>` whose schema is declared as a non-null `T`.
    #[must_use]
    pub(crate) fn result<O: JsonSchema>(self) -> Self {
        self.output_schema(result_schema::<O>())
    }

    #[must_use]
    pub fn permission_resource(mut self, resource: ResourceId) -> Self {
        self.execution.permission_resource = Some(resource);
        self
    }
}

impl<A> Default for ToolOptions<A> {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}
