use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::Arc,
};

use futures_util::future::BoxFuture;
use schemars::{JsonSchema, schema_for};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{
    provider::protocol::ToolDefinition as ProviderToolDefinition,
    tool::policy::{Capability, CapabilitySet, PathAccess, ResourceId},
};

use super::{ToolContext, ToolError, ToolOutput};

#[derive(Clone)]
pub struct RegisteredTool {
    definition: Arc<dyn ToolDefinition>,
    execution: ToolExecution,
    handler: ToolHandler,
}

type ToolHandler = Arc<
    dyn Fn(ToolContext, Value) -> BoxFuture<'static, Result<ToolOutput, ToolError>> + Send + Sync,
>;
type CapabilityResolver = Arc<dyn Fn(&Value) -> Result<Vec<Capability>, ToolError> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub exposure: ToolExposure,
    pub script_binding: ScriptBinding,
}

/// Generates the final model-facing definition for a particular capability set.
///
/// Dynamic adapters can implement this trait directly. The returned schemas must
/// already be final: the registry never removes fields from them afterward.
pub trait ToolDefinition: Send + Sync {
    fn name(&self) -> &str;
    fn generate(&self, capabilities: &CapabilitySet) -> Option<ToolSpec>;
}

type SchemaGenerator = Arc<dyn Fn(&CapabilitySet) -> Value + Send + Sync>;

#[derive(Clone)]
struct GeneratedToolDefinition {
    name: String,
    description: String,
    input_schema: SchemaGenerator,
    output_schema: Option<SchemaGenerator>,
    exposure: ToolExposure,
    script_binding: ScriptBinding,
    supports_background: bool,
    required: BTreeSet<Capability>,
}

impl ToolDefinition for GeneratedToolDefinition {
    fn name(&self) -> &str {
        &self.name
    }

    fn generate(&self, capabilities: &CapabilitySet) -> Option<ToolSpec> {
        self.required
            .iter()
            .all(|capability| capabilities.contains(*capability))
            .then(|| {
                let mut input_schema = (self.input_schema)(capabilities);
                if self.supports_background {
                    add_background(&mut input_schema);
                }
                sanitize_schema(&mut input_schema);
                let output_schema = self.output_schema.as_ref().map(|schema| {
                    let mut schema = schema(capabilities);
                    sanitize_schema(&mut schema);
                    schema
                });
                ToolSpec {
                    name: self.name.clone(),
                    description: self.description.clone(),
                    input_schema,
                    output_schema,
                    exposure: self.exposure,
                    script_binding: self.script_binding.clone(),
                }
            })
    }
}

macro_rules! dynamic_registrations {
    ($($method:ident => $placement:expr),+ $(,)?) => {$(
        pub fn $method<F, Fut>(
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
            self.register_dynamic_at(name, description, input_schema, options, $placement, handler)
        }
    )+};
}

macro_rules! typed_registrations {
    ($($method:ident => $placement:expr),+ $(,)?) => {$(
        pub fn $method<I, O, F, Fut>(
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
            self.register_typed_at(name, description, options, $placement, handler)
        }
    )+};
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolPlacement {
    #[default]
    Host,
    InheritWorkspace,
    TargetedWorkspace,
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
}

#[derive(Clone)]
pub struct ToolOptions {
    execution: ToolExecution,
    pub supports_background: bool,
    exposure: ToolExposure,
    script_binding: ScriptBinding,
    required: BTreeSet<Capability>,
    conditional_inputs: Vec<(String, Capability, Value)>,
    output_schema: Option<SchemaGenerator>,
}

/// Execution metadata paired with an erased [`ToolDefinition`].
#[derive(Clone, Default)]
pub struct ToolExecution {
    pub capabilities: Vec<Capability>,
    pub accepts_input: bool,
    placement: ToolPlacement,
    permission_resource: Option<ResourceId>,
    path_arguments: Vec<PathArgument>,
    capability_resolver: Option<CapabilityResolver>,
}

impl ToolExecution {
    #[must_use]
    pub fn new(capabilities: Vec<Capability>) -> Self {
        Self {
            capabilities,
            ..Self::default()
        }
    }

    #[must_use]
    pub const fn input(mut self) -> Self {
        self.accepts_input = true;
        self
    }

    #[must_use]
    pub const fn placement(mut self, placement: ToolPlacement) -> Self {
        self.placement = placement;
        self
    }

    #[must_use]
    pub fn permission_resource(mut self, resource: ResourceId) -> Self {
        self.permission_resource = Some(resource);
        self
    }
}

impl ToolOptions {
    #[must_use]
    pub fn new(capabilities: Vec<Capability>) -> Self {
        let required = capabilities.iter().copied().collect();
        Self {
            supports_background: false,
            execution: ToolExecution::new(capabilities),
            exposure: ToolExposure::ModelVisible,
            script_binding: ScriptBinding::TopLevel,
            required,
            conditional_inputs: Vec::new(),
            output_schema: None,
        }
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
        mut self,
        name: impl Into<String>,
        capability: Capability,
        schema: Value,
    ) -> Self {
        self.conditional_inputs
            .push((name.into(), capability, schema));
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
        self.execution.path_arguments.push(PathArgument {
            name: name.into(),
            access,
            kind,
            default: None,
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
    ) -> Self {
        self.execution.path_arguments.push(PathArgument {
            name: name.into(),
            access,
            kind,
            default: Some(default.into()),
        });
        self
    }

    #[must_use]
    pub fn output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(Arc::new(move |_| schema.clone()));
        self
    }

    #[must_use]
    pub fn generated_output_schema(
        mut self,
        generator: impl Fn(&CapabilitySet) -> Value + Send + Sync + 'static,
    ) -> Self {
        self.output_schema = Some(Arc::new(generator));
        self
    }

    #[must_use]
    pub fn permission_resource(mut self, resource: ResourceId) -> Self {
        self.execution.permission_resource = Some(resource);
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

    pub fn capabilities_for(&self, arguments: &Value) -> Result<Vec<Capability>, ToolError> {
        self.execution.capability_resolver.as_ref().map_or_else(
            || Ok(self.execution.capabilities.clone()),
            |resolver| resolver(arguments),
        )
    }

    #[must_use]
    pub fn name(&self) -> &str {
        self.definition.name()
    }

    #[must_use]
    pub(crate) const fn placement(&self) -> ToolPlacement {
        self.execution.placement
    }

    pub(crate) fn path_arguments(&self) -> &[PathArgument] {
        &self.execution.path_arguments
    }

    pub(crate) const fn accepts_input(&self) -> bool {
        self.execution.accepts_input
    }

    pub(crate) fn permission_resource(&self) -> Option<&ResourceId> {
        self.execution.permission_resource.as_ref()
    }
}

impl ToolSpec {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            output_schema: None,
            exposure: ToolExposure::ModelVisible,
            script_binding: ScriptBinding::TopLevel,
        }
    }

    #[must_use]
    pub fn output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }
}

#[derive(Clone, Default)]
pub struct ToolSurface {
    tools: BTreeMap<String, ToolSpec>,
}

impl ToolSurface {
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    pub fn validate_arguments(&self, name: &str, arguments: &Value) -> Result<(), ToolError> {
        let tool = self
            .get(name)
            .ok_or_else(|| ToolError::InvalidArguments(format!("tool `{name}` is unavailable")))?;
        let arguments = arguments
            .as_object()
            .ok_or(ToolError::ArgumentsMustBeObject)?;
        if tool.input_schema["additionalProperties"] == false
            && let Some(argument) = arguments.keys().find(|argument| {
                !tool.input_schema["properties"]
                    .as_object()
                    .is_some_and(|properties| properties.contains_key(*argument))
            })
        {
            return Err(ToolError::InvalidArguments(format!(
                "unknown argument `{argument}`"
            )));
        }
        Ok(())
    }

    #[must_use]
    pub fn definitions(&self) -> Vec<ProviderToolDefinition> {
        self.tools
            .values()
            .filter(|tool| tool.exposure == ToolExposure::ModelVisible)
            .map(|tool| {
                let description = if tool.name == "script" {
                    self.script_description(&tool.description)
                } else {
                    tool.description.clone()
                };
                ProviderToolDefinition {
                    name: tool.name.clone(),
                    description: describe_output(description, tool.output_schema.as_ref()),
                    input_schema: tool.input_schema.clone(),
                }
            })
            .collect()
    }

    pub(crate) fn script_manifests(&self) -> Vec<ScriptManifest> {
        self.tools
            .values()
            .filter_map(ScriptManifest::from_tool)
            .collect()
    }

    fn script_description(&self, base: &str) -> String {
        let documented = self
            .tools
            .values()
            .filter(|tool| match &tool.script_binding {
                ScriptBinding::TopLevel => tool.exposure == ToolExposure::ScriptOnly,
                ScriptBinding::JobMethod { .. } => {
                    tool.exposure == ToolExposure::ScriptOnly || tool.name == "wait"
                }
                ScriptBinding::Unavailable => false,
            })
            .map(script_documentation)
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

    pub fn tools(&self) -> impl Iterator<Item = &Arc<RegisteredTool>> {
        self.tools.values()
    }

    #[must_use]
    pub fn surface(&self, capabilities: &CapabilitySet) -> ToolSurface {
        let tools = self
            .tools
            .values()
            .filter_map(|tool| {
                tool.definition
                    .generate(capabilities)
                    .map(|spec| (spec.name.clone(), spec))
            })
            .collect();
        ToolSurface { tools }
    }

    pub fn split_execution(
        &self,
        tool: &ToolSpec,
        mut arguments: Value,
    ) -> Result<(Value, bool), ToolError> {
        let object = arguments
            .as_object_mut()
            .ok_or(ToolError::ArgumentsMustBeObject)?;
        let supports_background = tool.input_schema["properties"]["bg"].is_object();
        let background = match object.remove("bg") {
            None => false,
            Some(Value::Bool(value)) if supports_background => value,
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

    dynamic_registrations! {
        register_dynamic => ToolPlacement::Host,
        register_dynamic_workspace => ToolPlacement::InheritWorkspace,
        register_dynamic_targeted => ToolPlacement::TargetedWorkspace,
    }

    pub fn register_erased<F, Fut>(
        &mut self,
        definition: Arc<dyn ToolDefinition>,
        execution: ToolExecution,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        self.register_definition(definition, execution, handler)
    }

    fn register_dynamic_at<F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        mut options: ToolOptions,
        placement: ToolPlacement,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        options.execution.placement = placement;
        if placement == ToolPlacement::TargetedWorkspace {
            ensure_no_target(&input_schema)?;
            options =
                options.conditional_input("target", Capability::Targets, target_property_schema());
        }
        self.register_dynamic_inner((name, description), input_schema, options, handler)
    }

    fn register_dynamic_inner<F, Fut>(
        &mut self,
        identity: (impl Into<String>, impl Into<String>),
        input_schema: Value,
        options: ToolOptions,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        let (name, description) = identity;
        let name = name.into();
        validate_name(&name)?;
        validate_schema(&input_schema)?;
        let ToolOptions {
            execution,
            supports_background,
            exposure,
            script_binding,
            required,
            conditional_inputs,
            output_schema,
        } = options;
        let schema = move |capabilities: &CapabilitySet| {
            let mut schema = input_schema.clone();
            for (name, capability, property) in &conditional_inputs {
                if capabilities.contains(*capability) {
                    add_schema_property(&mut schema, name, property.clone());
                }
            }
            schema
        };
        let definition = Arc::new(GeneratedToolDefinition {
            name,
            description: description.into(),
            input_schema: Arc::new(schema),
            output_schema,
            exposure,
            script_binding,
            supports_background,
            required,
        });
        self.register_definition(definition, execution, handler)
    }

    fn register_definition<F, Fut>(
        &mut self,
        definition: Arc<dyn ToolDefinition>,
        execution: ToolExecution,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        let name = definition.name().to_owned();
        validate_name(&name)?;
        if self.tools.contains_key(&name) {
            return Err(RegistryError::Duplicate(name));
        }
        for capabilities in capability_subsets() {
            if let Some(spec) = definition.generate(&capabilities) {
                if spec.name != name {
                    return Err(RegistryError::DefinitionName {
                        registered: name,
                        generated: spec.name,
                    });
                }
                validate_object_schema(&spec.input_schema)?;
                if let Some(schema) = &spec.output_schema {
                    validate_output_schema(schema)?;
                }
            }
        }
        let handler = Arc::new(move |context, arguments| {
            Box::pin(handler(context, arguments)) as BoxFuture<'static, _>
        });
        self.tools.insert(
            name,
            Arc::new(RegisteredTool {
                definition,
                execution,
                handler,
            }),
        );
        Ok(self)
    }

    typed_registrations! {
        register => ToolPlacement::Host,
        register_workspace => ToolPlacement::InheritWorkspace,
        register_targeted => ToolPlacement::TargetedWorkspace,
    }

    fn register_typed_at<I, O, F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        mut options: ToolOptions,
        placement: ToolPlacement,
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
        if options.output_schema.is_none() {
            options = options.output_schema(output_schema);
        }
        if placement == ToolPlacement::TargetedWorkspace {
            ensure_no_target(&input_schema)?;
            options =
                options.conditional_input("target", Capability::Targets, target_property_schema());
        }
        options.execution.placement = placement;
        self.register_dynamic_inner(
            (name, description),
            input_schema,
            options,
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

    pub fn register_capability_resolver<I, O, F, Fut, E>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        options: ToolOptions,
        capability_resolver: E,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: Serialize + JsonSchema + Send + 'static,
        F: Fn(ToolContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, ToolError>> + Send + 'static,
        E: Fn(&I) -> Vec<Capability> + Send + Sync + 'static,
    {
        let mut options = options;
        options.execution.capability_resolver = Some(Arc::new(move |arguments| {
            let input: I = serde_json::from_value(arguments.clone())
                .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
            Ok(capability_resolver(&input))
        }));
        self.register_typed_at(name, description, options, ToolPlacement::Host, handler)
    }

    #[must_use]
    pub fn build(self) -> ToolRegistry {
        ToolRegistry {
            tools: Arc::new(self.tools),
        }
    }
}

fn ensure_no_target(schema: &Value) -> Result<(), RegistryError> {
    validate_schema(schema)?;
    if schema["properties"].get("target").is_some() {
        return Err(RegistryError::ReservedTarget);
    }
    Ok(())
}

fn target_property_schema() -> Value {
    serde_json::json!({
        "type": ["string", "null"],
        "description": "Execution target. Omit to inherit the calling agent's target."
    })
}

fn add_schema_property(schema: &mut Value, name: &str, property: Value) {
    schema
        .as_object_mut()
        .expect("registered schemas have object roots")
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("validated schema properties are objects")
        .insert(name.to_owned(), property);
}

fn capability_subsets() -> impl Iterator<Item = CapabilitySet> {
    (0_usize..(1 << Capability::ALL.len())).map(|mask| {
        let mut capabilities = CapabilitySet::default();
        Capability::ALL
            .iter()
            .enumerate()
            .for_each(|(index, capability)| {
                if mask & (1 << index) == 0 {
                    capabilities.remove(*capability);
                } else {
                    capabilities.insert(*capability);
                }
            });
        capabilities
    })
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
    validate_object_schema(schema)?;
    if schema["properties"].get("bg").is_some() {
        return Err(RegistryError::ReservedBackground);
    }
    Ok(())
}

fn validate_object_schema(schema: &Value) -> Result<(), RegistryError> {
    let object = schema
        .as_object()
        .ok_or_else(|| RegistryError::Schema("root must be an object".to_owned()))?;
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return Err(RegistryError::Schema("root type must be object".to_owned()));
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

fn add_background(schema: &mut Value) {
    add_schema_property(
        schema,
        "bg",
        serde_json::json!({
            "type": "boolean",
            "default": false,
            "description": "Run as a background job and return a job envelope immediately."
        }),
    );
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
    fn from_tool(tool: &ToolSpec) -> Option<Self> {
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
            ScriptBinding::Unavailable => return None,
        };
        let properties = tool.input_schema["properties"]
            .as_object()
            .map(|properties| {
                properties
                    .keys()
                    .filter(|property| Some(property.as_str()) != excluded)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let required = tool.input_schema["required"]
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

fn script_documentation(tool: &ToolSpec) -> String {
    let schema = &tool.input_schema;
    let excluded = match &tool.script_binding {
        ScriptBinding::JobMethod { job_argument, .. } => Some(job_argument.as_str()),
        ScriptBinding::TopLevel | ScriptBinding::Unavailable => None,
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
            format!("{name}{optional}: {}{default}", schema_type(field, schema))
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
        ScriptBinding::Unavailable => unreachable!(),
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
    #[error("tool definition registered as `{registered}` generated the name `{generated}`")]
    DefinitionName {
        registered: String,
        generated: String,
    },
    #[error("tool schema is invalid: {0}")]
    Schema(String),
    #[error("`bg` is reserved by the harness")]
    ReservedBackground,
    #[error("`target` is reserved for structurally targeted tools")]
    ReservedTarget,
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

    fn context(agents: bool) -> CapabilitySet {
        let mut capabilities = crate::tool::policy::CapabilitySet::default();
        if !agents {
            capabilities.remove(Capability::Agents);
        }
        capabilities
    }

    fn target_context(enabled: bool) -> CapabilitySet {
        let mut context = context(true);
        if enabled {
            context.insert(Capability::Targets);
        }
        context
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
                ToolOptions::default().background(),
                |_context, args| async move { Ok(args.value) },
            )
            .unwrap();
        let registry = builder.build();
        let surface = registry.surface(&context(true));
        let tool = surface.get("echo").unwrap();
        assert!(surface.definitions()[0].input_schema["properties"]["bg"].is_object());
        let (arguments, background) = registry
            .split_execution(tool, serde_json::json!({"value":"x", "bg":true}))
            .unwrap();
        assert!(background);
        assert!(arguments.get("bg").is_none());
        assert_eq!(
            surface.get("echo").unwrap().output_schema.as_ref().unwrap()["type"],
            "string"
        );
        assert!(
            registry.surface(&context(true)).definitions()[0]
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
        let output = &builder.build().surface(&context(true)).definitions()[0].input_schema;
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
    fn capabilities_drive_all_registry_queries() {
        let mut builder = ToolRegistry::builder();
        for (name, options) in [
            ("always", ToolOptions::default()),
            ("agent", ToolOptions::default().requires(Capability::Agents)),
        ] {
            builder
                .register::<Args, String, _, _>(name, name, options, |_context, args| async move {
                    Ok(args.value)
                })
                .unwrap();
        }
        let registry = builder.build();
        let names = |agents| {
            registry
                .surface(&context(agents))
                .definitions()
                .into_iter()
                .map(|definition| definition.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(false), ["always"]);
        assert_eq!(names(true), ["agent", "always"]);
        assert!(
            registry
                .surface(&context(true))
                .script_manifests()
                .iter()
                .any(|manifest| manifest.name == "always")
        );
    }

    #[test]
    fn targeted_metadata_is_filtered_from_provider_and_javascript_surfaces() {
        let mut builder = ToolRegistry::builder();
        builder
            .register_dynamic_targeted(
                "renamed_remote_tool",
                "A structurally targeted dynamic tool.",
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"value": {"type": "string"}}
                }),
                ToolOptions::default(),
                |_context, arguments| async move { Ok(ToolOutput::new(arguments)) },
            )
            .unwrap();
        let registry = builder.build();

        let hidden = registry.surface(&target_context(false));
        let definition = &hidden.definitions()[0];
        assert!(
            definition.input_schema["properties"]
                .get("target")
                .is_none()
        );
        let manifest = &hidden.script_manifests()[0];
        assert!(!manifest.properties.iter().any(|name| name == "target"));
        assert!(
            hidden
                .validate_arguments(
                    "renamed_remote_tool",
                    &serde_json::json!({"target": "build"}),
                )
                .is_err()
        );

        let enabled = registry.surface(&target_context(true));
        assert!(enabled.definitions()[0].input_schema["properties"]["target"].is_object());
        assert!(
            enabled.script_manifests()[0]
                .properties
                .iter()
                .any(|name| name == "target")
        );
    }

    #[test]
    fn erased_definitions_generate_final_capability_aware_schemas() {
        struct AdapterDefinition;

        impl ToolDefinition for AdapterDefinition {
            fn name(&self) -> &str {
                "adapter_tool"
            }

            fn generate(&self, capabilities: &CapabilitySet) -> Option<ToolSpec> {
                let mut schema = serde_json::json!({
                    "type": "object",
                    "properties": {"query": {"type": "string"}}
                });
                if capabilities.contains(Capability::Targets) {
                    add_schema_property(
                        &mut schema,
                        "endpoint",
                        serde_json::json!({"type": "string"}),
                    );
                }
                Some(ToolSpec::new(
                    self.name(),
                    "A dynamically supplied tool.",
                    schema,
                ))
            }
        }

        let mut builder = ToolRegistry::builder();
        builder
            .register_erased(
                Arc::new(AdapterDefinition),
                ToolExecution::default().placement(ToolPlacement::TargetedWorkspace),
                |_context, arguments| async move { Ok(ToolOutput::new(arguments)) },
            )
            .unwrap();
        let registry = builder.build();

        assert!(
            registry.surface(&target_context(false)).definitions()[0].input_schema["properties"]
                .get("endpoint")
                .is_none()
        );
        assert!(registry.surface(&target_context(true)).definitions()[0].input_schema
            ["properties"]["endpoint"]
            .is_object());
    }
}
