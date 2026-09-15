//! Preserve upstream JSON Schema semantics while reserving the host job envelope.
use crate::tool::ToolError;
use serde_json::{Map, Value, json};

struct NoExternalSchemas;

impl jsonschema::Retrieve for NoExternalSchemas {
    fn retrieve(
        &self,
        _uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external MCP schema references are not permitted".into())
    }
}

pub(super) struct Arguments {
    pub(super) schema: Value,
    validator: jsonschema::Validator,
    pub(super) wrapped: bool,
}

impl Arguments {
    pub(super) fn new(original: Value) -> Result<Self, String> {
        if original["type"] != "object" {
            return Err("input schema root type must be object".to_owned());
        }
        // Never load server-supplied references from the host filesystem or
        // network, even if feature unification later enables a default resolver.
        let validator = jsonschema::options()
            .with_retriever(NoExternalSchemas)
            .build(&original)
            .map_err(|error| error.to_string())?;
        // The registry consumes bg before validation. Unless the root explicitly
        // excludes it, preserve the entire upstream input under an envelope.
        // Pattern schemas also need wrapping because the registry's basic
        // unknown-property check only understands named properties.
        let wrapped = original["additionalProperties"] != false
            || original.get("$ref").is_some()
            || original["properties"].get("bg").is_some()
            || original.get("patternProperties").is_some()
            // These root constraints may count, reject, or otherwise interpret
            // the synthetic bg property. Keep them inside the envelope too.
            || [
                "allOf", "anyOf", "oneOf", "not", "if", "then", "else",
                "dependencies", "dependentSchemas", "propertyNames",
                "minProperties", "maxProperties", "unevaluatedProperties",
                "$dynamicRef", "$recursiveRef", "enum", "const",
            ]
            .iter()
            .any(|keyword| original.get(*keyword).is_some())
            || original["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|name| name == "bg"));
        let draft = jsonschema::Draft::default().detect(&original);
        let schema = if wrapped {
            let mut embedded = if original.get("$ref").is_some()
                && matches!(
                    draft,
                    jsonschema::Draft::Draft4
                        | jsonschema::Draft::Draft6
                        | jsonschema::Draft::Draft7
                ) {
                // Older drafts ignore every $ref sibling, including an injected
                // resource ID. Normalize the root alias before giving it scope.
                let canonical = jsonschema::canonical::options()
                    .with_retriever(NoExternalSchemas)
                    .canonicalize(&original)
                    .map_err(|error| error.to_string())?;
                if canonical.kind() == jsonschema::canonical::CanonicalKind::Raw {
                    return Err("legacy root-reference schema cannot be safely embedded".to_owned());
                }
                let mut schema = canonical.to_json_schema();
                if let Some(object) = schema.as_object_mut()
                    && let Some(reference) = object.remove("$ref")
                {
                    // The normalized document contains only the effective alias
                    // plus its definitions, not the ignored assertion siblings.
                    object.insert("allOf".to_owned(), json!([{"$ref": reference}]));
                }
                schema
            } else {
                original.clone()
            };
            if embedded.is_boolean() {
                embedded = json!({"allOf": [embedded]});
            }
            // A subschema's local fragment references must still refer to that
            // schema, not to our new envelope. A resource ID creates that scope.
            let id_keyword = if draft == jsonschema::Draft::Draft4 {
                "id"
            } else {
                "$id"
            };
            if embedded.get(id_keyword).is_none() {
                let hash = crate::sha256_hex(original.to_string().as_bytes());
                embedded[id_keyword] = Value::String(format!("urn:skyhook:mcp-schema:{hash}"));
            }
            let mut envelope = json!({
                "type": "object",
                "properties": {"arguments": embedded},
                "required": ["arguments"],
                "additionalProperties": false,
            });
            // Dialects are document-scoped in older drafts. Keep their keyword
            // semantics (including draft 4's `id`) after introducing a root.
            if let Some(dialect) = original.get("$schema") {
                envelope["$schema"] = dialect.clone();
            }
            // Do not publish an envelope with dangling references even when
            // the original document was valid in its unwrapped scope.
            jsonschema::options()
                .with_retriever(NoExternalSchemas)
                .build(&envelope)
                .map_err(|error| format!("cannot embed input schema: {error}"))?;
            envelope
        } else {
            original
        };
        Ok(Self {
            schema,
            validator,
            wrapped,
        })
    }

    pub(super) fn extract<'a>(
        &self,
        value: &'a Value,
    ) -> Result<&'a Map<String, Value>, ToolError> {
        let object = value.as_object().ok_or(ToolError::ArgumentsMustBeObject)?;
        if self.wrapped {
            if let Some(name) = object.keys().find(|name| name.as_str() != "arguments") {
                return Err(ToolError::InvalidArguments(format!(
                    "unknown argument `{name}`"
                )));
            }
            object
                .get("arguments")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    ToolError::InvalidArguments("`arguments` must be an object".to_owned())
                })
        } else {
            Ok(object)
        }
    }

    pub(super) fn validate(&self, value: &Value) -> Result<(), ToolError> {
        let object = self.extract(value)?;
        self.validator
            .validate(&Value::Object(object.clone()))
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface(schema: &Value) -> impl Fn(&Value) -> bool {
        let validator = jsonschema::options()
            .with_retriever(NoExternalSchemas)
            .build(schema)
            .unwrap_or_else(|error| panic!("{error}: {schema}"));
        move |instance| validator.is_valid(instance)
    }

    #[test]
    fn reserved_and_open_ended_inputs_are_wrapped_without_losing_bg() {
        for schema in [
            json!({"type":"object"}),
            json!({"type":"object","additionalProperties":true}),
            json!({"type":"object","additionalProperties":{"type":"string"}}),
            json!({"type":"object","properties":{"bg":{"type":"string"}},"additionalProperties":false}),
            json!({"type":"object","patternProperties":{"^b":{"type":"string"}},"additionalProperties":false}),
            // Reference siblings cannot hide an upstream bg either.
            json!({"$schema":"http://json-schema.org/draft-07/schema#", "type":"object",
                "additionalProperties":false, "definitions":{"open":{"type":"object"}},
                "$ref":"#/definitions/open"}),
        ] {
            let arguments = Arguments::new(schema).unwrap();
            let value = json!({"arguments":{"bg":"upstream"}});
            arguments.validate(&value).unwrap();
            assert!(surface(&arguments.schema)(&value));
            assert_eq!(arguments.extract(&value).unwrap()["bg"], "upstream");
        }
        let arguments = Arguments::new(json!({"type":"object"})).unwrap();
        for invalid in [
            json!({}),
            json!({"arguments":null}),
            json!({"arguments":{},"other":1}),
        ] {
            assert!(arguments.validate(&invalid).is_err());
        }
    }

    #[test]
    fn object_wide_constraints_do_not_interpret_the_job_bg_property() {
        for constraint in [
            json!({"maxProperties":1}),
            json!({"propertyNames":{"enum":["count"]}}),
            json!({"allOf":[{"properties":{"count":{"minimum":1}},"additionalProperties":false}]}),
            json!({"const":{"count":1}}),
        ] {
            let mut original = json!({
                "type":"object", "properties":{"count":{"type":"integer"}},
                "required":["count"], "additionalProperties":false
            });
            let constraint = constraint.as_object().unwrap().clone();
            original.as_object_mut().unwrap().extend(constraint);
            let arguments = Arguments::new(original).unwrap();
            arguments
                .validate(&json!({"arguments":{"count":1}}))
                .unwrap();
            let mut schema = arguments.schema.clone();
            schema["properties"]["bg"] = json!({"type":"boolean"});
            assert!(surface(&schema)(
                &json!({"arguments":{"count":1},"bg":true})
            ));
        }
    }

    #[test]
    fn local_and_recursive_references_keep_their_meaning_in_wrapped_schema() {
        let local = |dialect| {
            json!({"$schema":dialect, "type":"object",
                "definitions":{"number":{"type":"integer","minimum":1}},
                "properties":{"count":{"$ref":"#/definitions/number"}},"required":["count"]})
        };
        // Legacy recursive root aliases retain their constraints.
        let recursive = |dialect| {
            json!({"$schema":dialect, "type":"object", "additionalProperties":false,
                "$ref":"#/definitions/node", "definitions":{"node":{
                    "type":"object", "required":["count"],
                    "properties":{"count":{"type":"integer","minimum":1},
                                  "child":{"$ref":"#/definitions/node"}}}}})
        };
        let (draft4, draft7) = (
            "http://json-schema.org/draft-04/schema#",
            "http://json-schema.org/draft-07/schema#",
        );
        let mut cases = Vec::new();
        for dialect in [
            draft4,
            draft7,
            "https://json-schema.org/draft/2019-09/schema",
            "https://json-schema.org/draft/2020-12/schema",
        ] {
            let valid = json!({"arguments":{"count":2}});
            cases.push((
                dialect,
                local(dialect),
                valid,
                json!({"arguments":{"count":0}}),
            ));
        }
        for dialect in [draft4, draft7] {
            let valid = json!({"arguments":{"count":1,"child":{"count":2},"bg":"upstream"}});
            let invalid = json!({"arguments":{"count":1,"child":{"count":0}}});
            cases.push((dialect, recursive(dialect), valid, invalid));
        }
        for (dialect, schema, valid, invalid) in cases {
            let arguments = Arguments::new(schema).unwrap();
            let surface = surface(&arguments.schema);
            assert!(
                arguments.validate(&valid).is_ok() && surface(&valid),
                "{dialect}"
            );
            assert!(
                arguments.validate(&invalid).is_err() && !surface(&invalid),
                "{dialect}"
            );
        }
    }

    #[test]
    fn invalid_schemas_and_external_references_are_rejected() {
        for schema in [
            json!({"type":"array"}),
            json!({"type":"object","properties":{"x":{"type":"not-a-type"}}}),
            json!({"type":"object","$ref":"https://example.invalid/schema.json"}),
            json!({"type":"object","$ref":"file:///etc/passwd"}),
            json!({"type":"object","properties":{"secret":{"$ref":"file:///etc/passwd"}}}),
            json!({"type":"object","$schema":"https://example.invalid/metaschema.json"}),
            json!({"type":"object","$id":"https://example.invalid/root","$ref":"child.json"}),
        ] {
            assert!(Arguments::new(schema.clone()).is_err(), "accepted {schema}");
        }
    }
}
