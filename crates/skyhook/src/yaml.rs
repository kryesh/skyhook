//! YAML is an input format, not a second runtime value model. Each document is read into
//! a [`Node`] tree, every value placed where it is written, which configuration decodes;
//! keep that boundary explicit: no coerced mapping keys, lossy tags, merge keys, or
//! non-finite numbers. Parser budgets remain enabled, and diagnostics must not include
//! surrounding source text that may contain credentials.

use std::{fmt, path::Path, sync::Arc};

use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_saphyr::{DuplicateKeyPolicy, MergeKeyPolicy, Options, Spanned, Tagged};
use skyhook_base::decode::{Context, Decode, Diagnostics, Entry, Map, Node, Number, Source, Value};

fn options() -> Options {
    serde_saphyr::options! {
        strict_booleans: true,
        duplicate_keys: DuplicateKeyPolicy::Error,
        merge_keys: MergeKeyPolicy::Error,
        reject_unsupported_tags: true,
        with_snippet: false,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum YamlError {
    #[error("invalid YAML: {0}")]
    Syntax(#[from] serde_saphyr::Error),
    #[error("{0}")]
    Invalid(#[from] Diagnostics),
    #[error(transparent)]
    Value(#[from] serde_json::Error),
    #[error(transparent)]
    Emit(#[from] serde_saphyr::ser::Error),
}

/// A document's values, each placed where `file` writes it.
pub fn read(text: &str, file: Option<Arc<Path>>) -> Result<Node, serde_saphyr::Error> {
    let mut node = serde_saphyr::from_str_with_options::<Spanned<Read>>(text, options())
        .map(|spanned| placed(spanned, None))?;
    if file.is_some() {
        filed(&mut node, &file);
    }
    Ok(node)
}

fn filed(node: &mut Node, file: &Option<Arc<Path>>) {
    node.at.file.clone_from(file);
    match &mut node.value {
        Value::Seq(items) => items.iter_mut().for_each(|item| filed(item, file)),
        Value::Map(map) => {
            for entry in map.iter_mut() {
                entry.at.file.clone_from(file);
                filed(&mut entry.value, file);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Str(_) => {}
    }
}

/// A document's values as JSON, for YAML that is not configuration.
pub fn from_str(text: &str) -> Result<serde_json::Value, serde_saphyr::Error> {
    read(text, None).map(|node| skyhook_base::decode::json(&node))
}

/// A document held as text, decoded as configuration is.
pub fn parse<T: Decode>(text: &str) -> Result<T, YamlError> {
    let node = read(text, None)?;
    let mut cx = Context::default();
    let value = T::decode(&node, &mut cx);
    Ok(cx.finish(value)?)
}

/// Preserve the serialized JSON shape, including explicit `null` values.
pub fn to_string(value: &impl serde::Serialize) -> Result<String, YamlError> {
    Ok(serde_saphyr::to_string(&serde_json::to_value(value)?)?)
}

/// The fields `value` serializes as.
pub(crate) fn object(value: &impl serde::Serialize) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::to_value(value).expect("configuration serializes") {
        serde_json::Value::Object(fields) => fields,
        _ => unreachable!("configuration fields serialize as a mapping"),
    }
}

/// `spanned` placed where it is written; serde's buffering loses a span, so such a value
/// is placed where `enclosing` is.
fn placed(spanned: Spanned<Read>, enclosing: Option<&Source>) -> Node {
    let location = Some(spanned.referenced).filter(|location| location.line() > 0);
    let at = match (location, enclosing) {
        (Some(location), _) => Source {
            file: None,
            line: location.line(),
            column: location.column(),
        },
        (None, Some(enclosing)) => enclosing.clone(),
        (None, None) => Source::START,
    };
    Node {
        value: spanned.value.0,
        at,
    }
}

// serde_json's ordinary visitor requests String keys, allowing the YAML
// deserializer to coerce non-string scalars before they can be checked. This
// boundary visitor builds the same JSON values but validates keys before insertion.
struct Read(Value);

impl<'de> Deserialize<'de> for Read {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Values;

        impl<'de> Visitor<'de> for Values {
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
                Ok(match u64::try_from(value) {
                    Ok(value) => Value::Number(Number::Unsigned(value)),
                    Err(_) => Value::Number(Number::Signed(value)),
                })
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
                Ok(Value::Number(Number::Unsigned(value)))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
                match value.is_finite() {
                    true => Ok(Value::Number(Number::Float(value))),
                    false => Err(E::custom("YAML numbers must be finite")),
                }
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
                self.visit_string(value.to_owned())
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
                Ok(Value::Str(value))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = sequence.next_element()? {
                    items.push(placed(item, None));
                }
                Ok(Value::Seq(items))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut mapping: A) -> Result<Value, A::Error> {
                let mut map = Map::default();
                while let Some(key) = mapping.next_key::<Spanned<StringKey>>()? {
                    if map.contains(&key.value.0) {
                        return Err(de::Error::custom("duplicate mapping key"));
                    }
                    let at = Source {
                        file: None,
                        line: key.referenced.line(),
                        column: key.referenced.column(),
                    };
                    let value = placed(mapping.next_value()?, Some(&at));
                    map.insert(Entry {
                        key: key.value.0,
                        at,
                        value,
                    });
                }
                Ok(Value::Map(map))
            }
        }

        deserializer.deserialize_any(Values).map(Self)
    }
}

struct StringKey(String);

impl<'de> Deserialize<'de> for StringKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Infer keys exactly like values rather than asking for a coerced String.
        // Retain the resolved tag too: !!binary and !!timestamp can otherwise
        // deserialize as strings despite not being YAML string keys.
        let Tagged(value, tag) = Tagged::<serde_json::Value>::deserialize(deserializer)?;
        match (value, tag.as_deref()) {
            (serde_json::Value::String(key), None | Some("!" | "tag:yaml.org,2002:str")) => {
                Ok(Self(key))
            }
            _ => Err(de::Error::custom("YAML mapping keys must be strings")),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn values_are_placed_where_their_file_writes_them() {
        let file: Arc<Path> = Path::new("/c.yaml").into();
        let node = read(
            "a:\n  b: [1, x]\nshared: &s {d: 1}\nalias: *s\n",
            Some(file),
        )
        .unwrap();
        let at = |path: &[&str]| {
            let mut node = &node;
            for key in path {
                node = match key.parse::<usize>() {
                    Ok(index) => match &node.value {
                        Value::Seq(items) => &items[index],
                        _ => unreachable!(),
                    },
                    Err(_) => node.as_map().unwrap().get(key).unwrap(),
                };
            }
            node.at.to_string()
        };
        assert_eq!(at(&["a", "b", "1"]), "/c.yaml:2:10");
        assert_eq!(at(&["a", "b"]), "/c.yaml:2:6");
        // An alias is placed where it is used, not where its anchor is defined.
        assert_eq!(at(&["alias"]), "/c.yaml:4:8");
        let key = &node.as_map().unwrap().entry("shared").unwrap().at;
        assert_eq!(key.to_string(), "/c.yaml:3:1");
    }

    #[test]
    fn serialization_preserves_nulls_and_respects_declared_omission() {
        #[derive(serde::Serialize)]
        struct Written {
            #[serde(skip_serializing_if = "Option::is_none")]
            omitted: Option<bool>,
            clear: Option<bool>,
            nested: serde_json::Value,
        }
        let written = Written {
            omitted: None,
            clear: None,
            nested: json!({"routing": {"zdr": null}, "items": [null, {"key": null}]}),
        };
        assert_eq!(
            from_str(&to_string(&written).unwrap()).unwrap(),
            json!({"clear": null, "nested": written.nested})
        );
    }

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
        assert_eq!(from_str("").unwrap(), serde_json::Value::Null);
        assert_eq!(
            from_str("# comment only\n").unwrap(),
            serde_json::Value::Null
        );
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
