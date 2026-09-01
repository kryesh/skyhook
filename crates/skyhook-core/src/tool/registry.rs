use std::{collections::BTreeMap, future::Future, sync::Arc};

use futures_util::future::BoxFuture;
use schemars::{JsonSchema, schema_for};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{
    identity::AgentId,
    provider::protocol::ToolDefinition,
    tool::policy::{PathAccess, ToolEffect},
};

use super::{ToolContext, ToolError, ToolOutput};

#[derive(Clone)]
pub struct RegisteredTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub effects: Vec<ToolEffect>,
    pub supports_background: bool,
    pub accepts_input: bool,
    pub exposure: ToolExposure,
    pub script_binding: ScriptBinding,
    placement: ToolPlacement,
    path_arguments: Vec<PathArgument>,
    handler: ToolHandler,
    effect_resolver: Option<EffectResolver>,
    availability: Option<AvailabilityPredicate>,
}

type ToolHandler = Arc<
    dyn Fn(ToolContext, Value) -> BoxFuture<'static, Result<ToolOutput, ToolError>> + Send + Sync,
>;
type EffectResolver = Arc<dyn Fn(&Value) -> Result<Vec<ToolEffect>, ToolError> + Send + Sync>;
type AvailabilityPredicate = Arc<dyn Fn(&ToolVisibilityContext) -> bool + Send + Sync>;

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
    Special,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ToolPlacement {
    #[default]
    Host,
    Workspace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathKind {
    Existing,
    Writable,
    Removable,
}

#[derive(Clone, Debug)]
pub(crate) struct PathArgument {
    pub name: String,
    pub access: PathAccess,
    pub kind: PathKind,
    pub default: Option<String>,
    pub skip_for_remote_target: bool,
}

#[derive(Clone, Debug)]
pub struct ToolVisibilityContext {
    pub agent: AgentId,
}

impl ToolVisibilityContext {
    #[must_use]
    pub const fn new(agent: AgentId) -> Self {
        Self { agent }
    }
}

#[derive(Clone)]
pub struct ToolOptions {
    pub effects: Vec<ToolEffect>,
    pub supports_background: bool,
    pub accepts_input: bool,
    exposure: ToolExposure,
    script_binding: ScriptBinding,
    availability: Option<AvailabilityPredicate>,
    placement: ToolPlacement,
    path_arguments: Vec<PathArgument>,
    output_schema: Option<Value>,
}

impl ToolOptions {
    #[must_use]
    pub const fn new(effects: Vec<ToolEffect>) -> Self {
        Self {
            effects,
            supports_background: false,
            accepts_input: false,
            exposure: ToolExposure::ModelVisible,
            script_binding: ScriptBinding::TopLevel,
            availability: None,
            placement: ToolPlacement::Host,
            path_arguments: Vec::new(),
            output_schema: None,
        }
    }

    #[must_use]
    pub const fn background(mut self) -> Self {
        self.supports_background = true;
        self
    }

    #[must_use]
    pub const fn input(mut self) -> Self {
        self.accepts_input = true;
        self
    }

    #[must_use]
    pub fn script_only(mut self) -> Self {
        self.exposure = ToolExposure::ScriptOnly;
        self
    }

    #[must_use]
    pub fn available_when<F>(mut self, predicate: F) -> Self
    where
        F: Fn(&ToolVisibilityContext) -> bool + Send + Sync + 'static,
    {
        self.availability = Some(Arc::new(predicate));
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
    pub fn script_special(mut self) -> Self {
        self.script_binding = ScriptBinding::Special;
        self
    }

    #[must_use]
    pub fn script_unavailable(mut self) -> Self {
        self.script_binding = ScriptBinding::Unavailable;
        self
    }

    #[must_use]
    pub(crate) const fn workspace_bound(mut self) -> Self {
        self.placement = ToolPlacement::Workspace;
        self
    }

    #[must_use]
    pub(crate) fn path_argument(
        mut self,
        name: impl Into<String>,
        access: PathAccess,
        kind: PathKind,
    ) -> Self {
        self.path_arguments.push(PathArgument {
            name: name.into(),
            access,
            kind,
            default: None,
            skip_for_remote_target: false,
        });
        self
    }

    #[must_use]
    pub(crate) fn target_path_argument(
        mut self,
        name: impl Into<String>,
        access: PathAccess,
        kind: PathKind,
    ) -> Self {
        self.path_arguments.push(PathArgument {
            name: name.into(),
            access,
            kind,
            default: None,
            skip_for_remote_target: true,
        });
        self
    }

    #[must_use]
    pub(crate) fn default_path_argument(
        mut self,
        name: impl Into<String>,
        default: impl Into<String>,
        access: PathAccess,
        kind: PathKind,
        skip_for_remote_target: bool,
    ) -> Self {
        self.path_arguments.push(PathArgument {
            name: name.into(),
            access,
            kind,
            default: Some(default.into()),
            skip_for_remote_target,
        });
        self
    }

    #[must_use]
    pub fn output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }
}

impl Default for ToolOptions {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl RegisteredTool {
    pub async fn call(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        (self.handler)(context, arguments).await
    }

    pub fn effects_for(&self, arguments: &Value) -> Result<Vec<ToolEffect>, ToolError> {
        self.effect_resolver
            .as_ref()
            .map_or_else(|| Ok(self.effects.clone()), |resolver| resolver(arguments))
    }

    #[must_use]
    fn definition(&self, description: String) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: describe_output(description, self.output_schema.as_ref()),
            input_schema: outbound_schema(&self.input_schema, self.supports_background),
        }
    }

    #[must_use]
    pub fn is_available(&self, context: &ToolVisibilityContext) -> bool {
        self.availability
            .as_ref()
            .is_none_or(|predicate| predicate(context))
    }

    #[must_use]
    pub(crate) fn is_workspace_bound(&self) -> bool {
        self.placement == ToolPlacement::Workspace
    }

    pub(crate) fn path_arguments(&self) -> &[PathArgument] {
        &self.path_arguments
    }
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Arc<BTreeMap<String, Arc<RegisteredTool>>>,
}

impl ToolRegistry {
    #[must_use]
    pub fn builder() -> ToolRegistryBuilder {
        ToolRegistryBuilder::default()
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<RegisteredTool>> {
        self.tools.get(name).cloned()
    }

    #[must_use]
    pub fn definitions(&self, context: &ToolVisibilityContext) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .filter(|tool| {
                tool.exposure == ToolExposure::ModelVisible && tool.is_available(context)
            })
            .map(|tool| {
                let description = if tool.name == "script" {
                    self.script_description(&tool.description, context)
                } else {
                    tool.description.clone()
                };
                tool.definition(description)
            })
            .collect()
    }

    pub fn tools(&self) -> impl Iterator<Item = &Arc<RegisteredTool>> {
        self.tools.values()
    }

    pub(crate) fn script_manifests(&self, context: &ToolVisibilityContext) -> Vec<ScriptManifest> {
        self.tools
            .values()
            .filter(|tool| tool.is_available(context))
            .filter_map(|tool| ScriptManifest::from_tool(tool))
            .collect()
    }

    fn script_description(&self, base: &str, context: &ToolVisibilityContext) -> String {
        let documented = self
            .tools
            .values()
            .filter(|tool| tool.is_available(context))
            .filter(|tool| match &tool.script_binding {
                ScriptBinding::TopLevel => tool.exposure == ToolExposure::ScriptOnly,
                ScriptBinding::JobMethod { .. } => {
                    tool.exposure == ToolExposure::ScriptOnly || tool.name == "wait"
                }
                ScriptBinding::Special | ScriptBinding::Unavailable => false,
            })
            .map(|tool| script_documentation(tool))
            .collect::<Vec<_>>();
        if documented.is_empty() {
            base.to_owned()
        } else {
            format!(
                "{base}\n\nAdditional script APIs:\n{}",
                documented.join("\n")
            )
        }
    }

    pub fn split_execution(
        &self,
        tool: &RegisteredTool,
        mut arguments: Value,
    ) -> Result<(Value, bool), ToolError> {
        let object = arguments
            .as_object_mut()
            .ok_or(ToolError::ArgumentsMustBeObject)?;
        let background = match object.remove("bg") {
            None => false,
            Some(Value::Bool(value)) if tool.supports_background => value,
            Some(Value::Bool(_)) => {
                return Err(ToolError::BackgroundUnsupported(tool.name.clone()));
            }
            Some(_) => return Err(ToolError::InvalidBackground),
        };
        Ok((arguments, background))
    }
}

#[derive(Default)]
pub struct ToolRegistryBuilder {
    tools: BTreeMap<String, Arc<RegisteredTool>>,
}

impl ToolRegistryBuilder {
    pub fn extend(&mut self, registry: &ToolRegistry) -> Result<&mut Self, RegistryError> {
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
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        self.register_dynamic_inner(name, description, input_schema, options, None, handler)
    }

    pub fn register_dynamic_effects<F, Fut, E>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        options: ToolOptions,
        effect_resolver: E,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
        E: Fn(&Value) -> Result<Vec<ToolEffect>, ToolError> + Send + Sync + 'static,
    {
        self.register_dynamic_inner(
            name,
            description,
            input_schema,
            options,
            Some(Arc::new(effect_resolver)),
            handler,
        )
    }

    fn register_dynamic_inner<F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        options: ToolOptions,
        effect_resolver: Option<EffectResolver>,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        let name = name.into();
        validate_name(&name)?;
        validate_schema(&input_schema)?;
        if self.tools.contains_key(&name) {
            return Err(RegistryError::Duplicate(name));
        }
        let handler = Arc::new(move |context, arguments| {
            Box::pin(handler(context, arguments)) as BoxFuture<'static, _>
        });
        let ToolOptions {
            effects,
            supports_background,
            accepts_input,
            exposure,
            script_binding,
            availability,
            placement,
            path_arguments,
            mut output_schema,
        } = options;
        if let Some(output_schema) = &output_schema {
            validate_output_schema(output_schema)?;
        }
        if let Some(output_schema) = &mut output_schema {
            sanitize_schema(output_schema);
        }
        self.tools.insert(
            name.clone(),
            Arc::new(RegisteredTool {
                name,
                description: description.into(),
                input_schema,
                output_schema,
                effects,
                supports_background,
                accepts_input,
                exposure,
                script_binding,
                placement,
                path_arguments,
                handler,
                effect_resolver,
                availability,
            }),
        );
        Ok(self)
    }

    pub fn register<I, O, F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        options: ToolOptions,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: Serialize + JsonSchema + Send + 'static,
        F: Fn(ToolContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, ToolError>> + Send + 'static,
    {
        let input_schema = serde_json::to_value(schema_for!(I))
            .map_err(|error| RegistryError::Schema(error.to_string()))?;
        let output_schema = serde_json::to_value(schema_for!(O))
            .map_err(|error| RegistryError::Schema(error.to_string()))?;
        self.register_dynamic(
            name,
            description,
            input_schema,
            options.output_schema(output_schema),
            move |context, arguments| {
                let parsed = serde_json::from_value(arguments);
                let future = parsed.map(|input| handler(context, input));
                async move {
                    let output = future
                        .map_err(|error| ToolError::InvalidArguments(error.to_string()))?
                        .await?;
                    Ok(ToolOutput::new(serde_json::to_value(output)?))
                }
            },
        )
    }

    pub fn register_effectful<I, O, F, Fut, E>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        options: ToolOptions,
        effect_resolver: E,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: Serialize + JsonSchema + Send + 'static,
        F: Fn(ToolContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, ToolError>> + Send + 'static,
        E: Fn(&I) -> Vec<ToolEffect> + Send + Sync + 'static,
    {
        let input_schema = serde_json::to_value(schema_for!(I))
            .map_err(|error| RegistryError::Schema(error.to_string()))?;
        let output_schema = serde_json::to_value(schema_for!(O))
            .map_err(|error| RegistryError::Schema(error.to_string()))?;
        self.register_dynamic_effects(
            name,
            description,
            input_schema,
            options.output_schema(output_schema),
            move |arguments| {
                let input: I = serde_json::from_value(arguments.clone())
                    .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
                Ok(effect_resolver(&input))
            },
            move |context, arguments| {
                let parsed = serde_json::from_value(arguments);
                let future = parsed.map(|input| handler(context, input));
                async move {
                    let output = future
                        .map_err(|error| ToolError::InvalidArguments(error.to_string()))?
                        .await?;
                    Ok(ToolOutput::new(serde_json::to_value(output)?))
                }
            },
        )
    }

    #[must_use]
    pub fn build(self) -> ToolRegistry {
        ToolRegistry {
            tools: Arc::new(self.tools),
        }
    }
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

fn validate_schema(schema: &Value) -> Result<(), RegistryError> {
    let object = schema
        .as_object()
        .ok_or_else(|| RegistryError::Schema("root must be an object".to_owned()))?;
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return Err(RegistryError::Schema("root type must be object".to_owned()));
    }
    if object
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| properties.contains_key("bg"))
    {
        return Err(RegistryError::ReservedBackground);
    }
    Ok(())
}

fn validate_output_schema(schema: &Value) -> Result<(), RegistryError> {
    if schema.is_object() || schema.is_boolean() {
        Ok(())
    } else {
        Err(RegistryError::Schema(
            "output schema root must be an object or boolean".to_owned(),
        ))
    }
}

fn outbound_schema(schema: &Value, supports_background: bool) -> Value {
    let mut schema = schema.clone();
    if supports_background {
        let object = schema
            .as_object_mut()
            .expect("registered schemas have object roots");
        let properties = object
            .entry("properties")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("object schema properties are objects");
        properties.insert(
            "bg".to_owned(),
            serde_json::json!({
                "type": "boolean",
                "default": false,
                "description": "Run as a background job and return a job envelope immediately."
            }),
        );
    }
    sanitize_schema(&mut schema);
    schema
}

fn sanitize_schema(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("$schema");
            object.remove("title");
            object.remove("format");
            for value in object.values_mut() {
                sanitize_schema(value);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(sanitize_schema),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn describe_output(description: String, schema: Option<&Value>) -> String {
    schema.map_or(description.clone(), |schema| {
        format!("{description} Returns `{}`.", schema_type(schema, schema))
    })
}

#[derive(Clone, Serialize)]
pub(crate) struct ScriptManifest {
    name: String,
    properties: Vec<String>,
    required: Vec<String>,
    #[serde(flatten)]
    binding: ScriptManifestBinding,
}

#[derive(Clone, Serialize)]
#[serde(tag = "binding", rename_all = "snake_case")]
enum ScriptManifestBinding {
    TopLevel,
    JobMethod {
        method: String,
        job_argument: String,
    },
}

impl ScriptManifest {
    fn from_tool(tool: &RegisteredTool) -> Option<Self> {
        let (binding, excluded) = match &tool.script_binding {
            ScriptBinding::TopLevel => (ScriptManifestBinding::TopLevel, None),
            ScriptBinding::JobMethod {
                method,
                job_argument,
            } => (
                ScriptManifestBinding::JobMethod {
                    method: method.clone(),
                    job_argument: job_argument.clone(),
                },
                Some(job_argument.as_str()),
            ),
            ScriptBinding::Special | ScriptBinding::Unavailable => return None,
        };
        let definition = outbound_schema(&tool.input_schema, tool.supports_background);
        let properties = definition["properties"]
            .as_object()
            .map(|properties| {
                properties
                    .keys()
                    .filter(|property| Some(property.as_str()) != excluded)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let required = definition["required"]
            .as_array()
            .map(|required| {
                required
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|property| Some(*property) != excluded)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            name: tool.name.clone(),
            properties,
            required,
            binding,
        })
    }
}

fn script_documentation(tool: &RegisteredTool) -> String {
    let schema = outbound_schema(&tool.input_schema, tool.supports_background);
    let excluded = match &tool.script_binding {
        ScriptBinding::JobMethod { job_argument, .. } => Some(job_argument.as_str()),
        ScriptBinding::TopLevel | ScriptBinding::Special | ScriptBinding::Unavailable => None,
    };
    let required = schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let fields = schema["properties"]
        .as_object()
        .into_iter()
        .flat_map(|properties| properties.iter())
        .filter(|(name, _)| Some(name.as_str()) != excluded)
        .map(|(name, field)| {
            let optional = if required.contains(&name.as_str()) {
                ""
            } else {
                "?"
            };
            let default = field
                .get("default")
                .map_or_else(String::new, |value| format!("={}", compact_json(value)));
            format!("{name}{optional}: {}{default}", schema_type(field, &schema))
        })
        .collect::<Vec<_>>();
    let arguments = if fields.is_empty() {
        "()".to_owned()
    } else {
        format!("({{{}}})", fields.join(", "))
    };
    let call = match &tool.script_binding {
        ScriptBinding::TopLevel => format!("tool.{}{arguments}", tool.name),
        ScriptBinding::JobMethod { method, .. } => format!("tool.job(job).{method}{arguments}"),
        ScriptBinding::Special | ScriptBinding::Unavailable => unreachable!(),
    };
    let field_docs = schema["properties"]
        .as_object()
        .into_iter()
        .flat_map(|properties| properties.iter())
        .filter(|(name, field)| {
            Some(name.as_str()) != excluded && field.get("description").is_some()
        })
        .map(|(name, field)| {
            format!(
                " `{name}`: {}",
                field["description"].as_str().unwrap_or_default()
            )
        })
        .collect::<String>();
    let description = describe_output(tool.description.clone(), tool.output_schema.as_ref());
    format!("- `{call}` — {description}{field_docs}")
}

fn schema_type(field: &Value, root: &Value) -> String {
    if let Some(reference) = field.get("$ref").and_then(Value::as_str)
        && let Some(name) = reference.strip_prefix("#/$defs/")
        && let Some(definition) = root.get("$defs").and_then(|defs| defs.get(name))
    {
        return schema_type(definition, root);
    }
    if let Some(values) = field.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .map(compact_json)
            .collect::<Vec<_>>()
            .join(" | ");
    }
    if let Some(variants) = field
        .get("anyOf")
        .or_else(|| field.get("oneOf"))
        .and_then(Value::as_array)
    {
        let types = variants
            .iter()
            .map(|variant| schema_type(variant, root))
            .collect::<Vec<_>>();
        return types.join(" | ");
    }
    match field.get("type").and_then(Value::as_str) {
        Some("string" | "integer" | "number" | "boolean" | "null") => {
            field["type"].as_str().unwrap_or("JSON").to_owned()
        }
        Some("array") => format!("{}[]", schema_type(&field["items"], root)),
        Some("object") => {
            let required = field["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>();
            let properties = field["properties"]
                .as_object()
                .into_iter()
                .flat_map(|properties| properties.iter())
                .map(|(name, value)| {
                    let optional = if required.contains(&name.as_str()) {
                        ""
                    } else {
                        "?"
                    };
                    format!("{name}{optional}:{}", schema_type(value, root))
                })
                .collect::<Vec<_>>();
            if properties.is_empty() {
                "object".to_owned()
            } else {
                format!("{{{}}}", properties.join(", "))
            }
        }
        _ => "JSON".to_owned(),
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "JSON".to_owned())
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
}

#[cfg(test)]
mod tests {
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Args {
        value: String,
    }

    fn context(depth: usize) -> ToolVisibilityContext {
        let mut agent =
            crate::identity::AgentId::root(crate::identity::SessionId::from_bytes([1; 16]));
        for segment in 0..depth {
            agent = agent.child(u32::try_from(segment + 1).unwrap());
        }
        ToolVisibilityContext::new(agent)
    }

    fn contains_metadata(value: &Value) -> bool {
        match value {
            Value::Object(object) => {
                object
                    .keys()
                    .any(|key| matches!(key.as_str(), "$schema" | "title" | "format"))
                    || object.values().any(contains_metadata)
            }
            Value::Array(values) => values.iter().any(contains_metadata),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
        }
    }

    #[test]
    fn background_is_generated_not_owned_by_handler_schema() {
        let mut builder = ToolRegistry::builder();
        builder
            .register::<Args, String, _, _>(
                "echo",
                "Echo input",
                ToolOptions {
                    effects: Vec::new(),
                    supports_background: true,
                    accepts_input: false,
                    exposure: ToolExposure::ModelVisible,
                    script_binding: ScriptBinding::TopLevel,
                    availability: None,
                    placement: ToolPlacement::Host,
                    path_arguments: Vec::new(),
                    output_schema: None,
                },
                |_context, args| async move { Ok(args.value) },
            )
            .unwrap();
        let registry = builder.build();
        let tool = registry.get("echo").unwrap();
        assert!(registry.definitions(&context(0))[0].input_schema["properties"]["bg"].is_object());
        let (arguments, background) = registry
            .split_execution(&tool, serde_json::json!({"value":"x", "bg":true}))
            .unwrap();
        assert!(background);
        assert!(arguments.get("bg").is_none());
        assert_eq!(tool.output_schema.as_ref().unwrap()["type"], "string");
        assert!(
            registry.definitions(&context(0))[0]
                .description
                .contains("Returns `string`.")
        );
    }

    #[test]
    fn outbound_schemas_drop_only_prompt_metadata() {
        let schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Root",
            "type": "object",
            "additionalProperties": false,
            "required": ["item"],
            "properties": {
                "item": {
                    "title": "Item",
                    "anyOf": [{"$ref": "#/$defs/Item"}, {"type": "null"}],
                    "default": null,
                    "description": "An optional bounded item.",
                    "format": "custom"
                }
            },
            "$defs": {
                "Item": {"title": "Item", "type": "integer", "minimum": 1, "maximum": 9}
            }
        });
        let mut builder = ToolRegistry::builder();
        builder
            .register_dynamic(
                "strict",
                "Strict",
                schema,
                ToolOptions::new(Vec::new()).background(),
                |_context, _arguments| async { Ok(ToolOutput::new(Value::Null)) },
            )
            .unwrap();
        let output = &builder.build().definitions(&context(0))[0].input_schema;
        assert!(!contains_metadata(output));
        assert_eq!(output["additionalProperties"], false);
        assert_eq!(output["properties"]["item"]["default"], Value::Null);
        assert_eq!(
            output["properties"]["item"]["description"],
            "An optional bounded item."
        );
        assert_eq!(output["properties"]["item"]["anyOf"][1]["type"], "null");
        assert_eq!(output["$defs"]["Item"]["minimum"], 1);
        assert_eq!(output["$defs"]["Item"]["maximum"], 9);
        assert_eq!(output["properties"]["bg"]["default"], false);
    }

    #[test]
    fn defaults_and_independent_predicates_drive_all_registry_queries() {
        let mut builder = ToolRegistry::builder();
        for (name, options) in [
            ("always", ToolOptions::default()),
            (
                "shallow",
                ToolOptions::default().available_when(|context| context.agent.depth() < 1),
            ),
            (
                "deep",
                ToolOptions::default().available_when(|context| context.agent.depth() > 0),
            ),
        ] {
            builder
                .register::<Args, String, _, _>(name, name, options, |_context, args| async move {
                    Ok(args.value)
                })
                .unwrap();
        }
        let registry = builder.build();
        let names = |depth| {
            registry
                .definitions(&context(depth))
                .into_iter()
                .map(|definition| definition.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(0), ["always", "shallow"]);
        assert_eq!(names(1), ["always", "deep"]);
        assert!(
            registry
                .script_manifests(&context(0))
                .iter()
                .any(|manifest| manifest.name == "always")
        );
    }
}
