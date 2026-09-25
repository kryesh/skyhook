//! Schema-guided repair of model tool arguments, such as `{"timeout": "30"}`
//! for an integer. Only lossless conversions to a type the destination
//! requires are tried, and a repair is kept only if the result validates.
use crate::json_schema::{MAX_DEPTH, Node, Resolver, accepts, declared_types, value_type};
use jsonschema::{JsonType, JsonTypeSet};
use serde_json::{Map, Number, Value};

/// 2^53: larger floats cannot be converted to integers exactly.
const MAX_EXACT_FLOAT_INTEGER: f64 = 9_007_199_254_740_992.0;

/// Repair `arguments` in place when that makes them valid against `schema`.
pub(crate) fn coerce_arguments(schema: &Value, arguments: &mut Value) {
    let Ok(validator) = jsonschema::options()
        .with_retriever(NoExternalSchemas)
        .build(schema)
    else {
        return;
    };
    // JSON Schema accepts `3.0` as an integer but typed tools do not, so
    // valid arguments still get integral floats normalized, and nothing else.
    let integral_only = validator.is_valid(arguments);
    let mut repaired = arguments.clone();
    Walk::new(schema, integral_only).coerce_root(&mut repaired);
    if validator.is_valid(&repaired) {
        *arguments = repaired;
    }
}

struct NoExternalSchemas;

impl jsonschema::Retrieve for NoExternalSchemas {
    fn retrieve(
        &self,
        _uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external schema references are not loaded".into())
    }
}

struct Walk<'a> {
    resolver: Resolver<'a>,
    integral_only: bool,
}

impl<'a> Walk<'a> {
    fn new(root: &'a Value, integral_only: bool) -> Self {
        Self {
            resolver: Resolver::new(root),
            integral_only,
        }
    }

    fn coerce_root(&self, value: &mut Value) {
        self.coerce(self.resolver.root(), value, 0);
    }

    /// Spend one step of the budget; false once it or the depth is exhausted.
    fn spend(&self, depth: usize) -> bool {
        depth <= MAX_DEPTH && self.resolver.spend()
    }

    /// Convert `value` only when the schema's declared types reject it, then
    /// repair its contents. An untyped schema accepts anything as it is.
    fn coerce(&self, node: Node<'a>, value: &mut Value, depth: usize) {
        if let Some(kinds) = self.kinds(node, depth)
            && !accepts(kinds, value)
        {
            let conversion = |kind| {
                if self.integral_only && !(kind == JsonType::Integer && value.is_f64()) {
                    None
                } else {
                    convert(value, kind)
                }
            };
            match kinds.iter().find_map(conversion) {
                Some(converted) => *value = converted,
                None => return,
            }
        }
        self.descend(node, value, depth);
    }

    /// Follow `$ref`s within the nearest `$id` resource (or the named one),
    /// spending one step per node.
    fn resolve(&self, node: Node<'a>, depth: usize) -> Option<Node<'a>> {
        if !self.spend(depth) {
            return None;
        }
        self.resolver.resolve(node)
    }

    /// The types a node declares, from its own `type`, `const` or `enum`, else
    /// from its `anyOf`/`oneOf` alternatives. `None` means untyped.
    fn kinds(&self, node: Node<'a>, depth: usize) -> Option<JsonTypeSet> {
        let node = self.resolve(node, depth)?;
        let schema = node.schema;
        if let Some(kinds) = declared_types(schema) {
            return Some(kinds);
        }
        let mut kinds = JsonTypeSet::empty();
        let values = schema.get("const").into_iter().chain(
            schema
                .get("enum")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        );
        let mut constrained = false;
        for value in values {
            kinds = kinds.insert(value_type(value));
            constrained = true;
        }
        if constrained {
            return Some(kinds);
        }
        if !has_union(schema) {
            return None;
        }
        // Any untyped alternative accepts every value.
        for branch in branches(node) {
            kinds = kinds.union(self.kinds(branch, depth + 1)?);
        }
        Some(kinds)
    }

    fn descend(&self, node: Node<'a>, value: &mut Value, depth: usize) {
        let Some(node) = self.resolve(node, depth) else {
            return;
        };
        let schema = node.schema;
        let child = |schema| node.child(schema);
        match value {
            Value::Object(object) => {
                let properties = schema.get("properties").and_then(Value::as_object);
                let additional = schema
                    .get("additionalProperties")
                    .filter(|value| value.is_object());
                for (key, property) in object.iter_mut() {
                    if let Some(schema) = properties
                        .and_then(|properties| properties.get(key))
                        .or(additional)
                    {
                        self.coerce(child(schema), property, depth + 1);
                    }
                }
                for all in schema
                    .get("allOf")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    self.descend(child(all), value, depth + 1);
                }
            }
            Value::Array(items) => {
                match schema.get("prefixItems").or_else(|| schema.get("items")) {
                    Some(Value::Array(tuple)) => {
                        for (item, schema) in items.iter_mut().zip(tuple) {
                            self.coerce(child(schema), item, depth + 1);
                        }
                    }
                    Some(schema @ Value::Object(_)) => {
                        for item in items {
                            self.coerce(child(schema), item, depth + 1);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        // Refine through the first alternative that accepts the value and
        // whose discriminators (`const`/`enum` properties) it does not contradict.
        if has_union(schema)
            && let Some(branch) = branches(node).into_iter().find(|branch| {
                self.kinds(*branch, depth + 1)
                    .is_none_or(|kinds| accepts(kinds, value))
                    && !self.contradicts(*branch, value, depth + 1)
            })
        {
            self.descend(branch, value, depth + 1);
        }
    }

    /// Whether an object property's value is excluded by the alternative's
    /// `const` or `enum` for that property.
    fn contradicts(&self, node: Node<'a>, value: &Value, depth: usize) -> bool {
        let (Some(node), Value::Object(object)) = (self.resolve(node, depth), value) else {
            return false;
        };
        let Some(properties) = node.schema.get("properties").and_then(Value::as_object) else {
            return false;
        };
        properties.iter().any(|(key, property)| {
            let (Some(actual), Some(property)) = (
                object.get(key),
                self.resolve(node.child(property), depth + 1),
            ) else {
                return false;
            };
            let schema = property.schema;
            schema
                .get("const")
                .is_some_and(|expected| expected != actual)
                || schema
                    .get("enum")
                    .and_then(Value::as_array)
                    .is_some_and(|allowed| !allowed.contains(actual))
        })
    }
}

fn has_union(schema: &Value) -> bool {
    ["anyOf", "oneOf"]
        .iter()
        .any(|key| schema.get(*key).is_some_and(Value::is_array))
}

/// The `anyOf`/`oneOf` alternatives of a node with a union.
fn branches(node: Node<'_>) -> Vec<Node<'_>> {
    ["anyOf", "oneOf"]
        .iter()
        .filter_map(|key| node.schema.get(*key)?.as_array())
        .flatten()
        .map(|schema| node.child(schema))
        .collect()
}

fn convert(value: &Value, kind: JsonType) -> Option<Value> {
    match (value, kind) {
        (Value::String(text), JsonType::Integer) => {
            let text = text.trim();
            text.parse::<i64>()
                .map(Value::from)
                .or_else(|_| text.parse::<u64>().map(Value::from))
                .ok()
        }
        (Value::String(text), JsonType::Number) => {
            let number: Number = serde_json::from_str(text.trim()).ok()?;
            Some(Value::Number(number))
        }
        (Value::Number(number), JsonType::Integer) => {
            let float = number.as_f64()?;
            (float.fract() == 0.0 && float.abs() < MAX_EXACT_FLOAT_INTEGER)
                .then(|| Value::from(float as i64))
        }
        (Value::String(text), JsonType::Boolean) => match text.trim() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        (Value::String(text), JsonType::Null) => (text.trim() == "null").then_some(Value::Null),
        (Value::String(text), JsonType::Object) => {
            serde_json::from_str::<Map<String, Value>>(text.trim())
                .ok()
                .map(Value::Object)
        }
        (Value::String(text), JsonType::Array) => serde_json::from_str::<Vec<Value>>(text.trim())
            .ok()
            .map(Value::Array),
        // Only integers: a float's spelling (`1.10` → `1.1`) is not preserved.
        (Value::Number(number), JsonType::String) if number.is_i64() || number.is_u64() => {
            Some(Value::String(number.to_string()))
        }
        (Value::Bool(flag), JsonType::String) => Some(Value::String(flag.to_string())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn coerced(schema: Value, mut arguments: Value) -> Value {
        coerce_arguments(&schema, &mut arguments);
        arguments
    }

    #[test]
    fn stringified_scalars_become_declared_types() {
        let schema = json!({"type":"object", "properties":{
            "timeout":{"type":"integer"}, "ratio":{"type":"number"}, "bg":{"type":"boolean"},
            "limit":{"type":["integer","null"]}, "name":{"type":"string"},
            "count":{"type":"integer"}
        }});
        let arguments = json!({"timeout":"30", "ratio":"0.5", "bg":"true", "limit":"null",
            "name":42, "count":3.0});
        assert_eq!(
            coerced(schema, arguments),
            json!({"timeout":30, "ratio":0.5, "bg":true, "limit":null, "name":"42", "count":3})
        );
    }

    #[test]
    fn valid_lossy_and_unrepairable_arguments_are_left_alone() {
        let schema = json!({"type":"object", "properties":{
            "n":{"type":"integer"}, "s":{"type":"string"}, "b":{"type":"boolean"},
            "either":{"type":["string","integer"]}, "big":{"type":"string"}
        }});
        // "1.5" cannot become an integer losslessly, and floats have no
        // preserved spelling, so the whole repair is discarded.
        for arguments in [
            json!({"n":"1.5", "s":"30", "b":"true", "either":"7", "extra":"1"}),
            json!({"big":1e20, "b":"true"}),
            json!({"big":1.10, "b":"true"}),
            json!({"b":"yes"}),
        ] {
            assert_eq!(coerced(schema.clone(), arguments.clone()), arguments);
        }
        // Already valid: untouched even where a conversion would be possible.
        let valid = json!({"s":"30", "either":"7"});
        assert_eq!(coerced(schema, valid.clone()), valid);
    }

    #[test]
    fn valid_union_arguments_are_not_rewritten_into_another_branch() {
        let schema = json!({"type":"object", "oneOf":[
            {"properties":{"kind":{"const":"a"}, "n":{"type":"integer"}}, "required":["kind"]},
            {"properties":{"kind":{"const":"b"}, "n":{"type":"string"}}, "required":["kind"]}
        ]});
        let arguments = json!({"kind":"b", "n":"5"});
        assert_eq!(coerced(schema, arguments.clone()), arguments);
    }

    #[test]
    fn nested_objects_arrays_refs_and_unions_are_followed() {
        let schema = json!({"type":"object",
            "properties":{
                "items":{"type":"array", "items":{"$ref":"#/$defs/Item"}},
                "opt":{"anyOf":[{"$ref":"#/$defs/Item"}, {"type":"null"}]},
                "tuple":{"type":"array", "prefixItems":[{"type":"integer"},{"type":"boolean"}]},
                "json":{"type":"object", "properties":{"x":{"type":"integer"}}},
                "list":{"type":"array", "items":{"type":"integer"}},
                "mode":{"enum":[1, 2]}
            },
            "$defs":{"Item":{"type":"object", "properties":{"size":{"type":"integer"}}}}
        });
        let arguments = json!({
            "items":[{"size":"4"}, {"size":5}],
            "opt":{"size":"6"},
            "tuple":["7", "false"],
            "json":"{\"x\":\"8\"}",
            "list":"[9, \"10\"]",
            "mode":"2"
        });
        assert_eq!(
            coerced(schema, arguments),
            json!({"items":[{"size":4}, {"size":5}], "opt":{"size":6}, "tuple":[7, false],
                "json":{"x":8}, "list":[9, 10], "mode":2})
        );
    }

    #[test]
    fn recursive_and_branching_schemas_are_bounded() {
        // Returns the step budget left once the walk terminates.
        let steps_left = |schema: &Value, mut value: Value| {
            let walk = Walk::new(schema, false);
            walk.coerce_root(&mut value);
            walk.resolver.steps_left()
        };
        let schema = json!({"anyOf":[{"$ref":"#"},{"$ref":"#"}]});
        assert!(steps_left(&schema, json!({"x":"1"})) > 0, "depth-bounded");
        let mut defs = serde_json::Map::new();
        for n in 0..40 {
            let next = format!("#/$defs/D{}", n + 1);
            defs.insert(
                format!("D{n}"),
                json!({"anyOf":[{"$ref":next},{"$ref":next}]}),
            );
        }
        defs.insert("D40".into(), json!({"type":"integer"}));
        let chain = json!({"$ref":"#/$defs/D0", "$defs":defs});
        assert_eq!(steps_left(&chain, json!("1")), 0);
        let looping = json!({"$ref":"#/$defs/Loop", "$defs":{"Loop":{"$ref":"#/$defs/Loop"}}});
        assert!(steps_left(&looping, json!({"x":"1"})) > 0, "depth-bounded");
    }

    #[test]
    fn only_types_the_destination_rejects_are_converted() {
        // `b` makes the arguments invalid, so a repair runs; `a` is accepted by
        // an untyped alternative and `s` by a string type, so neither changes.
        let schema = json!({"type":"object", "properties":{
            "a":{"anyOf":[{"type":"integer"}, {}]},
            "s":{"type":["string","integer"]},
            "b":{"type":"integer"}
        }});
        assert_eq!(
            coerced(schema, json!({"a":"5", "s":"6", "b":"7"})),
            json!({"a":"5", "s":"6", "b":7})
        );
        // A root's own properties are repaired alongside its alternatives.
        let schema = json!({"type":"object",
            "properties":{"n":{"type":"integer"}},
            "anyOf":[{"required":["n"]}, {"required":["m"]}]});
        assert_eq!(coerced(schema, json!({"n":"5"})), json!({"n":5}));
    }

    #[test]
    fn integral_floats_become_integers_where_an_integer_is_required() {
        let schema = json!({"type":"object", "properties":{
            "n":{"type":"integer"}, "x":{"type":"number"}, "s":{"type":"string"},
            "either":{"type":["integer","number"]}
        }});
        // Valid as JSON Schema, but typed tools reject `3.0` for an integer.
        // Only that is normalized; other valid values are untouched.
        assert_eq!(
            coerced(
                schema.clone(),
                json!({"n":3.0, "x":2.0, "s":"5", "either":4.0})
            ),
            json!({"n":3, "x":2.0, "s":"5", "either":4.0})
        );
        // A non-integral float is not an integer; validation reports it.
        let invalid = json!({"n":3.5});
        assert_eq!(coerced(schema, invalid.clone()), invalid);
    }

    #[test]
    fn union_branches_contradicted_by_a_discriminator_are_skipped() {
        // `t` makes the arguments invalid, so a repair runs. `kind: "b"` rules
        // out branch a, and branch b does not type `n`, so `n` is untouched.
        let schema = json!({"type":"object",
        "properties":{"t":{"type":"integer"}},
        "anyOf":[
            {"properties":{"kind":{"const":"a"}, "n":{"type":"integer"}}},
            {"properties":{"kind":{"const":"b"}}}
        ]});
        assert_eq!(
            coerced(schema.clone(), json!({"kind":"b", "n":"5", "t":"1"})),
            json!({"kind":"b", "n":"5", "t":1})
        );
        assert_eq!(
            coerced(schema, json!({"kind":"a", "n":"5", "t":"1"})),
            json!({"kind":"a", "n":5, "t":1})
        );
    }
}
