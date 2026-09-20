use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    pub provider: String,
    pub model: String,
    pub reasoning: Option<String>,
    pub max_context: u64,
    pub max_output: u64,
    #[serde(default)]
    pub supports_images: bool,
    #[serde(default)]
    pub state_mode: StateMode,
    /// Describes the model to agents choosing one for a child; without it they cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// How per-request runtime state (date, jobs, todos) reaches the model.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StateMode {
    /// Never send runtime state.
    None,
    /// Send current state after history; it never enters history.
    #[default]
    Dynamic,
    /// Commit each request's state to history, keeping history append-only for providers
    /// that bind signed reasoning to the exact earlier conversation.
    Persist,
}

impl ModelProfile {
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        reasoning: Option<String>,
        max_context: u64,
        max_output: u64,
        supports_images: bool,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            reasoning,
            max_context,
            max_output,
            supports_images,
            state_mode: StateMode::default(),
            hint: None,
        }
    }

    /// Relational limits are checked at config ingress, not on construction.
    pub(crate) fn validate_limits(&self) -> Result<(), &'static str> {
        if self.max_context == 0 {
            return Err("max_context must be positive");
        }
        if self.max_output == 0 {
            return Err("max_output must be positive");
        }
        if self.max_output >= self.max_context {
            return Err("max_output must be smaller than max_context");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_flat_serde_shape() {
        for (context, output) in [(0, 1), (1, 0), (1, 1), (1, 2)] {
            assert!(
                ModelProfile::new("v", "m", None, context, output, false)
                    .validate_limits()
                    .is_err()
            );
        }
        let raw = serde_json::json!({"provider":"arbitrary/vendor", "model":"unlisted:model", "reasoning":"opaque vendor mode", "max_context":4096, "max_output":512, "supports_images":false, "state_mode":"persist"});
        let profile: ModelProfile = serde_json::from_value(raw.clone()).unwrap();
        assert!(profile.validate_limits().is_ok());
        assert_eq!(serde_json::to_value(profile).unwrap(), raw);
        let minimal = serde_json::json!({"provider":"p", "model":"m", "reasoning":null, "max_context":2, "max_output":1});
        let profile: ModelProfile = serde_json::from_value(minimal).unwrap();
        assert_eq!(profile.state_mode, StateMode::Dynamic);
    }
}
