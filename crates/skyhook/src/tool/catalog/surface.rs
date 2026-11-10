//! The specs a capability set sees: generated per agent from registered definitions.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex, PoisonError},
};

use serde_json::Value;

use super::{
    Arguments, InvocationKind, RegisteredTool, ScriptBinding,
    options::{ComputedSchema, SchemaOrigin},
    schema::{add_background, normalize_result_schema, optional_defaults, sanitize_schema_inner},
};
use crate::tool::ToolError;
use crate::tool::policy::{Capability, CapabilitySet};
use skyhook_tools::{
    arguments::{ArgumentValidator, Author, admit_arguments, argument_validator},
    error::AdmissionError,
};

/// A tool as a model is offered it.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub supports_background: bool,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// `input_schema` as declared, before sanitizing, compiled; the only authority on
    /// admission.
    pub(crate) validator: Arc<ArgumentValidator>,
    /// Handler result, without the background job alternative.
    pub result_schema: Option<Value>,
    pub script_binding: ScriptBinding,
}

impl ToolSpec {
    /// Admit a call's arguments against the schema this agent was shown. Only
    /// model output is repaired first; host and script callers must match exactly.
    pub fn admit(
        &self,
        kind: InvocationKind,
        arguments: Value,
    ) -> Result<Arguments, AdmissionError> {
        let validator = (self.validator.get()).map_err(|reason| {
            AdmissionError::failed(format!(
                "the tool's input schema does not compile: {reason}"
            ))
        })?;
        let author = match kind {
            InvocationKind::Model => Author::Model,
            InvocationKind::Host | InvocationKind::Script => Author::Program,
        };
        admit_arguments(&self.input_schema, validator, author, arguments)
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
    pub(super) script_binding: ScriptBinding,
    pub supports_background: bool,
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
pub enum AgentLevel {
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
pub enum Withheld {
    /// The agent's capabilities do not grant the tool, or leave its schema nothing to offer.
    #[error("the tool is not offered to this agent")]
    Withdrawn,
    /// The schema surfaced for the agent's capabilities does not compile: a form that
    /// breaks the registration's contract, which registration checks only for the
    /// capabilities it knows.
    #[error("the tool's input schema for this agent does not compile: {0}")]
    Invalid(String),
}

impl RegisteredTool {
    /// The spec an agent at `level` sees.
    pub fn spec(
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
            name: definition.name.clone(),
            description: definition.description.clone(),
            input_schema,
            validator,
            result_schema,
            script_binding: definition.script_binding,
        })
    }
}

#[derive(Default)]
pub struct ToolSurface {
    pub(super) tools: BTreeMap<String, ToolSpec>,
    /// Every other registered tool, with why it is left out.
    withheld: BTreeMap<String, Withheld>,
    /// What the model sees of `tools`, once asked for.
    pub(super) definitions: std::sync::OnceLock<Vec<Arc<ToolDefinition>>>,
}

impl ToolSurface {
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    /// The offered specs, by name.
    pub fn specs(&self) -> impl Iterator<Item = &ToolSpec> {
        self.tools.values()
    }

    /// The spec a call to tool `name` is admitted against, or why the call fails.
    pub fn offered(&self, name: &str) -> Result<&ToolSpec, ToolError> {
        match (self.tools.get(name), self.withheld.get(name)) {
            (Some(spec), _) => Ok(spec),
            (None, Some(Withheld::Withdrawn)) => Err(ToolError::unavailable(name)),
            (None, Some(invalid)) => Err(ToolError::failed(format!("`{name}`: {invalid}"))),
            (None, None) => Err(ToolError::unknown_tool(name)),
        }
    }

    /// Every registered tool, with its spec or why it is withheld.
    #[cfg(test)]
    pub(crate) fn every(&self) -> impl Iterator<Item = (&str, Result<&ToolSpec, &Withheld>)> {
        let offered = (self.tools.iter()).map(|(name, spec)| (name.as_str(), Ok(spec)));
        offered.chain((self.withheld.iter()).map(|(name, why)| (name.as_str(), Err(why))))
    }
}

pub(super) type CatalogTools = BTreeMap<String, Arc<RegisteredTool>>;

type Surfaces = HashMap<(CapabilitySet, AgentLevel), Arc<ToolSurface>>;

#[derive(Clone, Default)]
pub struct ToolRegistry {
    pub(super) tools: Arc<CatalogTools>,
    /// Surfaces generated so far; the tools never change, so neither do they.
    surfaces: Arc<Mutex<Surfaces>>,
}

impl ToolRegistry {
    /// Static requirements of every registration, including tools absent from a surface.
    pub fn required_capabilities(&self) -> impl Iterator<Item = &Capability> {
        self.tools.values().flat_map(|tool| {
            tool.execution
                .capabilities
                .iter()
                .chain(&tool.definition.required)
                .chain(&tool.definition.root_required)
        })
    }

    pub(super) fn new(tools: CatalogTools) -> Self {
        Self {
            tools: Arc::new(tools),
            surfaces: Arc::default(),
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<RegisteredTool>> {
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
        let key = (capabilities.clone(), AgentLevel::of(agent));
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
    use crate::tool::catalog::{ToolOptions, ToolRegistryBuilder};

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
                    let mut schema = skyhook_tools::schema::input_schema_for::<Act>();
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
            Err(crate::tool::catalog::RegistryError::Schema(_))
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
