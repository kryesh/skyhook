//! Preserve upstream JSON Schema semantics while reserving the host job envelope.
use crate::tool::{
    AdmissionError,
    diagnostic::{
        ArgumentPathSegment, Effects, NoExternalArgumentSchemas, Operation, Subject,
        safe_argument_path, safe_text, schema_argument_failure,
    },
};
use serde_json::{Map, Value, json};

fn rejected(error: AdmissionError, path: String) -> AdmissionError {
    error
        .operation(Operation::Validate, Subject::Argument(path))
        .effects(Effects::NotStarted)
}

pub(super) struct Arguments {
    pub(super) schema: Value,
    // Local references and validation paths remain in the upstream document's scope.
    upstream_schema: Value,
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
            .with_retriever(NoExternalArgumentSchemas)
            .build(&original)
            .map_err(|error| {
                format!(
                    "invalid input schema at {}",
                    safe_text(error.instance_path().as_str())
                )
            })?;
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
                    .with_retriever(NoExternalArgumentSchemas)
                    .canonicalize(&original)
                    .map_err(|_| {
                        "legacy root-reference schema cannot be canonicalized".to_owned()
                    })?;
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
                .with_retriever(NoExternalArgumentSchemas)
                .build(&envelope)
                .map_err(|error| {
                    format!(
                        "cannot embed input schema at {}",
                        safe_text(error.instance_path().as_str())
                    )
                })?;
            envelope
        } else {
            original.clone()
        };
        Ok(Self {
            schema,
            upstream_schema: original,
            validator,
            wrapped,
        })
    }

    pub(super) fn extract<'a>(
        &self,
        value: &'a Value,
    ) -> Result<&'a Map<String, Value>, AdmissionError> {
        let object = value
            .as_object()
            .ok_or_else(|| rejected(AdmissionError::ArgumentsMustBeObject, String::new()))?;
        if self.wrapped {
            if let Some(name) = object.keys().find(|name| name.as_str() != "arguments") {
                return Err(rejected(
                    AdmissionError::InvalidArguments("unknown argument".into()),
                    safe_argument_path(&self.schema, [ArgumentPathSegment::Property(name)]),
                ));
            }
            object
                .get("arguments")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    rejected(
                        AdmissionError::InvalidArguments("`arguments` must be an object".into()),
                        "/arguments".into(),
                    )
                })
        } else {
            Ok(object)
        }
    }

    pub(super) fn validate(&self, value: &Value) -> Result<(), AdmissionError> {
        let input = Value::Object(self.extract(value)?.clone());
        self.validator.validate(&input).map_err(|error| {
            let (path, expectation) =
                schema_argument_failure(&self.upstream_schema, &input, &error);
            let prefix = if self.wrapped { "/arguments" } else { "" };
            rejected(
                AdmissionError::InvalidArguments(expectation),
                format!("{prefix}{path}"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface(schema: &Value) -> impl Fn(&Value) -> bool {
        let validator = jsonschema::options()
            .with_retriever(NoExternalArgumentSchemas)
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
        for (dialect, legacy) in [
            ("http://json-schema.org/draft-04/schema#", true),
            ("http://json-schema.org/draft-07/schema#", true),
            ("https://json-schema.org/draft/2019-09/schema", false),
            ("https://json-schema.org/draft/2020-12/schema", false),
        ] {
            let check = |schema, valid, invalid| {
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
            };
            check(
                json!({"$schema":dialect, "type":"object",
                    "definitions":{"number":{"type":"integer","minimum":1}},
                    "properties":{"count":{"$ref":"#/definitions/number"}},"required":["count"]}),
                json!({"arguments":{"count":2}}),
                json!({"arguments":{"count":0}}),
            );
            // Legacy recursive root aliases retain their constraints.
            if legacy {
                check(
                    json!({"$schema":dialect, "type":"object", "additionalProperties":false,
                        "$ref":"#/definitions/node", "definitions":{"node":{
                            "type":"object", "required":["count"],
                            "properties":{"count":{"type":"integer","minimum":1},
                                          "child":{"$ref":"#/definitions/node"}}}}}),
                    json!({"arguments":{"count":1,"child":{"count":2},"bg":"upstream"}}),
                    json!({"arguments":{"count":1,"child":{"count":0}}}),
                );
            }
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

    #[test]
    fn validation_failures_name_declared_paths_and_hide_dynamic_keys() {
        let secret = "private-key";
        for (schema, value, path, expectations) in [
            (
                json!({"type":"object", "additionalProperties":false,
                    "properties":{"credentials":{"type":"object",
                        "properties":{"token":{"type":"integer"}}}}}),
                json!({"credentials":{"token":secret}}),
                "/credentials/token",
                vec!["expected an integer"],
            ),
            // Decode JSON pointers using the instance: numeric map keys are
            // private data, but array indices and escaped schema names survive.
            (
                json!({"type":"object", "properties":{"credentials":{"type":"object",
                    "additionalProperties":{"type":"object", "additionalProperties":{
                        "type":"array", "items":{"type":"object",
                        "properties":{"token/~":{"type":"integer"}}}}}}}}),
                json!({"arguments":{"credentials":{(secret):{"8675309":[{"token/~":secret}]}}}}),
                "/arguments/credentials/*/*/0/token~1~0",
                vec!["expected an integer"],
            ),
            (
                json!({"type":"object", "additionalProperties":false,
                    "properties":{"":{"type":"integer"}}}),
                json!({"":secret}),
                "/",
                vec!["expected an integer"],
            ),
            (
                json!({"type":"object"}),
                json!({"arguments":{}, (secret):secret}),
                "/*",
                vec!["unknown argument"],
            ),
            (
                json!({"type":"object", "additionalProperties":false}),
                json!({(secret):secret}),
                "",
                vec!["not allowed"],
            ),
            (
                json!({"type":"object", "additionalProperties":false,
                    "properties":{"command":{"type":"string"}}, "required":["command"]}),
                json!({}),
                "",
                vec!["missing required field `command`"],
            ),
            (
                json!({"type":"object", "additionalProperties":false,
                    "$defs":{"positive":{"minimum":1}},
                    "properties":{"count":{"$ref":"#/$defs/positive"}}}),
                json!({"count":0}),
                "/count",
                vec!["minimum 1"],
            ),
            (
                json!({"type":"object", "additionalProperties":false,
                    "properties":{"mode":{"enum":["fast", "safe"]}}}),
                json!({"mode":secret}),
                "/mode",
                vec!["expected one of", "fast", "safe"],
            ),
            (
                json!({"type":"object", "propertyNames":{"enum":["count"]}}),
                json!({"arguments":{(secret):secret}}),
                "/arguments",
                vec!["invalid property name", "expected one of", "count"],
            ),
        ] {
            let error = Arguments::new(schema)
                .unwrap()
                .validate(&value)
                .unwrap_err();
            let diagnostic = error.diagnostic();
            assert_eq!(diagnostic.context.subject, Subject::Argument(path.into()));
            let text = diagnostic.render(&Default::default());
            for expectation in expectations {
                assert!(text.contains(expectation), "{text}");
            }
            let stored = serde_json::to_string(&diagnostic).unwrap();
            for hidden in [secret, "8675309"] {
                assert!(!stored.contains(hidden), "{stored}");
            }
        }
    }
}
