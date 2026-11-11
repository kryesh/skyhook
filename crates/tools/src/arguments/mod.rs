//! Tool arguments: admitted against their schema, refused without quoting them, and
//! deserialized.

mod coerce;
mod failure;
mod unquoted;

use serde_json::{Map, Value};

pub use failure::schema_argument_failure;
pub(crate) use failure::{ArgumentPathSegment, safe_argument_path};
use unquoted::{Reason, Unquoted};

use crate::error::AdmissionError;
use skyhook_worker::diagnostic::{Operation, Subject};

/// Deserialize arguments `schema` admitted, once, at the argument boundary: admission
/// already explained what the schema rejects, so a rejection here is the type's, located
/// by `schema`'s declared names. It never quotes the arguments, which may hold
/// credentials or user-provided content.
pub fn deserialize_arguments<T: serde::de::DeserializeOwned>(
    schema: &serde_json::Value,
    arguments: &Arguments,
) -> Result<T, AdmissionError> {
    let unquoted = Unquoted::Object(arguments);
    serde_path_to_error::deserialize(unquoted).map_err(|error| {
        let mut segments: Vec<_> = error.path().iter().collect();
        // An unknown field is located at the object it is in.
        if let Reason::UnknownField { field, .. } = error.inner()
            && matches!(segments.last(), Some(serde_path_to_error::Segment::Map { key }) if key == field)
        {
            segments.pop();
        }
        let path = safe_argument_path(
            schema,
            segments.into_iter().map(|segment| match segment {
                serde_path_to_error::Segment::Map { key } => ArgumentPathSegment::Property(key),
                serde_path_to_error::Segment::Seq { index } => ArgumentPathSegment::Index(*index),
                _ => ArgumentPathSegment::Unknown,
            }),
        );
        AdmissionError::invalid_arguments(error.inner().to_string())
            .operation(Operation::Validate, Subject::Argument(path))
    })
}

/// A tool's argument schema as declared, compiled: as the tool is offered, or at its
/// first use for a schema that compiles by construction.
#[derive(Debug)]
pub enum ArgumentValidator {
    Compiled(std::sync::Arc<jsonschema::Validator>),
    Deferred(
        Value,
        std::sync::OnceLock<std::sync::Arc<jsonschema::Validator>>,
    ),
}

impl ArgumentValidator {
    /// The compiled schema, or why it does not compile.
    pub fn get(&self) -> Result<&jsonschema::Validator, String> {
        match self {
            Self::Compiled(validator) => Ok(validator),
            Self::Deferred(_, compiled) if let Some(validator) = compiled.get() => Ok(validator),
            Self::Deferred(schema, compiled) => {
                let validator = argument_validator(schema)?;
                Ok(compiled.get_or_init(|| validator))
            }
        }
    }
}

/// Compile an argument schema for validation, or say why it cannot be compiled. Each
/// schema compiles once per process: every registry and surface declaring it shares the
/// outcome, so the process keeps one per distinct schema it has validated.
pub fn argument_validator(
    schema: &serde_json::Value,
) -> Result<std::sync::Arc<jsonschema::Validator>, String> {
    use std::{
        collections::HashMap,
        sync::{Arc, LazyLock, Mutex, PoisonError},
    };
    type Compiled = Result<Arc<jsonschema::Validator>, String>;
    static COMPILED: LazyLock<Mutex<HashMap<String, Compiled>>> = LazyLock::new(Mutex::default);
    let compiled = || COMPILED.lock().unwrap_or_else(PoisonError::into_inner);
    let key = schema.to_string();
    if let Some(compiled) = compiled().get(&key) {
        return compiled.clone();
    }
    let built = jsonschema::options()
        .with_retriever(NoExternalArgumentSchemas)
        .build(schema)
        .map(Arc::new)
        .map_err(|error| error.to_string());
    compiled().entry(key).or_insert(built).clone()
}

/// Schema diagnostics must never retrieve filesystem or network resources, even
/// when dependency feature unification enables a default external resolver.
pub struct NoExternalArgumentSchemas;

impl jsonschema::Retrieve for NoExternalArgumentSchemas {
    fn retrieve(
        &self,
        _uri: &jsonschema::Uri<String>,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external argument schema references are not permitted".into())
    }
}

/// A call's arguments once validated: always a JSON object.
pub type Arguments = Map<String, Value>;

/// Who wrote a call's arguments: only a model's are repaired before they are admitted;
/// a program's must match exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Author {
    Model,
    Program,
}

/// `arguments` admitted against `schema`, which `validator` compiles.
pub fn admit_arguments(
    schema: &Value,
    validator: &jsonschema::Validator,
    author: Author,
    mut arguments: Value,
) -> Result<Arguments, AdmissionError> {
    if !arguments.is_object() {
        return Err(AdmissionError::arguments_must_be_object());
    }
    if author == Author::Model {
        coerce::coerce_arguments(schema, validator, &mut arguments);
    }
    if let Err(error) = validator.validate(&arguments) {
        let (path, expectation) = schema_argument_failure(schema, &arguments, &error);
        return Err(AdmissionError::invalid_arguments(expectation)
            .operation(Operation::Validate, Subject::Argument(path)));
    }
    match arguments {
        Value::Object(arguments) => Ok(arguments),
        _ => unreachable!("checked above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::TargetView;
    use schemars::JsonSchema;
    use serde_json::json;
    use std::collections::BTreeMap;

    /// A value its schema accepts but its type rejects is located through optional
    /// references and explained by the type, without being quoted.
    #[test]
    fn a_type_rejection_names_its_field_and_reason() {
        #[derive(serde::Deserialize, JsonSchema)]
        #[serde(try_from = "i64")]
        #[schemars(with = "i64")]
        struct Positive;
        impl TryFrom<i64> for Positive {
            type Error = &'static str;
            fn try_from(value: i64) -> Result<Self, &'static str> {
                (value > 0).then_some(Self).ok_or("an id must be positive")
            }
        }
        #[derive(serde::Deserialize, JsonSchema)]
        struct Input {
            #[expect(dead_code)]
            report: Option<Report>,
        }
        #[derive(serde::Deserialize, JsonSchema)]
        struct Report {
            #[expect(dead_code)]
            notes: Vec<Note>,
        }
        #[derive(serde::Deserialize, JsonSchema)]
        struct Note {
            #[expect(dead_code)]
            candidate: Positive,
        }
        let value = json!({"report": {"notes": [{"candidate": -7311}]}});
        let schema = schemars::schema_for!(Input).to_value();
        let error = deserialize_arguments::<Input>(&schema, value.as_object().unwrap())
            .err()
            .unwrap();
        let diagnostic = error.diagnostic();
        assert_eq!(
            diagnostic.context.subject,
            Subject::Argument("/report/notes/0/candidate".into())
        );
        let text = diagnostic.render(TargetView::Hidden);
        assert!(
            text.contains("an id must be positive") && !text.contains("7311"),
            "{text}"
        );
        // A field the schema admits but the type does not take is named in its object.
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Closed {
            #[expect(dead_code)]
            id: u32,
        }
        let value = json!({"report": {"id": 1, "status": "secret-value"}});
        let error = deserialize_arguments::<BTreeMap<String, Closed>>(
            &json!({}),
            value.as_object().unwrap(),
        )
        .err()
        .unwrap();
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.context.subject, Subject::Argument("/*".into()));
        let text = diagnostic.render(TargetView::Hidden);
        assert!(
            text.contains("unknown field `status`, expected one of `id`")
                && !text.contains("secret"),
            "{text}"
        );
    }
}
