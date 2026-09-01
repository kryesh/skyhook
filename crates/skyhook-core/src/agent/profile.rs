use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentProfile {
    pub instructions: String,
    pub model_profile: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}
