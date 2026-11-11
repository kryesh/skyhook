//! Enums spelled once: a unit enum's variants, shared by JSON, the session database's
//! dictionaries, `Display` and parsing ([`derive@NamedEnum`]), and the unit enum naming a
//! data enum's variants ([`derive@Kinds`]).

use std::{fmt, marker::PhantomData};

use serde::de::{self, Deserializer, Visitor};

pub use skyhook_macros::{Kinds, NamedEnum};

/// A unit enum whose variants are spelled once, as [`derive@NamedEnum`] reads them.
pub trait NamedEnum: Copy + 'static {
    const ALL: &'static [Self];
    const NAMES: &'static [&'static str];
    fn as_str(self) -> &'static str;
    fn parse(value: &str) -> Option<Self>;
}

/// Deserializes a [`NamedEnum`] from its spellings. Anything but a string is
/// refused with the spellings listed, which a derived enum cannot do once the
/// input is a JSON value.
pub fn deserialize<'de, T: NamedEnum, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    struct Names<T>(PhantomData<T>);

    impl<T: NamedEnum> Visitor<'_> for Names<T> {
        type Value = T;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "one of {}", T::NAMES.join(", "))
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<T, E> {
            T::parse(value).ok_or_else(|| E::unknown_variant(value, T::NAMES))
        }
    }

    deserializer.deserialize_str(Names(PhantomData))
}

/// Decodes a [`NamedEnum`] from a string node, refusing anything else with the spellings.
pub fn decode<T: NamedEnum>(
    node: &crate::decode::Node,
    cx: &mut crate::decode::Context,
) -> Option<T> {
    let names = || crate::decode::one_of(T::NAMES);
    let Some(value) = node.as_str() else {
        cx.expected(node, &format!("one of {}", names()));
        return None;
    };
    let refused = |cx: &mut crate::decode::Context| {
        cx.error(
            node,
            format_args!("unknown variant `{value}`, expected one of {}", names()),
        )
    };
    T::parse(value).or_else(|| {
        refused(cx);
        None
    })
}

/// A name outside an enum's spellings.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind} {value:?}; expected one of {expected}")]
pub struct UnknownName {
    kind: &'static str,
    value: String,
    expected: String,
}

impl UnknownName {
    pub fn of<T: NamedEnum>(kind: &'static str, value: &str) -> Self {
        Self {
            kind,
            value: value.to_owned(),
            expected: T::NAMES.join(", "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Kinds, NamedEnum};
    use serde::Serialize;

    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
    #[serde(rename_all = "snake_case")]
    enum Flavour {
        #[serde(alias = "original")]
        Plain,
        SaltAndVinegar,
        /// A spelling that differs from the variant.
        #[serde(rename = "salt+pepper")]
        SaltAndPepper,
    }

    #[test]
    fn one_spelling_serves_json_text_and_parsing() {
        assert_eq!(Flavour::NAMES, ["plain", "salt_and_vinegar", "salt+pepper"]);
        for flavour in Flavour::ALL {
            let name = flavour.as_str();
            assert_eq!(flavour.to_string(), name);
            assert_eq!(name.parse::<Flavour>().unwrap(), flavour);
            assert_eq!(serde_json::to_value(flavour).unwrap(), name);
            assert_eq!(
                serde_json::from_value::<Flavour>(serde_json::json!(name)).unwrap(),
                flavour
            );
        }
        assert_eq!("original".parse::<Flavour>().unwrap(), Flavour::Plain);
        // A value that is not a string still names the spellings.
        let error = serde_json::from_value::<Flavour>(serde_json::json!(true)).unwrap_err();
        assert!(
            (error.to_string()).ends_with("one of plain, salt_and_vinegar, salt+pepper"),
            "{error}"
        );
        for invalid in ["", "PLAIN", " plain", "SaltAndVinegar", "unknown"] {
            assert!(invalid.parse::<Flavour>().is_err(), "{invalid:?}");
        }
        assert_eq!(
            "x".parse::<Flavour>().unwrap_err().to_string(),
            "unknown Flavour \"x\"; expected one of plain, salt_and_vinegar, salt+pepper"
        );
    }

    /// Variants with error messages display those instead of their spellings.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error, NamedEnum)]
    enum Refusal {
        #[error("not today")]
        Declined,
    }

    #[test]
    fn error_messages_display_instead_of_spellings() {
        assert_eq!(Refusal::Declined.as_str(), "Declined");
        assert_eq!(Refusal::Declined.to_string(), "not today");
    }

    /// A data enum's kinds, spelled as given, whatever its generics, fields and other
    /// serde attributes, with no serde derive of its own.
    #[derive(Kinds)]
    #[kinds(name = ShapeKind, derive(Hash))]
    #[serde(bound(serialize = "T: Clone"), rename_all = "snake_case")]
    #[expect(dead_code, reason = "only its kinds are read")]
    enum Shape<T> {
        Point,
        Circle(T),
        Box { side: T },
    }

    #[test]
    fn kinds_name_each_variant() {
        let shapes = [Shape::Point, Shape::Circle(1), Shape::Box { side: 2 }];
        assert_eq!(shapes.map(|shape| shape.kind()), ShapeKind::ALL);
        assert_eq!(ShapeKind::NAMES, ["point", "circle", "box"]);
    }
}
