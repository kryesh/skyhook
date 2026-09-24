//! One spelling per unit-enum variant, shared by JSON, the session database's
//! dictionaries, `Display` and parsing.

/// A unit enum whose variants are spelled once, as `named_enum!` declares them.
pub(crate) trait NamedEnum: Copy + 'static {
    const ALL: &'static [Self];
    fn as_str(self) -> &'static str;
    fn parse(value: &str) -> Option<Self>;
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
/// spellings after `|` parse to the variant but are never emitted. The enum
/// must derive `Serialize` and `Deserialize`.
macro_rules! named_enum {
    (
        $(#[$attr:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vattr:meta])* $variant:ident = $text:literal $(| $alias:literal)* ),+ $(,)?
        }
    ) => {
        $(#[$attr])*
        $vis enum $name {
            $( $(#[$vattr])* #[serde(rename = $text)] $variant ),+
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

        impl ::std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}
pub(crate) use named_enum;

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    named_enum! {
        #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
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
        for invalid in ["", "PLAIN", " plain", "plain,salt+vinegar", "unknown"] {
            assert!(invalid.parse::<Flavour>().is_err(), "{invalid:?}");
        }
        assert_eq!(
            "x".parse::<Flavour>().unwrap_err().to_string(),
            "unknown Flavour \"x\"; expected one of plain, salt+vinegar"
        );
    }
}
