//! How tool input and result schemas are generated from their types.

pub mod navigate;

use schemars::{JsonSchema, generate::SchemaSettings, transform::RecursiveTransform};
use serde_json::Value;

/// Typed arguments keep the deserialization contract: defaults and `Option`
/// fields remain omissible even when the output type would emit the same fields.
pub fn input_schema_for<I: JsonSchema + 'static>() -> Value {
    generated::<I>(Contract::Input, || {
        input_settings()
            .into_generator()
            .into_root_schema_for::<I>()
    })
}

/// How tool input schemas are generated: by the deserialization contract, each
/// description reflowed from its doc comment.
pub fn input_settings() -> SchemaSettings {
    SchemaSettings::default()
        .for_deserialize()
        .with_transform(RecursiveTransform(reflow_description))
}

/// Results are described by their serialization contract.
pub fn result_schema<O: JsonSchema + 'static>() -> Value {
    generated::<O>(Contract::Result, || {
        SchemaSettings::default()
            .for_serialize()
            .with_transform(RecursiveTransform(omit_null))
            .with_transform(RecursiveTransform(reflow_description))
            .into_generator()
            .into_root_schema_for::<O>()
    })
}

/// A doc comment's wrapped lines joined into the description a model reads.
pub fn reflow_description(schema: &mut schemars::Schema) {
    if let Some(serde_json::Value::String(description)) = schema.get_mut("description") {
        *description = skyhook_base::schema::reflow(description);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Contract {
    Input,
    Result,
}

/// `T`'s schema under `contract`, generated once per process: every registry
/// declaring the type shares it.
fn generated<T: 'static>(contract: Contract, generate: impl FnOnce() -> schemars::Schema) -> Value {
    use std::{
        any::TypeId,
        collections::HashMap,
        sync::{LazyLock, Mutex, PoisonError},
    };
    static GENERATED: LazyLock<Mutex<HashMap<(TypeId, Contract), Value>>> =
        LazyLock::new(Mutex::default);
    let generated = || GENERATED.lock().unwrap_or_else(PoisonError::into_inner);
    let key = (TypeId::of::<T>(), contract);
    if let Some(schema) = generated().get(&key) {
        return schema.clone();
    }
    let schema = schema_value(generate());
    generated().entry(key).or_insert(schema).clone()
}

/// schemars gives every `Option` a null alternative, but a result omits `None`:
/// an optional property declares only the schema of its present value.
fn omit_null(schema: &mut schemars::Schema) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let required = object.get("required").cloned().unwrap_or_default();
    let required = required.as_array().map(Vec::as_slice).unwrap_or_default();
    let Some(Value::Object(properties)) = object.get_mut("properties") else {
        return;
    };
    for (name, property) in properties {
        if required.iter().any(|required| required == name) {
            continue;
        }
        let Value::Object(property) = property else {
            continue;
        };
        if let Some(Value::Array(types)) = property.get_mut("type") {
            types.retain(|kind| kind != "null");
            if let [kind] = types.as_slice() {
                let kind = kind.clone();
                property.insert("type".to_owned(), kind);
            }
        } else if let Some(Value::Array(options)) = property.get("anyOf")
            && let [Value::Object(present), null] = options.as_slice()
            && null == &serde_json::json!({"type": "null"})
        {
            let mut present = present.clone();
            property.shift_remove("anyOf");
            present.append(property);
            *property = present;
        }
    }
}

/// Serialized rather than unwrapped: serialization puts keywords in schemars'
/// canonical order, the order tools are offered in.
fn schema_value(schema: schemars::Schema) -> Value {
    serde_json::to_value(schema).expect("generated schemas serialize")
}
