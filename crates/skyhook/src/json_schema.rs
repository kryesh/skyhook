//! Local JSON Schema navigation: `$ref` resolution within a schema document and
//! JSON type matching as typed tools read values. External documents are never loaded.
use jsonschema::{JsonType, JsonTypeSet};
use serde_json::Value;
use std::cell::Cell;

/// Bound on nesting and `$ref` indirection along one path.
pub(crate) const MAX_DEPTH: usize = 64;
/// Bound on schema nodes one resolver visits, against exponential unions.
const MAX_STEPS: usize = 10_000;

/// A value's JSON type as typed tools read it: only a number that parses as a
/// 64-bit integer is an `integer`, so `3.0` is a `number`.
pub(crate) fn value_type(value: &Value) -> JsonType {
    match value {
        Value::Null => JsonType::Null,
        Value::Bool(_) => JsonType::Boolean,
        Value::Number(number) if number.is_i64() || number.is_u64() => JsonType::Integer,
        Value::Number(_) => JsonType::Number,
        Value::String(_) => JsonType::String,
        Value::Array(_) => JsonType::Array,
        Value::Object(_) => JsonType::Object,
    }
}

/// Whether `value` is an instance of any of `types`; every integer is also a number.
pub(crate) fn accepts(types: JsonTypeSet, value: &Value) -> bool {
    let actual = value_type(value);
    types.contains(actual) || (actual == JsonType::Integer && types.contains(JsonType::Number))
}

/// The recognized types a schema's `type` keyword declares, if any.
pub(crate) fn declared_types(schema: &Value) -> Option<JsonTypeSet> {
    let types = match schema.get("type")? {
        Value::String(name) => JsonTypeSet::from(name.parse::<JsonType>().ok()?),
        Value::Array(names) => names
            .iter()
            .filter_map(|name| name.as_str()?.parse().ok())
            .fold(JsonTypeSet::empty(), JsonTypeSet::insert),
        _ => return None,
    };
    (!types.is_empty()).then_some(types)
}

/// A schema node and the `$id` resource its local references resolve in.
#[derive(Clone, Copy)]
pub(crate) struct Node<'a> {
    pub(crate) schema: &'a Value,
    resource: &'a Value,
}

impl<'a> Node<'a> {
    /// The root of a schema document.
    pub(crate) fn root(schema: &'a Value) -> Self {
        Self::within(schema, schema)
    }

    /// A subschema of this node; one that declares an `$id` starts its own resource.
    pub(crate) fn child(self, schema: &'a Value) -> Self {
        Self::within(schema, self.resource)
    }

    /// Whether both nodes are the same schema object.
    pub(crate) fn is(self, other: Self) -> bool {
        std::ptr::eq(self.schema, other.schema)
    }

    /// Whether this node lies in its document's root resource.
    pub(crate) fn in_root_resource(self, root: &Value) -> bool {
        std::ptr::eq(self.resource, root)
    }

    fn within(schema: &'a Value, resource: &'a Value) -> Self {
        let resource = if resource_id(schema).is_some() {
            schema
        } else {
            resource
        };
        Self { schema, resource }
    }
}

fn resource_id(schema: &Value) -> Option<&str> {
    ["$id", "id"]
        .iter()
        .find_map(|key| schema.get(*key)?.as_str())
}

/// Resolves local `$ref`s in one schema document within a budget of visited nodes.
pub(crate) struct Resolver<'a> {
    root: &'a Value,
    steps: Cell<usize>,
}

impl<'a> Resolver<'a> {
    pub(crate) fn new(root: &'a Value) -> Self {
        Self {
            root,
            steps: Cell::new(MAX_STEPS),
        }
    }

    pub(crate) fn root(&self) -> Node<'a> {
        Node::root(self.root)
    }

    /// Spend one step of the budget; false once it is exhausted.
    pub(crate) fn spend(&self) -> bool {
        let remaining = self.steps.get();
        self.steps.set(remaining.saturating_sub(1));
        remaining > 0
    }

    #[cfg(test)]
    pub(crate) fn steps_left(&self) -> usize {
        self.steps.get()
    }

    /// The target of the node's own `$ref`. A fragment resolves within the
    /// node's resource; a URI names an embedded resource by its `$id`. `None`
    /// without a `$ref`, or when it is unresolvable or the budget is exhausted.
    pub(crate) fn target(&self, node: Node<'a>) -> Option<Node<'a>> {
        let reference = node.schema.get("$ref")?.as_str()?;
        if !self.spend() {
            return None;
        }
        let (uri, fragment) = reference.split_once('#').unwrap_or((reference, ""));
        let resource = if uri.is_empty() {
            node.resource
        } else {
            self.find_resource(self.root, uri, 0)?
        };
        Some(Node::within(resource.pointer(fragment)?, resource))
    }

    /// Follow `$ref`s to a node without one. `None` when a reference is
    /// unresolvable, the chain is cyclic or too long, or the budget is exhausted.
    pub(crate) fn resolve(&self, mut node: Node<'a>) -> Option<Node<'a>> {
        for _ in 0..MAX_DEPTH {
            if node.schema.get("$ref").is_none() {
                return Some(node);
            }
            node = self.target(node)?;
        }
        None
    }

    fn find_resource(&self, schema: &'a Value, uri: &str, depth: usize) -> Option<&'a Value> {
        if depth > MAX_DEPTH || !self.spend() {
            return None;
        }
        if resource_id(schema) == Some(uri) {
            return Some(schema);
        }
        let children: Box<dyn Iterator<Item = &'a Value>> = match schema {
            Value::Object(object) => Box::new(object.values()),
            Value::Array(items) => Box::new(items.iter()),
            _ => return None,
        };
        children
            .filter(|child| child.is_object() || child.is_array())
            .find_map(|child| self.find_resource(child, uri, depth + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn integers_are_numbers_but_integral_floats_are_not_integers() {
        let integer = JsonTypeSet::from(JsonType::Integer);
        let number = JsonTypeSet::from(JsonType::Number);
        assert!(accepts(integer, &json!(3)) && accepts(number, &json!(3)));
        assert!(!accepts(integer, &json!(3.0)) && accepts(number, &json!(3.0)));
        assert_eq!(
            declared_types(&json!({"type": ["string", "bogus", "null"]})),
            Some(JsonType::String | JsonType::Null)
        );
        assert_eq!(declared_types(&json!({"type": "bogus"})), None);
    }

    #[test]
    fn references_resolve_within_their_id_resource() {
        let schema = json!({
            "$defs": {"leaf": {"const": "root"}, "alias": {"$ref": "#/$defs/leaf"}},
            "properties": {
                "local": {"$ref": "#/$defs/alias"},
                "embedded": {
                    "$id": "urn:embedded",
                    "$defs": {"leaf": {"const": "embedded"}},
                    "properties": {"inner": {"$ref": "#/$defs/leaf"}}
                },
                "named": {"$ref": "urn:embedded#/$defs/leaf"},
                "cycle": {"$ref": "#/properties/cycle"},
                "external": {"$ref": "https://example.test/schema"}
            }
        });
        fn property<'a>(node: Node<'a>, name: &str) -> Node<'a> {
            node.child(&node.schema["properties"][name])
        }
        let resolver = Resolver::new(&schema);
        let root = resolver.root();
        let resolved = |node| {
            resolver
                .resolve(node)
                .map(|node| node.schema["const"].clone())
        };
        assert_eq!(resolved(property(root, "local")), Some(json!("root")));
        assert_eq!(
            resolved(property(property(root, "embedded"), "inner")),
            Some(json!("embedded"))
        );
        assert_eq!(resolved(property(root, "named")), Some(json!("embedded")));
        assert!(resolved(property(root, "cycle")).is_none());
        assert!(resolved(property(root, "external")).is_none());
    }
}
