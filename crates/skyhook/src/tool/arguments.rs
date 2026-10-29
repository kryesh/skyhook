//! Tool arguments deserialized with reasons that never quote them. serde's errors embed
//! the rejected value, which may be a credential or user content, so arguments are
//! read through a deserializer whose error keeps only what a type expected, what kind
//! of value it found, the declared names involved, and a type's own explanation.

use serde::de::{
    self, Deserializer, Expected, IntoDeserializer, Unexpected, Visitor,
    value::{MapAccessDeserializer, MapDeserializer, SeqDeserializer},
};
use serde_json::{Map, Value};

use super::{
    AdmissionError,
    diagnostic::{Operation, Subject, quoted},
    output::{FieldPointer, escape_pointer_segment},
};

/// Why a value did not deserialize, without the value.
#[derive(Debug)]
pub(crate) struct Reason(String);

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Reason {}

fn kind(unexpected: Unexpected<'_>) -> &'static str {
    use jsonschema::JsonType;
    argument_type(match unexpected {
        Unexpected::Bool(_) => JsonType::Boolean,
        Unexpected::Unsigned(_) | Unexpected::Signed(_) => JsonType::Integer,
        Unexpected::Float(_) => JsonType::Number,
        Unexpected::Char(_) | Unexpected::Str(_) => JsonType::String,
        Unexpected::Unit => JsonType::Null,
        Unexpected::Seq => JsonType::Array,
        Unexpected::Map => JsonType::Object,
        Unexpected::Bytes(_) => return "bytes",
        _ => return "another value",
    })
}

fn one_of(names: &'static [&'static str]) -> String {
    let names: Vec<_> = names.iter().map(|name| format!("`{name}`")).collect();
    names.join(", ")
}

impl de::Error for Reason {
    /// The explanation a type gives for rejecting a value it parsed, such as a
    /// conversion's error. serde's own messages, which quote the value, never reach it.
    fn custom<T: std::fmt::Display>(message: T) -> Self {
        Self(message.to_string())
    }

    fn invalid_type(unexpected: Unexpected<'_>, expected: &dyn Expected) -> Self {
        Self(format!("expected {expected}, found {}", kind(unexpected)))
    }

    fn invalid_value(unexpected: Unexpected<'_>, expected: &dyn Expected) -> Self {
        Self(format!("found {} that is not {expected}", kind(unexpected)))
    }

    fn invalid_length(length: usize, expected: &dyn Expected) -> Self {
        Self(format!("expected {expected}, found {length} elements"))
    }

    fn unknown_variant(_variant: &str, expected: &'static [&'static str]) -> Self {
        Self(format!(
            "unknown variant, expected one of {}",
            one_of(expected)
        ))
    }

    fn unknown_field(_field: &str, expected: &'static [&'static str]) -> Self {
        Self(format!(
            "unknown field, expected one of {}",
            one_of(expected)
        ))
    }

    fn missing_field(field: &'static str) -> Self {
        Self(format!("missing field `{field}`"))
    }

    fn duplicate_field(field: &'static str) -> Self {
        Self(format!("duplicate field `{field}`"))
    }
}

/// JSON read with [`Reason`] errors: a value, or the object of a call's arguments.
#[derive(Clone, Copy)]
pub(crate) enum Unquoted<'a> {
    Value(&'a Value),
    Object(&'a Map<String, Value>),
}

impl<'de> IntoDeserializer<'de, Reason> for Unquoted<'_> {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

impl<'a> Unquoted<'a> {
    fn object(self) -> Option<&'a Map<String, Value>> {
        match self {
            Self::Object(object) | Self::Value(Value::Object(object)) => Some(object),
            Self::Value(_) => None,
        }
    }
}

fn entries<'de>(
    object: &Map<String, Value>,
) -> MapDeserializer<'de, impl Iterator<Item = (Key<'_>, Unquoted<'_>)>, Reason> {
    let entries = object.iter();
    MapDeserializer::new(entries.map(|(key, value)| (Key(key), Unquoted::Value(value))))
}

/// An object's key, read as serde_json reads one: its text, or the number or boolean
/// that text spells for a type keyed by one.
#[derive(Clone, Copy)]
struct Key<'a>(&'a str);

impl<'de> IntoDeserializer<'de, Reason> for Key<'_> {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

impl Key<'_> {
    fn number<'de, V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Reason> {
        match self.0.parse() {
            Ok(number) => Unquoted::Value(&Value::Number(number)).deserialize_any(visitor),
            Err(_) => Err(de::Error::invalid_type(Unexpected::Str(self.0), &visitor)),
        }
    }

    /// The integer the key spells in JSON's grammar, parsed straight to a type wider than a
    /// [`serde_json::Number`] holds.
    fn wide<T: std::str::FromStr>(self, visitor: &dyn Expected) -> Result<T, Reason> {
        let digits = self.0.strip_prefix('-').unwrap_or(self.0);
        let json = digits == "0"
            || digits.starts_with(|digit| matches!(digit, '1'..='9'))
                && digits.bytes().all(|digit| digit.is_ascii_digit());
        (json.then(|| self.0.parse().ok()).flatten())
            .ok_or_else(|| de::Error::invalid_type(Unexpected::Str(self.0), visitor))
    }
}

macro_rules! numeric_keys {
    ($($method:ident)*) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Reason> {
            self.number(visitor)
        }
    )*};
}

impl<'de> Deserializer<'de> for Key<'_> {
    type Error = Reason;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Reason> {
        visitor.visit_str(self.0)
    }

    numeric_keys! {
        deserialize_i8 deserialize_i16 deserialize_i32 deserialize_i64
        deserialize_u8 deserialize_u16 deserialize_u32 deserialize_u64
        deserialize_f32 deserialize_f64
    }

    fn deserialize_i128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Reason> {
        let key = self.wide(&visitor)?;
        visitor.visit_i128(key)
    }

    fn deserialize_u128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Reason> {
        let key = self.wide(&visitor)?;
        visitor.visit_u128(key)
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Reason> {
        match self.0 {
            "true" => visitor.visit_bool(true),
            "false" => visitor.visit_bool(false),
            _ => Err(de::Error::invalid_type(Unexpected::Str(self.0), &visitor)),
        }
    }

    /// A key is never null.
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Reason> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Reason> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Reason> {
        visitor.visit_enum(self.0.into_deserializer())
    }

    serde::forward_to_deserialize_any! {
        char str string bytes byte_buf unit unit_struct seq tuple tuple_struct map struct
        identifier ignored_any
    }
}

fn visit_object<'de, V: Visitor<'de>>(
    object: &Map<String, Value>,
    visitor: V,
) -> Result<V::Value, Reason> {
    let mut map = entries(object);
    let value = visitor.visit_map(&mut map)?;
    map.end()?;
    Ok(value)
}

impl<'de> Deserializer<'de> for Unquoted<'_> {
    type Error = Reason;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Reason> {
        let value = match self {
            Self::Object(object) => return visit_object(object, visitor),
            Self::Value(value) => value,
        };
        match value {
            Value::Null => visitor.visit_unit(),
            Value::Bool(value) => visitor.visit_bool(*value),
            Value::Number(number) => match (number.as_u64(), number.as_i64()) {
                (Some(value), _) => visitor.visit_u64(value),
                (_, Some(value)) => visitor.visit_i64(value),
                _ => visitor.visit_f64(number.as_f64().unwrap_or(f64::NAN)),
            },
            Value::String(text) => visitor.visit_str(text),
            Value::Array(items) => {
                let mut items = SeqDeserializer::new(items.iter().map(Unquoted::Value));
                let value = visitor.visit_seq(&mut items)?;
                items.end()?;
                Ok(value)
            }
            Value::Object(object) => visit_object(object, visitor),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Reason> {
        match self {
            Self::Value(Value::Null) => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Reason> {
        visitor.visit_newtype_struct(self)
    }

    /// A unit variant is its name; any other is an object of one entry.
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Reason> {
        match (self, self.object()) {
            (Self::Value(Value::String(name)), _) => {
                visitor.visit_enum(name.as_str().into_deserializer())
            }
            (_, Some(object)) if object.len() == 1 => {
                visitor.visit_enum(MapAccessDeserializer::new(entries(object)))
            }
            _ => self.deserialize_any(visitor),
        }
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        unit unit_struct seq tuple tuple_struct map struct identifier ignored_any
    }
}

/// Deserialize arguments `schema` admitted, once, at the argument boundary: admission
/// already explained what the schema rejects, so a rejection here is the type's, located
/// by `schema`'s declared names. It never quotes the arguments, which may hold
/// credentials or user-provided content.
pub fn deserialize_arguments<T: serde::de::DeserializeOwned>(
    schema: &serde_json::Value,
    arguments: &crate::tool::registry::Arguments,
) -> Result<T, AdmissionError> {
    let unquoted = Unquoted::Object(arguments);
    serde_path_to_error::deserialize(unquoted).map_err(|error| {
        let path = safe_argument_path(
            schema,
            error.path().iter().map(|segment| match segment {
                serde_path_to_error::Segment::Map { key } => ArgumentPathSegment::Property(key),
                serde_path_to_error::Segment::Seq { index } => ArgumentPathSegment::Index(*index),
                _ => ArgumentPathSegment::Unknown,
            }),
        );
        AdmissionError::invalid_arguments(error.inner().to_string())
            .operation(Operation::Deserialize, Subject::Argument(path))
    })
}

/// Compile an argument schema for validation, or say why it cannot be compiled.
pub(crate) fn argument_validator(
    schema: &serde_json::Value,
) -> Result<jsonschema::Validator, String> {
    jsonschema::options()
        .with_retriever(NoExternalArgumentSchemas)
        .build(schema)
        .map_err(|error| error.to_string())
}

/// Schema diagnostics must never retrieve filesystem or network resources, even
/// when dependency feature unification enables a default external resolver.
pub(crate) struct NoExternalArgumentSchemas;

impl jsonschema::Retrieve for NoExternalArgumentSchemas {
    fn retrieve(
        &self,
        _uri: &jsonschema::Uri<String>,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external argument schema references are not permitted".into())
    }
}

/// Keep only declared expectations from a typed validation failure. Neither the
/// rejected instance nor dynamic property names from its error kind are rendered.
pub(crate) fn schema_argument_failure(
    schema: &serde_json::Value,
    input: &serde_json::Value,
    error: &jsonschema::ValidationError<'_>,
) -> (String, String) {
    // JSON pointers cannot distinguish numeric map keys from array indices.
    // Decode against the instance, preserving empty property names too.
    let pointer: FieldPointer = error.instance_path().as_str().parse().unwrap_or_default();
    let components: Vec<_> = pointer.segments().collect();
    let mut node = input;
    let segments = components.iter().map(|component| {
        let segment = match node {
            serde_json::Value::Object(_) => ArgumentPathSegment::Property(component),
            serde_json::Value::Array(_) => component
                .parse::<usize>()
                .map_or(ArgumentPathSegment::Unknown, ArgumentPathSegment::Index),
            _ => ArgumentPathSegment::Unknown,
        };
        node = match segment {
            ArgumentPathSegment::Property(key) => node.get(key),
            ArgumentPathSegment::Index(index) => node.get(index),
            ArgumentPathSegment::Unknown => None,
        }
        .unwrap_or(&serde_json::Value::Null);
        segment
    });
    (
        safe_argument_path(schema, segments),
        validation_expectation(error.kind(), 0),
    )
}

fn validation_expectation(kind: &jsonschema::error::ValidationErrorKind, depth: usize) -> String {
    use jsonschema::error::{TypeKind, ValidationErrorKind as Kind};
    match kind {
        Kind::Required { property } => {
            format!("missing required field {}", quoted_schema(property))
        }
        Kind::Type { kind } => match kind {
            TypeKind::Single(kind) => format!("expected {}", argument_type(*kind)),
            TypeKind::Multiple(kinds) => format!(
                "expected {}",
                kinds
                    .iter()
                    .map(argument_type)
                    .collect::<Vec<_>>()
                    .join(" or ")
            ),
        },
        Kind::Enum { options } => format!("expected one of {}", quoted_schema(options)),
        Kind::Constant { expected_value } => {
            format!("expected constant {}", quoted_schema(expected_value))
        }
        Kind::Minimum { limit } => format!("minimum {limit}"),
        Kind::Maximum { limit } => format!("maximum {limit}"),
        Kind::ExclusiveMinimum { limit } => format!("must be greater than {limit}"),
        Kind::ExclusiveMaximum { limit } => format!("must be less than {limit}"),
        Kind::MultipleOf { multiple_of } => format!("must be a multiple of {multiple_of}"),
        Kind::MinLength { limit } => format!("minimum length {limit}"),
        Kind::MaxLength { limit } => format!("maximum length {limit}"),
        Kind::MinItems { limit } => format!("minimum items {limit}"),
        Kind::MaxItems { limit } => format!("maximum items {limit}"),
        Kind::MinProperties { limit } => format!("minimum properties {limit}"),
        Kind::MaxProperties { limit } => format!("maximum properties {limit}"),
        Kind::AdditionalItems { limit } => format!("maximum items {limit}"),
        Kind::UniqueItems => "array items must be unique".into(),
        Kind::Contains => "array must contain an item matching the declared schema".into(),
        Kind::Pattern { pattern } => format!("must match pattern {}", quoted(pattern)),
        Kind::Format { format } => format!("expected format {}", quoted(format)),
        Kind::ContentEncoding { content_encoding } => {
            format!("expected content encoding {}", quoted(content_encoding))
        }
        Kind::ContentMediaType { content_media_type } => {
            format!("expected content media type {}", quoted(content_media_type))
        }
        Kind::AdditionalProperties { .. } | Kind::UnevaluatedProperties { .. } => {
            "additional properties are not allowed".into()
        }
        Kind::UnevaluatedItems { .. } => "additional array items are not allowed".into(),
        Kind::AnyOf { context } | Kind::OneOfNotValid { context } if depth < 3 => {
            let alternatives = context
                .iter()
                .take(4)
                .map(|errors| {
                    errors
                        .iter()
                        .take(2)
                        .map(|error| validation_expectation(error.kind(), depth + 1))
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .collect::<Vec<_>>()
                .join(" / ");
            let requirement = if matches!(kind, Kind::OneOfNotValid { .. }) {
                "exactly one"
            } else {
                "at least one"
            };
            format!("must match {requirement} schema alternative: {alternatives}")
        }
        Kind::AnyOf { .. } => "must match a schema alternative".into(),
        Kind::OneOfNotValid { .. } | Kind::OneOfMultipleValid { .. } => {
            "must match exactly one schema alternative".into()
        }
        Kind::PropertyNames { error } if depth < 3 => format!(
            "invalid property name: {}",
            validation_expectation(error.kind(), depth + 1)
        ),
        Kind::PropertyNames { .. } => "property names must match the declared schema".into(),
        Kind::Not { .. } | Kind::FalseSchema => "value is not allowed by the schema".into(),
        Kind::FromUtf8 { .. } => "content must decode to UTF-8".into(),
        Kind::BacktrackLimitExceeded { .. }
        | Kind::RegexEngineFailure { .. }
        | Kind::Referencing(_)
        | Kind::Custom { .. } => "input could not be validated against the schema".into(),
    }
}

fn quoted_schema(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => quoted(value),
        value => quoted(&value.to_string()),
    }
}

fn argument_type(kind: jsonschema::JsonType) -> &'static str {
    use jsonschema::JsonType;
    match kind {
        JsonType::Integer => "an integer",
        JsonType::Number => "a number",
        JsonType::String => "a string",
        JsonType::Boolean => "a boolean",
        JsonType::Array => "an array",
        JsonType::Object => "an object",
        JsonType::Null => "null",
    }
}

/// A parsed argument location, independent of the native or MCP validator.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ArgumentPathSegment<'a> {
    Property(&'a str),
    Index(usize),
    Unknown,
}

/// Render a JSON pointer using only schema-declared names and indices. Dynamic
/// keys become `*`, but their value schemas can still declare safe descendants.
/// Common local references are followed; unsupported schemas hide names. Root is "".
pub(crate) fn safe_argument_path<'a>(
    schema: &serde_json::Value,
    path: impl IntoIterator<Item = ArgumentPathSegment<'a>>,
) -> String {
    let resolver = crate::json_schema::Resolver::new(schema);
    let mut node = Some(resolver.root());
    let mut rendered = String::new();
    // The first alternative, through references, that declares what the segment takes.
    let declaring = |node, keyword: &str, key: Option<&str>| {
        declaring(&resolver, node, &|schema: &serde_json::Value| {
            let declared = schema.get(keyword);
            key.map_or(declared, |key| declared?.get(key))
        })
    };
    for segment in path {
        rendered.push('/');
        node = match segment {
            ArgumentPathSegment::Property(key) => {
                match node.and_then(|node| declaring(node, "properties", Some(key))) {
                    Some(child) => {
                        rendered.push_str(&escape_pointer_segment(key));
                        Some(child)
                    }
                    None => {
                        rendered.push('*');
                        node.and_then(|node| declaring(node, "additionalProperties", None))
                    }
                }
            }
            ArgumentPathSegment::Index(index) => {
                rendered.push_str(&index.to_string());
                node.and_then(|node| declaring(node, "items", None))
            }
            ArgumentPathSegment::Unknown => {
                rendered.push('*');
                None
            }
        };
    }
    rendered
}

/// The subschema `select` picks from `node` or, failing that, from the first of its
/// alternatives that has one, following references.
fn declaring<'a>(
    resolver: &crate::json_schema::Resolver<'a>,
    node: crate::json_schema::Node<'a>,
    select: &dyn Fn(&'a serde_json::Value) -> Option<&'a serde_json::Value>,
) -> Option<crate::json_schema::Node<'a>> {
    let node = resolver.resolve(node)?;
    if let Some(selected) = select(node.schema) {
        return Some(node.child(selected));
    }
    let alternatives = ["oneOf", "anyOf", "allOf"].into_iter();
    (alternatives.filter_map(|keyword| node.schema.get(keyword)?.as_array()))
        .flatten()
        .find_map(|branch| {
            resolver
                .spend()
                .then(|| declaring(resolver, node.child(branch), select))?
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::policy::CapabilitySet;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn read<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, Reason> {
        T::deserialize(Unquoted::Value(value))
    }

    /// Keys convert as serde_json converts them, to numbers and booleans, and a key a
    /// type refuses is explained without being quoted.
    #[test]
    fn object_keys_convert_as_serde_json_converts_them() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Keyed {
            ids: BTreeMap<u32, String>,
            offsets: BTreeMap<i64, bool>,
            flags: BTreeMap<bool, u8>,
            names: BTreeMap<String, u8>,
            wide: BTreeMap<u128, u8>,
            low: BTreeMap<i128, u8>,
        }
        let value = json!({
            "ids": {"7": "seven"},
            "offsets": {"-3": true},
            "flags": {"true": 1, "false": 0},
            "names": {"7": 7},
            "wide": {"18446744073709551616": 1, "0": 0},
            "low": {"-9223372036854775809": 1, "-0": 0},
        });
        let expected: Keyed = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(read::<Keyed>(&value).unwrap(), expected);
        fn refused<T: serde::de::DeserializeOwned + Ord + std::fmt::Debug>(key: &str) {
            let value = json!({key: 0});
            assert!(serde_json::from_value::<BTreeMap<T, u8>>(value.clone()).is_err());
            let reason = read::<BTreeMap<T, u8>>(&value).unwrap_err().to_string();
            assert!(!reason.contains(key), "{reason}");
        }
        for key in ["secret", "300", "07", "+1"] {
            refused::<u8>(key);
        }
        for key in ["-0", "07", "+1", "340282366920938463463374607431768211456"] {
            refused::<u128>(key);
        }
    }

    #[test]
    fn native_arguments_report_declared_paths_and_types_without_user_data() {
        #[derive(Debug, Deserialize, JsonSchema)]
        #[allow(dead_code)]
        struct Input {
            entries: Vec<std::collections::HashMap<String, Entry>>,
        }
        #[derive(Debug, Deserialize, JsonSchema)]
        #[allow(dead_code)]
        struct Entry {
            #[serde(rename = "a~/b")]
            count: u32,
            mode: Option<Mode>,
        }
        #[derive(Debug, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        enum Mode {
            Fast,
            Safe,
        }
        let schema = schemars::schema_for!(Input).to_value();
        let validator = argument_validator(&schema).unwrap();
        for (value, path, expectations) in [
            (
                json!({"entries": [{"secret-key": {"a~/b": "secret-value"}}]}),
                "/entries/0/*/a~0~1b",
                vec!["expected an integer"],
            ),
            (json!({}), "", vec!["missing required field `entries`"]),
            (
                json!({"entries": [{"secret-key": {}}]}),
                "/entries/0/*",
                vec!["missing required field `a~/b`"],
            ),
            (
                json!({"entries": [{"secret-key": {"a~/b": -1}}]}),
                "/entries/0/*/a~0~1b",
                vec!["minimum 0"],
            ),
            (
                json!({"entries": [{"secret-key": {"a~/b": 1, "mode": "secret-value"}}]}),
                "/entries/0/*/mode",
                vec!["expected one of", "fast", "safe"],
            ),
        ] {
            let error = validator.validate(&value).unwrap_err();
            let (located, text) = schema_argument_failure(&schema, &value, &error);
            assert_eq!(located, path);
            for expectation in expectations {
                assert!(text.contains(expectation), "{text}");
            }
            assert!(
                !located.contains("secret") && !text.contains("secret"),
                "{text}"
            );
        }
    }

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
        let text = diagnostic.render(&CapabilitySet::default());
        assert!(
            text.contains("an id must be positive") && !text.contains("7311"),
            "{text}"
        );
    }

    #[test]
    fn argument_locations_preserve_declarations_and_hide_unknown_schema_paths() {
        use ArgumentPathSegment::{Index as I, Property as P, Unknown as U};
        let schema = json!({
            "$defs": {"entry/name": {"properties": {
                "value": {},
                "next": {"$ref": "#/$defs/entry~1name"}
            }}},
            "properties": {
                "map": {"additionalProperties": {"$ref": "#/$defs/entry~1name"}},
                "array": {"items": {"$ref": "#/$defs/entry~1name"}},
                "~a/b": {"properties": {"": {"properties": {"007": {}}}}},
                "cycle": {"$ref": "#/properties/cycle"},
                "external": {"$ref": "https://example.test/schema"},
                "pattern": {"patternProperties": {".*": {"properties": {"value": {}}}}},
                "choice": {"anyOf": [{"type": "integer"}, {"type": "string"}]}
            }
        });
        for (path, expected) in [
            (vec![], ""),
            (vec![P("map"), P("secret"), P("value")], "/map/*/value"),
            (
                vec![P("array"), I(2), P("next"), P("value")],
                "/array/2/next/value",
            ),
            (vec![P("~a/b"), P(""), P("007")], "/~0a~1b//007"),
            (vec![P("map"), P("123456"), P("unknown")], "/map/*/*"),
            (vec![P("unknown"), P("map")], "/*/*"),
            (vec![U, P("map")], "/*/*"),
            (vec![P("cycle"), P("value")], "/cycle/*"),
            (vec![P("external"), P("value")], "/external/*"),
            (vec![P("pattern"), P("secret"), P("value")], "/pattern/*/*"),
            (vec![P("choice")], "/choice"),
        ] {
            assert_eq!(safe_argument_path(&schema, path), expected);
        }
    }
}
