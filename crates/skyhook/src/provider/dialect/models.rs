//! Flat model declarations. Partial settings retain inheritance until admission.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value, json};

use super::{ModelError, Placements, Profile};
use crate::{
    media::{ImageBytes, ImageEdge, ImageLimits},
    newtype::{Blank, nonblank, string_newtype},
    provider::{
        codec::{Cache, Codec, CodecName, CyberAccessProgram, ReasoningSummary, path},
        profile::ModelProfile,
        settings::{Nullable, Patch},
    },
};

string_newtype! {
    /// A service tier's wire ID. Catalogs may advertise tiers unknown to this client.
    pub struct ServiceTier(Blank) = |value| nonblank("service_tier", value);
}

crate::provider::settings::settings! {
    /// The request settings every dialect shares.
    pub struct Request => RequestPatch {
        @flatten { pub placements: Placements }
        pub codec: CodecName => required,
        /// The wire tier ID, sent without client aliases.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub service_tier: Option<ServiceTier> => default,
        /// Responses only: the summary detail requested in place of the dialect's
        /// default; null requests none.
        #[serde(default, skip_serializing_if = "Nullable::is_default")]
        pub reasoning_summary: Nullable<ReasoningSummary> => default,
        /// Responses only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub access_programs: Option<AccessPrograms> => object(AccessPrograms),
        /// Chat and Responses: cache only at marked stable-prefix breakpoints instead of
        /// leaving placement to the backend.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub explicit_caching: Option<bool> => default,
        /// What the model accepts of each image, in place of the dialect's limits.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub image_limits: Option<ImageLimitSettings> => object(ImageLimitSettings),
    }
}

crate::provider::settings::settings! {
    #[derive(Default)]
    pub struct AccessPrograms => AccessProgramsPatch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cyber: Option<CyberAccessProgram> => default,
    }
}

crate::provider::settings::settings! {
    /// Per-image limits; a limit left out keeps the dialect's.
    #[derive(Default)]
    pub struct ImageLimitSettings => ImageLimitSettingsPatch {
        /// The longest edge in pixels.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_edge: Option<ImageEdge> => optional(ImageEdge),
        /// The encoded size in bytes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_bytes: Option<ImageBytes> => optional(ImageBytes),
    }
}

crate::named_enum::named_enum! {
    /// A setting only the Responses codec can send.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum ResponseSetting {
        ReasoningSummary = "reasoning_summary",
        AccessPrograms = "access_programs",
    }
}

impl Request {
    /// The wire conventions with the shared selections applied, or the first one
    /// the codec cannot send.
    pub(crate) fn apply(&self, mut wire: Profile) -> Result<Profile, ModelError> {
        wire.codec = self.placements.apply(wire.codec)?;
        if self.explicit_caching == Some(true) {
            *wire.codec.cache_mut().ok_or(ModelError::ExplicitCaching)? = Cache::Explicit;
        }
        if let Some(limits) = &self.image_limits {
            let images = ImageLimits {
                max_edge: limits.max_edge.unwrap_or(wire.images.max_edge),
                max_bytes: limits.max_bytes.unwrap_or(wire.images.max_bytes),
            };
            // Sealed replay is bound to the images sent with it, which other limits refit.
            if images != wire.images {
                wire.discriminate("image_limits", &images);
                wire.images = images;
            }
        }
        if let Some(tier) = &self.service_tier {
            let field = (const { path("service_tier") }, json!(tier));
            wire.transport.fields.every.push(field);
        }
        let cyber = self
            .access_programs
            .as_ref()
            .and_then(|programs| programs.cyber);
        if let Codec::Responses(dialect) = &mut wire.codec {
            dialect.reasoning_summary = self.reasoning_summary.or(dialect.reasoning_summary);
            dialect.cyber_access_program = cyber;
        } else if let Some(setting) = [
            (
                ResponseSetting::ReasoningSummary,
                matches!(self.reasoning_summary, Nullable::Value(_)),
            ),
            (ResponseSetting::AccessPrograms, cyber.is_some()),
        ]
        .into_iter()
        .find_map(|(setting, selected)| selected.then_some(setting))
        {
            return Err(ModelError::ResponsesOnly(setting));
        }
        Ok(wire)
    }
}

/// Request settings as a provider or model declares them.
#[derive(Clone, Debug, Default)]
pub struct RequestSettings<P> {
    pub request: RequestPatch,
    pub dialect: P,
}

impl<P: Patch> RequestSettings<P> {
    pub(crate) fn overlay(&self, model: &Self) -> Self {
        Self {
            request: self.request.overlay(&model.request),
            dialect: self.dialect.overlay(&model.dialect),
        }
    }

    pub(crate) fn fields(&self) -> Map<String, Value> {
        let mut fields = object(&self.dialect);
        fields.extend(object(&self.request));
        fields
    }
}

pub(crate) fn object(value: &impl Serialize) -> Map<String, Value> {
    match serde_json::to_value(value).expect("configuration serializes") {
        Value::Object(fields) => fields,
        _ => unreachable!("configuration fields serialize as a mapping"),
    }
}

impl<'de, P: Patch> Deserialize<'de> for RequestSettings<P> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        fn parse<T: Patch, E: de::Error>(fields: Map<String, Value>) -> Result<T, E> {
            serde_path_to_error::deserialize(Value::Object(fields)).map_err(E::custom)
        }
        let fields = Map::<String, Value>::deserialize(deserializer)?;
        let (request, dialect) = fields
            .into_iter()
            .partition(|(key, _)| RequestPatch::FIELDS.contains(&key.as_str()));
        Ok(Self {
            request: parse(request)?,
            dialect: parse(dialect)?,
        })
    }
}

impl<P: Patch> Serialize for RequestSettings<P> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.fields().serialize(serializer)
    }
}

#[derive(Clone, Debug)]
pub struct ModelSpec<P> {
    pub profile: ModelProfile,
    pub settings: RequestSettings<P>,
}

impl<'de, P: Patch> Deserialize<'de> for ModelSpec<P> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut fields = Map::new();
        let profile = crate::yaml::split(deserializer, &mut fields)?;
        let settings =
            RequestSettings::deserialize(Value::Object(fields)).map_err(de::Error::custom)?;
        Ok(Self { profile, settings })
    }
}

impl<P: Patch> Serialize for ModelSpec<P> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = object(&self.profile);
        // Omission here is config-only: the shared profile's JSON contract is unchanged.
        fields.retain(|_, value| !value.is_null());
        fields.extend(self.settings.fields());
        fields.serialize(serializer)
    }
}

impl<P> ModelSpec<P> {
    pub(crate) fn validate(&self, wire: &Profile) -> Result<(), ModelError> {
        self.profile.validate_limits()?;
        super::placements::check(&wire.codec, &wire.transport.fields)?;
        let levels = wire.codec.name().effort_levels();
        if let Some(effort) = &self.profile.reasoning
            && !levels.accepts(effort)
        {
            return Err(ModelError::Reasoning {
                effort: effort.clone(),
                levels,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::dialect::{DialectConfig, compatible};

    fn apply(text: &str) -> Result<Profile, ModelError> {
        let request: Request = crate::yaml::parse(text).unwrap();
        request.apply(compatible::Config::default().admit(request.codec).unwrap())
    }

    #[test]
    fn codecs_refuse_selections_they_cannot_send_but_accept_empty_ones() {
        for codec in ["chat_completions", "messages"] {
            for (selection, setting) in [
                ("reasoning_summary: auto", ResponseSetting::ReasoningSummary),
                (
                    "access_programs: {cyber: standard}",
                    ResponseSetting::AccessPrograms,
                ),
            ] {
                assert_eq!(
                    apply(&format!("codec: {codec}\n{selection}")).unwrap_err(),
                    ModelError::ResponsesOnly(setting)
                );
            }
            assert!(
                apply(&format!(
                    "codec: {codec}\nreasoning_summary: null\naccess_programs: {{}}\nexplicit_caching: false"
                ))
                .is_ok()
            );
        }
        assert_eq!(
            apply("codec: messages\nexplicit_caching: true").unwrap_err(),
            ModelError::ExplicitCaching
        );
    }

    #[test]
    fn responses_selections_are_parsed_at_the_boundary() {
        for refused in [
            "service_tier: '  '",
            "service_tier: true",
            "reasoning_summary: brief",
            "explicit_caching: 'true'",
            "access_programs: {cyber: daybreak-blue}",
            "access_programs: {unknown: standard}",
        ] {
            assert!(
                crate::yaml::parse::<Request>(&format!("codec: responses\n{refused}")).is_err(),
                "{refused}"
            );
        }
    }
}
