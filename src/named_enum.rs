//! One spelling per unit-enum variant, shared by JSON, the session database's
//! dictionaries, `Display` and parsing.

use std::{fmt, marker::PhantomData};

use serde::de::{self, Deserializer, Visitor};

/// A unit enum whose variants are spelled once, as `named_enum!` declares them.
pub(crate) trait NamedEnum: Copy + 'static {
    const ALL: &'static [Self];
    const NAMES: &'static [&'static str];
    fn as_str(self) -> &'static str;
    fn parse(value: &str) -> Option<Self>;
}

/// Deserializes a [`NamedEnum`] from its spellings. Anything but a string is
/// refused with the spellings listed, which a derived enum cannot do once the
/// input is a JSON value.
pub(crate) fn deserialize<'de, T: NamedEnum, D: Deserializer<'de>>(
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

/// A name outside an enum's spellings.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind} {value:?}; expected one of {expected}")]
pub struct UnknownName {
    kind: &'static str,
    value: String,
    expected: String,
}

impl UnknownName {
    pub(crate) fn of<T: NamedEnum>(kind: &'static str, value: &str) -> Self {
        let expected: Vec<_> = T::ALL.iter().map(|variant| variant.as_str()).collect();
        Self {
            kind,
            value: value.to_owned(),
            expected: expected.join(", "),
        }
    }
}

macro_rules! count {
    () => { 0usize };
    ($head:tt $($tail:tt)*) => { 1usize + $crate::named_enum::count!($($tail)*) };
}
pub(crate) use count;

/// Declare a unit enum with one spelling per variant: the serde name, the SQL
/// dictionary key, `as_str`, `FromStr` and `Display` all use it. Further
/// spellings after `|` parse and deserialize to the variant but are never
/// emitted. The enum must derive `Serialize`; `Deserialize` comes with it. A
/// `parsed enum` is only parsed and has neither. An `error enum` displays its
/// own messages instead of its spellings.
macro_rules! named_enum {
    ($(#[$attr:meta])* $vis:vis parsed enum $($rest:tt)*) => {
        $crate::named_enum::named_enum!(@any all $(#[$attr])* $vis enum $($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis error enum $($rest:tt)*) => {
        $crate::named_enum::named_enum!(@all any $(#[$attr])* $vis enum $($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis enum $($rest:tt)*) => {
        $crate::named_enum::named_enum!(@all all $(#[$attr])* $vis enum $($rest)*);
    };
    (
        @$serde:ident $display:ident $(#[$attr:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vattr:meta])* $variant:ident = $text:literal $(| $alias:literal)* ),+ $(,)?
        }
    ) => {
        $(#[$attr])*
        $vis enum $name {
            $( $(#[$vattr])* #[cfg_attr($serde(), serde(rename = $text))] $variant ),+
        }

        #[cfg($serde())]
        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                $crate::named_enum::deserialize(deserializer)
            }
        }

        impl $name {
            pub const ALL: [Self; $crate::named_enum::count!($($variant)+)] = [$(Self::$variant),+];

            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
        }

        impl $crate::named_enum::NamedEnum for $name {
            const ALL: &'static [Self] = &Self::ALL;
            const NAMES: &'static [&'static str] = &[$($text),+];

            fn as_str(self) -> &'static str {
                Self::as_str(self)
            }

            fn parse(value: &str) -> Option<Self> {
                match value {
                    $($text $(| $alias)* => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::named_enum::UnknownName;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                <Self as $crate::named_enum::NamedEnum>::parse(value)
                    .ok_or_else(|| $crate::named_enum::UnknownName::of::<Self>(stringify!($name), value))
            }
        }

        #[cfg($display())]
        impl ::std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}
pub(crate) use named_enum;

/// A [`detailed_enum`] variant's payload, journaled beside its kind as text.
pub(crate) trait Detail: Sized {
    fn text(&self) -> &str;
    fn parse(text: String) -> Option<Self>;
}

impl Detail for String {
    fn text(&self) -> &str {
        self
    }

    fn parse(text: String) -> Option<Self> {
        Some(text)
    }
}

impl<T: NamedEnum> Detail for T {
    fn text(&self) -> &str {
        NamedEnum::as_str(*self)
    }

    fn parse(text: String) -> Option<Self> {
        <T as NamedEnum>::parse(&text)
    }
}

/// Declare an enum whose variants each carry at most one [`Detail`], naming its
/// kinds with a `named_enum` (`Enum / Kind`): `kind` and `parts` split a value
/// into what the journal keeps, and `from_parts` rejoins them. A detail may be
/// followed by a [`NamedEnum`] class, journaled in a column of its own. A
/// variant's serde name is its kind's, so the enum must derive `Serialize`.
macro_rules! detailed_enum {
    (@detailed) => { false };
    (@detailed $detail:ty) => { true };
    (@bind $value:ident $detail:ty) => { $value };
    (@text) => { None };
    (@text $value:ident: $detail:ty) => {
        Some(<$detail as $crate::named_enum::Detail>::text($value))
    };
    (@name) => { None };
    (@name $value:ident: $class:ty) => {
        Some(<$class as $crate::named_enum::NamedEnum>::as_str(*$value))
    };
    (@build $variant:path, $class:ident, $value:ident) => {
        ($class.is_none() && $value.is_none()).then_some($variant)
    };
    (@build $variant:path, $class:ident, $value:ident, $detail:ty) => {
        $value.filter(|_| $class.is_none())
            .and_then(<$detail as $crate::named_enum::Detail>::parse)
            .map($variant)
    };
    (@build $variant:path, $class:ident, $value:ident, $detail:ty, $named:ty) => {
        $value.and_then(<$detail as $crate::named_enum::Detail>::parse)
            .zip($class.as_deref().and_then(<$named as $crate::named_enum::NamedEnum>::parse))
            .map(|(detail, class)| $variant(detail, class))
    };
    (
        $(#[$attr:meta])*
        $vis:vis enum $name:ident / $kind:ident {
            $( $(#[$vattr:meta])* $variant:ident $(($detail:ty $(, $class:ty)?))? = $text:literal ),+ $(,)?
        }
    ) => {
        $(#[$attr])*
        $vis enum $name {
            $( $(#[$vattr])* #[serde(rename = $text)] $variant $(($detail $(, $class)?))? ),+
        }

        $crate::named_enum::named_enum! {
            #[doc = concat!("A [`", stringify!($name), "`]'s kind, as the journal names it.")]
            #[derive(Clone, Copy, Debug, PartialEq, Eq, ::serde::Serialize)]
            $vis enum $kind { $( $variant = $text ),+ }
        }

        impl $kind {
            /// Whether a value of this kind carries a detail.
            pub(crate) const fn detailed(self) -> bool {
                match self {
                    $( Self::$variant => $crate::named_enum::detailed_enum!(@detailed $($detail)?) ),+
                }
            }
        }

        impl $name {
            pub(crate) const fn kind(&self) -> $kind {
                match self {
                    $( Self::$variant { .. } => $kind::$variant ),+
                }
            }

            /// The detail and the name of its class, as the journal keeps them.
            pub(crate) fn parts(&self) -> (Option<&str>, Option<&'static str>) {
                match self {
                    $(
                        Self::$variant $((
                            $crate::named_enum::detailed_enum!(@bind value $detail)
                            $(, $crate::named_enum::detailed_enum!(@bind class $class))?
                        ))? => (
                            $crate::named_enum::detailed_enum!(@text $(value: $detail)?),
                            $crate::named_enum::detailed_enum!(@name $($(class: $class)?)?),
                        )
                    ),+
                }
            }

            /// The value of `kind` with `class` and `detail`, when they agree.
            pub(crate) fn from_parts(
                kind: $kind,
                class: Option<String>,
                detail: Option<String>,
            ) -> Option<Self> {
                match kind {
                    $( $kind::$variant => $crate::named_enum::detailed_enum!(@build Self::$variant, class, detail $(, $detail $(, $class)?)?) ),+
                }
            }
        }
    };
}
pub(crate) use detailed_enum;

#[cfg(test)]
mod tests {
    use serde::Serialize;

    named_enum! {
        #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
        enum Flavour {
            Plain = "plain" | "original",
            /// A spelling that differs from the variant.
            SaltAndVinegar = "salt+vinegar",
        }
    }

    #[test]
    fn one_spelling_serves_json_text_and_parsing() {
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
        assert!(error.to_string().ends_with("one of plain, salt+vinegar"));
        for invalid in ["", "PLAIN", " plain", "plain,salt+vinegar", "unknown"] {
            assert!(invalid.parse::<Flavour>().is_err(), "{invalid:?}");
        }
        assert_eq!(
            "x".parse::<Flavour>().unwrap_err().to_string(),
            "unknown Flavour \"x\"; expected one of plain, salt+vinegar"
        );
    }
}
