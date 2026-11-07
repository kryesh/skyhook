//! Provider-facing definitions and compact schema documentation.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

use serde_json::Value;

use crate::json_schema::{Node, Resolver};
use crate::provider::protocol::ToolDefinition as ProviderToolDefinition;

use super::ToolSurface;

static JOB_VIEW_TYPE: LazyLock<String> = LazyLock::new(|| {
    let (rendered, definitions) =
        result_type(&crate::job::JOB_VIEW_SCHEMAS.view, Documented::Expanded);
    format!("{rendered}{definitions}")
});

/// The schema keyword marking a type the prompt spells out in full, holding the name a result
/// type gives it instead of expanding it. A mark, not a title, so that a host type sharing the
/// name keeps its fields.
pub(crate) const DOCUMENTED: &str = "x-skyhook-documented";

/// Append the response type to `description` in its own paragraph, as the tool's rather
/// than its last sentence's: a JobView of the result type, unless the result is plain JSON.
fn describe(description: &str, result: Option<&Value>) -> String {
    let Some(schema) = result else {
        return description.to_owned();
    };
    let (rendered, definitions) = result_type(schema, Documented::Named);
    if rendered == "JSON" {
        return description.to_owned();
    }
    format!("{description}\n\nResult: `JobView<{rendered}>{definitions}`")
}

impl ToolSurface {
    /// The definitions a model sees, generated once per surface and shared.
    pub fn definitions(&self) -> Vec<std::sync::Arc<ProviderToolDefinition>> {
        (self.definitions)
            .get_or_init(|| self.generate_definitions())
            .clone()
    }

    fn generate_definitions(&self) -> Vec<std::sync::Arc<ProviderToolDefinition>> {
        self.tools
            .values()
            .map(|tool| {
                std::sync::Arc::new(ProviderToolDefinition {
                    name: tool.name.clone(),
                    description: describe(&tool.description, tool.result_schema.as_ref()),
                    input_schema: tool.input_schema.clone(),
                })
            })
            .collect()
    }
}

pub(crate) fn job_view_type() -> &'static str {
    &JOB_VIEW_TYPE
}

/// Render a result type, defining each definition referenced more than once
/// a single time after the type instead of expanding it at every use: the type
/// and those definitions' suffix.
fn result_type(schema: &Value, documented: Documented) -> (String, String) {
    let mut counts = BTreeMap::<&str, usize>::new();
    count_references(schema, schema, documented, &mut counts);
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
    let renderer = Renderer::new(schema, shared, documented);
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
        (rendered, String::new())
    } else {
        (rendered, format!("` where `{}", definitions.join("; ")))
    }
}

/// Count the references `value` renders, walking each definition it reaches once: a documented
/// type named by `mode` renders by name, so what it references is left out.
fn count_references<'a>(
    root: &'a Value,
    value: &'a Value,
    mode: Documented,
    counts: &mut BTreeMap<&'a str, usize>,
) {
    let named = |schema| mode == Documented::Named && documented(schema).is_some();
    match value {
        Value::Object(object) if !named(value) => {
            for (key, value) in object {
                match (key.as_str(), value) {
                    ("$defs", _) => {}
                    ("$ref", Value::String(reference)) => {
                        let target =
                            (reference.strip_prefix('#')).and_then(|pointer| root.pointer(pointer));
                        let Some(target) = target else { continue };
                        if named(target) {
                            continue;
                        }
                        let count = counts.entry(reference.as_str()).or_default();
                        *count += 1;
                        if *count == 1 && reference != "#" {
                            count_references(root, target, mode, counts);
                        }
                    }
                    _ => count_references(root, value, mode, counts),
                }
            }
        }
        Value::Array(values) => values
            .iter()
            .for_each(|value| count_references(root, value, mode, counts)),
        _ => {}
    }
}

/// The name `schema` is documented under, when it is marked [`DOCUMENTED`].
fn documented(schema: &Value) -> Option<&str> {
    schema.get(DOCUMENTED)?.as_str()
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
    documented: Documented,
}

/// How a renderer shows a type marked [`DOCUMENTED`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Documented {
    /// By its name, wherever it occurs: the prompt spells it out.
    Named,
    /// In full, as any other type.
    Expanded,
}

impl<'a> Renderer<'a> {
    fn new(root: &'a Value, shared: BTreeSet<&'a str>, documented: Documented) -> Self {
        Self {
            resolver: Resolver::new(root),
            root,
            shared,
            documented,
        }
    }

    /// `active` holds the referenced schemas being expanded, outermost first.
    fn render(&self, node: Node<'a>, array_item: bool, active: &mut Vec<Node<'a>>) -> String {
        let field = node.schema;
        if let Some(name) = self.named(field) {
            return name.to_owned();
        }
        if let Some(reference) = field.get("$ref").and_then(Value::as_str)
            && let Some(target) = self.resolver.target(node)
        {
            if let Some(name) = self.named(target.schema) {
                return name.to_owned();
            }
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
        if let Some(value) = field.get("const") {
            return compact_json(value);
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

    /// The name `schema` renders by, when it is documented and named here.
    fn named(&self, schema: &'a Value) -> Option<&'a str> {
        (self.documented == Documented::Named)
            .then(|| documented(schema))
            .flatten()
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
        assert_eq!(
            result_type(&node, Documented::Expanded),
            ("Node".into(), "` where `Node = {next:Node | null}".into())
        );
        let tree = json!({"title":"Tree","type":"object", "properties":{
            "children":{"type":"array","items":{"$ref":"#"}}
        },"required":["children"]});
        assert_eq!(
            result_type(&tree, Documented::Expanded),
            ("{children:Tree[]}".into(), String::new())
        );
        let view = job_view_type();
        assert!(view.contains("result?:JSON"));
        assert!(view.contains("JobView"));
        assert!(
            view.len() < 10_000,
            "recursive response description expanded unexpectedly"
        );
    }

    #[test]
    fn result_types_define_shared_definitions_once() {
        let body =
            json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]});
        let schema = json!({
            "anyOf":[
                {"type":"object","properties":{"kind":{"type":"string","const":"full"},"body":{"$ref":"#/$defs/Body"}},"required":["kind","body"]},
                {"type":"object","properties":{"body":{"$ref":"#/$defs/Body"},"once":{"$ref":"#/$defs/Once"}}}
            ],
            "$defs":{"Body":body, "Once":{"type":"integer"}}
        });
        let result = schema;
        assert_eq!(
            describe("Send.", Some(&result)),
            "Send.\n\nResult: `JobView<{kind:\"full\", body:Body} | {body?:Body, once?:integer}>` where `Body = {text:string}`"
        );
        let listing = &crate::job::JOB_VIEW_SCHEMAS.listing;
        assert_eq!(
            describe("List.", Some(listing)),
            "List.\n\nResult: `JobView<{jobs:JobView[], older?:integer}>`",
            "a documented type renders by name, without what it references"
        );
        let host = json!({
            "title":"JobView","type":"object",
            "properties":{"job":{"$ref":"#/$defs/JobView"}},"required":["job"],
            "$defs":{"JobView":{"type":"object","properties":{"rows":{"type":"integer"}}}}
        });
        assert_eq!(
            describe("Count.", Some(&host)),
            "Count.\n\nResult: `JobView<{job:{rows?:integer}}>`",
            "a host type sharing the name keeps its fields"
        );
    }
}
