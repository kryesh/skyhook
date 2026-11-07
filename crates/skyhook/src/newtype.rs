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
        $crate::described($schema, &$crate::string_newtype!(@docs [] $(#[$($attr)*])*))
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
pub use string_newtype;

/// `schema`, described by `docs`, the lines of a type's doc comment, unless it has a
/// description of its own.
pub fn described(mut schema: schemars::Schema, docs: &[&str]) -> schemars::Schema {
    let description = reflow(&docs.join("\n"));
    if !description.is_empty() && schema.get("description").is_none() {
        schema.insert("description".to_owned(), description.into());
    }
    schema
}

/// Doc comment `text` as one reader-facing description: the lines a paragraph wraps
/// over are joined, while blank-line paragraph breaks, list items and fenced code keep
/// their lines.
pub fn reflow(text: &str) -> String {
    #[derive(PartialEq)]
    enum Last {
        Nothing,
        Prose,
        Fence,
    }
    let item = |line: &str| {
        let numbered = line.trim_start_matches(|c: char| c.is_ascii_digit());
        ["- ", "* ", "+ "]
            .iter()
            .any(|marker| line.starts_with(marker))
            || (numbered.len() < line.len()
                && (numbered.starts_with(". ") || numbered.starts_with(") ")))
    };
    let (mut out, mut last, mut paragraph, mut fenced) =
        (String::new(), Last::Nothing, false, false);
    for line in text.lines() {
        if fenced {
            out.push('\n');
            out.push_str(line);
            fenced = !line.trim_start().starts_with("```");
            last = Last::Fence;
            continue;
        }
        let line = line.trim();
        if line.is_empty() {
            paragraph = last != Last::Nothing;
            continue;
        }
        let fence = line.starts_with("```");
        out.push_str(match last {
            Last::Nothing => "",
            _ if paragraph => "\n\n",
            Last::Prose if !fence && !item(line) => " ",
            Last::Prose | Last::Fence => "\n",
        });
        out.push_str(line);
        (fenced, paragraph) = (fence, false);
        last = if fence { Last::Fence } else { Last::Prose };
    }
    out
}

string_newtype! {
    /// Text written for a reader
    pub struct Prose(Blank) = |text| nonblank("text", text),
        schema = schemars::json_schema!({"type": "string", "pattern": "\\S"});
}

/// An integer argument from `MIN` through `MAX`, a range its schema carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct Bounded<const MIN: u64, const MAX: u64>(u64);

impl<const MIN: u64, const MAX: u64> Bounded<MIN, MAX> {
    /// A constant within the range, checked at compile time.
    pub const fn new<const VALUE: u64>() -> Self {
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

    /// Wrapped lines join; paragraphs, list items and fenced code keep their breaks.
    #[test]
    fn doc_text_reflows_into_paragraphs() {
        let doc = "Markdown shown beside the option:\na short artifact.\n\nTwo kinds:\n- code,\n  wrapped\n1. a mockup\n```rust\nlet a = 1;\n  let b = 2;\n```\nAfter it.";
        assert_eq!(
            reflow(doc),
            "Markdown shown beside the option: a short artifact.\n\nTwo kinds:\n- code, wrapped\n1. a mockup\n```rust\nlet a = 1;\n  let b = 2;\n```\nAfter it."
        );
        assert_eq!(reflow("\n  One line.  \n\n"), "One line.");
    }

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
