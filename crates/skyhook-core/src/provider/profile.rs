use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ModelProfile {
    pub provider: String,
    pub model: String,
    pub reasoning: Option<String>,
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub supports_images: bool,
}
