//! Flat model declarations. Partial settings retain inheritance until admission.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value};

use super::{ModelError, Placements};
use crate::provider::{
    codec::{Codec, CodecName},
    profile::ModelProfile,
    settings::Patch,
};

crate::provider::settings::settings! {
    /// The request settings every dialect shares.
    pub struct Request => RequestPatch {
        @flatten { pub placements: Placements }
        pub codec: CodecName => required,
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
