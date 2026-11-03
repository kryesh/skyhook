//! The specs a capability set sees: generated per agent from registered definitions.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex, PoisonError},
};

use serde_json::Value;

use super::{
    Arguments, CatalogEntry, OutputValue, ScriptBinding, ScriptManifest, ToolExposure,
    options::{ComputedSchema, SchemaOrigin},
    schema::{add_background, normalize_result_schema, optional_defaults, sanitize_schema_inner},
};
use crate::tool::{
    AdmissionError,
    arguments::{ArgumentValidator, argument_validator, schema_argument_failure},
    diagnostic::{Operation, Subject},
    executor::InvocationKind,
    invocation::OperationError,
    policy::{Capability, CapabilitySet},
};

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub supports_background: bool,
    pub job_role: crate::job::JobRole,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// `input_schema` as declared, before sanitizing, compiled; the only authority on
    /// admission.
    pub(crate) validator: Arc<ArgumentValidator>,
    /// Handler result, without the background job alternative.
    pub result_schema: Option<Value>,
    pub exposure: ToolExposure,
    pub script_binding: ScriptBinding,
}

impl ToolSpec {
    /// Admit a call's arguments against the schema this agent was shown. Only
    /// model output is repaired first; host and script callers must match exactly.
    pub(crate) fn admit(
        &self,
        kind: InvocationKind,
        mut arguments: Value,
    ) -> Result<Arguments, AdmissionError> {
        if !arguments.is_object() {
            return Err(AdmissionError::arguments_must_be_object());
        }
        let validator = (self.validator.get()).map_err(|reason| {
            AdmissionError::failed(format!(
                "the tool's input schema does not compile: {reason}"
            ))
        })?;
        if matches!(kind, InvocationKind::Model) {
            crate::tool::coerce::coerce_arguments(&self.input_schema, validator, &mut arguments);
        }
        if let Err(error) = validator.validate(&arguments) {
            let (path, expectation) =
                schema_argument_failure(&self.input_schema, &arguments, &error);
            return Err(AdmissionError::invalid_arguments(expectation)
                .operation(Operation::Deserialize, Subject::Argument(path)));
        }
        match arguments {
            Value::Object(arguments) => Ok(arguments),
            _ => unreachable!("checked above"),
        }
    }
}

pub(super) type SchemaGenerator = Arc<dyn Fn(&CapabilitySet) -> Value + Send + Sync>;

#[derive(Clone)]
pub(super) enum OutputSchema {
    Static(Value),
    Generated(SchemaGenerator),
}

impl OutputSchema {
    pub(super) fn generate(&self, capabilities: &CapabilitySet) -> Value {
        match self {
            Self::Static(schema) => schema.clone(),
            Self::Generated(generate) => generate(capabilities),
        }
    }
}

#[derive(Clone)]
pub(super) struct GeneratedToolDefinition {
    pub(super) name: String,
    pub(super) description: String,
    /// `None` withdraws the tool from that capability set's surface.
    pub(super) input_schema: ComputedSchema,
    pub(super) output_schema: Option<OutputSchema>,
    pub(super) exposure: ToolExposure,
    pub(super) script_binding: ScriptBinding,
    pub(super) supports_background: bool,
    pub(super) schema_origin: SchemaOrigin,
    pub(super) compiled: Compiled,
    pub(super) required: BTreeSet<Capability>,
    pub(super) root_required: BTreeSet<Capability>,
}

/// When a surface compiles a tool's argument schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Compiled {
    /// A typed schema, compiled at a host's registration or, for a built-in, by test,
    /// so only a call to the tool compiles it.
    OnFirstUse,
    /// An external or computed schema may not compile: the surface compiles it as it
    /// admits the tool, and withholds the tool when it does not.
    AsOffered,
}

/// Which agent a spec is generated for: root-only requirements bind only the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AgentLevel {
    Root,
    Child,
}

impl AgentLevel {
    pub(crate) fn of(agent: &crate::identity::AgentId) -> Self {
        if agent.depth() == 0 {
            Self::Root
        } else {
            Self::Child
        }
    }
}

/// Why an agent's surface leaves a tool out.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Withheld {
    /// The agent's capabilities do not grant the tool, or leave its schema nothing to offer.
    #[error("the tool is not offered to this agent")]
    Withdrawn,
    /// The schema surfaced for the agent's capabilities does not compile: a form that
    /// breaks the registration's contract, which registration checks only for the
    /// capabilities it knows.
    #[error("the tool's input schema for this agent does not compile: {0}")]
    Invalid(String),
}

impl<C, O: OutputValue> CatalogEntry<C, O> {
    /// The spec an agent at `level` sees.
    pub(crate) fn spec(
        &self,
        capabilities: &CapabilitySet,
        level: AgentLevel,
    ) -> Result<ToolSpec, Withheld> {
        let definition = &self.definition;
        let root_required = (level == AgentLevel::Root).then_some(&definition.root_required);
        let granted = definition
            .required
            .iter()
            .chain(root_required.into_iter().flatten())
            .all(|capability| capabilities.contains(capability));
        if !granted {
            return Err(Withheld::Withdrawn);
        }
        let mut input_schema =
            (definition.input_schema)(capabilities).ok_or(Withheld::Withdrawn)?;
        if definition.supports_background {
            add_background(&mut input_schema);
        }
        if definition.schema_origin == SchemaOrigin::Native {
            optional_defaults(&mut input_schema);
        }
        // Admission checks the schema as declared; only what providers see is sanitized.
        let validator = Arc::new(match definition.compiled {
            Compiled::OnFirstUse => {
                ArgumentValidator::Deferred(input_schema.clone(), Default::default())
            }
            Compiled::AsOffered => ArgumentValidator::Compiled(
                argument_validator(&input_schema).map_err(Withheld::Invalid)?,
            ),
        });
        sanitize_schema_inner(&mut input_schema, definition.schema_origin);
        // The stored native payload's schema; every presentation wraps it.
        let result_schema = definition.output_schema.as_ref().map(|schema| {
            let mut schema = schema.generate(capabilities);
            normalize_result_schema(&mut schema);
            schema
        });
        Ok(ToolSpec {
            supports_background: definition.supports_background,
            job_role: self.execution.job_role,
            name: definition.name.clone(),
            description: definition.description.clone(),
            input_schema,
            validator,
            result_schema,
            exposure: definition.exposure,
            script_binding: definition.script_binding.clone(),
        })
    }
}

#[derive(Default)]
pub struct ToolSurface {
    pub(super) tools: BTreeMap<String, ToolSpec>,
    /// Every other registered tool, with why it is left out.
    withheld: BTreeMap<String, Withheld>,
    /// What the model sees of `tools`, once asked for.
    pub(super) definitions:
        std::sync::OnceLock<Vec<Arc<crate::provider::protocol::ToolDefinition>>>,
}

impl ToolSurface {
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    /// The spec a call to tool `name` is admitted against, or why the call fails.
    pub(crate) fn offered<O>(&self, name: &str) -> Result<&ToolSpec, OperationError<O>> {
        match (self.tools.get(name), self.withheld.get(name)) {
            (Some(spec), _) => Ok(spec),
            (None, Some(Withheld::Withdrawn)) => Err(OperationError::unavailable(name)),
            (None, Some(invalid)) => Err(OperationError::failed(format!("`{name}`: {invalid}"))),
            (None, None) => Err(OperationError::unknown_tool(name)),
        }
    }

    /// Every registered tool, with its spec or why it is withheld.
    #[cfg(test)]
    pub(crate) fn every(&self) -> impl Iterator<Item = (&str, Result<&ToolSpec, &Withheld>)> {
        let offered = (self.tools.iter()).map(|(name, spec)| (name.as_str(), Ok(spec)));
        offered.chain((self.withheld.iter()).map(|(name, why)| (name.as_str(), Err(why))))
    }

    pub(crate) fn script_manifests(&self) -> Vec<ScriptManifest> {
        self.tools
            .values()
            .filter_map(ScriptManifest::from_tool)
            .collect()
    }
}

pub(super) type CatalogTools<C, O> = BTreeMap<String, Arc<CatalogEntry<C, O>>>;

type Surfaces = HashMap<(CapabilitySet, AgentLevel), Arc<ToolSurface>>;

#[derive(Clone)]
pub struct Catalog<C, O: OutputValue> {
    pub(super) tools: Arc<CatalogTools<C, O>>,
    /// Surfaces generated so far; the tools never change, so neither do they.
    surfaces: Arc<Mutex<Surfaces>>,
}

impl<C, O: OutputValue> Catalog<C, O> {
    /// Static requirements of every registration, including tools absent from a surface.
    pub(crate) fn required_capabilities(&self) -> impl Iterator<Item = &Capability> {
        self.tools.values().flat_map(|tool| {
            tool.execution
                .capabilities
                .iter()
                .chain(&tool.definition.required)
                .chain(&tool.definition.root_required)
        })
    }

    pub(super) fn new(tools: CatalogTools<C, O>) -> Self {
        Self {
            tools: Arc::new(tools),
            surfaces: Arc::default(),
        }
    }
}

impl<C, O: OutputValue> Default for Catalog<C, O> {
    fn default() -> Self {
        Self::new(CatalogTools::default())
    }
}

impl<C: Send + 'static, O: OutputValue> Catalog<C, O> {
    pub fn get(&self, name: &str) -> Option<Arc<CatalogEntry<C, O>>> {
        self.tools.get(name).cloned()
    }

    /// Generate a surface for the receiving agent, preserving root-only requirements. A
    /// tool whose schema does not compile is left out too; a call to it reports why.
    #[must_use]
    pub fn surface_for_agent(
        &self,
        capabilities: &CapabilitySet,
        agent: &crate::identity::AgentId,
    ) -> Arc<ToolSurface> {
        self.surface(capabilities, AgentLevel::of(agent))
    }

    pub(crate) fn surface(
        &self,
        capabilities: &CapabilitySet,
        level: AgentLevel,
    ) -> Arc<ToolSurface> {
        let key = (capabilities.clone(), level);
        let mut surfaces = self.surfaces.lock().unwrap_or_else(PoisonError::into_inner);
        let surface = surfaces
            .entry(key)
            .or_insert_with_key(|(capabilities, level)| {
                let mut surface = ToolSurface::default();
                for (name, tool) in self.tools.iter() {
                    match tool.spec(capabilities, *level) {
                        Ok(spec) => drop(surface.tools.insert(name.clone(), spec)),
                        Err(withheld) => drop(surface.withheld.insert(name.clone(), withheld)),
                    }
                }
                Arc::new(surface)
            });
        surface.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::tool::{ToolOptions, ToolRegistryBuilder};

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Act {
        #[serde(rename = "action")]
        _action: Action,
    }

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    #[serde(rename_all = "snake_case")]
    #[schemars(inline)]
    enum Action {
        Read,
        Write,
    }

    /// A computed root follows the agent's capabilities: no permitted actions
    /// withdraws the tool, a host capability selects more of the typed actions, and
    /// admission holds arguments to the surfaced schema. A form that does not compile
    /// is told apart from a withdrawal, and a host's typed schema that does not compile
    /// is refused as it registers.
    #[test]
    fn computed_roots_withdraw_tools_and_bound_admission() {
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        struct Unmatchable {
            #[schemars(regex(pattern = "("))]
            #[serde(rename = "value")]
            _value: String,
        }
        let write: Capability = "host.write".parse().unwrap();
        let broken: Capability = "host.broken".parse().unwrap();
        let (host, breaks) = (write.clone(), broken.clone());
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register::<Act, String, _, _>(
                "actions",
                "Capability-gated actions",
                // Each form selects actions of the typed schema.
                ToolOptions::default().computed_schema(move |capabilities| {
                    if !capabilities.contains(&Capability::Read) {
                        return None;
                    }
                    let mut schema = crate::tool::input_schema_for::<Act>();
                    let actions = (schema["properties"]["action"]["enum"].as_array_mut())
                        .expect("the typed actions");
                    actions.retain(|action| action != "write" || capabilities.contains(&write));
                    if capabilities.contains(&broken) {
                        schema["pattern"] = "(".into();
                    }
                    Some(schema)
                }),
                |_, _| async { Ok(String::new()) },
            )
            .unwrap()
            // A computed property is as unproven as a computed root.
            .register::<Act, String, _, _>(
                "property",
                "Capability-gated property",
                ToolOptions::default().computed_input("extra", move |_| {
                    Some(serde_json::json!({"type": "string", "pattern": "("}))
                }),
                |_, _| async { Ok(String::new()) },
            )
            .unwrap();
        let refused = builder.register::<Unmatchable, String, _, _>(
            "typed",
            "Uncompilable typed input",
            ToolOptions::default(),
            |_, _| async { Ok(String::new()) },
        );
        assert!(matches!(
            refused,
            Err(crate::tool::RegistryError::Schema(_))
        ));
        let registry = builder.build();
        assert!(registry.get("typed").is_none());
        assert_matches!(
            (registry.get("property").unwrap()).spec(&CapabilitySet::empty(), AgentLevel::Root),
            Err(Withheld::Invalid(_))
        );
        let tool = registry.get("actions").unwrap();
        assert_matches!(
            tool.spec(&CapabilitySet::empty(), AgentLevel::Root),
            Err(Withheld::Withdrawn)
        );
        assert_matches!(
            tool.spec(
                &[Capability::Read, breaks].into_iter().collect(),
                AgentLevel::Root
            ),
            Err(Withheld::Invalid(_))
        );
        let spec = tool
            .spec(&[Capability::Read].into_iter().collect(), AgentLevel::Root)
            .unwrap();
        let admit = |arguments| spec.admit(InvocationKind::Script, arguments);
        assert!(admit(serde_json::json!({"action":"read"})).is_ok());
        let hosted = (tool.spec(
            &[Capability::Read, host].into_iter().collect(),
            AgentLevel::Root,
        ))
        .unwrap();
        assert!(
            hosted
                .admit(
                    InvocationKind::Script,
                    serde_json::json!({"action":"write"})
                )
                .is_ok()
        );
        let refused = |arguments| admit(arguments).unwrap_err().to_string();
        assert!(refused(serde_json::json!({"action":"write"})).contains("expected one of"));
        let extra = refused(serde_json::json!({"action":"read","extra":1}));
        assert!(extra.contains("property `extra` is not allowed"), "{extra}");
    }
}
