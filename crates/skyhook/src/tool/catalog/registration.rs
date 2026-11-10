//! Registration: typed admission and the catalog builder.

use std::{future::Future, sync::Arc};

use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use skyhook_tools::schema::input_schema_for;

use super::{
    AdmittedInput, Arguments, FlownInvocation, Invocation, JobLocation, PathArgument,
    RegisteredTool, RegistryError, ToolOptions, ToolPlacement,
    envelope::{NAME, TARGET, name_schema},
    options::{ArgumentHooks, SchemaOrigin},
    schema::{
        add_conditional, check_conditional, compile_schema, tags_first, validate_name,
        validate_output_schema, validate_schema,
    },
    surface::{CatalogTools, Compiled, GeneratedToolDefinition, OutputSchema, ToolRegistry},
};
use crate::remote::{Landed, PreparedConnection};
use crate::tool::policy::{Capability, CapabilitySet, PermissionUse};
use crate::tool::{ToolContext, ToolError};
use futures_util::future::BoxFuture;
use skyhook_tools::target::ExecutionLocation;
use skyhook_tools::{arguments::deserialize_arguments, error::AdmissionError};
use tokio::sync::oneshot;

/// A registration's admitted input with the hooks and handler that consume it.
struct TypedInput<A> {
    input: A,
    hooks: Arc<ArgumentHooks<A>>,
    invoke: Arc<dyn Fn(A) -> Invocation + Send + Sync>,
    /// The invocation of its operation once that flew.
    flown: Option<oneshot::Receiver<FlownInvocation<A>>>,
}

impl<A: Send + 'static> AdmittedInput for TypedInput<A> {
    fn paths(&mut self) -> Vec<PathArgument<'_>> {
        (self.hooks.paths.as_ref()).map_or_else(Vec::new, |paths| paths(&mut self.input))
    }

    fn permissions(
        &self,
        location: &ExecutionLocation,
    ) -> Result<Vec<PermissionUse>, AdmissionError> {
        (self.hooks.permissions.as_ref()).map_or(Ok(Vec::new()), |permissions| {
            permissions(location, &self.input)
        })
    }

    fn job_location(&self) -> Option<JobLocation> {
        (self.hooks.location.as_ref()).map(|location| location(&self.input))
    }

    fn result_schema(&self) -> Option<Value> {
        (self.hooks.result.as_ref()).map(|result| result(&self.input))
    }

    fn source(&self) -> Option<&skyhook_tools::target::TargetedPath> {
        (self.hooks.source.as_ref()).and_then(|source| source(&self.input))
    }

    fn fly(
        &mut self,
        connection: &PreparedConnection,
        location: &ExecutionLocation,
        spills: skyhook_tools::spec::Spills,
    ) -> Option<BoxFuture<'static, Result<Landed, AdmissionError>>> {
        let (Some(fly), Some(paths)) = (&self.hooks.flight, &self.hooks.paths) else {
            return None;
        };
        let request = skyhook_tools::path::request(&paths(&mut self.input), location)?;
        let site = (location.clone(), spills);
        let flight = fly(&mut self.input, request, connection.clone(), site);
        let (flown, invocation) = oneshot::channel();
        self.flown = Some(invocation);
        Some(Box::pin(async move {
            let (landed, invocation) = flight.await?;
            let _ = flown.send(invocation);
            Ok(landed)
        }))
    }

    fn invoke(self: Box<Self>) -> Invocation {
        let Self {
            input,
            invoke,
            flown,
            ..
        } = *self;
        match flown.and_then(|mut flown| flown.try_recv().ok()) {
            Some(flown) => flown(input),
            None => invoke(input),
        }
    }
}

#[derive(Default)]
pub struct ToolRegistryBuilder {
    pub(super) tools: CatalogTools,
}

impl ToolRegistryBuilder {
    /// Check names already claimed by builtin or host-supplied tools.
    pub fn contains_name(&self, name: &str) -> bool {
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

    /// Register a built-in tool whose typed input `I` admits into its handler's input
    /// `A`; `invoke` runs the admitted input. Its schema compiles at its first call: a
    /// test compiles every built-in's.
    pub fn register_checked<I, A>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        options: ToolOptions<A>,
        admission: impl Fn(I) -> Result<A, AdmissionError> + Send + Sync + 'static,
        invoke: impl Fn(A) -> Invocation + Send + Sync + 'static,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + 'static,
        A: Send + 'static,
    {
        let schema = input_schema_for::<I>();
        let typed = schema.clone();
        let admission =
            move |arguments: &Arguments| admission(deserialize_arguments::<I>(&typed, arguments)?);
        self.register_schema(name, description, schema, options, admission, invoke)
    }

    /// Register a host's tool: schemars accepts declared constraints, such as a
    /// pattern, without checking them, so its typed schema compiles here.
    fn register_host<I, A>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        options: ToolOptions<A>,
        admission: impl Fn(I) -> Result<A, AdmissionError> + Send + Sync + 'static,
        invoke: impl Fn(A) -> Invocation + Send + Sync + 'static,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + 'static,
        A: Send + 'static,
    {
        compile_schema(&input_schema_for::<I>())?;
        self.register_checked(name, description, options, admission, invoke)
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
        F: Fn(ToolContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, ToolError>> + Send + 'static,
    {
        self.register_admitted::<I, I, O, _, _>(name, description, options, Ok, handler)
    }

    /// Register a tool whose typed input `I` admits into its handler's input `A`:
    /// `admission` refuses, before authorization, arguments the schema accepts but
    /// `A` cannot hold, such as a combination of fields.
    pub fn register_admitted<I, A, O, F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        mut options: ToolOptions<A>,
        admission: impl Fn(I) -> Result<A, AdmissionError> + Send + Sync + 'static,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + 'static,
        A: Send + 'static,
        O: Serialize + JsonSchema + Send + 'static,
        F: Fn(ToolContext, A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, ToolError>> + Send + 'static,
    {
        if !options.declares_result() {
            options = options.result::<O>();
        }
        let handler = Arc::new(handler);
        self.register_host(name, description, options, admission, move |input| {
            let handler = handler.clone();
            Invocation::new(move |context| {
                let future = handler(context, input);
                async move { super::serialized(future.await?) }
            })
        })
    }

    /// Register a typed input with an already-constructed output product; `O`
    /// supplies only the advertised result schema. The product is never
    /// serialized, so its captures and images survive to completion.
    #[cfg(test)]
    pub(crate) fn register_product<I, O, F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        mut options: ToolOptions<I>,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: JsonSchema + 'static,
        F: Fn(ToolContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<crate::tool::ToolOutput, ToolError>> + Send + 'static,
    {
        if !options.declares_result() {
            options = options.result::<O>();
        }
        let handler = Arc::new(handler);
        self.register_host(name, description, options, Ok, move |input| {
            let handler = handler.clone();
            Invocation::new(move |context| handler(context, input))
        })
    }

    /// Register a tool whose job completes without a result.
    #[cfg(test)]
    pub(crate) fn register_unit<I, F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        options: ToolOptions<I>,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        F: Fn(ToolContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), ToolError>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        self.register_host(name, description, options, Ok, move |input| {
            let handler = handler.clone();
            Invocation::unit(move |context| handler(context, input))
        })
    }

    /// Register a tool described by `input_schema`, whose arguments `admission`
    /// turns into its handler's input `A` before authorization; `invoke` runs the
    /// admitted input.
    pub fn register_schema<A: Send + 'static>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        mut options: ToolOptions<A>,
        admission: impl Fn(&Arguments) -> Result<A, AdmissionError> + Send + Sync + 'static,
        invoke: impl Fn(A) -> Invocation + Send + Sync + 'static,
    ) -> Result<&mut Self, RegistryError> {
        let all: CapabilitySet = Capability::ALL.into_iter().collect();
        // A computed root is validated in its widest built-in form. The typed schema is
        // validated too; forms only a host capability selects rely on
        // `ToolOptions::computed_schema`'s contract to select from it.
        // Only a typed schema with nothing computed into it is known to compile.
        let fixed = options.computed_schema.is_none() && options.conditional_inputs.is_empty();
        let compiled = match (fixed, options.schema_origin) {
            (true, SchemaOrigin::Native) => Compiled::OnFirstUse,
            _ => Compiled::AsOffered,
        };
        let typed = match options.schema_origin {
            SchemaOrigin::Native => Compiled::OnFirstUse,
            SchemaOrigin::External => Compiled::AsOffered,
        };
        validate_schema(&input_schema, typed)?;
        let widest = match (options.computed_schema.as_ref()).and_then(|computed| computed(&all)) {
            Some(widest) => {
                validate_schema(&widest, Compiled::AsOffered)?;
                widest
            }
            None => input_schema.clone(),
        };
        if options.execution.placement == ToolPlacement::TargetedWorkspace {
            if widest["properties"].get(TARGET).is_some() {
                return Err(RegistryError::ReservedTarget);
            }
            options = options.conditional_input(
                TARGET,
                Capability::Targets,
                skyhook_tools::target::TargetRef::schema(),
            );
        }
        let name = name.into();
        validate_name(&name)?;
        check_conditional(&widest, &options.conditional_inputs, "input")?;
        let ToolOptions {
            execution,
            hooks,
            supports_background,
            schema_origin,
            script_binding,
            required,
            root_required,
            conditional_inputs,
            conditional_outputs,
            computed_schema,
            output_schema,
        } = options;
        if hooks.result.is_some() && !conditional_outputs.is_empty() {
            return Err(RegistryError::Schema(
                "internal result schemas cannot add conditional outputs".to_owned(),
            ));
        }
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
        // Output roots do not vary with capabilities; validate the full one once.
        if let Some(schema) = &output_schema {
            validate_output_schema(&schema.generate(&all))?;
        }
        let supports_name = execution.supports_name;
        if supports_name && widest["properties"].get(NAME).is_some() {
            return Err(RegistryError::Schema(
                "named tools reserve the name argument for job metadata".to_owned(),
            ));
        }
        let schema = move |capabilities: &CapabilitySet| {
            let mut schema = match &computed_schema {
                Some(computed) => computed(capabilities)?,
                None => input_schema.clone(),
            };
            if supports_name {
                super::schema::add_schema_property(&mut schema, NAME, name_schema());
            }
            if !add_conditional(&mut schema, &conditional_inputs, capabilities) {
                return None;
            }
            tags_first(&mut schema);
            Some(schema)
        };
        let hooks = Arc::new(hooks);
        let invoke: Arc<dyn Fn(A) -> Invocation + Send + Sync> = Arc::new(invoke);
        let definition = GeneratedToolDefinition {
            name,
            description: description.into(),
            input_schema: Arc::new(schema),
            output_schema,
            script_binding,
            supports_background,
            schema_origin,
            compiled,
            required,
            root_required,
        };
        if self.tools.contains_key(&definition.name) {
            return Err(RegistryError::Duplicate(definition.name));
        }
        self.tools.insert(
            definition.name.clone(),
            Arc::new(RegisteredTool {
                definition,
                execution,
                admit: Arc::new(move |arguments| {
                    Ok(Box::new(TypedInput {
                        input: admission(arguments)?,
                        hooks: hooks.clone(),
                        invoke: invoke.clone(),
                        flown: None,
                    }))
                }),
            }),
        );
        Ok(self)
    }

    pub fn build(self) -> ToolRegistry {
        ToolRegistry::new(self.tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::catalog::AgentLevel;

    #[derive(serde::Deserialize, JsonSchema)]
    struct Empty {}

    /// Inputs follow the deserialization contract and results the serialization
    /// one: an always-emitted `Option` is required and nullable, a skipped one is
    /// optional, and every value the type serializes validates.
    #[test]
    fn typed_schemas_use_their_respective_serde_contracts() {
        #[derive(serde::Deserialize, JsonSchema)]
        struct Input {
            #[serde(default, rename = "defaulted")]
            _defaulted: bool,
            #[serde(rename = "optional")]
            _optional: Option<String>,
        }
        #[derive(Serialize, JsonSchema)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Choice {
            Empty,
            Value { value: Option<u64> },
        }
        #[derive(Serialize, JsonSchema)]
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
        let tool = builder.build().get("contracts").unwrap();
        let spec = tool
            .spec(&CapabilitySet::default(), AgentLevel::Root)
            .unwrap();

        let input = jsonschema::validator_for(&spec.input_schema).unwrap();
        assert!(input.is_valid(&serde_json::json!({})));
        assert!(input.is_valid(&serde_json::json!({"optional": null})));

        let schema = spec.result_schema.unwrap();
        let required = schema["required"].as_array().unwrap();
        for field in ["emitted", "defaulted", "choice"] {
            assert!(required.contains(&Value::String(field.into())), "{schema}");
        }
        assert!(!required.contains(&Value::String("skipped".into())));
        assert_eq!(
            schema["properties"]["skipped"],
            serde_json::json!({"type": "string"})
        );
        let output = jsonschema::validator_for(&schema).unwrap();
        for value in [
            Output {
                emitted: None,
                defaulted: false,
                skipped: None,
                choice: Choice::Empty,
            },
            Output {
                emitted: Some("present".into()),
                defaulted: true,
                skipped: Some("present".into()),
                choice: Choice::Value { value: None },
            },
        ] {
            let value = serde_json::to_value(value).unwrap();
            assert!(output.is_valid(&value), "{value} rejected by {schema}");
        }
        assert!(!output.is_valid(&serde_json::json!({
            "defaulted": false, "choice": {"kind": "empty"}
        })));
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
            let spec = registry
                .get("nested")
                .unwrap()
                .spec(capabilities, AgentLevel::Root);
            spec.unwrap().input_schema["$defs"]["Inner"]["properties"]
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
        builder
            .register::<Empty, String, _, _>(
                "computed",
                "Computed tagged input",
                ToolOptions::default().computed_input("operation", |_| {
                    Some(serde_json::json!({
                        "oneOf":[{"type":"object","properties":{
                            "value":{"type":"string"},"action":{"const":"record"}
                        },"required":["value","action"]},{"type":"object","properties":{
                            "value":{"type":"string"},"mode":{"enum":["x"]}
                        }}]
                    }))
                }),
                |_, _| async { Ok(String::new()) },
            )
            .unwrap();
        let registry = builder.build();
        let spec = |name| {
            registry
                .get(name)
                .unwrap()
                .spec(&CapabilitySet::default(), AgentLevel::Root)
                .unwrap()
        };
        let variants = spec("tagged").input_schema["$defs"]["Auth"]["oneOf"]
            .as_array()
            .unwrap()
            .clone();
        let key = variants
            .iter()
            .find(|variant| variant["properties"]["kind"]["const"] == "key")
            .unwrap();
        let keys: Vec<_> = key["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["kind", "path"]);
        let computed = spec("computed").input_schema["properties"]["operation"]["oneOf"].clone();
        let keys = |variant: usize| -> Vec<String> {
            computed[variant]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect()
        };
        assert_eq!(keys(0), ["action", "value"]);
        assert_eq!(keys(1), ["mode", "value"]);
    }
}
