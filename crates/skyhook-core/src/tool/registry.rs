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
    execution::ExecutionLocation,
    provider::protocol::ToolDefinition as ProviderToolDefinition,
    tool::policy::{Capability, CapabilitySet, PathAccess, PermissionUse, ResourceId},
};

use super::{ToolContext, ToolError, ToolOutput};

#[derive(Clone)]
pub struct RegisteredTool {
    definition: GeneratedToolDefinition,
    execution: ToolExecution,
    handler: ToolHandler,
}

type ToolHandler = Arc<
    dyn Fn(ToolContext, Value) -> BoxFuture<'static, Result<ToolOutput, ToolError>> + Send + Sync,
>;
type ArgumentPermissions =
    Arc<dyn Fn(&ExecutionLocation, &Value) -> Result<Vec<PermissionUse>, ToolError> + Send + Sync>;
type ArgumentPaths = Arc<dyn Fn(&Value) -> Result<Vec<PathArgument>, ToolError> + Send + Sync>;
type ArgumentValidator = Arc<dyn Fn(&Value) -> Result<(), ToolError> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct ToolSpec {
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
    pub(crate) fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolError> {
        let arguments = arguments
            .as_object()
            .ok_or(ToolError::ArgumentsMustBeObject)?;
        if self.input_schema["additionalProperties"] == false
            && let Some(argument) = arguments.keys().find(|argument| {
                !self.input_schema["properties"]
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
}

type SchemaGenerator = Arc<dyn Fn(&CapabilitySet) -> Value + Send + Sync>;

#[derive(Clone)]
enum OutputSchema {
    Static(Value),
    Generated(SchemaGenerator),
}

impl OutputSchema {
    fn generate(&self, capabilities: &CapabilitySet) -> Value {
        match self {
            Self::Static(schema) => schema.clone(),
            Self::Generated(generate) => generate(capabilities),
        }
    }
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

impl GeneratedToolDefinition {
    fn generate(&self, capabilities: &CapabilitySet) -> Option<ToolSpec> {
        self.generate_scoped(capabilities, false)
    }

    fn generate_scoped(&self, capabilities: &CapabilitySet, child: bool) -> Option<ToolSpec> {
        self.required
            .iter()
            .chain(self.root_required.iter().filter(|_| !child))
            .all(|capability| capabilities.contains(*capability))
            .then(|| {
                let mut input_schema = (self.input_schema)(capabilities);
                if self.supports_background {
                    add_background(&mut input_schema);
                }
                if !self.preserve_required {
                    optional_defaults(&mut input_schema);
                }
                sanitize_schema_inner(&mut input_schema, self.preserve_schema_dialect);
                let result_schema = self.output_schema.as_ref().map(|schema| {
                    let mut schema = schema.generate(capabilities);
                    sanitize_schema(&mut schema);
                    schema
                });
                let output_schema = result_schema.as_ref().map(|schema| {
                    let mut schema = schema.clone();
                    if self.supports_background {
                        schema = output_union(
                            schema,
                            crate::job::presented_job_schema(capabilities, false),
                        );
                    }
                    sanitize_schema(&mut schema);
                    schema
                });
                ToolSpec {
                    name: self.name.clone(),
                    description: self.description.clone(),
                    input_schema,
                    result_schema,
                    output_schema,
                    exposure: self.exposure,
                    script_binding: self.script_binding.clone(),
                }
            })
    }
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
pub enum PathKind {
    Existing,
    Writable,
    Removable,
}

#[derive(Clone, Debug)]
pub struct PathArgument {
    pub(crate) name: String,
    pub(crate) access: PathAccess,
    pub(crate) kind: PathKind,
    pub(crate) default: Option<String>,
    /// JSON pointer for nested paths; unlike legacy top-level paths these always
    /// contribute a permission, even inside the authorization root.
    pub(crate) pointer: Option<String>,
}

impl PathArgument {
    #[must_use]
    pub fn pointer(pointer: impl Into<String>, access: PathAccess, kind: PathKind) -> Self {
        let pointer = pointer.into();
        Self {
            name: pointer.clone(),
            access,
            kind,
            default: None,
            pointer: Some(pointer),
        }
    }
}

#[derive(Clone)]
pub struct ToolOptions {
    execution: ToolExecution,
    pub supports_background: bool,
    preserve_required: bool,
    preserve_schema_dialect: bool,
    exposure: ToolExposure,
    script_binding: ScriptBinding,
    required: BTreeSet<Capability>,
    root_required: BTreeSet<Capability>,
    conditional_inputs: Vec<(String, Capability, Value)>,
    output_schema: Option<OutputSchema>,
}

/// Internal execution metadata.
#[derive(Clone, Default)]
struct ToolExecution {
    pub capabilities: Vec<Capability>,
    pub accepts_input: bool,
    supports_name: bool,
    placement: ToolPlacement,
    permission_resource: Option<ResourceId>,
    path_arguments: Vec<PathArgument>,
    argument_validator: Option<ArgumentValidator>,
    argument_permissions: Option<ArgumentPermissions>,
    argument_paths: Option<ArgumentPaths>,
    read_error_output: Option<fn(&str, &ToolError) -> Option<ToolOutput>>,
}

impl ToolOptions {
    /// Built-in read only: expected OS read failures are successful structured results.
    pub(crate) fn read_error_output(
        mut self,
        convert: fn(&str, &ToolError) -> Option<ToolOutput>,
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
            exposure: ToolExposure::ModelVisible,
            script_binding: ScriptBinding::TopLevel,
            required,
            root_required: BTreeSet::new(),
            conditional_inputs: Vec::new(),
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
            pointer: None,
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
            pointer: None,
        });
        self
    }

    /// Extract invocation-specific permissions after tool argument validation.
    /// These replace static permissions of the same capability. Remote path and
    /// network permissions are forwarded from the destination for host approval;
    /// other namespaces are authorized by the host before dispatch.
    #[must_use]
    pub fn argument_permissions(
        mut self,
        extract: impl Fn(&ExecutionLocation, &Value) -> Result<Vec<PermissionUse>, ToolError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.execution.argument_permissions = Some(Arc::new(extract));
        self
    }

    /// Extract concrete JSON pointers (including array indices) to filesystem
    /// inputs/outputs. Each path is resolved and rewritten before authorization.
    /// Dynamic paths require their capability even when inside the workspace.
    #[must_use]
    pub fn argument_paths(
        mut self,
        extract: impl Fn(&Value) -> Result<Vec<PathArgument>, ToolError> + Send + Sync + 'static,
    ) -> Self {
        self.execution.argument_paths = Some(Arc::new(extract));
        self
    }

    /// Validate tool-specific arguments before requesting authorization.
    #[must_use]
    pub fn argument_validator(
        mut self,
        validate: impl Fn(&Value) -> Result<(), ToolError> + Send + Sync + 'static,
    ) -> Self {
        self.execution.argument_validator = Some(Arc::new(validate));
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

impl Default for ToolOptions {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl RegisteredTool {
    pub(crate) fn output_schema(&self, capabilities: &CapabilitySet) -> Option<Value> {
        self.definition
            .output_schema
            .as_ref()
            .map(|schema| schema.generate(capabilities))
    }

    pub(crate) fn take_job_name(&self, arguments: &mut Value) -> Result<Option<String>, ToolError> {
        if !self.execution.supports_name {
            return Ok(None);
        }
        let value = arguments
            .as_object_mut()
            .ok_or(ToolError::ArgumentsMustBeObject)?
            .remove("name");
        match value {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(name)) if valid_job_name(&name) => Ok(Some(name)),
            _ => Err(ToolError::InvalidArguments(
                "name must be lowercase kebab-case: start with a letter, use only a-z, 0-9, and single hyphens between nonempty words".to_owned(),
            )),
        }
    }

    pub async fn call(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let read_path = self.execution.read_error_output.and_then(|_| {
            arguments
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        match (self.handler)(context, arguments).await {
            Err(error) => match read_path.and_then(|path| self.read_error_output(&path, &error)) {
                Some(output) => Ok(output),
                None => Err(error),
            },
            result => result,
        }
    }

    pub(crate) fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolError> {
        if let Some(validate) = &self.execution.argument_validator {
            validate(arguments)?;
        }
        Ok(())
    }

    pub(crate) fn capabilities(&self) -> Vec<Capability> {
        self.execution.capabilities.clone()
    }

    pub(crate) fn spec(
        &self,
        capabilities: &CapabilitySet,
        agent: &crate::identity::AgentId,
    ) -> Option<ToolSpec> {
        self.definition
            .generate_scoped(capabilities, agent.parent().is_some())
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.definition.name
    }

    #[must_use]
    pub(crate) const fn placement(&self) -> ToolPlacement {
        self.execution.placement
    }

    pub(crate) fn read_error_output(&self, path: &str, error: &ToolError) -> Option<ToolOutput> {
        self.execution
            .read_error_output
            .and_then(|convert| convert(path, error))
    }

    pub(crate) fn path_arguments(&self, arguments: &Value) -> Result<Vec<PathArgument>, ToolError> {
        let mut paths = self.execution.path_arguments.clone();
        if let Some(extract) = &self.execution.argument_paths {
            paths.extend(extract(arguments)?);
        }
        Ok(paths)
    }

    pub(crate) fn argument_permissions(
        &self,
        location: &ExecutionLocation,
        arguments: &Value,
    ) -> Result<Vec<PermissionUse>, ToolError> {
        self.execution
            .argument_permissions
            .as_ref()
            .map_or_else(|| Ok(Vec::new()), |extract| extract(location, arguments))
    }

    pub(crate) const fn accepts_input(&self) -> bool {
        self.execution.accepts_input
    }

    pub(crate) fn permission_resource(&self) -> Option<&ResourceId> {
        self.execution.permission_resource.as_ref()
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
        tool.validate_arguments(arguments)
    }

    #[must_use]
    pub fn definitions(&self) -> Vec<ProviderToolDefinition> {
        let job_envelope = self
            .tools
            .get("job_cancel")
            .and_then(|tool| tool.output_schema.as_ref());
        self.tools
            .values()
            .filter(|tool| tool.exposure == ToolExposure::ModelVisible)
            .map(|tool| {
                let description = if tool.name == "script" {
                    let mut description = self.script_description(&tool.description);
                    if let Some(schema) = &tool.result_schema {
                        let result_type = output_type(schema, job_envelope);
                        description.push_str(&format!("\n\nScript return: `{result_type}`."));
                    }
                    description
                } else {
                    let native = tool
                        .result_schema
                        .as_ref()
                        .map(|schema| output_type(schema, job_envelope));
                    if tool.name == "job_output" {
                        tool.description.clone()
                    } else {
                        format!(
                            "{} Result: `{}`.",
                            tool.description,
                            native.unwrap_or_else(|| "JSON".into())
                        )
                    }
                };
                ProviderToolDefinition {
                    name: tool.name.clone(),
                    description,
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
        let job_envelope = self
            .tools
            .get("job_cancel")
            .and_then(|tool| tool.output_schema.as_ref());
        let documented = self
            .tools
            .values()
            .filter(|tool| match &tool.script_binding {
                ScriptBinding::TopLevel => tool.exposure == ToolExposure::ScriptOnly,
                ScriptBinding::JobMethod { .. } => {
                    tool.exposure == ToolExposure::ScriptOnly || tool.name == "job_output"
                }
                ScriptBinding::Unavailable => false,
            })
            .map(|tool| script_documentation(tool, job_envelope))
            .collect::<Vec<_>>();
        if documented.is_empty() {
            base.to_owned()
        } else {
            format!(
                "{base}\n\nJob controls and additional script APIs:\n{}",
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
    pub fn get(&self, name: &str) -> Option<Arc<RegisteredTool>> {
        self.tools.get(name).cloned()
    }

    pub fn tools(&self) -> impl Iterator<Item = &Arc<RegisteredTool>> {
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
                    .generate_scoped(capabilities, child)
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
    /// Check names already claimed by builtin or host-supplied tools.
    pub(crate) fn contains_name(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

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
        mut options: ToolOptions,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        if options.execution.placement == ToolPlacement::TargetedWorkspace {
            ensure_no_target(&input_schema)?;
            options =
                options.conditional_input("target", Capability::Targets, target_property_schema());
        }
        let name = name.into();
        validate_name(&name)?;
        validate_schema(&input_schema)?;
        let ToolOptions {
            execution,
            supports_background,
            preserve_required,
            preserve_schema_dialect,
            exposure,
            script_binding,
            required,
            root_required,
            conditional_inputs,
            output_schema,
        } = options;
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
            for (name, capability, property) in &conditional_inputs {
                if capabilities.contains(*capability) {
                    add_schema_property(&mut schema, name, property.clone());
                }
            }
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
        self.register_definition(definition, execution, handler)
    }

    fn register_definition<F, Fut>(
        &mut self,
        definition: GeneratedToolDefinition,
        execution: ToolExecution,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        let name = definition.name.clone();
        validate_name(&name)?;
        if self.tools.contains_key(&name) {
            return Err(RegistryError::Duplicate(name));
        }
        // Input transformations only add properties/defaults; their object root is
        // validated at registration. Static outputs likewise have invariant root
        // types. Only arbitrary output generators require every capability variant.
        let generated_output = matches!(definition.output_schema, Some(OutputSchema::Generated(_)));
        for capabilities in capability_subsets() {
            if let Some(spec) = definition.generate(&capabilities) {
                validate_object_schema(&spec.input_schema)?;
                if let Some(schema) = &spec.output_schema {
                    validate_output_schema(schema)?;
                }
                if !generated_output {
                    break;
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

    pub fn register<I, O, F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        mut options: ToolOptions,
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
        self.register_dynamic(
            name,
            description,
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
        "description": "Execution target."
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

fn valid_job_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn add_background(schema: &mut Value) {
    add_schema_property(
        schema,
        "bg",
        serde_json::json!({
            "type": "boolean",
            "default": false,
            "description": "Run in background."
        }),
    );
}

// A documented default always permits omission from tool input. Apply this to
// schema nodes only, leaving default/example JSON values untouched.
fn optional_defaults(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let defaults: BTreeSet<String> = object
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, field)| field.get("default").is_some())
        .map(|(name, _)| name.clone())
        .collect();
    if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|name| !name.as_str().is_some_and(|name| defaults.contains(name)));
    }
    for key in [
        "properties",
        "$defs",
        "definitions",
        "patternProperties",
        "dependentSchemas",
    ] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_object_mut) {
            children.values_mut().for_each(optional_defaults);
        }
    }
    for key in [
        "items",
        "additionalProperties",
        "contains",
        "not",
        "if",
        "then",
        "else",
    ] {
        if let Some(child) = object.get_mut(key) {
            optional_defaults(child);
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_array_mut) {
            children.iter_mut().for_each(optional_defaults);
        }
    }
}

fn sanitize_schema(value: &mut Value) {
    sanitize_schema_inner(value, false);
}

fn sanitize_schema_inner(value: &mut Value, preserve_dialect: bool) {
    // `true` and `{}` both accept any JSON value. Some provider-side tool
    // schema converters (including llama.cpp) only accept the object form.
    // This traversal visits schema nodes, never literal defaults or examples.
    // Keep `false` intact: internal validation relies on closed-object flags.
    if value.as_bool() == Some(true) {
        *value = Value::Object(Map::new());
        return;
    }
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if !preserve_dialect {
        object.remove("$schema");
    }
    object.remove("title");
    object.remove("format");
    // Only descend into schemas. Property names and literal enum/default/example
    // payloads may themselves contain keys such as "title" or "format".
    for key in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
        "dependencies",
    ] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_object_mut) {
            children
                .values_mut()
                .for_each(|child| sanitize_schema_inner(child, preserve_dialect));
        }
    }
    for key in [
        "items",
        "additionalItems",
        "additionalProperties",
        "unevaluatedItems",
        "unevaluatedProperties",
        "contains",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "contentSchema",
    ] {
        if let Some(child) = object.get_mut(key) {
            if let Some(children) = child.as_array_mut() {
                children
                    .iter_mut()
                    .for_each(|child| sanitize_schema_inner(child, preserve_dialect));
            } else {
                sanitize_schema_inner(child, preserve_dialect);
            }
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_array_mut) {
            children
                .iter_mut()
                .for_each(|child| sanitize_schema_inner(child, preserve_dialect));
        }
    }
}

fn describe_output(
    description: &str,
    schema: Option<&Value>,
    job_envelope: Option<&Value>,
) -> String {
    schema.map_or_else(
        || description.to_owned(),
        |schema| {
            format!(
                "{description} Returns `{}`.",
                output_type(schema, job_envelope)
            )
        },
    )
}

fn output_type(schema: &Value, job_envelope: Option<&Value>) -> String {
    let rendered = schema_type(schema, schema);
    job_envelope.map_or_else(
        || rendered.clone(),
        |envelope| {
            let metadata = schema_type(envelope, envelope);
            if rendered == format!("{metadata}[]") {
                "job metadata array".to_owned()
            } else {
                rendered.replace(&metadata, "job metadata")
            }
        },
    )
}

pub(crate) fn job_view_type(capabilities: &CapabilitySet) -> String {
    let schema = crate::job::output::view_schema(capabilities);
    schema_type(&schema, &schema)
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

fn script_documentation(tool: &ToolSpec, job_envelope: Option<&Value>) -> String {
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
        ScriptBinding::JobMethod { method, .. } => format!("tool.job(id).{method}{arguments}"),
        ScriptBinding::Unavailable => unreachable!(),
    };
    if tool.exposure == ToolExposure::ModelVisible
        && matches!(tool.script_binding, ScriptBinding::JobMethod { .. })
    {
        return format!("- `{call}` — Same as `{}`.", tool.name);
    }
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
    let description = describe_output(&tool.description, tool.output_schema.as_ref(), job_envelope);
    format!("- `{call}` — {description}{field_docs}")
}

/// Hoist definitions so both union members retain valid, unambiguous references.
fn output_union(foreground: Value, background: Value) -> Value {
    fn rename(value: &mut Value, prefix: &str) {
        match value {
            Value::Object(object) => {
                if let Some(Value::String(reference)) = object.get_mut("$ref")
                    && let Some(name) = reference.strip_prefix("#/$defs/")
                {
                    *reference = format!("#/$defs/{prefix}{name}");
                }
                for value in object.values_mut() {
                    rename(value, prefix);
                }
            }
            Value::Array(values) => {
                for value in values {
                    rename(value, prefix);
                }
            }
            _ => {}
        }
    }
    let mut definitions = serde_json::Map::new();
    let variants = [("Foreground_", foreground), ("Job_", background)]
        .into_iter()
        .map(|(prefix, mut schema)| {
            rename(&mut schema, prefix);
            if let Some(Value::Object(defs)) = schema
                .as_object_mut()
                .and_then(|object| object.remove("$defs"))
            {
                for (name, value) in defs {
                    definitions.insert(format!("{prefix}{name}"), value);
                }
            }
            schema
        })
        .collect::<Vec<_>>();
    serde_json::json!({"anyOf": variants, "$defs": definitions})
}

fn schema_type(field: &Value, root: &Value) -> String {
    schema_type_inner(field, root, false)
}

fn schema_type_inner(field: &Value, root: &Value, array_item: bool) -> String {
    if let Some(reference) = field.get("$ref").and_then(Value::as_str)
        && let Some(name) = reference.strip_prefix("#/$defs/")
        && let Some(definition) = root.get("$defs").and_then(|defs| defs.get(name))
    {
        return schema_type_inner(definition, root, array_item);
    }
    if array_item
        && ["enum", "anyOf", "oneOf", "type"].iter().any(|key| {
            field
                .get(*key)
                .and_then(Value::as_array)
                .is_some_and(|values| values.len() > 1)
        })
    {
        return format!("({})", schema_type(field, root));
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
    if let Some(types) = field.get("type").and_then(Value::as_array) {
        return types
            .iter()
            .map(|kind| {
                let mut variant = field.clone();
                variant["type"] = kind.clone();
                schema_type(&variant, root)
            })
            .collect::<Vec<_>>()
            .join(" | ");
    }
    match field.get("type").and_then(Value::as_str) {
        Some("string" | "integer" | "number" | "boolean" | "null") => {
            field["type"].as_str().unwrap_or("JSON").to_owned()
        }
        Some("array") => format!("{}[]", schema_type_inner(&field["items"], root, true)),
        Some("object") => {
            let required = field["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>();
            let mut properties = field["properties"]
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
            if let Some(values) = field
                .get("additionalProperties")
                .filter(|value| **value != false)
            {
                properties.push(format!("[key:string]:{}", schema_type(values, root)));
            }
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
    #[error("`target` is reserved for structurally targeted tools")]
    ReservedTarget,
}

#[cfg(test)]
mod schema_normalization_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitization_preserves_property_names_and_literal_payloads() {
        let literal = json!({"title": "literal", "format": "custom", "$schema": "payload"});
        let mut schema = json!({
            "type": "object", "title": "schema title", "$schema": "dialect",
            "properties": {
                "title": {"type": "string", "title": "annotation"},
                "format": {"type": "string", "format": "uri"},
                "$schema": {"type": "object", "default": literal.clone()},
                "value": {"enum": [literal.clone()], "examples": [literal.clone()]}
            },
            "$defs": {"title": {"type": "string", "title": "definition annotation"}},
            "items": [{"type": "string", "format": "uri"}]
        });
        sanitize_schema(&mut schema);
        assert!(schema.get("title").is_none());
        assert!(schema.get("$schema").is_none());
        assert_eq!(schema["properties"]["title"], json!({"type": "string"}));
        assert_eq!(schema["properties"]["format"], json!({"type": "string"}));
        assert_eq!(schema["properties"]["$schema"]["default"], literal);
        assert_eq!(schema["properties"]["value"]["enum"][0], literal);
        assert_eq!(schema["properties"]["value"]["examples"][0], literal);
        assert_eq!(schema["$defs"]["title"], json!({"type": "string"}));
        assert_eq!(schema["items"][0], json!({"type": "string"}));
    }

    #[test]
    fn unrestricted_schemas_use_object_form_without_rewriting_boolean_data() {
        let literal = json!({"properties":{"value":true}, "items":false, "$schema":true});
        let original = json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object", "additionalProperties":false, "readOnly":true,
            "properties": {
                "value":true, "forbidden":false,
                "flag":{"type":"boolean", "default":true, "const":true, "enum":[true,false], "examples":[true,literal]},
                "list":{"type":"array", "items":true},
                "tuple":{"type":"array", "prefixItems":[true,false]},
                "open":{"type":"object", "additionalProperties":true},
                "union":{"anyOf":[true,false]}
            },
            "$defs":{"anything":true, "nothing":false},
            "required":["value"]
        });
        for preserve_dialect in [false, true] {
            let mut normalized = original.clone();
            sanitize_schema_inner(&mut normalized, preserve_dialect);
            for pointer in [
                "/properties/value",
                "/properties/list/items",
                "/properties/tuple/prefixItems/0",
                "/properties/open/additionalProperties",
                "/properties/union/anyOf/0",
                "/$defs/anything",
            ] {
                assert_eq!(
                    normalized.pointer(pointer).unwrap(),
                    &json!({}),
                    "{pointer}"
                );
            }
            for pointer in [
                "/additionalProperties",
                "/properties/forbidden",
                "/properties/tuple/prefixItems/1",
                "/properties/union/anyOf/1",
                "/$defs/nothing",
            ] {
                assert_eq!(
                    normalized.pointer(pointer),
                    Some(&Value::Bool(false)),
                    "{pointer}"
                );
            }
            assert_eq!(
                normalized["properties"]["flag"],
                original["properties"]["flag"]
            );
            assert_eq!(normalized["readOnly"], true);
            assert_eq!(normalized.get("$schema").is_some(), preserve_dialect);
            let before = jsonschema::validator_for(&original).unwrap();
            let after = jsonschema::validator_for(&normalized).unwrap();
            for value in [
                Value::Null,
                json!(true),
                json!(false),
                json!(42),
                json!("text"),
                json!([1, false]),
                json!({"enabled":true}),
            ] {
                let valid = json!({"value":value});
                assert!(before.is_valid(&valid));
                assert!(after.is_valid(&valid));
                for invalid in [
                    json!({"value":value,"forbidden":null}),
                    json!({"value":value,"unknown":1}),
                ] {
                    assert!(!before.is_valid(&invalid));
                    assert!(!after.is_valid(&invalid));
                }
            }
            let once = normalized.clone();
            sanitize_schema_inner(&mut normalized, preserve_dialect);
            assert_eq!(normalized, once);
        }
    }

    #[test]
    fn unrestricted_output_schema_is_normalized_too() {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_dynamic(
                "any_output",
                "test",
                json!({"type":"object"}),
                ToolOptions::default().output_schema(Value::Bool(true)),
                |_, _| async { Ok(ToolOutput::new(Value::Null)) },
            )
            .unwrap();
        let registry = builder.build();
        let surface = registry.surface(&CapabilitySet::default());
        assert_eq!(
            surface.get("any_output").unwrap().output_schema,
            Some(json!({}))
        );
    }

    #[test]
    fn external_schema_defaults_do_not_make_required_fields_optional() {
        let mut builder = ToolRegistryBuilder::default();
        let schema = json!({"type": "object", "properties": {
            "value": {"type": "string", "default": "example"}
        }, "required": ["value"]});
        for (name, options) in [
            ("external", ToolOptions::default().preserve_required()),
            ("builtin", ToolOptions::default()),
        ] {
            builder
                .register_dynamic(
                    name,
                    "test",
                    schema.clone(),
                    options,
                    |_context, _args| async { Ok(ToolOutput::new(Value::Null)) },
                )
                .unwrap();
        }
        let registry = builder.build();
        let surface = registry.surface(&CapabilitySet::default());
        let definitions = surface.definitions();
        assert_eq!(
            definitions
                .iter()
                .find(|d| d.name == "external")
                .unwrap()
                .input_schema["required"],
            json!(["value"])
        );
        assert_eq!(
            definitions
                .iter()
                .find(|d| d.name == "builtin")
                .unwrap()
                .input_schema["required"],
            json!([])
        );
    }
}
