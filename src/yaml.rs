//! YAML is an input format, not a second runtime value model. Keep the JSON
//! boundary explicit: no coerced mapping keys, lossy tags, merge keys, or
//! non-finite numbers. Parser budgets remain enabled, and diagnostics must not
//! include surrounding source text that may contain credentials.

use std::fmt;

use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value, map::Entry};
use serde_saphyr::{DuplicateKeyPolicy, MergeKeyPolicy, Options, Tagged};

fn options() -> Options {
    serde_saphyr::options! {
        strict_booleans: true,
        duplicate_keys: DuplicateKeyPolicy::Error,
        merge_keys: MergeKeyPolicy::Error,
        reject_unsupported_tags: true,
        with_snippet: false,
    }
}

pub(crate) fn from_str(text: &str) -> Result<Value, serde_saphyr::Error> {
    serde_saphyr::from_str_with_options::<JsonValue>(text, options()).map(|value| value.0)
}

/// Typed extraction through the JSON value tree, exactly as config loading does.
pub(crate) fn parse<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, String> {
    let value = from_str(text).map_err(|error| format!("invalid YAML: {error}"))?;
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// Unset optional fields are omitted rather than written as `null`.
pub(crate) fn to_string(value: &impl serde::Serialize) -> Result<String, String> {
    fn prune(value: &mut Value) {
        match value {
            Value::Object(fields) => {
                fields.retain(|_, field| !field.is_null());
                fields.values_mut().for_each(prune);
            }
            Value::Array(items) => items.iter_mut().for_each(prune),
            _ => {}
        }
    }
    let mut value = serde_json::to_value(value).map_err(|error| error.to_string())?;
    prune(&mut value);
    serde_saphyr::to_string(&value).map_err(|error| error.to_string())
}

// serde_json's ordinary visitor requests String keys, allowing the YAML
// deserializer to coerce non-string scalars before they can be checked. This
// boundary visitor builds the same JSON values but validates keys before insertion.
struct JsonValue(Value);

impl<'de> Deserialize<'de> for JsonValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsonVisitor;

        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = Value;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON-compatible YAML value")
            }

            fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }

            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
                Ok(Value::Bool(value))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
                Ok(Value::Number(value.into()))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
                Ok(Value::Number(value.into()))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
                Number::from_f64(value)
                    .map(Value::Number)
                    .ok_or_else(|| E::custom("YAML numbers must be finite"))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
                self.visit_string(value.to_owned())
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
                Ok(Value::String(value))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
                let mut values = Vec::new();
                while let Some(JsonValue(value)) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(Value::Array(values))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut mapping: A) -> Result<Value, A::Error> {
                let mut values = Map::new();
                while let Some(StringKey(key)) = mapping.next_key()? {
                    match values.entry(key) {
                        Entry::Vacant(entry) => {
                            let JsonValue(value) = mapping.next_value()?;
                            entry.insert(value);
                        }
                        Entry::Occupied(_) => {
                            return Err(de::Error::custom("duplicate mapping key"));
                        }
                    }
                }
                Ok(Value::Object(values))
            }
        }

        deserializer.deserialize_any(JsonVisitor).map(Self)
    }
}

struct StringKey(String);

impl<'de> Deserialize<'de> for StringKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Infer keys exactly like values rather than asking for a coerced String.
        // Retain the resolved tag too: !!binary and !!timestamp can otherwise
        // deserialize as strings despite not being YAML string keys.
        let Tagged(value, tag) = Tagged::<Value>::deserialize(deserializer)?;
        match (value, tag.as_deref()) {
            (Value::String(key), None | Some("!" | "tag:yaml.org,2002:str")) => Ok(Self(key)),
            _ => Err(de::Error::custom("YAML mapping keys must be strings")),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn json_values_keep_types_order_and_literal_strings() {
        let value = from_str(
            "z: [true, False, 3, -4, 1.25, null]\na: {first: yes, second: no, third: on, fourth: off}\n\
             strings: ['42', 'true', '${HOME}', '$HOME']\n",
        )
        .unwrap();
        assert_eq!(
            value,
            json!({
                "z": [true, false, 3, -4, 1.25, null],
                "a": {"first": "yes", "second": "no", "third": "on", "fourth": "off"},
                "strings": ["42", "true", "${HOME}", "$HOME"]
            })
        );
        assert_eq!(
            value
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["z", "a", "strings"]
        );
        assert_eq!(from_str("").unwrap(), Value::Null);
        assert_eq!(from_str("# comment only\n").unwrap(), Value::Null);
    }

    #[test]
    fn mapping_keys_must_be_strings_at_every_depth() {
        for key in [
            "null",
            "~",
            "true",
            "FALSE",
            "42",
            "-4",
            "1.5",
            "0x10",
            "[a, b]",
            "{nested: key}",
            "!!int '42'",
            "!!bool 'true'",
            "!!null 'null'",
            "!!float '1.5'",
            "!!binary YXBwcm92ZV9hbGw=",
            "!!timestamp 2026-09-21",
            "1e999",
            ".nan",
            ".inf",
            "-.inf",
            "18446744073709551616",
        ] {
            let yaml = format!("outer: {{ {key}: value }}");
            assert!(from_str(&yaml).is_err(), "accepted non-string key: {yaml}");
        }
        assert!(from_str("?\n: value\n").is_err());
        assert_eq!(
            from_str("'null': 1\n'42': 2\n'': 3\n!!str true: 4\n").unwrap(),
            json!({"null": 1, "42": 2, "": 3, "true": 4})
        );
    }

    #[test]
    fn keys_cannot_collide_after_yaml_scalar_conversion() {
        for mapping in [
            "approve_all: false\n!!binary YXBwcm92ZV9hbGw=: true\n",
            "!!binary YXBwcm92ZV9hbGw=: true\napprove_all: false\n",
            "same: 1\n!!str same: 2\n",
            "same: 1\n!<tag:yaml.org,2002:str> same: 2\n",
            "same: 1\n! same: 2\n",
            "first: &key same\nsame: 1\n? *key\n: 2\n",
            "first: &key !!binary YXBwcm92ZV9hbGw=\napprove_all: false\n? *key\n: true\n",
        ] {
            let nested = mapping
                .lines()
                .map(|line| format!("  {line}\n"))
                .collect::<String>();
            for yaml in [
                mapping.to_owned(),
                format!("outer:\n{nested}"),
                format!("-\n{nested}"),
            ] {
                assert!(from_str(&yaml).is_err(), "accepted colliding keys: {yaml}");
            }
        }
        assert!(
            from_str("%TAG !s! tag:yaml.org,2002:\n---\n!s!str true: 1\n! 1e999: 2\n'1e999': 3\n")
                .is_err(),
            "distinct string tags must not bypass collision checks",
        );
        assert_eq!(
            from_str("%TAG !s! tag:yaml.org,2002:\n---\n!s!str true: 1\n! 1e999: 2\n").unwrap(),
            json!({"true": 1, "1e999": 2}),
        );
    }

    #[test]
    fn rejects_duplicates_tags_merge_keys_and_multiple_documents() {
        for yaml in [
            "same: 1\nsame: 2\n",
            "outer: {same: 1, same: 2}",
            "items: [{same: 1, same: 2}]",
            "extra: !custom value",
            "extra: !include /private/file",
            "base: &base {a: 1}\nother: {<<: *base}",
            "outer: {<<: {a: 1}}",
            "a: 1\n---\nb: 2\n",
        ] {
            assert!(from_str(yaml).is_err(), "accepted invalid YAML: {yaml}");
        }
        assert_eq!(
            from_str("a: &shared {value: 1}\nb: *shared\n").unwrap(),
            json!({"a": {"value": 1}, "b": {"value": 1}})
        );
    }

    #[test]
    fn rejects_non_finite_numbers_and_explicit_integer_overflow() {
        for scalar in [
            ".nan",
            ".inf",
            "-.inf",
            "1e999",
            "!!int 18446744073709551616",
            "!!int -9223372036854775809",
        ] {
            let yaml = format!("value: {scalar}");
            assert!(from_str(&yaml).is_err(), "accepted invalid number: {yaml}");
        }
        assert_eq!(
            from_str("min: -9223372036854775808\nmax: 18446744073709551615\n").unwrap(),
            json!({"min": i64::MIN, "max": u64::MAX})
        );
    }

    #[test]
    fn diagnostics_do_not_include_neighboring_secrets() {
        let error = from_str("secret: do-not-leak-this\nbroken: [}\n").unwrap_err();
        let message = error.to_string();
        assert!(!message.contains("do-not-leak-this"), "{message}");
        assert!(!message.contains("broken: [}"), "{message}");
    }
}
