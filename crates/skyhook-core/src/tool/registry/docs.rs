//! Provider-facing definitions, script manifests, and compact schema documentation.

use serde::Serialize;
use serde_json::Value;

use crate::{
    provider::protocol::ToolDefinition as ProviderToolDefinition, tool::policy::CapabilitySet,
};

use super::{ScriptBinding, ToolExposure, ToolSpec, ToolSurface};

impl ToolSurface {
    #[must_use]
    pub fn definitions(&self) -> Vec<ProviderToolDefinition> {
        let job_envelope = &self.job_envelope;
        self.tools
            .values()
            .filter(|tool| tool.exposure == ToolExposure::ModelVisible)
            .map(|tool| {
                let description = if tool.job_role == crate::job::JobRole::Script {
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
                    if tool.result_policy == super::ToolResultPolicy::JobView {
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

    fn script_description(&self, base: &str) -> String {
        let job_envelope = &self.job_envelope;
        let documented = self
            .tools
            .values()
            .filter(|tool| match &tool.script_binding {
                ScriptBinding::TopLevel => tool.exposure == ToolExposure::ScriptOnly,
                ScriptBinding::JobMethod { .. } => {
                    tool.exposure == ToolExposure::ScriptOnly
                        || tool.result_policy == super::ToolResultPolicy::JobView
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

fn describe_output(description: &str, schema: Option<&Value>, job_envelope: &Value) -> String {
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

fn output_type(schema: &Value, job_envelope: &Value) -> String {
    let rendered = schema_type(schema, schema);
    let metadata = schema_type(job_envelope, job_envelope);
    if rendered == format!("{metadata}[]") {
        "job metadata array".to_owned()
    } else {
        rendered.replace(&metadata, "job metadata")
    }
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
    pub(super) fn from_tool(tool: &ToolSpec) -> Option<Self> {
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

fn script_documentation(tool: &ToolSpec, job_envelope: &Value) -> String {
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
