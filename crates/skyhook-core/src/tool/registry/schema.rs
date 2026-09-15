//! Validation and normalization of tool input and output schemas.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::tool::policy::CapabilitySet;

use super::{GeneratedToolDefinition, OutputSchema, RegistryError, ToolExecution, ToolSpec};

impl OutputSchema {
    pub(super) fn generate(&self, capabilities: &CapabilitySet) -> Value {
        match self {
            Self::Static(schema) => schema.clone(),
            Self::Generated(generate) => generate(capabilities),
        }
    }
}

impl GeneratedToolDefinition {
    pub(super) fn generate_scoped(
        &self,
        capabilities: &CapabilitySet,
        execution: &ToolExecution,
        child: bool,
    ) -> Option<ToolSpec> {
        self.required
            .iter()
            .chain(self.root_required.iter().filter(|_| !child))
            .all(|capability| capabilities.contains(*capability))
            .then(|| {
                let mut input_schema = (self.input_schema)(capabilities);
                if self.supports_background {
                    add_background(&mut input_schema);
                }
                if !self.preserve_required {
                    optional_defaults(&mut input_schema);
                }
                sanitize_schema_inner(&mut input_schema, self.preserve_schema_dialect);
                let result_schema = self.output_schema.as_ref().map(|schema| {
                    let mut schema = schema.generate(capabilities);
                    sanitize_schema(&mut schema);
                    schema
                });
                let output_schema = result_schema.as_ref().map(|schema| {
                    let mut schema = schema.clone();
                    if self.supports_background {
                        schema = output_union(
                            schema,
                            crate::job::presented_job_schema(capabilities, false),
                        );
                    }
                    sanitize_schema(&mut schema);
                    schema
                });
                ToolSpec {
                    supports_background: self.supports_background,
                    job_role: execution.job_role,
                    result_policy: execution.result_policy,
                    name: self.name.clone(),
                    description: self.description.clone(),
                    input_schema,
                    result_schema,
                    output_schema,
                    exposure: self.exposure,
                    script_binding: self.script_binding.clone(),
                }
            })
    }
}

pub(super) fn ensure_no_target(schema: &Value) -> Result<(), RegistryError> {
    validate_schema(schema)?;
    if schema["properties"].get("target").is_some() {
        return Err(RegistryError::ReservedTarget);
    }
    Ok(())
}

pub(super) fn target_property_schema() -> Value {
    serde_json::json!({
        "type": ["string", "null"],
        "description": "Execution target."
    })
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

pub(super) fn validate_schema(schema: &Value) -> Result<(), RegistryError> {
    validate_object_schema(schema)?;
    if schema["properties"].get("bg").is_some() {
        return Err(RegistryError::ReservedBackground);
    }
    Ok(())
}

pub(super) fn validate_object_schema(schema: &Value) -> Result<(), RegistryError> {
    let object = schema
        .as_object()
        .ok_or_else(|| RegistryError::Schema("root must be an object".to_owned()))?;
    if object.get("type").and_then(Value::as_str) != Some("object") {
        return Err(RegistryError::Schema("root type must be object".to_owned()));
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

fn add_background(schema: &mut Value) {
    add_schema_property(
        schema,
        "bg",
        serde_json::json!({
            "type": "boolean",
            "default": false,
            "description": "Run in background."
        }),
    );
}

// A documented default always permits omission from tool input. Apply this to
// schema nodes only, leaving default/example JSON values untouched.
fn optional_defaults(schema: &mut Value) {
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
    for key in [
        "properties",
        "$defs",
        "definitions",
        "patternProperties",
        "dependentSchemas",
    ] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_object_mut) {
            children.values_mut().for_each(optional_defaults);
        }
    }
    for key in [
        "items",
        "additionalProperties",
        "contains",
        "not",
        "if",
        "then",
        "else",
    ] {
        if let Some(child) = object.get_mut(key) {
            optional_defaults(child);
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_array_mut) {
            children.iter_mut().for_each(optional_defaults);
        }
    }
}

fn sanitize_schema(value: &mut Value) {
    sanitize_schema_inner(value, false);
}

fn sanitize_schema_inner(value: &mut Value, preserve_dialect: bool) {
    // `true` and `{}` both accept any JSON value. Some provider-side tool
    // schema converters (including llama.cpp) only accept the object form.
    // This traversal visits schema nodes, never literal defaults or examples.
    // Keep `false` intact: internal validation relies on closed-object flags.
    if value.as_bool() == Some(true) {
        *value = Value::Object(Map::new());
        return;
    }
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if !preserve_dialect {
        object.remove("$schema");
    }
    object.remove("title");
    object.remove("format");
    // Only descend into schemas. Property names and literal enum/default/example
    // payloads may themselves contain keys such as "title" or "format".
    for key in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
        "dependencies",
    ] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_object_mut) {
            children
                .values_mut()
                .for_each(|child| sanitize_schema_inner(child, preserve_dialect));
        }
    }
    for key in [
        "items",
        "additionalItems",
        "additionalProperties",
        "unevaluatedItems",
        "unevaluatedProperties",
        "contains",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "contentSchema",
    ] {
        if let Some(child) = object.get_mut(key) {
            if let Some(children) = child.as_array_mut() {
                children
                    .iter_mut()
                    .for_each(|child| sanitize_schema_inner(child, preserve_dialect));
            } else {
                sanitize_schema_inner(child, preserve_dialect);
            }
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get_mut(key).and_then(Value::as_array_mut) {
            children
                .iter_mut()
                .for_each(|child| sanitize_schema_inner(child, preserve_dialect));
        }
    }
}

/// Hoist definitions so both union members retain valid, unambiguous references.
fn output_union(foreground: Value, background: Value) -> Value {
    fn rename(value: &mut Value, prefix: &str) {
        match value {
            Value::Object(object) => {
                if let Some(Value::String(reference)) = object.get_mut("$ref")
                    && let Some(name) = reference.strip_prefix("#/$defs/")
                {
                    *reference = format!("#/$defs/{prefix}{name}");
                }
                for value in object.values_mut() {
                    rename(value, prefix);
                }
            }
            Value::Array(values) => {
                for value in values {
                    rename(value, prefix);
                }
            }
            _ => {}
        }
    }
    let mut definitions = serde_json::Map::new();
    let variants = [("Foreground_", foreground), ("Job_", background)]
        .into_iter()
        .map(|(prefix, mut schema)| {
            rename(&mut schema, prefix);
            if let Some(Value::Object(defs)) = schema
                .as_object_mut()
                .and_then(|object| object.remove("$defs"))
            {
                for (name, value) in defs {
                    definitions.insert(format!("{prefix}{name}"), value);
                }
            }
            schema
        })
        .collect::<Vec<_>>();
    serde_json::json!({"anyOf": variants, "$defs": definitions})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolOptions, ToolOutput, ToolRegistryBuilder};
    use serde_json::json;

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
        let validator = jsonschema::validator_for(&normalized).unwrap();
        for value in [
            Value::Null,
            json!(true),
            json!(false),
            json!(42),
            json!("text"),
            json!([1, false]),
            json!({"enabled":true}),
        ] {
            let valid = json!({"value":value});
            assert!(validator.is_valid(&valid));
        }
        for invalid in [
            json!({"value": null, "forbidden": null}),
            json!({"value": null, "unknown": 1}),
        ] {
            assert!(!validator.is_valid(&invalid));
        }
        let once = normalized.clone();
        sanitize_schema(&mut normalized);
        assert_eq!(normalized, once);

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
        builder
            .build()
            .surface(&CapabilitySet::default())
            .get("test")
            .unwrap()
            .clone()
    }

    #[test]
    fn unrestricted_output_schema_is_normalized_too() {
        let tool = tool(
            json!({"type": "object"}),
            ToolOptions::default().output_schema(Value::Bool(true)),
        );
        assert_eq!(tool.output_schema, Some(json!({})));
    }

    #[test]
    fn external_schema_defaults_do_not_make_required_fields_optional() {
        let schema = json!({"type": "object", "properties": {
            "value": {"type": "string", "default": "example"}
        }, "required": ["value"]});
        for (options, accepts_omission) in [
            (ToolOptions::default().preserve_required(), false),
            (ToolOptions::default(), true),
        ] {
            let schema = tool(schema.clone(), options).input_schema;
            let validator = jsonschema::validator_for(&schema).unwrap();
            assert_eq!(validator.is_valid(&json!({})), accepts_omission);
        }
    }
}
