//! Shared image metadata and submission limits.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImageReference {
    pub sha256: String,
    pub media_type: String,
    pub name: String,
    pub bytes: u64,
    /// Request-local payload. Session persistence stores image bytes in the blob directory.
    #[serde(skip)]
    #[schemars(skip)]
    pub data_base64: Option<String>,
}

pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_IMAGES_PER_SUBMISSION: usize = 8;
pub const MAX_IMAGE_BYTES_PER_SUBMISSION: u64 = 32 * 1024 * 1024;
