//! A research tool's input: an object naming its `action`, with that action's arguments
//! beside it. The schema's properties are the union of the offered actions' arguments, and
//! `action`'s description gives each action's signature, which admission enforces.

use std::{borrow::Cow, collections::BTreeMap};

use schemars::{JsonSchema, Schema, SchemaGenerator, generate::SchemaSettings};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use skyhook::tool::{
    AdmissionError,
    arguments::deserialize_arguments,
    diagnostic::{Operation, Subject},
    input_schema_for,
    policy::CapabilitySet,
};

use super::{Action, Actions};

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
                .operation(Operation::Deserialize, Subject::argument([name]))
        };
        // Named only when declared, so a refusal never quotes what the call made up.
        let declared = |name: &String| catalog.schema["properties"].get(name).is_some();
        if let Some(name) =
            (self.arguments.keys()).find(|name| !fields.contains_key(*name) && declared(name))
        {
            let takes: Vec<_> = fields.keys().map(|name| format!("`{name}`")).collect();
            let takes = if takes.is_empty() {
                "none".to_owned()
            } else {
                takes.join(", ")
            };
            return Err(refused(
                name,
                format!("not an argument of action `{action}`, which takes {takes}"),
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
    SchemaSettings::default().for_deserialize().into_generator()
}

/// The object root taking `actions`: `action` names one, and every argument any of them
/// takes is a property. A name the actions give different types holds the alternatives,
/// when the tool lists it among its unions.
fn root<N: Actions>(generator: &mut SchemaGenerator, actions: &[N]) -> Schema {
    let mut properties = Map::new();
    let mut unions: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut signatures = Vec::new();
    for &action in actions {
        let arguments = Arguments::of(action.arguments(generator));
        signatures.push(arguments.signature(action));
        for field in arguments.fields {
            if N::UNIONS.contains(&field.name.as_str()) {
                properties.entry(&field.name).or_insert(Value::Null);
                let alternatives = unions.entry(field.name).or_default();
                if !(alternatives.iter_mut()).any(|known| merge(known, field.schema.clone())) {
                    alternatives.push(field.schema);
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
    for (name, mut alternatives) in unions {
        properties[&name] = match alternatives.len() {
            1 => alternatives.remove(0),
            _ => json!({"anyOf": alternatives}),
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
        "additionalProperties": false,
    }))
    .expect("an object")
}

/// One action's arguments, read from its schema.
struct Arguments {
    /// The arguments type's own doc.
    doc: Option<String>,
    fields: Vec<Field>,
}

struct Field {
    name: String,
    schema: Value,
    required: bool,
}

impl Arguments {
    /// An object's properties, or the alternatives' of a tagged or untagged enum: required
    /// only where every alternative requires it, with a tag taking each one's value.
    /// Defaults are left out; the signature marks what may be omitted.
    fn of(schema: Schema) -> Self {
        let mut schema = schema.to_value();
        let doc = (schema.get("description").and_then(Value::as_str)).map(str::to_owned);
        let alternatives = (schema.get_mut("oneOf").map(Value::take))
            .or_else(|| schema.get_mut("anyOf").map(Value::take));
        let alternatives = match alternatives {
            Some(Value::Array(alternatives)) => alternatives,
            _ => vec![schema],
        };
        let count = alternatives.len();
        let mut fields: Vec<(Field, usize)> = Vec::new();
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
                        Field {
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
            .map(|(field, requiring)| Field {
                required: requiring == count,
                ..field
            })
            .collect();
        Self { doc, fields }
    }

    /// `name(required, optional?): doc`, the action's doc followed by its arguments type's.
    fn signature(&self, action: impl Actions) -> String {
        let fields: Vec<_> = (self.fields.iter())
            .map(|field| match field.required {
                true => field.name.clone(),
                false => format!("{}?", field.name),
            })
            .collect();
        let doc = [Some(action.doc()), self.doc.as_deref()]
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
