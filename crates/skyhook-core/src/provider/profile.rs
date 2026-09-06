use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    pub provider: String,
    pub model: String,
    pub reasoning: Option<String>,
    pub max_context: u64,
    pub max_output: u64,
    #[serde(default)]
    pub supports_images: bool,
}

impl ModelProfile {
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
        if self.max_output > u64::from(u32::MAX) {
            return Err("max_output exceeds the provider u32 request limit");
        }
        Ok(())
    }
}
