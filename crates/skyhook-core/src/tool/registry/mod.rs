//! Tool registration, execution metadata, and capability-scoped surfaces.

mod docs;
mod schema;

pub(crate) use docs::{ScriptManifest, job_view_type};
use schema::{
    add_schema_property, ensure_no_target, target_property_schema, validate_object_schema,
    validate_output_schema, validate_schema,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::Arc,
};

use futures_util::future::BoxFuture;
use schemars::{JsonSchema, schema_for};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;

use crate::{
    execution::ExecutionLocation,
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

    pub(crate) fn script_manifests(&self) -> Vec<ScriptManifest> {
        self.tools
            .values()
            .filter_map(ScriptManifest::from_tool)
            .collect()
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

fn valid_job_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
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
