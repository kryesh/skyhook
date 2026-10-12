//! Flat model declarations. Partial settings retain inheritance until admission.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value};

use super::{ModelError, Placements};
use crate::provider::{
    codec::{Codec, CodecName, CyberAccessProgram, ServiceTier, responses::Cache},
    profile::ModelProfile,
    settings::Patch,
};

crate::provider::settings::settings! {
    /// The request settings every dialect shares.
    pub struct Request => RequestPatch {
        @flatten { pub placements: Placements }
        pub codec: CodecName => required,
        /// Responses only. The wire tier ID, sent without client aliases.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub service_tier: Option<ServiceTier> => default,
        /// Responses only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub access_programs: Option<AccessPrograms> => object(AccessPrograms),
        /// Responses only. Mark stable-prefix checkpoints instead of leaving caching to
        /// the backend.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub explicit_caching: Option<bool> => default,
    }
}

crate::provider::settings::settings! {
    #[derive(Default)]
    pub struct AccessPrograms => AccessProgramsPatch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cyber: Option<CyberAccessProgram> => default,
    }
}

crate::named_enum::named_enum! {
    /// A setting only the Responses codec can send.
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum ResponseSetting {
        ServiceTier = "service_tier",
        AccessPrograms = "access_programs",
        ExplicitCaching = "explicit_caching",
    }
}

impl Request {
    /// The codec with the shared selections applied, or the first one it cannot send.
    pub(crate) fn apply(&self, codec: Codec) -> Result<Codec, ModelError> {
        let mut codec = self.placements.apply(codec)?;
        let cyber = self
            .access_programs
            .as_ref()
            .and_then(|programs| programs.cyber);
        let cache = match self.explicit_caching {
            Some(true) => Cache::Explicit,
            Some(false) | None => Cache::Automatic,
        };
        if let Codec::Responses(dialect) = &mut codec {
            dialect.service_tier = self.service_tier.clone();
            dialect.cyber_access_program = cyber;
            dialect.cache = cache;
        } else if let Some(setting) = [
            (ResponseSetting::ServiceTier, self.service_tier.is_some()),
            (ResponseSetting::AccessPrograms, cyber.is_some()),
            (ResponseSetting::ExplicitCaching, cache == Cache::Explicit),
        ]
        .into_iter()
        .find_map(|(setting, selected)| selected.then_some(setting))
        {
            return Err(ModelError::ResponsesOnly(setting));
        }
        Ok(codec)
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
        if fields.contains_key("overrides") {
            return Err(de::Error::custom(
                "overrides was removed; put its settings directly on the model",
            ));
        }
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
        let profile = crate::yaml::split(deserializer, &mut Fields(&mut fields))?;
        let settings =
            RequestSettings::deserialize(Value::Object(fields)).map_err(de::Error::custom)?;
        Ok(Self { profile, settings })
    }
}

pub(crate) struct Fields<'a>(pub &'a mut Map<String, Value>);

impl<'de> crate::yaml::Rest<'de> for Fields<'_> {
    fn read<A: de::MapAccess<'de>>(&mut self, key: String, map: &mut A) -> Result<(), A::Error> {
        self.0.insert(key, map.next_value()?);
        Ok(())
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
    pub(crate) fn validate(&self, codec: &Codec) -> Result<(), ModelError> {
        self.profile.validate_limits()?;
        super::placements::check(codec)?;
        let levels = codec.effort().levels;
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
    use crate::provider::dialect::compatible::base;

    fn apply(text: &str) -> Result<Codec, ModelError> {
        let request: Request = crate::yaml::parse(text).unwrap();
        request.apply(base(request.codec))
    }

    #[test]
    fn responses_selections_default_to_unset_and_reach_the_codec() {
        let Codec::Responses(unset) = apply("codec: responses").unwrap() else {
            panic!("Responses fixture");
        };
        assert_eq!(
            (unset.service_tier, unset.cyber_access_program, unset.cache),
            (None, None, Cache::Automatic)
        );
        let Codec::Responses(selected) = apply(
            "codec: responses\nservice_tier: priority\naccess_programs: {cyber: daybreak_blue}\nexplicit_caching: true",
        )
        .unwrap() else {
            panic!("Responses fixture");
        };
        assert_eq!(selected.service_tier, Some("priority".parse().unwrap()));
        assert_eq!(
            selected.cyber_access_program,
            Some(CyberAccessProgram::DaybreakBlue)
        );
        assert_eq!(selected.cache, Cache::Explicit);
    }

    #[test]
    fn other_codecs_refuse_effective_responses_selections_but_accept_empty_ones() {
        for codec in ["chat_completions", "messages"] {
            for (selection, setting) in [
                ("service_tier: fast", ResponseSetting::ServiceTier),
                (
                    "access_programs: {cyber: standard}",
                    ResponseSetting::AccessPrograms,
                ),
                ("explicit_caching: true", ResponseSetting::ExplicitCaching),
            ] {
                assert_eq!(
                    apply(&format!("codec: {codec}\n{selection}")).unwrap_err(),
                    ModelError::ResponsesOnly(setting)
                );
            }
            assert!(
                apply(&format!(
                    "codec: {codec}\naccess_programs: {{}}\nexplicit_caching: false"
                ))
                .is_ok()
            );
        }
    }

    #[test]
    fn responses_selections_are_parsed_at_the_boundary() {
        for refused in [
            "service_tier: '  '",
            "service_tier: true",
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
