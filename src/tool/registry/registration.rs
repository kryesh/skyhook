//! Registration: typed admission and the catalog builder.

use std::{future::Future, sync::Arc};

use futures_util::future::BoxFuture;
use schemars::{JsonSchema, generate::SchemaSettings};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::{
    Admitted, AdmittedInput, Arguments, CatalogEntry, Invocation, JobLocation, OutputValue,
    PathArgument, RegistryError, ScriptBinding, ToolOptions, ToolPlacement,
    envelope::{JOB, NAME, TARGET, name_schema},
    options::ArgumentHooks,
    schema::{
        add_conditional, check_conditional, tags_first, validate_name, validate_output_schema,
        validate_schema,
    },
    surface::{Catalog, CatalogTools, GeneratedToolDefinition, OutputSchema},
};
use crate::{
    execution::ExecutionLocation,
    tool::{
        ToolContext, ToolOutput,
        diagnostic::deserialize_arguments,
        invocation::{AdmissionError, LocalContext, OperationError},
        output::ProducedOutput,
        policy::{Capability, CapabilitySet, PermissionUse},
    },
};

/// A registration's admitted input with the hooks and handler that consume it.
struct TypedInput<A, C, O> {
    input: A,
    hooks: Arc<ArgumentHooks<A>>,
    invoke: Arc<dyn Fn(A) -> Invocation<C, O> + Send + Sync>,
}

impl<A: Send, C, O> AdmittedInput<C, O> for TypedInput<A, C, O> {
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

    fn invoke(self: Box<Self>) -> Invocation<C, O> {
        (self.invoke)(self.input)
    }
}

pub struct CatalogBuilder<C, O: OutputValue> {
    pub(super) tools: CatalogTools<C, O>,
}

impl<C, O: OutputValue> Default for CatalogBuilder<C, O> {
    fn default() -> Self {
        Self {
            tools: Default::default(),
        }
    }
}

impl<C: Send + 'static, P: OutputValue> CatalogBuilder<C, P> {
    /// Check names already claimed by builtin or host-supplied tools.
    pub(crate) fn contains_name(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn extend(&mut self, registry: &Catalog<C, P>) -> Result<&mut Self, RegistryError> {
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
        F: Fn(C, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<P, OperationError<P>>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let admission = |arguments: &Arguments| Ok(Value::Object(arguments.clone()));
        self.register_schema(
            name,
            description,
            input_schema,
            options,
            admission,
            move |arguments| {
                let handler = handler.clone();
                Invocation::new(move |context| handler(context, arguments))
            },
        )
    }

    /// Register a tool whose typed input `I` admits into its handler's input `A`;
    /// `invoke` runs the admitted input.
    pub(crate) fn register_checked<I, A>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        options: ToolOptions<A>,
        admission: impl Fn(I) -> Result<A, AdmissionError> + Send + Sync + 'static,
        invoke: impl Fn(A) -> Invocation<C, P> + Send + Sync + 'static,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + 'static,
        A: Send + 'static,
    {
        let admission =
            move |arguments: &Arguments| admission(deserialize_arguments::<I>(arguments)?);
        self.register_schema(
            name,
            description,
            input_schema::<I>(),
            options,
            admission,
            invoke,
        )
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
        F: Fn(C, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, OperationError<P>>> + Send + 'static,
    {
        self.register_product::<I, O, _, _>(name, description, options, move |context, input| {
            let future = handler(context, input);
            async move { super::serialized(future.await?) }
        })
    }

    /// Register a typed input with an already-constructed output product.
    /// `O` supplies only the advertised result schema. Unlike `register`, this
    /// adapter never serializes the product, so native capture ownership and
    /// images survive until the canonical completion owner consumes them.
    pub fn register_product<I, O, F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        mut options: ToolOptions<I>,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: JsonSchema + 'static,
        F: Fn(C, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<P, OperationError<P>>> + Send + 'static,
    {
        if options.output_schema.is_none() {
            options = options.result::<O>();
        }
        let handler = Arc::new(handler);
        self.register_checked(name, description, options, Ok, move |input| {
            let handler = handler.clone();
            Invocation::new(move |context| handler(context, input))
        })
    }

    /// Register a tool whose job completes without a result.
    pub fn register_unit<I, F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        options: ToolOptions<I>,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        F: Fn(C, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), OperationError<P>>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        self.register_checked(name, description, options, Ok, move |input| {
            let handler = handler.clone();
            Invocation::unit(move |context| handler(context, input))
        })
    }

    /// Register a tool described by `input_schema`, whose arguments `admission`
    /// turns into its handler's input `A` before authorization; `invoke` runs the
    /// admitted input.
    pub(crate) fn register_schema<A: Send + 'static>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        mut input_schema: Value,
        mut options: ToolOptions<A>,
        admission: impl Fn(&Arguments) -> Result<A, AdmissionError> + Send + Sync + 'static,
        invoke: impl Fn(A) -> Invocation<C, P> + Send + Sync + 'static,
    ) -> Result<&mut Self, RegistryError> {
        tags_first(&mut input_schema);
        validate_schema(&input_schema)?;
        if options.execution.placement == ToolPlacement::TargetedWorkspace {
            if input_schema["properties"].get(TARGET).is_some() {
                return Err(RegistryError::ReservedTarget);
            }
            options = options.conditional_input(
                TARGET,
                Capability::Targets,
                crate::target::TargetRef::schema(),
            );
        }
        let name = name.into();
        validate_name(&name)?;
        if matches!(options.script_binding, ScriptBinding::TopLevel) && name == JOB {
            return Err(RegistryError::ReservedScriptName(name));
        }
        check_conditional(&input_schema, &options.conditional_inputs, "input")?;
        let all: CapabilitySet = Capability::ALL.into_iter().collect();
        let ToolOptions {
            execution,
            hooks,
            supports_background,
            preserve_required,
            preserve_schema_dialect,
            exposure,
            script_binding,
            required,
            root_required,
            conditional_inputs,
            conditional_outputs,
            output_schema,
        } = options;
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
        if supports_name && input_schema["properties"].get(NAME).is_some() {
            return Err(RegistryError::Schema(
                "named tools reserve the name argument for job metadata".to_owned(),
            ));
        }
        let schema = move |capabilities: &CapabilitySet| {
            let mut schema = input_schema.clone();
            if supports_name {
                super::schema::add_schema_property(&mut schema, NAME, name_schema());
            }
            add_conditional(&mut schema, &conditional_inputs, capabilities);
            schema
        };
        let hooks = Arc::new(hooks);
        let invoke: Arc<dyn Fn(A) -> Invocation<C, P> + Send + Sync> = Arc::new(invoke);
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
        if self.tools.contains_key(&definition.name) {
            return Err(RegistryError::Duplicate(definition.name));
        }
        self.tools.insert(
            definition.name.clone(),
            Arc::new(CatalogEntry {
                definition,
                execution,
                admit: Arc::new(move |arguments| {
                    Ok(Admitted::new(TypedInput {
                        input: admission(arguments)?,
                        hooks: hooks.clone(),
                        invoke: invoke.clone(),
                    }))
                }),
            }),
        );
        Ok(self)
    }

    pub fn build(self) -> Catalog<C, P> {
        Catalog {
            tools: Arc::new(self.tools),
        }
    }
}

/// Typed arguments keep the deserialization contract: defaults and `Option`
/// fields remain omissible even when the output type would emit the same fields.
fn input_schema<I: JsonSchema>() -> Value {
    schema_value(
        SchemaSettings::default()
            .for_deserialize()
            .into_generator()
            .into_root_schema_for::<I>(),
    )
}

/// Results are described by their serialization contract. schemars gives every
/// `Option` a null alternative, so a field that omits `None` declares the schema
/// of its present value with `#[schemars(with = "T")]`.
pub(crate) fn result_schema<O: JsonSchema>() -> Value {
    schema_value(
        SchemaSettings::default()
            .for_serialize()
            .into_generator()
            .into_root_schema_for::<O>(),
    )
}

/// Serialized rather than unwrapped: serialization puts keywords in schemars'
/// canonical order, which is the order tools have always been offered in.
fn schema_value(schema: schemars::Schema) -> Value {
    serde_json::to_value(schema).expect("generated schemas serialize")
}

struct HostAuthorizer(ToolContext);

impl crate::tool::invocation::LocalAuthorizer for HostAuthorizer {
    fn authorize(
        &self,
        request: crate::tool::authorization::Reauthorization,
    ) -> BoxFuture<'static, Result<(), AdmissionError>> {
        let context = self.0.clone();
        let (permissions, origin) = request.into_parts(&context.execution_location().target);
        Box::pin(async move { Ok(context.authorize(permissions, origin).await?) })
    }
}

/// A local tool's admitted input, run under a host job.
struct HostedInput(Admitted<LocalContext, ProducedOutput>);

impl AdmittedInput<ToolContext, ToolOutput> for HostedInput {
    fn paths(&mut self) -> Vec<PathArgument<'_>> {
        self.0.paths()
    }

    fn permissions(
        &self,
        location: &ExecutionLocation,
    ) -> Result<Vec<PermissionUse>, AdmissionError> {
        self.0.permissions(location)
    }

    fn job_location(&self) -> Option<JobLocation> {
        self.0.job_location()
    }

    fn invoke(self: Box<Self>) -> Invocation<ToolContext, ToolOutput> {
        let admitted = self.0.0.invoke();
        Invocation {
            result_policy: admitted.result_policy(),
            ..Invocation::new(move |context: ToolContext| async move {
                let output =
                    crate::job::output::HostOutput::new(context.store().clone(), context.job());
                let local = LocalContext::new(
                    context.execution_location().clone(),
                    context.capabilities().clone(),
                    context.process_environment.clone(),
                    context.cancellation_token(),
                    output.context(),
                    Arc::new(HostAuthorizer(context.clone())),
                )
                .with_source(context.source().cloned());
                let result = admitted.call(local).await;
                output.context().settle().await?;
                match result {
                    Ok(value) => Ok(output.finish(value)?),
                    Err(error) => Err(error.try_map_output(|value| output.finish(value))?),
                }
            })
        }
    }
}

impl super::ToolRegistryBuilder {
    pub(crate) fn register_local(
        &mut self,
        register: impl FnOnce(
            &mut crate::tool::invocation::LocalCatalogBuilder,
        ) -> Result<(), RegistryError>,
    ) -> Result<(), RegistryError> {
        let mut builder = crate::tool::invocation::LocalCatalogBuilder::default();
        register(&mut builder)?;
        for (name, tool) in builder.tools {
            if self.tools.contains_key(&name) {
                return Err(RegistryError::Duplicate(name));
            }
            let local = tool.clone();
            let host = CatalogEntry {
                definition: tool.definition.clone(),
                execution: tool.execution.clone(),
                admit: Arc::new(move |arguments: &Arguments| {
                    Ok(Admitted::new(HostedInput((local.admit)(arguments)?)))
                }),
            };
            self.tools.insert(name, Arc::new(host));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobSpec;
    use crate::tool::registry::AgentLevel;
    use crate::tool::{ToolError, ToolOutput, ToolRegistryBuilder};
    use std::io::Write as _;

    #[derive(serde::Deserialize, JsonSchema)]
    struct Empty {}

    #[test]
    fn javascript_job_namespace_is_reserved_but_unwrap_is_an_ordinary_tool_name() {
        let mut builder = ToolRegistryBuilder::default();
        let registered = builder.register::<Empty, Value, _, _>(
            "job",
            "reserved namespace",
            ToolOptions::default(),
            |_, _| async { Ok(Value::Null) },
        );
        assert!(matches!(
            registered,
            Err(RegistryError::ReservedScriptName(_))
        ));
        for (name, options) in [
            ("job", ToolOptions::default().script_unavailable()),
            ("unwrap", ToolOptions::default()),
        ] {
            builder
                .register::<Empty, Value, _, _>(name, "allowed binding", options, |_, _| async {
                    Ok(Value::Null)
                })
                .unwrap();
        }
    }

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
        let schema = schema.schema();
        let required = schema["required"].as_array().unwrap();
        for field in ["emitted", "defaulted", "choice"] {
            assert!(required.contains(&Value::String(field.into())), "{schema}");
        }
        assert!(!required.contains(&Value::String("skipped".into())));
        let output = jsonschema::validator_for(schema).unwrap();
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

    #[tokio::test]
    async fn product_registration_preserves_capture_evidence_without_serialization() {
        #[derive(serde::Deserialize, JsonSchema)]
        struct Input {
            content: String,
        }
        // This is a schema-only declaration, deliberately not Serialize.
        #[derive(JsonSchema)]
        struct Output {
            #[serde(rename = "content")]
            _content: String,
        }
        let runtime = crate::tests::TestRuntime::new().await;
        let lease = runtime
            .jobs
            .create(JobSpec::test(runtime.agent.clone(), "product"));
        let lease = lease.await.unwrap().test_run().await;
        let job = lease.id();
        let (context, worker) = runtime.tool_context(lease);
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_product::<Input, Output, _, _>(
                "product",
                "Native output product",
                ToolOptions::default(),
                |context, input| async move {
                    let capture =
                        context.text_capture(crate::tool::output::TextCaptureField::Content);
                    let mut capture = capture.await?.open();
                    capture.write_all(input.content.as_bytes())?;
                    let completed = capture.finish()?;
                    Ok(ToolOutput::new(serde_json::json!({})).with_captures(vec![completed]))
                },
            )
            .unwrap();
        let registry = builder.build();
        let tool = registry.get("product").unwrap();
        let spec = tool
            .spec(&CapabilitySet::default(), AgentLevel::Root)
            .unwrap();
        assert_eq!(
            spec.result_schema.unwrap().schema()["properties"]["content"]["type"],
            "string"
        );
        let arguments = serde_json::json!({"content":"native evidence"});
        let arguments = arguments.as_object().unwrap();
        let admitted = tool.invoke(tool.admit(arguments).unwrap());
        let output = admitted.call(context).await.unwrap();
        assert_eq!(output.value, serde_json::json!({}));
        assert_eq!(output.captures.len(), 1);
        assert!(output.captures[0].matches(job, "/result/content"));
        worker.fail(ToolError::cancelled().into()).await;
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
        let tool = builder.build().get("tagged").unwrap();
        let spec = tool
            .spec(&CapabilitySet::default(), AgentLevel::Root)
            .unwrap();
        let variants = spec.input_schema["$defs"]["Auth"]["oneOf"]
            .as_array()
            .unwrap();
        let key = variants
            .iter()
            .find(|variant| variant["properties"]["kind"]["const"] == "key")
            .unwrap();
        let keys: Vec<_> = key["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["kind", "path"]);
    }

    /// A unit registration's job completes without a result, and the script
    /// docs promise none.
    #[tokio::test]
    async fn unit_results_complete_without_a_result() {
        let runtime = crate::tests::TestRuntime::new().await;
        let mut builder = ToolRegistryBuilder::default();
        let script = ToolOptions::default().job_role(crate::job::JobRole::Script);
        builder
            .register_unit::<Empty, _, _>(
                "noop",
                "Do nothing.",
                ToolOptions::default().script_only(),
                |_, _| async { Ok(()) },
            )
            .unwrap()
            .register::<Empty, Value, _, _>("script", "Run scripts.", script, |_, _| async {
                unreachable!("documentation-only fixture")
            })
            .unwrap();
        let executor = runtime.executor(builder);
        let definitions = executor.surface_for_agent(&runtime.agent).definitions();
        assert!(
            definitions[0]
                .description
                .ends_with("- `tool.noop()` — Do nothing.")
        );
        let response = executor
            .execute_script(runtime.agent.clone(), "noop", serde_json::json!({}), None)
            .await
            .unwrap();
        assert_eq!(response.output.value, serde_json::json!({}));
    }
}
