use std::{fmt, num::NonZeroU64, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{
    named_enum::named_enum,
    newtype::{Prose, string_newtype},
    provider::protocol::WireModel,
};

/// A configured key that also appears in a qualified model reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("{0} must not be blank")]
    Blank(&'static str),
    #[error("{0} must not contain '/'")]
    Slash(&'static str),
}

fn name(what: &'static str, value: &str) -> Result<(), NameError> {
    if value.trim().is_empty() {
        Err(NameError::Blank(what))
    } else if value.contains('/') {
        Err(NameError::Slash(what))
    } else {
        Ok(())
    }
}

string_newtype! {
    /// The key of a configured provider.
    #[derive(PartialOrd, Ord)]
    pub struct ProviderName(NameError) = |value| name("provider name", value);
}

string_newtype! {
    /// The key of a model under its provider; the wire model identifier is free-form.
    #[derive(PartialOrd, Ord)]
    pub struct ModelName(NameError) = |value| name("model name", value);
}

/// A configured model, named `provider/model` wherever a model is chosen or recorded.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModelRef {
    pub provider: ProviderName,
    pub model: ModelName,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModelRefError {
    #[error("a model is named provider/model")]
    Unqualified,
    #[error(transparent)]
    Name(#[from] NameError),
}

impl ModelRef {
    pub fn new(provider: ProviderName, model: ModelName) -> Self {
        Self { provider, model }
    }
}

impl FromStr for ModelRef {
    type Err = ModelRefError;

    fn from_str(value: &str) -> Result<Self, ModelRefError> {
        let (provider, model) = value.split_once('/').ok_or(ModelRefError::Unqualified)?;
        Ok(Self {
            provider: provider.parse()?,
            model: model.parse()?,
        })
    }
}

impl TryFrom<String> for ModelRef {
    type Error = ModelRefError;

    fn try_from(value: String) -> Result<Self, ModelRefError> {
        value.parse()
    }
}

impl From<ModelRef> for String {
    fn from(value: ModelRef) -> Self {
        value.to_string()
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    pub model: WireModel,
    pub reasoning: Option<String>,
    pub max_context: NonZeroU64,
    pub max_output: NonZeroU64,
    #[serde(default)]
    pub supports_images: bool,
    #[serde(default)]
    pub state_mode: StateMode,
    /// Describes the model to agents choosing one for a child; without it they cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<Prose>,
}

named_enum! {
    /// How per-request runtime state (date, jobs, todos) reaches the model.
    #[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
    pub enum StateMode {
        /// Never send runtime state.
        None = "none",
        /// Send current state after history; it never enters history.
        #[default]
        Dynamic = "dynamic",
        /// Commit each request's state to history, keeping history append-only for providers
        /// that bind signed reasoning to the exact earlier conversation.
        Persist = "persist",
    }
}

impl ModelProfile {
    pub fn new(
        model: WireModel,
        reasoning: Option<String>,
        max_context: NonZeroU64,
        max_output: NonZeroU64,
        supports_images: bool,
    ) -> Self {
        Self {
            model,
            reasoning,
            max_context,
            max_output,
            supports_images,
            state_mode: StateMode::default(),
            hint: None,
        }
    }

    /// Relational limits are checked at config ingress, not on construction.
    pub(crate) fn validate_limits(&self) -> Result<(), LimitsError> {
        if self.max_output >= self.max_context {
            return Err(LimitsError);
        }
        Ok(())
    }
}

/// A profile's output limit leaves no room for input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("max_output must be smaller than max_context")]
pub struct LimitsError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_flat_serde_shape() {
        for output in [1, 2] {
            let profile = ModelProfile {
                max_context: crate::tests::limit(1),
                max_output: crate::tests::limit(output),
                ..crate::tests::profile("m", false)
            };
            assert_eq!(profile.validate_limits(), Err(LimitsError));
        }
        let raw = serde_json::json!({"model":"unlisted:model/version", "reasoning":"opaque vendor mode", "max_context":4096, "max_output":512, "supports_images":false, "state_mode":"persist"});
        let profile: ModelProfile = serde_json::from_value(raw.clone()).unwrap();
        assert!(profile.validate_limits().is_ok());
        assert_eq!(serde_json::to_value(profile).unwrap(), raw);
        let minimal =
            serde_json::json!({"model":"m", "reasoning":null, "max_context":2, "max_output":1});
        let profile: ModelProfile = serde_json::from_value(minimal).unwrap();
        assert_eq!(profile.state_mode, StateMode::Dynamic);
    }

    #[test]
    fn model_references_qualify_keys_and_keep_wire_names_free() {
        let model: ModelRef = "vendor 任意/z first".parse().unwrap();
        assert_eq!(
            (model.provider.as_str(), model.model.as_str()),
            ("vendor 任意", "z first")
        );
        assert_eq!(model.to_string(), "vendor 任意/z first");
        assert_eq!(serde_json::to_value(&model).unwrap(), "vendor 任意/z first");
        assert_eq!(
            serde_json::from_value::<ModelRef>(serde_json::json!("v/m")).unwrap(),
            ModelRef::new("v".parse().unwrap(), "m".parse().unwrap())
        );
        assert_eq!(
            "unqualified".parse::<ModelRef>().unwrap_err(),
            ModelRefError::Unqualified
        );
        assert_eq!(
            "a/b/c".parse::<ModelRef>().unwrap_err(),
            ModelRefError::Name(NameError::Slash("model name"))
        );
        assert_eq!(
            " /m".parse::<ModelRef>().unwrap_err(),
            ModelRefError::Name(NameError::Blank("provider name"))
        );
        assert_eq!(
            "p/".parse::<ModelRef>().unwrap_err(),
            ModelRefError::Name(NameError::Blank("model name"))
        );
        assert!("a/b".parse::<ProviderName>().is_err());
    }
}
