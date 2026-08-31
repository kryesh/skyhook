use std::{collections::BTreeMap, future::Future, sync::Arc};

use futures_util::future::BoxFuture;
use schemars::{JsonSchema, schema_for};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{provider::protocol::ToolDefinition, tool::policy::ToolEffect};

use super::{ToolContext, ToolError, ToolOutput};

#[derive(Clone)]
pub struct RegisteredTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub effects: Vec<ToolEffect>,
    pub supports_background: bool,
    pub accepts_input: bool,
    handler: ToolHandler,
    effect_resolver: Option<EffectResolver>,
}

type ToolHandler = Arc<
    dyn Fn(ToolContext, Value) -> BoxFuture<'static, Result<ToolOutput, ToolError>> + Send + Sync,
>;
type EffectResolver = Arc<dyn Fn(&Value) -> Result<Vec<ToolEffect>, ToolError> + Send + Sync>;

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
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: schema_with_background(&self.input_schema, self.supports_background),
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

    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    pub fn tools(&self) -> impl Iterator<Item = &Arc<RegisteredTool>> {
        self.tools.values()
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
        effects: Vec<ToolEffect>,
        supports_background: bool,
        accepts_input: bool,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolOutput, ToolError>> + Send + 'static,
    {
        self.register_dynamic_inner(
            name,
            description,
            input_schema,
            effects,
            None,
            supports_background,
            accepts_input,
            handler,
        )
    }

    pub fn register_dynamic_effects<F, Fut, E>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        effects: Vec<ToolEffect>,
        effect_resolver: E,
        supports_background: bool,
        accepts_input: bool,
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
            effects,
            Some(Arc::new(effect_resolver)),
            supports_background,
            accepts_input,
            handler,
        )
    }

    fn register_dynamic_inner<F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        effects: Vec<ToolEffect>,
        effect_resolver: Option<EffectResolver>,
        supports_background: bool,
        accepts_input: bool,
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
        self.tools.insert(
            name.clone(),
            Arc::new(RegisteredTool {
                name,
                description: description.into(),
                input_schema,
                effects,
                supports_background,
                accepts_input,
                handler,
                effect_resolver,
            }),
        );
        Ok(self)
    }

    pub fn register<I, O, F, Fut>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        effects: Vec<ToolEffect>,
        supports_background: bool,
        accepts_input: bool,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: Serialize + Send + 'static,
        F: Fn(ToolContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, ToolError>> + Send + 'static,
    {
        let schema = serde_json::to_value(schema_for!(I))
            .map_err(|error| RegistryError::Schema(error.to_string()))?;
        self.register_dynamic(
            name,
            description,
            schema,
            effects,
            supports_background,
            accepts_input,
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
        effects: Vec<ToolEffect>,
        effect_resolver: E,
        supports_background: bool,
        accepts_input: bool,
        handler: F,
    ) -> Result<&mut Self, RegistryError>
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: Serialize + Send + 'static,
        F: Fn(ToolContext, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, ToolError>> + Send + 'static,
        E: Fn(&I) -> Vec<ToolEffect> + Send + Sync + 'static,
    {
        let schema = serde_json::to_value(schema_for!(I))
            .map_err(|error| RegistryError::Schema(error.to_string()))?;
        self.register_dynamic_effects(
            name,
            description,
            schema,
            effects,
            move |arguments| {
                let input: I = serde_json::from_value(arguments.clone())
                    .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
                Ok(effect_resolver(&input))
            },
            supports_background,
            accepts_input,
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

fn schema_with_background(schema: &Value, supports_background: bool) -> Value {
    if !supports_background {
        return schema.clone();
    }
    let mut schema = schema.clone();
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
    schema
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

    #[test]
    fn background_is_generated_not_owned_by_handler_schema() {
        let mut builder = ToolRegistry::builder();
        builder
            .register::<Args, String, _, _>(
                "echo",
                "Echo input",
                Vec::new(),
                true,
                false,
                |_context, args| async move { Ok(args.value) },
            )
            .unwrap();
        let registry = builder.build();
        let tool = registry.get("echo").unwrap();
        assert!(tool.definition().input_schema["properties"]["bg"].is_object());
        let (arguments, background) = registry
            .split_execution(&tool, serde_json::json!({"value":"x", "bg":true}))
            .unwrap();
        assert!(background);
        assert!(arguments.get("bg").is_none());
    }
}
