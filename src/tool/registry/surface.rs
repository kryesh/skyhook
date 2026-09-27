//! The specs a capability set sees: generated per agent from registered definitions.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use serde_json::Value;

use super::{
    Arguments, CatalogEntry, JobViewResult, OutputValue, ScriptBinding, ScriptManifest,
    ToolExposure,
    schema::{add_background, optional_defaults, sanitize_schema, sanitize_schema_inner},
};
use crate::tool::{
    AdmissionError,
    policy::{Capability, CapabilitySet},
};

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub supports_background: bool,
    pub job_role: crate::job::JobRole,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Handler result, without the background job alternative.
    pub result_schema: Option<ResultSchema>,
    pub exposure: ToolExposure,
    pub script_binding: ScriptBinding,
}

/// A tool's native result: JSON its schema describes, or job views.
#[derive(Clone, Debug)]
pub enum ResultSchema {
    Json(Value),
    JobViews(JobViewResult),
}

impl ResultSchema {
    #[must_use]
    pub fn schema(&self) -> &Value {
        match self {
            Self::Json(schema) => schema,
            Self::JobViews(JobViewResult::One) => &crate::job::JOB_VIEW_SCHEMAS.one,
            Self::JobViews(JobViewResult::Many) => &crate::job::JOB_VIEW_SCHEMAS.many,
        }
    }
}

impl ToolSpec {
    /// The call's arguments: an object naming only declared properties when the
    /// schema is closed.
    pub(crate) fn validate_arguments(&self, arguments: Value) -> Result<Arguments, AdmissionError> {
        let Value::Object(arguments) = arguments else {
            return Err(AdmissionError::arguments_must_be_object());
        };
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
        Ok(arguments)
    }
}

pub(super) type SchemaGenerator = Arc<dyn Fn(&CapabilitySet) -> Value + Send + Sync>;

#[derive(Clone)]
pub(super) enum OutputSchema {
    Static(Value),
    Generated(SchemaGenerator),
    JobViews(JobViewResult),
}

impl OutputSchema {
    pub(super) fn generate(&self, capabilities: &CapabilitySet) -> Value {
        match self {
            Self::Static(schema) => schema.clone(),
            Self::Generated(generate) => generate(capabilities),
            Self::JobViews(views) => ResultSchema::JobViews(*views).schema().clone(),
        }
    }
}

#[derive(Clone)]
pub(super) struct GeneratedToolDefinition {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) input_schema: SchemaGenerator,
    pub(super) output_schema: Option<OutputSchema>,
    pub(super) exposure: ToolExposure,
    pub(super) script_binding: ScriptBinding,
    pub(super) supports_background: bool,
    pub(super) preserve_required: bool,
    pub(super) preserve_schema_dialect: bool,
    pub(super) required: BTreeSet<Capability>,
    pub(super) root_required: BTreeSet<Capability>,
}

/// Which agent a spec is generated for: root-only requirements bind only the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

impl<C, O: OutputValue> CatalogEntry<C, O> {
    /// The spec an agent at `level` sees.
    pub(crate) fn spec(&self, capabilities: &CapabilitySet, level: AgentLevel) -> Option<ToolSpec> {
        let definition = &self.definition;
        let root_required = (level == AgentLevel::Root).then_some(&definition.root_required);
        definition
            .required
            .iter()
            .chain(root_required.into_iter().flatten())
            .all(|capability| capabilities.contains(*capability))
            .then(|| {
                let mut input_schema = (definition.input_schema)(capabilities);
                if definition.supports_background {
                    add_background(&mut input_schema);
                }
                if !definition.preserve_required {
                    optional_defaults(&mut input_schema);
                }
                sanitize_schema_inner(&mut input_schema, definition.preserve_schema_dialect);
                // This schema describes the handler's stored native payload.
                // Foreground/background presentation both wrap that payload in
                // JobView elsewhere, so a background envelope alternative here
                // would misdescribe the value persisted and later projected.
                let result_schema = definition
                    .output_schema
                    .as_ref()
                    .map(|schema| match schema {
                        OutputSchema::JobViews(views) => ResultSchema::JobViews(*views),
                        schema => {
                            let mut schema = schema.generate(capabilities);
                            sanitize_schema(&mut schema);
                            ResultSchema::Json(schema)
                        }
                    });
                ToolSpec {
                    supports_background: definition.supports_background,
                    job_role: self.execution.job_role,
                    name: definition.name.clone(),
                    description: definition.description.clone(),
                    input_schema,
                    result_schema,
                    exposure: definition.exposure,
                    script_binding: definition.script_binding.clone(),
                }
            })
    }
}

#[derive(Clone, Default)]
pub struct ToolSurface {
    pub(super) tools: BTreeMap<String, ToolSpec>,
}

impl ToolSurface {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    #[cfg(test)]
    pub(crate) fn result_schemas(&self) -> impl Iterator<Item = (&str, &Value)> {
        (self.tools.values())
            .filter_map(|tool| Some((tool.name.as_str(), tool.result_schema.as_ref()?.schema())))
    }

    pub(crate) fn script_manifests(&self) -> Vec<ScriptManifest> {
        self.tools
            .values()
            .filter_map(ScriptManifest::from_tool)
            .collect()
    }
}

pub(super) type CatalogTools<C, O> = BTreeMap<String, Arc<CatalogEntry<C, O>>>;

#[derive(Clone)]
pub struct Catalog<C, O: OutputValue> {
    pub(super) tools: Arc<CatalogTools<C, O>>,
}

impl<C, O: OutputValue> Default for Catalog<C, O> {
    fn default() -> Self {
        Self {
            tools: Arc::default(),
        }
    }
}

impl<C: Send + 'static, O: OutputValue> Catalog<C, O> {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<CatalogEntry<C, O>>> {
        self.tools.get(name).cloned()
    }

    /// Generate a surface for the receiving agent, preserving root-only requirements.
    #[must_use]
    pub fn surface_for_agent(
        &self,
        capabilities: &CapabilitySet,
        agent: &crate::identity::AgentId,
    ) -> ToolSurface {
        let level = AgentLevel::of(agent);
        let tools = self
            .tools
            .values()
            .filter_map(|tool| {
                (tool.spec(capabilities, level)).map(|spec| (spec.name.clone(), spec))
            })
            .collect();
        ToolSurface { tools }
    }
}
