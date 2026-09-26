//! `String` newtypes that only ever hold values their check accepted.

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
/// `as_str` and `From<Self> for String` come with it; add further derives
/// such as `Ord` as attributes.
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
            #[must_use]
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
            assert_eq!(
                Word::try_from(blank.to_owned()).unwrap_err().to_string(),
                "word must not be blank"
            );
            assert!(serde_json::from_value::<Word>(serde_json::json!(blank)).is_err());
        }
    }
}
