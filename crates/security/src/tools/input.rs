//! A research tool's input: an object naming its `action`, with that action's arguments
//! beside it. An action whose arguments differ by the kind of record takes them inside
//! [`RECORD`], a union tagged by `kind`. The schema's properties are the union of the offered
//! actions' arguments, and `action`'s description gives each action's signature, which
//! admission enforces: the root stays open so that a stray argument is refused here, by name.

use std::{borrow::Cow, collections::BTreeMap};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use skyhook::tool::policy::CapabilitySet;
use skyhook_tools::{
    arguments::deserialize_arguments, error::AdmissionError, schema::input_schema_for,
};
use skyhook_worker::diagnostic::{Operation, Subject};

use super::{Action, Actions};

/// The argument holding a record whose fields depend on its kind.
pub(super) const RECORD: &str = "record";

/// A call: its action and, beside it, that action's arguments.
#[derive(Deserialize)]
pub(super) struct Input<N> {
    pub(super) action: N,
    #[serde(flatten)]
    arguments: Map<String, Value>,
}

impl<N: Actions> Input<N> {
    /// The action's arguments, refusing another action's and a missing required one at
    /// their names.
    pub(super) fn arguments<P: DeserializeOwned>(&self) -> Result<P, AdmissionError> {
        let catalog = N::catalog();
        let action = self.action.name();
        let fields = &catalog.fields[action];
        let refused = |name: &str, reason: String| {
            AdmissionError::invalid_arguments(reason)
                .operation(Operation::Validate, Subject::argument([name]))
        };
        if let Some(name) = (self.arguments.keys()).find(|name| !fields.contains_key(*name)) {
            let takes: Vec<_> = fields.keys().map(|name| format!("`{name}`")).collect();
            let takes = if takes.is_empty() {
                "none".to_owned()
            } else {
                takes.join(", ")
            };
            let inside = match fields.contains_key(RECORD) {
                true => format!(": a field of the record goes inside `{RECORD}`"),
                false => String::new(),
            };
            return Err(refused(
                name,
                format!("not an argument of action `{action}`, which takes {takes}{inside}"),
            ));
        }
        if let Some(name) = (fields.iter())
            .find(|&(name, &required)| required && !self.arguments.contains_key(name))
            .map(|(name, _)| name)
        {
            return Err(refused(name, format!("required by action `{action}`")));
        }
        deserialize_arguments(&catalog.schema, &self.arguments)
    }
}

impl<N: Actions> JsonSchema for Input<N> {
    fn schema_name() -> Cow<'static, str> {
        "Input".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        root(generator, N::ALL)
    }
}

/// What admission reads of a tool's actions, once.
pub(super) struct Catalog {
    /// The whole input schema, which declares every argument name.
    schema: Value,
    /// Each action's arguments, and whether it requires each.
    fields: BTreeMap<&'static str, BTreeMap<String, bool>>,
    /// The schema of what each action returns.
    results: BTreeMap<&'static str, Value>,
}

impl Catalog {
    pub(super) fn new<N: Actions>() -> Self {
        let mut generator = generator();
        let fields: BTreeMap<_, BTreeMap<_, _>> = (N::ALL.iter())
            .map(|&action| {
                let arguments = Arguments::of(action.arguments(&mut generator));
                let fields = arguments.fields.into_iter();
                (
                    action.name(),
                    fields.map(|f| (f.name, f.required)).collect(),
                )
            })
            .collect();
        let schema = input_schema_for::<Input<N>>();
        for name in N::UNIONS {
            assert!(
                schema["properties"][name].get("anyOf").is_some(),
                "`{name}` is listed in `unions` but every action gives it one type"
            );
        }
        let results = (N::ALL.iter())
            .map(|&action| (action.name(), action.result()))
            .collect();
        Self {
            schema,
            fields,
            results,
        }
    }

    pub(super) fn result(&self, action: impl Actions) -> Value {
        self.results[action.name()].clone()
    }
}

/// The input schema offering only the actions `capabilities` permit, with the definitions
/// their arguments use. `None` withdraws the tool.
pub(super) fn schema<A: Action>(capabilities: &CapabilitySet) -> Option<Value> {
    let permitted: Vec<_> = (A::Name::ALL.iter().copied())
        .filter(|&action| A::permitted(action, capabilities))
        .collect();
    if permitted.is_empty() {
        return None;
    }
    let mut generator = generator();
    let mut schema = root(&mut generator, &permitted).to_value();
    let definitions = generator.take_definitions(true);
    if !definitions.is_empty() {
        schema["$defs"] = Value::Object(definitions);
    }
    Some(schema)
}

fn generator() -> SchemaGenerator {
    skyhook_tools::schema::input_settings().into_generator()
}

/// The object root taking `actions`: `action` names one, and every argument any of them
/// takes is a property. A name the actions give different types holds the alternatives,
/// when the tool lists it among its unions.
fn root<N: Actions>(generator: &mut SchemaGenerator, actions: &[N]) -> Schema {
    let mut properties = Map::new();
    let mut unions: BTreeMap<String, Vec<(Value, Vec<&str>)>> = BTreeMap::new();
    let mut signatures = Vec::new();
    for &action in actions {
        let arguments = Arguments::of(action.arguments(generator));
        signatures.push(arguments.signature(action));
        for field in arguments.fields {
            if N::UNIONS.contains(&field.name.as_str()) {
                properties.entry(&field.name).or_insert(Value::Null);
                let alternatives = unions.entry(field.name).or_default();
                let known = (alternatives.iter_mut()).find_map(|(known, taking)| {
                    merge(known, field.schema.clone()).then_some(taking)
                });
                match known {
                    Some(taking) => taking.push(action.name()),
                    None => alternatives.push((field.schema, vec![action.name()])),
                }
            } else if let Some(property) = properties.get_mut(&field.name) {
                assert!(
                    merge(property, field.schema),
                    "actions declare `{}` differently: give it one type and doc, or list it \
                     in `unions`",
                    field.name
                );
            } else {
                properties.insert(field.name, field.schema);
            }
        }
    }
    // Each alternative names the actions taking it.
    for (name, mut alternatives) in unions {
        properties[&name] = match alternatives.len() {
            1 => alternatives.remove(0).0,
            _ => {
                let alternatives = alternatives.into_iter().map(|(mut schema, taking)| {
                    let taking: Vec<_> =
                        taking.iter().map(|action| format!("`{action}`")).collect();
                    let doc = schema.get("description").and_then(Value::as_str);
                    let doc = [
                        Some(format!("For {}", taking.join(", "))),
                        doc.map(str::to_owned),
                    ];
                    schema["description"] = doc
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(": ")
                        .into();
                    schema
                });
                json!({"anyOf": alternatives.collect::<Vec<_>>()})
            }
        };
    }
    let names: Vec<_> = actions.iter().map(|action| action.name()).collect();
    let mut root = Map::new();
    root.insert(
        "action".to_owned(),
        json!({"type": "string", "enum": names, "description": signatures.join("\n")}),
    );
    root.append(&mut properties);
    Schema::try_from(json!({
        "type": "object",
        "properties": root,
        "required": ["action"],
    }))
    .expect("an object")
}

/// One action's arguments, read from its schema.
struct Arguments {
    /// The arguments type's own doc.
    doc: Option<String>,
    fields: Vec<Argument>,
}

struct Argument {
    name: String,
    schema: Value,
    required: bool,
}

impl Arguments {
    /// An object's properties, or the alternatives' of a tagged enum: required only where
    /// every alternative requires it, with a tag taking each one's value. Defaults are left
    /// out; the signature marks what may be omitted.
    fn of(schema: Schema) -> Self {
        let mut schema = schema.to_value();
        let doc = (schema.get("description").and_then(Value::as_str)).map(str::to_owned);
        let alternatives = match schema.get_mut("oneOf").map(Value::take) {
            Some(Value::Array(alternatives)) => alternatives,
            _ => vec![schema],
        };
        let count = alternatives.len();
        let mut fields: Vec<(Argument, usize)> = Vec::new();
        for mut alternative in alternatives {
            let required = alternative["required"].take();
            let Value::Object(properties) = alternative["properties"].take() else {
                continue;
            };
            for (name, mut schema) in properties {
                let required = (required.as_array().into_iter().flatten()).any(|r| *r == *name);
                if let Some(object) = schema.as_object_mut() {
                    object.remove("default");
                    if let Some(tag) = object.remove("const") {
                        object.insert("enum".to_owned(), json!([tag]));
                    }
                }
                match fields.iter_mut().find(|(field, _)| field.name == name) {
                    Some((field, requiring)) => {
                        assert!(
                            merge_variant(&mut field.schema, schema),
                            "variants declare `{name}` differently: give it one type and doc"
                        );
                        *requiring += usize::from(required);
                    }
                    None => fields.push((
                        Argument {
                            name,
                            schema,
                            required: false,
                        },
                        usize::from(required),
                    )),
                }
            }
        }
        let fields = (fields.into_iter())
            .map(|(field, requiring)| Argument {
                required: requiring == count,
                ..field
            })
            .collect();
        Self { doc, fields }
    }

    /// `name(required, optional?): doc`, its arguments type's doc followed by the action's,
    /// which ends with what the action returns.
    fn signature(&self, action: impl Actions) -> String {
        let fields: Vec<_> = (self.fields.iter())
            .map(|field| match field.required {
                true => field.name.clone(),
                false => format!("{}?", field.name),
            })
            .collect();
        let doc = [self.doc.as_deref(), Some(action.doc())]
            .into_iter()
            .flatten()
            .flat_map(str::split_whitespace)
            .collect::<Vec<_>>()
            .join(" ");
        let signature = format!("{}({})", action.name(), fields.join(", "));
        match doc.is_empty() {
            true => signature,
            false => format!("{signature}: {doc}"),
        }
    }
}

/// One field as two declarations give it: the same schema, or one only adding null.
fn merge(field: &mut Value, other: Value) -> bool {
    if *field == other || non_null(field) == other {
        return true;
    }
    if non_null(&other) == *field {
        *field = other;
        return true;
    }
    false
}

/// One field as two variants give it, a tag joining their values.
fn merge_variant(field: &mut Value, mut other: Value) -> bool {
    let values = |schema: &mut Value| (schema.as_object_mut()?).remove("enum");
    match (values(field), values(&mut other)) {
        (None, None) => merge(field, other),
        (Some(Value::Array(mut tags)), Some(Value::Array(more))) if *field == other => {
            tags.extend(more);
            field["enum"] = Value::Array(tags);
            true
        }
        _ => false,
    }
}

/// `schema` without the null an `Option` adds.
fn non_null(schema: &Value) -> Value {
    let mut schema = schema.clone();
    let Some(object) = schema.as_object_mut() else {
        return schema;
    };
    if let Some(Value::Array(types)) = object.get_mut("type") {
        types.retain(|kind| kind != "null");
        if let [kind] = types.as_slice() {
            let kind = kind.clone();
            object.insert("type".to_owned(), kind);
        }
    } else if let Some(Value::Array(options)) = object.get("anyOf")
        && let [Value::Object(present), null] = options.as_slice()
        && *null == json!({"type": "null"})
    {
        let mut present = present.clone();
        object.remove("anyOf");
        present.append(object);
        return Value::Object(present);
    }
    schema
}
