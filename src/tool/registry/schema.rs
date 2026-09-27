//! Validation and normalization of tool names and their input and output schemas.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::tool::policy::CapabilitySet;

use super::{RegistryError, envelope::BACKGROUND, options::ConditionalProperty};

/// The longest tool name every provider accepts.
pub(crate) const MAX_TOOL_NAME_BYTES: usize = 64;

pub(crate) fn is_tool_name_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
}

pub(crate) fn is_valid_tool_name(name: &str, max_bytes: usize) -> bool {
    !name.is_empty() && name.len() <= max_bytes && name.chars().all(is_tool_name_char)
}

pub(super) fn validate_name(name: &str) -> Result<(), RegistryError> {
    if is_valid_tool_name(name, MAX_TOOL_NAME_BYTES) {
        Ok(())
    } else {
        Err(RegistryError::InvalidName(name.to_owned()))
    }
}

pub(super) fn check_conditional(
    schema: &Value,
    properties: &[ConditionalProperty],
    kind: &str,
) -> Result<(), RegistryError> {
    match (properties.iter())
        .find(|(pointer, ..)| !schema.pointer(pointer).is_some_and(Value::is_object))
    {
        Some((pointer, ..)) => Err(RegistryError::Schema(format!(
            "conditional {kind} location `{pointer}` is not an object schema"
        ))),
        None => Ok(()),
    }
}

pub(super) fn add_conditional(
    schema: &mut Value,
    properties: &[ConditionalProperty],
    capabilities: &CapabilitySet,
) {
    for (pointer, name, property) in properties {
        if let Some(property) = property(capabilities) {
            add_nested_schema_property(schema, pointer, name, property);
        }
    }
}

/// Put an object's tag properties (a `const` or single-value `enum`) before the
/// rest. A decoder constrained to the schema in property order could otherwise
/// never reach a variant whose tag is not written first.
pub(super) fn tags_first(schema: &mut Value) {
    for_each_subschema(schema, &mut |schema| {
        let Some(Value::Object(properties)) = schema.get_mut("properties") else {
            return;
        };
        let is_tag = |property: &Value| {
            property.get("const").is_some()
                || property
                    .get("enum")
                    .and_then(Value::as_array)
                    .is_some_and(|values| values.len() == 1)
        };
        let (tags, rest): (Vec<_>, Vec<_>) = std::mem::take(properties)
            .into_iter()
            .partition(|(_, property)| is_tag(property));
        properties.extend(tags.into_iter().chain(rest));
    });
}

pub(super) fn add_schema_property(schema: &mut Value, name: &str, property: Value) {
    schema
        .as_object_mut()
        .expect("registered schemas have object roots")
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("validated schema properties are objects")
        .insert(name.to_owned(), property);
}

/// Add a property to the object schema at a JSON pointer (`""` is the root).
/// Registration guarantees the pointer names an object.
fn add_nested_schema_property(schema: &mut Value, pointer: &str, name: &str, property: Value) {
    let object = schema
        .pointer_mut(pointer)
        .expect("registered conditional inputs name schema objects");
    add_schema_property(object, name, property);
}

pub(super) fn validate_schema(schema: &Value) -> Result<(), RegistryError> {
    let object = schema
        .as_object()
        .ok_or_else(|| RegistryError::Schema("root must be an object".to_owned()))?;
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return Err(RegistryError::Schema("root type must be object".to_owned()));
    }
    if schema["properties"].get(BACKGROUND).is_some() {
        return Err(RegistryError::ReservedBackground);
    }
    Ok(())
}

pub(super) fn validate_output_schema(schema: &Value) -> Result<(), RegistryError> {
    if schema.is_object() || schema.is_boolean() {
        Ok(())
    } else {
        Err(RegistryError::Schema(
            "output schema root must be an object or boolean".to_owned(),
        ))
    }
}

pub(super) fn add_background(schema: &mut Value) {
    add_schema_property(
        schema,
        BACKGROUND,
        serde_json::json!({
            "type": "boolean",
            "default": false,
            "description": "Run in background."
        }),
    );
}

/// Visit `schema` and every nested schema node, never literal
/// default/enum/example payloads or property names.
fn for_each_subschema(schema: &mut Value, visit: &mut impl FnMut(&mut Value)) {
    visit(schema);
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    for (key, child) in object {
        match (key.as_str(), child) {
            (
                "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas"
                | "dependencies",
                Value::Object(children),
            ) => {
                for child in children.values_mut() {
                    for_each_subschema(child, visit);
                }
            }
            (
                "items"
                | "additionalItems"
                | "additionalProperties"
                | "unevaluatedItems"
                | "unevaluatedProperties"
                | "contains"
                | "propertyNames"
                | "not"
                | "if"
                | "then"
                | "else"
                | "contentSchema"
                | "allOf"
                | "anyOf"
                | "oneOf"
                | "prefixItems",
                child,
            ) => match child {
                Value::Array(children) => {
                    for child in children {
                        for_each_subschema(child, visit);
                    }
                }
                child => for_each_subschema(child, visit),
            },
            _ => {}
        }
    }
}

/// A documented default always permits omission from tool input.
pub(super) fn optional_defaults(schema: &mut Value) {
    for_each_subschema(schema, &mut |schema| {
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
    });
}

pub(super) fn sanitize_schema(value: &mut Value) {
    sanitize_schema_inner(value, false);
}

/// Strip annotations providers reject. `true` becomes `{}` because some
/// provider-side converters (including llama.cpp) only accept the object form;
/// `false` stays, since internal validation relies on closed-object flags.
pub(super) fn sanitize_schema_inner(value: &mut Value, preserve_dialect: bool) {
    for_each_subschema(value, &mut |schema| {
        if schema.as_bool() == Some(true) {
            *schema = Value::Object(Map::new());
        } else if let Some(object) = schema.as_object_mut() {
            if !preserve_dialect {
                object.remove("$schema");
            }
            object.remove("title");
            object.remove("format");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::registry::AgentLevel;
    use crate::tool::{ToolOptions, ToolOutput, ToolRegistryBuilder, ToolSpec};
    use serde_json::json;

    /// An internally tagged variant lists its tag last; a grammar built from that
    /// order can never reach the variant once the tag is written first.
    #[test]
    fn tags_lead_their_objects_at_every_depth() {
        let mut schema = json!({"type": "object", "properties": {"auth": {"oneOf": [
            {"type": "object", "properties": {"kind": {"const": "agent"}}},
            {"type": "object", "properties": {
                "path": {"type": "string"}, "kind": {"type": "string", "const": "key"}}},
            {"type": "object", "properties": {"path": {"type": "string"}, "mode": {"enum": ["x"]}}},
        ]}}});
        tags_first(&mut schema);
        let keys = |variant: usize| -> Vec<&str> {
            schema["properties"]["auth"]["oneOf"][variant]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect()
        };
        assert_eq!(keys(1), ["kind", "path"]);
        assert_eq!(keys(2), ["mode", "path"]);
    }

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
        let mut normalized = original.clone();
        sanitize_schema(&mut normalized);
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
        // The rewrites preserve what the schema accepts and rejects.
        let validator = jsonschema::validator_for(&normalized).unwrap();
        for value in [Value::Null, json!(42), json!([1, false]), json!({"a":true})] {
            assert!(validator.is_valid(&json!({"value":value})));
        }
        for invalid in [
            json!({"value":1, "forbidden":1}),
            json!({"value":1, "unknown":1}),
        ] {
            assert!(!validator.is_valid(&invalid), "{invalid}");
        }
        // Preserving the dialect must not change any other normalization behavior.
        let dialect = original["$schema"].clone();
        let mut preserved = original;
        sanitize_schema_inner(&mut preserved, true);
        assert_eq!(
            preserved.as_object_mut().unwrap().remove("$schema"),
            Some(dialect)
        );
        assert_eq!(preserved, normalized);
    }

    fn tool(schema: Value, options: ToolOptions) -> ToolSpec {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_dynamic("test", "test", schema, options, |_, _| async {
                Ok(ToolOutput::new(Value::Null))
            })
            .unwrap();
        let tool = builder.build().get("test").unwrap();
        tool.spec(&CapabilitySet::default(), AgentLevel::Root)
            .unwrap()
    }

    #[test]
    fn background_output_schema_is_the_normalized_native_payload() {
        let tool = tool(
            json!({"type": "object"}),
            ToolOptions::default()
                .background()
                .output_schema(Value::Bool(true)),
        );
        assert_eq!(tool.result_schema.unwrap().schema(), &json!({}));
    }

    #[test]
    fn external_schema_defaults_do_not_make_required_fields_optional() {
        let nested = json!({"type": "object", "properties": {"inner": {"default": 1}}, "required": ["inner"]});
        let schema = json!({"type": "object", "properties": {
            "value": {"type": "string", "default": "example"}
        }, "required": ["value"], "unevaluatedProperties": nested});
        for (options, accepts_omission) in [
            (ToolOptions::default().preserve_required(), false),
            (ToolOptions::default(), true),
        ] {
            let schema = tool(schema.clone(), options).input_schema;
            let validator = jsonschema::validator_for(&schema).unwrap();
            assert_eq!(validator.is_valid(&json!({})), accepts_omission);
            // Defaults are found inside every keyword holding a subschema.
            let extra = json!({"value": "given", "extra": {}});
            assert_eq!(validator.is_valid(&extra), accepts_omission);
        }
    }
}
