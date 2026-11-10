//! Registration options: how a tool is exposed, bound, authorized and described.

use std::{collections::BTreeSet, sync::Arc};

use schemars::JsonSchema;
use serde_json::Value;

use crate::job::JobRole;

use super::{
    JobLocation, PathArgument, ScriptBinding, ToolExecution, ToolPlacement, envelope::TARGET,
    surface::OutputSchema,
};
use crate::remote::{Landed, PreparedConnection};
use crate::tool::policy::{Capability, CapabilitySet, PermissionUse, ResourceId};
use futures_util::future::BoxFuture;
use skyhook_tools::error::AdmissionError;
use skyhook_tools::schema::result_schema;
use skyhook_tools::target::{ExecutionLocation, TargetedPath};

type ArgumentPaths<A> = Arc<dyn Fn(&mut A) -> Vec<PathArgument<'_>> + Send + Sync>;
type ArgumentPermissions<A> =
    Arc<dyn Fn(&ExecutionLocation, &A) -> Result<Vec<PermissionUse>, AdmissionError> + Send + Sync>;
type ArgumentLocation<A> = Arc<dyn Fn(&A) -> JobLocation + Send + Sync>;
pub(super) type ArgumentResult<A> = Arc<dyn Fn(&A) -> Value + Send + Sync>;
type ArgumentSource<A> = Arc<dyn Fn(&A) -> Option<&TargetedPath> + Send + Sync>;
/// Build a call's operation from its input, once, and start it on a connection in the
/// flight that resolves the paths the request names there.
pub(crate) type ArgumentFlight<A> = Arc<
    dyn Fn(&mut A, skyhook_worker::op::Resolve, PreparedConnection, FlightSite) -> Flight<A>
        + Send
        + Sync,
>;
/// Where a flown call runs, and what its target keeps in files instead of sending back.
pub(crate) type FlightSite = (ExecutionLocation, skyhook_tools::spec::Spills);
/// Where a call's flight landed, and the invocation of its operation.
pub(crate) type Flight<A> =
    BoxFuture<'static, Result<(Landed, FlownInvocation<A>), AdmissionError>>;
/// A flown call's invocation, from its input as its paths resolved.
pub(crate) type FlownInvocation<A> = Box<dyn FnOnce(A) -> super::Invocation + Send>;
/// Tool-specific derivations from a call's admitted input, run before authorization.
pub(super) struct ArgumentHooks<A> {
    pub(super) paths: Option<ArgumentPaths<A>>,
    pub(super) permissions: Option<ArgumentPermissions<A>>,
    pub(super) location: Option<ArgumentLocation<A>>,
    /// The schema the call's stored result follows, which tool descriptions leave out.
    pub(super) result: Option<ArgumentResult<A>>,
    pub(super) source: Option<ArgumentSource<A>>,
    pub(super) flight: Option<ArgumentFlight<A>>,
}

/// (object pointer, property name, the property's schema under an agent's capabilities)
pub(super) type ConditionalProperty = (String, String, ComputedSchema);
/// A schema, or `None` when the agent's capabilities leave nothing to offer.
pub(super) type ComputedSchema = Arc<dyn Fn(&CapabilitySet) -> Option<Value> + Send + Sync>;

fn gated(capability: Capability, schema: Value) -> ComputedSchema {
    Arc::new(move |capabilities: &CapabilitySet| {
        capabilities.contains(&capability).then(|| schema.clone())
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SchemaOrigin {
    /// Generated for a Skyhook handler: a documented default permits omission,
    /// and the dialect is the provider's.
    Native,
    /// Supplied by an external tool, whose required fields stay required even
    /// with defaults (defaults are annotations, not values Skyhook supplies) and
    /// whose declared dialect is retained.
    External,
}

/// Registration options for a tool whose handler input is `A`; dynamic tools
/// take their arguments as JSON.
pub struct ToolOptions<A = Value> {
    pub(super) execution: ToolExecution,
    pub(super) hooks: ArgumentHooks<A>,
    pub(super) supports_background: bool,
    pub(super) schema_origin: SchemaOrigin,
    pub(super) script_binding: ScriptBinding,
    pub(super) required: BTreeSet<Capability>,
    pub(super) root_required: BTreeSet<Capability>,
    pub(super) conditional_inputs: Vec<ConditionalProperty>,
    /// Like `conditional_inputs`, for properties of the result schema.
    pub(super) conditional_outputs: Vec<ConditionalProperty>,
    /// Replaces the typed input schema per capability set; see [`Self::computed_schema`].
    pub(super) computed_schema: Option<ComputedSchema>,
    /// The result schema tool descriptions document.
    pub(super) output_schema: Option<OutputSchema>,
}

impl<A> ToolOptions<A> {
    #[must_use]
    pub const fn job_role(mut self, role: JobRole) -> Self {
        self.execution.job_role = role;
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

    /// The source file the admitted input names, if any. The executor authorizes
    /// and loads it with the call, and the handler reads its bytes from the
    /// context. Its `target` follows the tool-level target rules.
    #[must_use]
    pub fn argument_source(
        mut self,
        source: impl Fn(&A) -> Option<&TargetedPath> + Send + Sync + 'static,
    ) -> Self {
        self.hooks.source = Some(Arc::new(source));
        self.conditional_nested_input(
            TargetedPath::SCHEMA,
            TARGET,
            Capability::Targets,
            TargetedPath::target_schema(&schemars::generate::Contract::Deserialize),
        )
    }

    /// How a remote call starts its operation in the flight that resolves its paths,
    /// once approved in advance for them all resolving inside the root.
    #[must_use]
    pub(crate) fn flight(mut self, flight: ArgumentFlight<A>) -> Self {
        self.hooks.flight = Some(flight);
        self
    }

    /// The handler spawns processes that receive the session's managed SSH agent
    /// with the `Targets` capability.
    #[must_use]
    pub const fn managed_agent(mut self) -> Self {
        self.execution.managed_agent = true;
        self
    }

    #[must_use]
    pub fn new(capabilities: Vec<Capability>) -> Self {
        let required = capabilities.iter().cloned().collect();
        Self {
            supports_background: false,
            schema_origin: SchemaOrigin::Native,
            execution: ToolExecution {
                capabilities,
                ..ToolExecution::default()
            },
            hooks: ArgumentHooks {
                paths: None,
                permissions: None,
                location: None,
                result: None,
                source: None,
                flight: None,
            },
            script_binding: ScriptBinding::TopLevel,
            required,
            root_required: BTreeSet::new(),
            conditional_inputs: Vec::new(),
            conditional_outputs: Vec::new(),
            computed_schema: None,
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

    /// The input schema is externally supplied: see [`SchemaOrigin::External`].
    #[must_use]
    pub fn external_schema(mut self) -> Self {
        self.schema_origin = SchemaOrigin::External;
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
    pub fn conditional_output(
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

    /// The whole input schema follows the agent's capabilities, such as only the
    /// permitted values of an action enum. `None` withdraws the tool from
    /// that agent's surface instead of advertising an unsatisfiable schema.
    ///
    /// Every form it returns must select from the typed input schema, keeping its root
    /// shape and adding no properties. Registration checks the typed schema and the form
    /// for every built-in capability; the registry knows no host capabilities, so forms
    /// only a host capability selects are held to this contract rather than checked.
    #[must_use]
    pub fn computed_schema(
        mut self,
        schema: impl Fn(&CapabilitySet) -> Option<Value> + Send + Sync + 'static,
    ) -> Self {
        self.computed_schema = Some(Arc::new(schema));
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
    pub fn script_unavailable(mut self) -> Self {
        self.script_binding = ScriptBinding::Unavailable;
        self
    }

    /// Derive invocation-specific permissions from the admitted input. These
    /// replace static permissions of the same capability.
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
    pub fn job_location(
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

    /// The schema a call's stored result follows, chosen from its admitted input:
    /// it describes the result to detection, presentation and scripts, but tool
    /// descriptions leave it out, documenting only a declared output schema.
    #[must_use]
    pub fn internal_result(mut self, schema: impl Fn(&A) -> Value + Send + Sync + 'static) -> Self {
        self.hooks.result = Some(Arc::new(schema));
        self
    }

    /// Whether a schema describes the tool's results, documented or chosen per call.
    pub(super) const fn declares_result(&self) -> bool {
        self.output_schema.is_some() || self.hooks.result.is_some()
    }

    /// Describe results by `O`'s serialization contract: an absent field is a
    /// skipped `Option<T>`, described as `T`.
    #[must_use]
    pub fn result<O: JsonSchema + 'static>(self) -> Self {
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
