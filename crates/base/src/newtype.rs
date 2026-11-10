//! Newtypes that only ever hold values their check accepted.

/// A value that must carry text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0} must not be blank")]
pub struct Blank(pub &'static str);

/// Rejects whitespace-only `value`, naming it `what` in the error.
pub fn nonblank(what: &'static str, value: &str) -> Result<(), Blank> {
    if value.trim().is_empty() {
        Err(Blank(what))
    } else {
        Ok(())
    }
}

/// Declare a `String` newtype whose values passed `$check`, a `fn(&str) ->
/// Result<(), $error>`. Serde, a string schema, `TryFrom<String>`, `FromStr`,
/// `Display`, `as_str` and `From<Self> for String` come with it; add further derives such
/// as `Ord` as attributes. `schema = ...` states a narrower schema than a plain string, which
/// the type's doc comment describes unless the schema has a description of its own; it is one
/// definition, which the fields of the type refer to.
#[macro_export]
macro_rules! string_newtype {
    (@named $name:ident) => {
        (true, <String as ::schemars::JsonSchema>::schema_name(), <String as ::schemars::JsonSchema>::schema_id())
    };
    (@named $name:ident $schema:expr) => {
        (
            false,
            ::std::borrow::Cow::Borrowed(stringify!($name)),
            ::std::borrow::Cow::Borrowed(concat!(module_path!(), "::", stringify!($name))),
        )
    };
    (@schema $generator:ident $(#[$($attr:tt)*])*) => {
        <String as ::schemars::JsonSchema>::json_schema($generator)
    };
    (@schema $generator:ident $(#[$($attr:tt)*])* ; $schema:expr) => {
        $crate::schema::described($schema, &$crate::string_newtype!(@docs [] $(#[$($attr)*])*))
    };
    (@docs [$($docs:literal)*]) => {
        [$($docs),*]
    };
    (@docs [$($docs:literal)*] #[doc = $doc:literal] $($rest:tt)*) => {
        $crate::string_newtype!(@docs [$($docs)* $doc] $($rest)*)
    };
    (@docs [$($docs:literal)*] #[$($attr:tt)*] $($rest:tt)*) => {
        $crate::string_newtype!(@docs [$($docs)*] $($rest)*)
    };
    (
        $(#[$($attr:tt)*])*
        $vis:vis struct $name:ident($error:ty) = $check:expr $(, schema = $schema:expr)?;
    ) => {
        $(#[$($attr)*])*
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

            fn try_from(value: String) -> ::std::result::Result<Self, $error> {
                let check: fn(&str) -> ::std::result::Result<(), $error> = $check;
                check(&value)?;
                Ok(Self(value))
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $error;

            fn from_str(value: &str) -> ::std::result::Result<Self, $error> {
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

        impl ::schemars::JsonSchema for $name {
            fn inline_schema() -> bool {
                $crate::string_newtype!(@named $name $($schema)?).0
            }
            fn schema_name() -> ::std::borrow::Cow<'static, str> {
                $crate::string_newtype!(@named $name $($schema)?).1
            }
            fn schema_id() -> ::std::borrow::Cow<'static, str> {
                $crate::string_newtype!(@named $name $($schema)?).2
            }
            fn json_schema(_generator: &mut ::schemars::SchemaGenerator) -> ::schemars::Schema {
                $crate::string_newtype!(@schema _generator $(#[$($attr)*])* $(; $schema)?)
            }
        }
    };
}

string_newtype! {
    /// Text written for a reader
    pub struct Prose(Blank) = |text| nonblank("text", text),
        schema = schemars::json_schema!({"type": "string", "pattern": "\\S"});
}

#[cfg(test)]
mod tests {
    use super::*;

    string_newtype! {
        /// A word that is not empty:
        ///
        /// one token.
        #[derive(PartialOrd, Ord)]
        struct Word(Blank) = |word| nonblank("word", word),
            schema = schemars::json_schema!({"type": "string", "pattern": "\\S"});
    }

    /// A word is checked once however it arrives, and its narrower schema is described by its
    /// doc, reflowed as every description is.
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
        assert_eq!(
            schemars::schema_for!(Word).get("description"),
            Some(&serde_json::json!(
                "A word that is not empty:\n\none token."
            ))
        );
    }
}
