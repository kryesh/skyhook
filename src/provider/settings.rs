//! Typed partial settings. Omission inherits; a present value replaces an
//! atomic field, including explicit defaults and null on nullable fields.
//!
//! Every setting but a list reads one way: left out, its default applies; an explicit
//! null sends nothing; a value is used as given. A setting whose default is nothing is an
//! `Option`; one whose default is something is [`Nullable`]; one that cannot be nothing
//! is declared `optional`, which refuses null. A list refuses null: `[]` is its empty
//! value.

use std::fmt::Debug;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};

pub trait Settings: Sized {
    type Patch: Patch;

    fn resolve(patch: &Self::Patch) -> Result<Self, MissingSetting>;
}

pub trait Patch: Clone + Debug + Default + Serialize + DeserializeOwned {
    /// The serialized field names, flattened groups included.
    const FIELDS: &'static [&'static str];

    /// Apply the higher-precedence partial settings without resolving defaults.
    fn overlay(&self, higher: &Self) -> Self;
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("missing required setting `{name}`")]
pub struct MissingSetting {
    pub name: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Setting<T> {
    #[default]
    Inherit,
    Set(T),
}

impl<T> Setting<T> {
    pub fn is_inherit(&self) -> bool {
        matches!(self, Self::Inherit)
    }
}

impl<T: Clone> Setting<T> {
    pub fn overlay(&self, higher: &Self) -> Self {
        match higher {
            Self::Inherit => self.clone(),
            Self::Set(_) => higher.clone(),
        }
    }

    pub fn required(&self, name: &'static str) -> Result<T, MissingSetting> {
        match self {
            Self::Inherit => Err(MissingSetting { name }),
            Self::Set(value) => Ok(value.clone()),
        }
    }

    pub fn optional(&self) -> Option<T> {
        match self {
            Self::Inherit => None,
            Self::Set(value) => Some(value.clone()),
        }
    }

    pub fn or_default(&self) -> T
    where
        T: Default,
    {
        match self {
            Self::Inherit => T::default(),
            Self::Set(value) => value.clone(),
        }
    }
}

impl<P: Patch> Setting<Option<P>> {
    pub fn overlay_object(&self, higher: &Self) -> Self {
        match (self, higher) {
            (_, Self::Inherit) => self.clone(),
            (Self::Set(Some(lower)), Self::Set(Some(higher))) => {
                Self::Set(Some(lower.overlay(higher)))
            }
            (_, Self::Set(_)) => higher.clone(),
        }
    }

    pub fn resolve_object<S: Settings<Patch = P>>(&self) -> Result<Option<S>, MissingSetting> {
        match self {
            Self::Inherit | Self::Set(None) => Ok(None),
            Self::Set(Some(patch)) => S::resolve(patch).map(Some),
        }
    }
}

impl<T: Serialize> Serialize for Setting<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Set(value) => value.serialize(serializer),
            Self::Inherit => Err(serde::ser::Error::custom(
                "inherited settings must be omitted",
            )),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Setting<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::Set)
    }
}

/// A setting whose default is something that an explicit null withholds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Nullable<T> {
    #[default]
    Default,
    Null,
    Value(T),
}

impl<T> Nullable<T> {
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }

    /// The value sent: `default` unless another value or nothing is chosen.
    pub fn or(&self, default: Option<T>) -> Option<T>
    where
        T: Clone,
    {
        match self {
            Self::Default => default,
            Self::Null => None,
            Self::Value(value) => Some(value.clone()),
        }
    }
}

impl<T: Serialize> Serialize for Nullable<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Default => Err(serde::ser::Error::custom(
                "default settings must be omitted",
            )),
            Self::Null => serializer.serialize_none(),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Nullable<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Option::<T>::deserialize(deserializer)?.map_or(Self::Null, Self::Value))
    }
}

/// Join the field names of flattened settings without maintaining another list.
pub(crate) const fn fields<const N: usize>(groups: &[&[&'static str]]) -> [&'static str; N] {
    let mut fields = [""; N];
    let mut offset = 0;
    let mut group = 0;
    while group < groups.len() {
        let mut field = 0;
        while field < groups[group].len() {
            fields[offset] = groups[group][field];
            offset += 1;
            field += 1;
        }
        group += 1;
    }
    fields
}

/// Declare resolved settings and their presence-preserving patch together.
/// `required` has no default; `default` resolves only after overlaying layers.
/// `optional(T)` resolves an `Option<T>` that is `None` only when left out: null is
/// refused. `object(T)` merges a nullable nested settings object field by field. A
/// `@flatten` group shares another settings declaration's fields.
/// Field serde attributes describe the resolved form, never the patch.
macro_rules! settings {
    (
        $(#[$attr:meta])* $vis:vis struct $config:ident => $patch:ident {
            $(@flatten { $( $flat_vis:vis $flat:ident: $flat_ty:ty ),* $(,)? })?
            $( $(#[$field_attr:meta])* $field_vis:vis $field:ident: $ty:ty
                => $kind:ident $(($inner:ty))? ),* $(,)?
        }
    ) => {
        $(#[$attr])*
        #[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
        #[serde(deny_unknown_fields)]
        $vis struct $config {
            $($(#[serde(flatten)] $flat_vis $flat: $flat_ty,)*)?
            $($(#[$field_attr])* $field_vis $field: $ty,)*
        }

        #[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
        #[serde(deny_unknown_fields)]
        $vis struct $patch {
            $($(#[serde(flatten)] $flat_vis $flat: <$flat_ty as $crate::provider::settings::Settings>::Patch,)*)?
            $(
                #[serde(default, skip_serializing_if = "crate::provider::settings::Setting::is_inherit")]
                $field_vis $field: $crate::provider::settings::settings!(@type $ty, $kind $(($inner))?),
            )*
        }

        impl $crate::provider::settings::Patch for $patch {
            const FIELDS: &'static [&'static str] = &{
                $crate::provider::settings::fields::<{
                    0 $($(+ <<$flat_ty as $crate::provider::settings::Settings>::Patch as $crate::provider::settings::Patch>::FIELDS.len())*)?
                        $(+ { let _ = stringify!($field); 1 })*
                }>(&[
                    $($(<<$flat_ty as $crate::provider::settings::Settings>::Patch as $crate::provider::settings::Patch>::FIELDS,)*)?
                    &[$(stringify!($field),)*],
                ])
            };

            fn overlay(&self, _higher: &Self) -> Self {
                Self {
                    $($($flat: $crate::provider::settings::Patch::overlay(&self.$flat, &_higher.$flat),)*)?
                    $($field: $crate::provider::settings::settings!(@overlay self.$field, _higher.$field, $kind $(($inner))?),)*
                }
            }
        }

        impl $crate::provider::settings::Settings for $config {
            type Patch = $patch;

            fn resolve(_patch: &Self::Patch) -> Result<Self, $crate::provider::settings::MissingSetting> {
                Ok(Self {
                    $($($flat: <$flat_ty as $crate::provider::settings::Settings>::resolve(&_patch.$flat)?,)*)?
                    $($field: $crate::provider::settings::settings!(@resolve _patch.$field, $field, $kind $(($inner))?),)*
                })
            }
        }
    };
    (@type $ty:ty, object($inner:ty)) => {
        $crate::provider::settings::Setting<Option<<$inner as $crate::provider::settings::Settings>::Patch>>
    };
    (@type $ty:ty, optional($inner:ty)) => { $crate::provider::settings::Setting<$inner> };
    (@type $ty:ty, $kind:ident) => { $crate::provider::settings::Setting<$ty> };
    (@overlay $lower:expr, $higher:expr, object($inner:ty)) => { $lower.overlay_object(&$higher) };
    (@overlay $lower:expr, $higher:expr, $kind:ident $(($inner:ty))?) => { $lower.overlay(&$higher) };
    (@resolve $value:expr, $field:ident, required) => { $value.required(stringify!($field))? };
    (@resolve $value:expr, $field:ident, optional($inner:ty)) => { $value.optional() };
    (@resolve $value:expr, $field:ident, default) => { $value.or_default() };
    (@resolve $value:expr, $field:ident, object($inner:ty)) => { $value.resolve_object::<$inner>()? };
}

pub(crate) use settings;

settings! {
    /// Settings for a dialect with no request-specific fields.
    #[derive(Default)]
    pub struct Empty => EmptyPatch {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    settings! {
        struct Example => ExamplePatch {
            name: String => required,
            #[serde(default)]
            enabled: bool => default,
            #[serde(default)]
            tags: Vec<String> => default,
            #[serde(default)]
            tenant: Option<String> => default,
            #[serde(default, skip_serializing_if = "Nullable::is_default")]
            level: Nullable<String> => default,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            role: Option<String> => optional(String),
        }
    }

    /// Left out, a setting keeps its default; null sends nothing where nothing is a
    /// choice and is refused where it is not; a value replaces the default.
    #[test]
    fn left_out_keeps_the_default_null_sends_nothing_and_values_replace_it() {
        let read = |yaml: &str| Example::resolve(&crate::yaml::parse(yaml).unwrap()).unwrap();
        let left_out = read("name: n");
        assert_eq!(left_out.level.or(Some("auto".into())), Some("auto".into()));
        assert_eq!(left_out.role, None);
        let chosen = read("name: n\nlevel: detailed\nrole: user");
        assert_eq!(
            chosen.level.or(Some("auto".into())),
            Some("detailed".into())
        );
        assert_eq!(chosen.role.as_deref(), Some("user"));
        let null = read("name: n\nlevel: null");
        assert_eq!(null.level.or(Some("auto".into())), None);
        assert!(crate::yaml::parse::<ExamplePatch>("role: null").is_err());
        let lower: ExamplePatch = crate::yaml::parse("level: null").unwrap();
        let higher: ExamplePatch = crate::yaml::parse("level: concise").unwrap();
        let merged = Example::resolve(
            &lower
                .overlay(&higher)
                .overlay(&crate::yaml::parse("name: n").unwrap()),
        )
        .unwrap();
        assert_eq!(merged.level, Nullable::Value("concise".into()));
        for yaml in ["level: null\n", "level: detailed\nrole: user\n"] {
            let patch: ExamplePatch = crate::yaml::parse(yaml).unwrap();
            assert_eq!(crate::yaml::to_string(&patch).unwrap(), yaml);
        }
    }

    #[test]
    fn presence_survives_roundtrip_and_resolution_waits_for_overlay() {
        let lower: ExamplePatch = serde_json::from_value(json!({
            "name": "inherited", "enabled": true, "tags": ["old"], "tenant": "old"
        }))
        .unwrap();
        let explicit = json!({"enabled": false, "tags": [], "tenant": null});
        let higher: ExamplePatch = serde_json::from_value(explicit.clone()).unwrap();
        assert_eq!(serde_json::to_value(&higher).unwrap(), explicit);
        assert_eq!(Example::resolve(&higher).unwrap_err().name, "name");
        let merged = lower.overlay(&higher);
        let resolved = Example::resolve(&merged).unwrap();
        assert_eq!(resolved.name, "inherited");
        assert!(!resolved.enabled);
        assert!(resolved.tags.is_empty());
        assert_eq!(resolved.tenant, None);
        assert_eq!(merged.overlay(&ExamplePatch::default()), merged);
        assert_eq!(
            serde_json::to_value(ExamplePatch::default()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn null_requires_a_nullable_field_and_unknown_fields_are_rejected() {
        for input in [
            json!({"enabled": null}),
            json!({"tags": null}),
            json!({"name": null}),
            json!({"unknown": true}),
        ] {
            assert!(serde_json::from_value::<ExamplePatch>(input).is_err());
        }
        let yaml = "enabled: false\ntags: []\ntenant: null\n";
        let patch: ExamplePatch = crate::yaml::parse(yaml).unwrap();
        assert_eq!(crate::yaml::to_string(&patch).unwrap(), yaml);
    }

    #[test]
    fn empty_settings_are_an_empty_object_not_an_open_map() {
        let patch: EmptyPatch = serde_json::from_value(json!({})).unwrap();
        assert_eq!(Empty::resolve(&patch).unwrap(), Empty {});
        assert_eq!(serde_json::to_value(&patch).unwrap(), json!({}));
        assert!(serde_json::from_value::<EmptyPatch>(json!({"extra": 1})).is_err());
    }
}
