//! Why arguments fail their schema, located and explained by what the schema declares.

use serde_json::Value;
use skyhook_worker::diagnostic::escape_pointer_segment;

use crate::{diagnostic::quoted, output::FieldPointer};

/// Keep only declared expectations from a typed validation failure. Neither the
/// rejected instance nor dynamic property names from its error kind are rendered.
pub fn schema_argument_failure(
    schema: &serde_json::Value,
    input: &serde_json::Value,
    error: &jsonschema::ValidationError<'_>,
) -> (String, String) {
    let (pointer, expectation) = match failing_alternative(schema, error) {
        Failure::Error(error) => {
            let found = input.pointer(error.instance_path().as_str());
            let expectation = match (expected_types(error), found) {
                (Some(expected), Some(found)) => {
                    type_expectation(&expected, crate::schema::navigate::value_type(found))
                }
                _ => validation_expectation(error.kind(), 0),
            };
            (error.instance_path().as_str().to_owned(), expectation)
        }
        Failure::Tag { at, tag, taken } => (
            format!("{}/{}", at.as_str(), escape_pointer_segment(tag)),
            format!("expected one of {}", quoted_schema(&Value::Array(taken))),
        ),
    };
    // JSON pointers cannot distinguish numeric map keys from array indices.
    // Decode against the instance, preserving empty property names too.
    let pointer: FieldPointer = pointer.parse().unwrap_or_default();
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
    (safe_argument_path(schema, segments), expectation)
}

/// Where a value fails: a validation failure, or a tag every alternative of a union declares
/// holding a value none of them takes.
enum Failure<'e, 'a, 's> {
    Error(&'e jsonschema::ValidationError<'a>),
    Tag {
        at: &'e jsonschema::paths::Location,
        tag: &'s str,
        taken: Vec<Value>,
    },
}

/// The failure a union's meant alternative reports, else the union's own.
fn failing_alternative<'e, 'a, 's>(
    schema: &'s Value,
    error: &'e jsonschema::ValidationError<'a>,
) -> Failure<'e, 'a, 's> {
    match meant(schema, error) {
        Some(Failure::Error(failure)) => failing_alternative(schema, failure),
        Some(tag) => tag,
        None => Failure::Error(error),
    }
}

/// One way to satisfy a union: an alternative, a union it holds at the same location expanded
/// into that union's alternatives. How the value fails it, and the schemas of the alternatives
/// it is reached through, where known.
struct Alternative<'e, 's> {
    failures: Vec<&'e jsonschema::ValidationError<'static>>,
    schemas: Vec<&'s Value>,
}

impl<'e, 's> Alternative<'e, 's> {
    /// The alternatives of union `error`, or `None` when it is no union.
    fn of(
        resolver: &crate::schema::navigate::Resolver<'s>,
        error: &'e jsonschema::ValidationError<'_>,
    ) -> Option<Vec<Self>> {
        use jsonschema::error::ValidationErrorKind as Kind;
        let (Kind::AnyOf { context } | Kind::OneOfNotValid { context }) = error.kind() else {
            return None;
        };
        let root = resolver.root();
        let declared = root.schema.pointer(error.schema_path().as_str());
        let mut alternatives = Vec::new();
        for (index, failures) in context.iter().enumerate() {
            // An alternative's own keywords beside its `$ref` hold for it as well.
            let declared = declared.and_then(|declared| declared.get(index));
            let resolved = declared.and_then(|declared| resolver.resolve(root.child(declared)));
            let schemas = declared
                .into_iter()
                .chain((resolved.map(|node| node.schema)).filter(|&resolved| {
                    !declared.is_some_and(|declared| std::ptr::eq(declared, resolved))
                }));
            let nested = (failures.iter().enumerate())
                .filter(|(_, failure)| failure.instance_path() == error.instance_path())
                .find_map(|(position, failure)| Some((position, Self::of(resolver, failure)?)));
            let Some((position, nested)) = nested else {
                alternatives.push(Self {
                    failures: failures.iter().collect(),
                    schemas: schemas.collect(),
                });
                continue;
            };
            for mut alternative in nested {
                let others = (failures.iter().enumerate()).filter(|&(other, _)| other != position);
                alternative
                    .failures
                    .extend(others.map(|(_, failure)| failure));
                alternative.schemas.extend(schemas.clone());
                alternatives.push(alternative);
            }
        }
        Some(alternatives)
    }

    /// Whether a value may hold `property` here: an unknown schema may.
    fn accepts(&self, property: &str) -> bool {
        self.schemas.iter().all(|schema| {
            schema["properties"].get(property).is_some()
                || schema.get("additionalProperties") != Some(&Value::Bool(false))
        })
    }

    /// Whether a schema it is reached through declares `property`.
    fn declares(&self, property: &str) -> bool {
        (self.schemas.iter()).any(|schema| schema["properties"].get(property).is_some())
    }

    /// The values a schema it is reached through restricts `property` to by `const` or
    /// `enum`, which make `property` one of its tags.
    fn tag(&self, property: &str) -> Option<&'s [Value]> {
        self.schemas.iter().find_map(|schema| {
            let declared = schema["properties"].get(property)?;
            match declared.get("const") {
                Some(value) => Some(std::slice::from_ref(value)),
                None => declared.get("enum")?.as_array().map(Vec::as_slice),
            }
        })
    }

    fn tagged(&self, property: &str, value: &Value) -> bool {
        self.tag(property)
            .is_some_and(|taken| taken.contains(value))
    }

    fn fails(&self, refusal: impl Fn(&jsonschema::ValidationError<'_>) -> bool) -> bool {
        self.failures.iter().any(|failure| refusal(failure))
    }
}

/// The failure of the alternative of union `error` the value was meant for. Of the
/// alternatives of the value's type, a tag they all declare decides when the value lacks it or
/// holds a value none takes. Otherwise an alternative is not meant when it does not take a tag
/// value another does, does not take a property the value holds, declares fewer of the value's
/// properties than another, or requires one the value lacks that another does not take. A tag
/// the rest all declare decides as before; else the one with the fewest failures is meant when
/// it alone has that few.
fn meant<'e, 'a, 's>(
    schema: &'s Value,
    error: &'e jsonschema::ValidationError<'a>,
) -> Option<Failure<'e, 'a, 's>> {
    use jsonschema::error::ValidationErrorKind as Kind;
    let resolver = crate::schema::navigate::Resolver::new(schema);
    let mut alternatives = Alternative::of(&resolver, error)?;
    let at = error.instance_path();
    let held = error.instance().as_object();
    let held = || held.into_iter().flatten();
    alternatives.retain(|alternative| {
        !alternative.fails(|failure| {
            matches!(failure.kind(), Kind::Type { .. }) && failure.instance_path() == at
        })
    });
    if let Some(failure) = tagged_failure(&alternatives.iter().collect::<Vec<_>>(), error) {
        return Some(failure);
    }
    for (property, value) in held() {
        if alternatives
            .iter()
            .any(|other| other.tagged(property, value))
        {
            alternatives
                .retain(|other| other.tag(property).is_none() || other.tagged(property, value));
        }
    }
    let extra = |alternative: &Alternative<'_, '_>| {
        alternative.fails(|failure| {
            matches!(failure.kind(), Kind::AdditionalProperties { .. })
                && failure.instance_path() == at
        })
    };
    if alternatives.iter().any(|alternative| !extra(alternative)) {
        alternatives.retain(|alternative| !extra(alternative));
    }
    let declared = |alternative: &Alternative<'_, '_>| {
        held()
            .filter(|(property, _)| alternative.declares(property))
            .count()
    };
    let most = alternatives.iter().map(declared).max()?;
    alternatives.retain(|alternative| declared(alternative) == most);
    let candidates: Vec<_> = (alternatives.iter())
        .filter(|alternative| {
            !alternative.fails(|failure| match failure.kind() {
                Kind::Required { property } if failure.instance_path() == at => {
                    let property = property.as_str().unwrap_or_default();
                    (alternatives.iter())
                        .any(|other| !std::ptr::eq(other, *alternative) && !other.accepts(property))
                }
                _ => false,
            })
        })
        .collect();
    if let Some(failure) = tagged_failure(&candidates, error) {
        return Some(failure);
    }
    let fewest = (candidates.iter())
        .map(|alternative| alternative.failures.len())
        .min();
    let mut closest =
        (candidates.into_iter()).filter(|alternative| Some(alternative.failures.len()) == fewest);
    match (closest.next(), closest.next()) {
        (Some(alternative), None) => {
            (alternative.failures.first()).map(|&first| Failure::Error(first))
        }
        _ => None,
    }
}

/// How union `error` fails at a tag all of `alternatives` declare: as missing it, or as holding
/// a value none of them takes.
fn tagged_failure<'e, 'a, 's>(
    alternatives: &[&Alternative<'e, 's>],
    error: &'e jsonschema::ValidationError<'a>,
) -> Option<Failure<'e, 'a, 's>> {
    use jsonschema::error::ValidationErrorKind as Kind;
    let at = error.instance_path();
    let held = error.instance().as_object();
    let tags = (alternatives.first()?.schemas.iter())
        .filter_map(|schema| schema["properties"].as_object())
        .flat_map(|properties| properties.keys())
        .filter(|tag| alternatives.iter().all(|other| other.tag(tag).is_some()));
    for tag in tags {
        match held.and_then(|held| held.get(tag)) {
            None => {
                let missing = (alternatives.iter().flat_map(|a| &a.failures)).find(|failure| {
                    failure.instance_path() == at
                        && matches!(failure.kind(), Kind::Required { property } if property == tag)
                });
                if let Some(&missing) = missing {
                    return Some(Failure::Error(missing));
                }
            }
            Some(value) if !alternatives.iter().any(|other| other.tagged(tag, value)) => {
                let mut taken = Vec::new();
                for value in
                    (alternatives.iter()).flat_map(|other| other.tag(tag).unwrap_or_default())
                {
                    if !taken.contains(value) {
                        taken.push(value.clone());
                    }
                }
                return Some(Failure::Tag { at, tag, taken });
            }
            Some(_) => {}
        }
    }
    None
}

/// The types `error` expects where it fails, when every failure it holds is of the value's type
/// there.
fn expected_types(error: &jsonschema::ValidationError<'_>) -> Option<Vec<jsonschema::JsonType>> {
    use jsonschema::error::{TypeKind, ValidationErrorKind as Kind};
    match error.kind() {
        Kind::Type {
            kind: TypeKind::Single(kind),
        } => Some(vec![*kind]),
        Kind::Type {
            kind: TypeKind::Multiple(kinds),
        } => Some(kinds.iter().collect()),
        Kind::AnyOf { context } | Kind::OneOfNotValid { context } => {
            let mut types = Vec::new();
            for failure in context.iter().flatten() {
                if failure.instance_path() != error.instance_path() {
                    return None;
                }
                for kind in expected_types(failure)? {
                    if !types.contains(&kind) {
                        types.push(kind);
                    }
                }
            }
            Some(types)
        }
        _ => None,
    }
}

/// What a value of the wrong type was expected to be, and what it is: an object or array sent
/// as its JSON text is told so.
fn type_expectation(expected: &[jsonschema::JsonType], found: jsonschema::JsonType) -> String {
    use jsonschema::JsonType;
    let expected_names: Vec<_> = expected.iter().map(|kind| argument_type(*kind)).collect();
    let text = format!(
        "expected {}, found {}",
        expected_names.join(" or "),
        argument_type(found)
    );
    match found == JsonType::String
        && (expected.iter()).any(|kind| matches!(kind, JsonType::Object | JsonType::Array))
    {
        true => format!("{text}: send the value itself, not its JSON text"),
        false => text,
    }
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
        Kind::AdditionalProperties { unexpected } | Kind::UnevaluatedProperties { unexpected } => {
            let names: Vec<_> = unexpected.iter().map(|name| quoted(name)).collect();
            match names.as_slice() {
                [name] => format!("property {name} is not allowed"),
                names => format!("properties {} are not allowed", names.join(", ")),
            }
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

pub(super) fn argument_type(kind: jsonschema::JsonType) -> &'static str {
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
    let resolver = crate::schema::navigate::Resolver::new(schema);
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
            ArgumentPathSegment::Property(key)
                if let Some(child) =
                    node.and_then(|node| declaring(node, "properties", Some(key))) =>
            {
                rendered.push_str(&escape_pointer_segment(key));
                Some(child)
            }
            ArgumentPathSegment::Property(_) => {
                rendered.push('*');
                node.and_then(|node| declaring(node, "additionalProperties", None))
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
    resolver: &crate::schema::navigate::Resolver<'a>,
    node: crate::schema::navigate::Node<'a>,
    select: &dyn Fn(&'a serde_json::Value) -> Option<&'a serde_json::Value>,
) -> Option<crate::schema::navigate::Node<'a>> {
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
    use crate::arguments::argument_validator;
    use schemars::JsonSchema;
    use serde_json::json;

    #[test]
    #[allow(dead_code)]
    fn native_arguments_report_declared_paths_and_types_without_user_data() {
        #[derive(JsonSchema)]
        struct Input {
            entries: Vec<std::collections::HashMap<String, Entry>>,
            links: Option<Links>,
            record: Option<Record>,
        }
        #[derive(JsonSchema)]
        #[serde(untagged, deny_unknown_fields)]
        enum Record {
            New { title: u32, body: u32, scope: u32 },
            Edit { id: u32 },
        }
        #[derive(JsonSchema)]
        #[serde(untagged)]
        enum Links {
            Add(Vec<Link>),
            Remove(Vec<Mode>),
        }
        #[derive(JsonSchema)]
        #[serde(rename_all = "snake_case", deny_unknown_fields)]
        enum Link {
            Step { path: u32 },
            State { state: u32 },
        }
        #[derive(JsonSchema)]
        struct Entry {
            #[serde(rename = "a~/b")]
            count: u32,
            mode: Option<Mode>,
        }
        #[derive(JsonSchema)]
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
            // A union fails at its closest alternative's element, naming the field it refuses.
            (
                json!({"entries": [], "links": [{"step": {"path": 1}}, {"step": {"path": 2}, "position": 1}]}),
                "/links/1",
                vec!["property `position` is not allowed"],
            ),
            // An alternative not taking a property the value holds is not the one meant.
            (
                json!({"entries": [], "record": {"title": 1}}),
                "/record",
                vec!["missing required field `body`"],
            ),
            (
                json!({"entries": "[]"}),
                "/entries",
                vec!["expected an array, found a string: send the value itself"],
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

    /// A union of tagged unions, some nesting another by a second tag, fails as the
    /// alternative its tags and properties name, or at a tag they all declare.
    #[test]
    fn tagged_unions_fail_as_the_alternative_their_tags_name() {
        #[allow(dead_code)]
        mod shapes {
            use schemars::JsonSchema;
            #[derive(JsonSchema)]
            pub struct Input {
                pub record: Option<Record>,
            }
            #[derive(JsonSchema)]
            #[serde(untagged)]
            pub enum Record {
                New(New),
                Edit(Edit),
                Status(Status),
            }
            #[derive(JsonSchema)]
            #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
            pub enum New {
                Note { title: String, body: String },
                Mark(Scoped),
            }
            #[derive(JsonSchema)]
            #[serde(tag = "scope", rename_all = "snake_case")]
            pub enum Scoped {
                Local { title: String },
                Shared { title: String, owner: u32 },
            }
            #[derive(JsonSchema)]
            #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
            pub enum Edit {
                Note { id: u32, title: Option<String> },
                Mark { id: u32, title: Option<String> },
            }
            #[derive(JsonSchema)]
            #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
            pub enum Status {
                Note { id: u32, done: bool },
                Mark(Marked),
            }
            #[derive(JsonSchema)]
            #[serde(tag = "status", rename_all = "snake_case")]
            pub enum Marked {
                Open { id: u32, reason: String },
                Closed { id: u32 },
            }
        }
        let schema = schemars::schema_for!(shapes::Input).to_value();
        let validator = argument_validator(&schema).unwrap();
        for (record, path, expectation) in [
            (
                json!({"kind": "mark", "scope": "shared", "owner": 1}),
                "/record",
                "missing required field `title`",
            ),
            (
                json!({"kind": "mark", "id": 1, "title": 2}),
                "/record/title",
                "expected null or a string, found an integer",
            ),
            (
                json!({"kind": "mark", "id": 1, "status": "open"}),
                "/record",
                "missing required field `reason`",
            ),
            (
                json!({"kind": "mark", "id": 1, "status": "secret"}),
                "/record/status",
                r#"expected one of `[\"open\",\"closed\"]`"#,
            ),
            (
                json!({"id": 1, "title": "t"}),
                "/record",
                "missing required field `kind`",
            ),
            (
                json!({"kind": "secret", "title": "t"}),
                "/record/kind",
                r#"expected one of `[\"note\",\"mark\"]`"#,
            ),
        ] {
            let value = json!({"record": record});
            let error = validator.validate(&value).unwrap_err();
            let failure = schema_argument_failure(&schema, &value, &error);
            assert_eq!(failure, (path.into(), expectation.into()), "{record}");
        }
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
