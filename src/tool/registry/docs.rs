//! Provider-facing definitions, script manifests, and compact schema documentation.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use crate::json_schema::{Node, Resolver};
use crate::provider::protocol::ToolDefinition as ProviderToolDefinition;

use super::{ScriptBinding, ToolExposure, ToolSpec, ToolSurface};

/// Sanitized job view schemas, as tool result schemas are stored.
#[derive(Clone, Default)]
pub(super) struct JobViewSchemas {
    one: Value,
    many: Value,
}

impl JobViewSchemas {
    pub(super) fn new() -> Self {
        let sanitized = |many| {
            let mut schema = crate::job::presented_job_schema(many);
            super::schema::sanitize_schema(&mut schema);
            schema
        };
        Self {
            one: sanitized(false),
            many: sanitized(true),
        }
    }

    /// Append a result type to `description`, unless it is plain JSON.
    fn describe(&self, description: &str, label: &str, schema: Option<&Value>) -> String {
        match schema.map(|schema| self.render(schema)) {
            Some(result) if result != "JSON" => format!("{description} {label} `{result}`."),
            _ => description.to_owned(),
        }
    }

    /// Render a result type, naming job views rather than expanding them.
    fn render(&self, schema: &Value) -> String {
        if *schema == self.one {
            "JobView".to_owned()
        } else if *schema == self.many {
            "JobView[]".to_owned()
        } else {
            result_type(schema)
        }
    }
}

impl ToolSurface {
    #[must_use]
    pub fn definitions(&self) -> Vec<ProviderToolDefinition> {
        self.tools
            .values()
            .filter(|tool| tool.exposure == ToolExposure::ModelVisible)
            .map(|tool| {
                let description = self.job_views.describe(
                    &tool.description,
                    "Result:",
                    tool.result_schema.as_ref(),
                );
                let description = if tool.job_role == crate::job::JobRole::Script {
                    self.script_description(&description)
                } else {
                    description
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
        let documented = self
            .tools
            .values()
            .filter(|tool| {
                tool.exposure == ToolExposure::ScriptOnly
                    && tool.script_binding != ScriptBinding::Unavailable
            })
            .map(|tool| script_documentation(tool, &self.job_views))
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

pub(crate) fn job_view_type() -> String {
    let schema = crate::job::presented_job_schema(false);
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

fn script_documentation(tool: &ToolSpec, job_views: &JobViewSchemas) -> String {
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
    let description = job_views.describe(&tool.description, "Returns", tool.output_schema.as_ref());
    format!("- `{call}` — {description}{field_docs}")
}

fn schema_type<'a>(field: &'a Value, root: &'a Value) -> String {
    let renderer = Renderer::new(root, BTreeSet::new());
    let node = Node::root(root).child(field);
    let mut active = if std::ptr::eq(field, root) {
        vec![node]
    } else {
        Vec::new()
    };
    renderer.render(node, false, &mut active)
}

/// Render a result type, defining each definition referenced more than once
/// a single time after the type instead of expanding it at every use.
fn result_type(schema: &Value) -> String {
    let mut counts = BTreeMap::<&str, usize>::new();
    count_references(schema, &mut counts);
    let shared = counts
        .into_iter()
        .filter(|(reference, count)| {
            *count > 1
                && *reference != "#"
                && reference
                    .strip_prefix('#')
                    .is_some_and(|pointer| schema.pointer(pointer).is_some())
        })
        .map(|(reference, _)| reference)
        .collect::<BTreeSet<_>>();
    let renderer = Renderer::new(schema, shared);
    let root = Node::root(schema);
    let rendered = renderer.render(root, false, &mut vec![root]);
    let definitions = (renderer.shared.iter())
        .filter_map(|reference| {
            let definition = root.child(schema.pointer(reference.strip_prefix('#')?)?);
            let body = renderer.render(definition, false, &mut vec![definition]);
            Some(format!(
                "{} = {body}",
                definition_name(definition.schema, reference)
            ))
        })
        .collect::<Vec<_>>();
    if definitions.is_empty() {
        rendered
    } else {
        format!("{rendered}` where `{}", definitions.join("; "))
    }
}

fn count_references<'a>(value: &'a Value, counts: &mut BTreeMap<&'a str, usize>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                match (key.as_str(), value) {
                    ("$ref", Value::String(reference)) => {
                        *counts.entry(reference.as_str()).or_default() += 1;
                    }
                    _ => count_references(value, counts),
                }
            }
        }
        Value::Array(values) => values
            .iter()
            .for_each(|value| count_references(value, counts)),
        _ => {}
    }
}

fn definition_name(definition: &Value, reference: &str) -> String {
    definition
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| (reference.rsplit(['/', '#']).next()).filter(|name| !name.is_empty()))
        .unwrap_or("JSON")
        .to_owned()
}

/// Compact type rendering over one schema document.
struct Renderer<'a> {
    resolver: Resolver<'a>,
    root: &'a Value,
    /// Root-resource references rendered by name and defined once after the type.
    shared: BTreeSet<&'a str>,
}

impl<'a> Renderer<'a> {
    fn new(root: &'a Value, shared: BTreeSet<&'a str>) -> Self {
        Self {
            resolver: Resolver::new(root),
            root,
            shared,
        }
    }

    /// `active` holds the referenced schemas being expanded, outermost first.
    fn render(&self, node: Node<'a>, array_item: bool, active: &mut Vec<Node<'a>>) -> String {
        let field = node.schema;
        if let Some(reference) = field.get("$ref").and_then(Value::as_str)
            && let Some(target) = self.resolver.target(node)
        {
            // Recursive schemas (including capture pages containing JobView) need
            // a named back-reference, not unbounded expansion in the tool prompt.
            if (self.shared.contains(reference) && node.in_root_resource(self.root))
                || active.iter().any(|expanding| expanding.is(target))
            {
                return definition_name(target.schema, reference);
            }
            active.push(target);
            let rendered = self.render(target, array_item, active);
            active.pop();
            return rendered;
        }
        if array_item
            && ["enum", "anyOf", "oneOf", "type"].iter().any(|key| {
                field
                    .get(*key)
                    .and_then(Value::as_array)
                    .is_some_and(|values| values.len() > 1)
            })
        {
            return format!("({})", self.render(node, false, active));
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
                .map(|variant| self.render(node.child(variant), false, active))
                .collect::<Vec<_>>();
            return types.join(" | ");
        }
        if let Some(types) = field.get("type").and_then(Value::as_array) {
            return types
                .iter()
                .map(|kind| self.typed(node, kind.as_str(), active))
                .collect::<Vec<_>>()
                .join(" | ");
        }
        self.typed(node, field.get("type").and_then(Value::as_str), active)
    }

    /// Render `node` as one of its declared types.
    fn typed(&self, node: Node<'a>, kind: Option<&str>, active: &mut Vec<Node<'a>>) -> String {
        let field = node.schema;
        match kind {
            Some(kind @ ("string" | "integer" | "number" | "boolean" | "null")) => kind.to_owned(),
            Some("array") => format!(
                "{}[]",
                self.render(node.child(&field["items"]), true, active)
            ),
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
                        let rendered = self.render(node.child(value), false, active);
                        format!("{name}{optional}:{rendered}")
                    })
                    .collect::<Vec<_>>();
                if let Some(values) = field
                    .get("additionalProperties")
                    .filter(|value| **value != false)
                {
                    let rendered = self.render(node.child(values), false, active);
                    properties.push(format!("[key:string]:{rendered}"));
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
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "JSON".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recursive_schema_descriptions_use_named_back_references() {
        let node = json!({
            "$ref":"#/$defs/Node",
            "$defs":{"Node":{"type":"object", "properties":{
                "next":{"anyOf":[{"$ref":"#/$defs/Node"},{"type":"null"}]}
            },"required":["next"]}}
        });
        assert_eq!(schema_type(&node, &node), "{next:Node | null}");
        let tree = json!({"title":"Tree","type":"object", "properties":{
            "children":{"type":"array","items":{"$ref":"#"}}
        },"required":["children"]});
        assert_eq!(schema_type(&tree, &tree), "{children:Tree[]}");
        let view = job_view_type();
        assert!(view.contains("has_result:boolean"));
        assert!(view.contains("JobView"));
        assert!(
            view.len() < 10_000,
            "recursive response description expanded unexpectedly"
        );
    }

    #[test]
    fn result_types_define_shared_definitions_once_and_name_job_views() {
        let body =
            json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]});
        let schema = json!({
            "anyOf":[
                {"type":"object","properties":{"body":{"$ref":"#/$defs/Body"}},"required":["body"]},
                {"type":"object","properties":{"body":{"$ref":"#/$defs/Body"},"once":{"$ref":"#/$defs/Once"}}}
            ],
            "$defs":{"Body":body, "Once":{"type":"integer"}}
        });
        assert_eq!(
            result_type(&schema),
            "{body:Body} | {body?:Body, once?:integer}` where `Body = {text:string}"
        );
        let views = JobViewSchemas::new();
        assert_eq!(views.render(&views.one), "JobView");
        assert_eq!(views.render(&views.many), "JobView[]");
    }
}
