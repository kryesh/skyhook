//! Tool arguments deserialized with reasons that never quote them. serde's errors embed
//! the rejected value, which may be a credential or user content, so arguments are
//! read through a deserializer whose error keeps only what a type expected, what kind
//! of value it found, the field names involved, and a type's own explanation.

use serde::de::{
    self, Deserializer, Expected, IntoDeserializer, Unexpected, Visitor,
    value::{MapAccessDeserializer, MapDeserializer, SeqDeserializer},
};
use serde_json::{Map, Value};

use super::failure::argument_type;
use crate::diagnostic::quoted;

/// Why a value did not deserialize, without the value.
#[derive(Debug)]
pub(crate) enum Reason {
    Explained(String),
    /// A field its object's type does not take: a field name is no value, so it is named.
    UnknownField {
        field: String,
        expected: &'static [&'static str],
    },
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Explained(reason) => f.write_str(reason),
            Self::UnknownField {
                field,
                expected: [],
            } => {
                write!(f, "unknown field {}", quoted(field))
            }
            Self::UnknownField { field, expected } => {
                let field = quoted(field);
                write!(
                    f,
                    "unknown field {field}, expected one of {}",
                    one_of(expected)
                )
            }
        }
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
        Self::Explained(message.to_string())
    }

    fn invalid_type(unexpected: Unexpected<'_>, expected: &dyn Expected) -> Self {
        Self::Explained(format!("expected {expected}, found {}", kind(unexpected)))
    }

    fn invalid_value(unexpected: Unexpected<'_>, expected: &dyn Expected) -> Self {
        Self::Explained(format!("found {} that is not {expected}", kind(unexpected)))
    }

    fn invalid_length(length: usize, expected: &dyn Expected) -> Self {
        Self::Explained(format!("expected {expected}, found {length} elements"))
    }

    fn unknown_variant(_variant: &str, expected: &'static [&'static str]) -> Self {
        Self::Explained(format!(
            "unknown variant, expected one of {}",
            one_of(expected)
        ))
    }

    fn unknown_field(field: &str, expected: &'static [&'static str]) -> Self {
        Self::UnknownField {
            field: field.to_owned(),
            expected,
        }
    }

    fn missing_field(field: &'static str) -> Self {
        Self::Explained(format!("missing field `{field}`"))
    }

    fn duplicate_field(field: &'static str) -> Self {
        Self::Explained(format!("duplicate field `{field}`"))
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
