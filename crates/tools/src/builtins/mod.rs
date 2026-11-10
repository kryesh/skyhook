//! The target tools: workspace files, search, processes and fetches.

pub mod atomic;
mod fetch;
mod filesystem;
mod process;
pub mod search;

pub use fetch::Fetch;
pub use filesystem::{Read, RemoveEntry, ReplaceText, WriteFile};
pub use process::RunCommand;
pub use search::{Glob, Search};

/// An attached image as a tool result describes it; the image itself is attached
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct ImageSummary {
    format: skyhook_media::ImageFormat,
    bytes: u64,
}

impl From<&skyhook_media::ImageRef> for ImageSummary {
    fn from(image: &skyhook_media::ImageRef) -> Self {
        Self {
            format: image.format,
            bytes: image.blob.bytes,
        }
    }
}

/// One value or a list of them, as a model may send a lone value where a list is
/// expected. `T` must not itself be an array, or the alternatives would overlap.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    fn as_slice(&self) -> &[T] {
        match self {
            Self::One(value) => std::slice::from_ref(value),
            Self::Many(values) => values,
        }
    }
}

impl<T> Default for OneOrMany<T> {
    fn default() -> Self {
        Self::Many(Vec::new())
    }
}

impl<T: schemars::JsonSchema> schemars::JsonSchema for OneOrMany<T> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        format!("OneOrMany_{}", T::schema_name()).into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"oneOf": [
            generator.subschema_for::<T>(),
            generator.subschema_for::<Vec<T>>(),
        ]})
    }
}

fn default_dot() -> skyhook_worker::op::TargetPath {
    crate::spec::DEFAULT_PATH.into()
}

use skyhook_base::bounded::Bounded;

/// Seconds a call may run.
type TimeoutSecs = Bounded<1, 3600>;

fn duration(timeout: TimeoutSecs) -> std::time::Duration {
    std::time::Duration::from_secs(timeout.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn one_or_many_takes_a_lone_value_as_a_one_item_list() {
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        struct Args {
            #[serde(default)]
            values: OneOrMany<String>,
        }
        let schema = crate::schema::input_schema_for::<Args>();
        let validator = jsonschema::validator_for(&schema).unwrap();
        for (input, expected) in [
            (json!({}), &[][..]),
            (json!({"values": "a"}), &["a"][..]),
            (json!({"values": ["a", "b"]}), &["a", "b"][..]),
        ] {
            assert!(validator.is_valid(&input), "{input}");
            let args: Args = serde_json::from_value(input).unwrap();
            assert_eq!(args.values.as_slice(), expected);
        }
        assert!(!validator.is_valid(&json!({"values": 1})));
    }
}
