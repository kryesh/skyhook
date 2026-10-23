//! Newtypes that only ever hold values their check accepted.

/// A value that must carry text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0} must not be blank")]
pub struct Blank(pub &'static str);

/// Rejects whitespace-only `value`, naming it `what` in the error.
pub(crate) fn nonblank(what: &'static str, value: &str) -> Result<(), Blank> {
    if value.trim().is_empty() {
        Err(Blank(what))
    } else {
        Ok(())
    }
}

/// Declare a `String` newtype whose values passed `$check`, a `fn(&str) ->
/// Result<(), $error>`. Serde, `TryFrom<String>`, `FromStr`, `Display`,
/// `as_str` and `From<Self> for String` come with it; add further derives such
/// as `Ord` as attributes.
macro_rules! string_newtype {
    (
        $(#[$attr:meta])*
        $vis:vis struct $name:ident($error:ty) = $check:expr;
    ) => {
        $(#[$attr])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, ::serde::Deserialize, ::serde::Serialize)]
        #[serde(try_from = "String", into = "String")]
        $vis struct $name(String);

        impl $name {
            $vis fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl ::std::convert::TryFrom<String> for $name {
            type Error = $error;

            fn try_from(value: String) -> Result<Self, $error> {
                let check: fn(&str) -> Result<(), $error> = $check;
                check(&value)?;
                Ok(Self(value))
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $error;

            fn from_str(value: &str) -> Result<Self, $error> {
                Self::try_from(value.to_owned())
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl ::std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}
pub(crate) use string_newtype;

string_newtype! {
    /// Text written for a reader, such as instructions or a hint: never blank.
    pub struct Prose(Blank) = |text| nonblank("text", text);
}

/// An integer argument from `MIN` through `MAX`, a range its schema carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct Bounded<const MIN: u64, const MAX: u64>(u64);

impl<const MIN: u64, const MAX: u64> Bounded<MIN, MAX> {
    /// A constant within the range, checked at compile time.
    pub(crate) const fn new<const VALUE: u64>() -> Self {
        const { assert!(MIN <= VALUE && VALUE <= MAX) };
        Self(VALUE)
    }
}

impl<const MIN: u64, const MAX: u64> TryFrom<u64> for Bounded<MIN, MAX> {
    type Error = String;

    fn try_from(value: u64) -> Result<Self, String> {
        (MIN..=MAX)
            .contains(&value)
            .then_some(Self(value))
            .ok_or_else(|| format!("must be {MIN} through {MAX}"))
    }
}

impl<const MIN: u64, const MAX: u64> From<Bounded<MIN, MAX>> for u64 {
    fn from(value: Bounded<MIN, MAX>) -> Self {
        value.0
    }
}

impl<const MIN: u64, const MAX: u64> schemars::JsonSchema for Bounded<MIN, MAX> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Bounded".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "integer",
            "format": "uint64",
            "minimum": MIN,
            "maximum": MAX,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    string_newtype! {
        /// A word that is not empty.
        #[derive(PartialOrd, Ord)]
        struct Word(Blank) = |word| nonblank("word", word);
    }

    #[test]
    fn checked_once_for_text_json_and_parsing() {
        let word = "hello".parse::<Word>().unwrap();
        assert_eq!(
            (word.as_str(), word.to_string().as_str()),
            ("hello", "hello")
        );
        assert_eq!(serde_json::to_value(&word).unwrap(), "hello");
        assert_eq!(
            serde_json::from_value::<Word>(serde_json::json!("hello")).unwrap(),
            word
        );
        assert_eq!(String::from(word), "hello");
        for blank in ["", " \n"] {
            assert_eq!(blank.parse::<Word>().unwrap_err(), Blank("word"));
            assert!(serde_json::from_value::<Word>(serde_json::json!(blank)).is_err());
        }
    }
}
