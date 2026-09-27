//! Conservative validation for strict Chat Structured Outputs.
use crate::json_schema::{MAX_DEPTH, accepts, declared_types};
use crate::provider::{ProviderError, ProviderErrorKind::InvalidRequest};
use serde_json::Value;
use std::collections::BTreeSet;

/// Deliberately conservative subset of strict Structured Outputs. Do not silently
/// rewrite optional properties, open objects, or unsupported schema constraints.
pub(crate) fn validate_schema(
    schema: &Value,
    root: &Value,
    depth: usize,
) -> Result<(), ProviderError> {
    if depth > MAX_DEPTH {
        return Err(InvalidRequest.error(format!(
            "Chat response schema nesting exceeds {MAX_DEPTH} levels"
        )));
    }
    let object = schema
        .as_object()
        .ok_or_else(|| InvalidRequest.error("Chat strict schemas must be objects"))?;
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "type"
                | "properties"
                | "required"
                | "additionalProperties"
                | "items"
                | "enum"
                | "anyOf"
                | "$ref"
                | "$defs"
                | "description"
                | "title"
        ) {
            return Err(
                InvalidRequest.error(format!("Unsupported Chat strict schema keyword: {key}"))
            );
        }
    }
    for key in ["description", "title"] {
        if object.get(key).is_some_and(|value| !value.is_string()) {
            return Err(InvalidRequest.error(format!("Chat schema {key} must be a string")));
        }
    }
    if let Some(defs) = object.get("$defs") {
        for definition in defs
            .as_object()
            .ok_or_else(|| InvalidRequest.error("Chat schema $defs must be an object"))?
            .values()
        {
            validate_schema(definition, root, depth + 1)?;
        }
    }
    if let Some(reference) = object.get("$ref") {
        let reference = reference
            .as_str()
            .ok_or_else(|| InvalidRequest.error("Chat schema $ref must be a string"))?;
        let pointer = reference
            .strip_prefix('#')
            .ok_or_else(|| InvalidRequest.error("Chat schemas support only local references"))?;
        if !pointer.is_empty() && !pointer.starts_with('/') {
            return Err(InvalidRequest.error("Chat schema references must be JSON pointers"));
        }
        if !root.pointer(pointer).is_some_and(Value::is_object) {
            return Err(
                InvalidRequest.error("Chat schema reference does not resolve to a schema object")
            );
        }
    }
    if let Some(variants) = object.get("anyOf") {
        let variants = variants
            .as_array()
            .filter(|values| !values.is_empty())
            .ok_or_else(|| InvalidRequest.error("Chat schema anyOf must be a nonempty array"))?;
        for variant in variants {
            validate_schema(variant, root, depth + 1)?;
        }
    }
    let mut types = BTreeSet::new();
    if let Some(kind) = object.get("type") {
        match kind {
            Value::String(kind) => {
                types.insert(kind.as_str());
            }
            Value::Array(kinds) if !kinds.is_empty() => {
                for kind in kinds {
                    let kind = kind.as_str().ok_or_else(|| {
                        InvalidRequest.error("Chat schema type entries must be strings")
                    })?;
                    if !types.insert(kind) {
                        return Err(InvalidRequest.error("Chat schema type entries must be unique"));
                    }
                }
            }
            _ => return Err(InvalidRequest.error("Invalid Chat schema type")),
        }
        if types.iter().any(|kind| {
            !matches!(
                *kind,
                "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
            )
        }) {
            return Err(InvalidRequest.error("Unsupported Chat schema type"));
        }
    } else if !object.contains_key("$ref") && !object.contains_key("anyOf") {
        return Err(InvalidRequest.error("Chat strict schema needs type, $ref, or anyOf"));
    }
    if types.contains("object") {
        if object.get("additionalProperties") != Some(&Value::Bool(false)) {
            return Err(InvalidRequest
                .error("Every Chat strict schema object requires additionalProperties: false"));
        }
        let properties = object
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| InvalidRequest.error("Chat strict schema objects require properties"))?;
        let required = object
            .get("required")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                InvalidRequest
                    .error("Chat strict schema objects require all properties to be required")
            })?;
        let mut names = BTreeSet::new();
        for name in required {
            let name = name.as_str().ok_or_else(|| {
                InvalidRequest.error("Chat schema required entries must be strings")
            })?;
            if !names.insert(name) || !properties.contains_key(name) {
                return Err(InvalidRequest
                    .error("Chat schema required must list each property exactly once"));
            }
        }
        if names.len() != properties.len() {
            return Err(
                InvalidRequest.error("Chat strict schema cannot contain optional properties")
            );
        }
        for property in properties.values() {
            validate_schema(property, root, depth + 1)?;
        }
    } else if ["properties", "required", "additionalProperties"]
        .iter()
        .any(|key| object.contains_key(*key))
    {
        return Err(InvalidRequest.error("Chat object schema keywords require object type"));
    }
    if types.contains("array") {
        validate_schema(
            object
                .get("items")
                .ok_or_else(|| InvalidRequest.error("Chat array schemas require items"))?,
            root,
            depth + 1,
        )?;
    } else if object.contains_key("items") {
        return Err(InvalidRequest.error("Chat items requires array type"));
    }
    if let Some(variants) = object.get("enum") {
        let variants = variants
            .as_array()
            .filter(|values| !values.is_empty())
            .ok_or_else(|| InvalidRequest.error("Chat schema enum must be nonempty"))?;
        for variant in variants {
            if declared_types(schema).is_some_and(|types| !accepts(types, variant)) {
                return Err(InvalidRequest.error("Chat schema enum value does not match its type"));
            }
        }
    }
    Ok(())
}
