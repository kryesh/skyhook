//! Images and attachments: source-format images, content-addressed blob
//! references recorded in the journal, and blobs loaded for provider requests.

use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

mod images;
pub use images::{FitError, Image, ImageBytes, ImageEdge, ImageFormat, ImageLimits, Prepared};

pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_IMAGES_PER_SUBMISSION: usize = 8;
pub const MAX_IMAGE_BYTES_PER_SUBMISSION: u64 = 32 * 1024 * 1024;
/// Largest text read whole into model context: prompt attachments, instruction
/// files, and skill text.
pub const MAX_TEXT_BYTES: u64 = 1024 * 1024;

/// A blob identity, never a path supplied by the caller. Canonical lowercase
/// matches the stored blob filenames.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde_with::SerializeDisplay,
    serde_with::DeserializeFromStr,
)]
pub struct BlobDigest([u8; 32]);

impl BlobDigest {
    pub fn of(bytes: &[u8]) -> Self {
        use sha2::Digest as _;
        Self(sha2::Sha256::digest(bytes).into())
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn to_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl FromStr for BlobDigest {
    type Err = MediaError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(MediaError::InvalidDigest);
        }
        crate::identity::decode_hex(value)
            .map(Self)
            .ok_or(MediaError::InvalidDigest)
    }
}

impl fmt::Display for BlobDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        crate::identity::write_hex(&self.0, formatter)
    }
}

impl schemars::JsonSchema for BlobDigest {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "BlobDigest".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "string", "pattern": "^[0-9a-f]{64}$"})
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("invalid blob digest: expected 64 lowercase hexadecimal characters")]
    InvalidDigest,
    #[error("data is not a supported image (PNG, JPEG, GIF or WebP)")]
    UnsupportedImage,
    #[error("image data is invalid: {0}")]
    InvalidImage(#[source] image::ImageError),
    #[error("image work stopped: {0}")]
    Task(#[from] tokio::task::JoinError),
    #[error("blob was not loaded for this request")]
    NotLoaded,
    #[error("text attachment is not valid UTF-8")]
    InvalidText,
    #[error("image has invalid base64")]
    InvalidBase64,
    #[error("blob exceeds its byte limit")]
    TooLarge,
    #[error(transparent)]
    Fit(#[from] FitError),
    #[error("blob allocation failed: {0}")]
    Allocation(#[source] std::collections::TryReserveError),
}

/// Whole file contents by presentation. NUL bytes mean binary, even in valid UTF-8.
#[derive(Debug, PartialEq, Eq)]
pub enum Classified {
    Image(Image),
    Text(String),
    Binary(Vec<u8>),
}

/// Bytes signed as a supported image must decode as one.
pub async fn classify(bytes: Vec<u8>) -> Result<Classified, MediaError> {
    if ImageFormat::sniff(&bytes).is_some() {
        return Image::decode(bytes).await.map(Classified::Image);
    }
    if bytes.contains(&0) {
        return Ok(Classified::Binary(bytes));
    }
    Ok(match String::from_utf8(bytes) {
        Ok(text) => Classified::Text(text),
        Err(error) => Classified::Binary(error.into_bytes()),
    })
}

/// Content attached to a prompt before it is stored. `file` records where the
/// content came from; pasted content has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attachment {
    Text {
        file: Option<PathBuf>,
        content: String,
    },
    Image {
        file: Option<PathBuf>,
        image: Image,
    },
}

impl Attachment {
    pub fn file(&self) -> Option<&Path> {
        match self {
            Self::Text { file, .. } | Self::Image { file, .. } => file.as_deref(),
        }
    }
}

/// A content-addressed blob in the session store.
#[derive(
    Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub struct BlobRef {
    pub sha256: BlobDigest,
    pub bytes: u64,
}

impl BlobRef {
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            sha256: BlobDigest::of(bytes),
            bytes: bytes.len() as u64,
        }
    }
}

/// A stored image. `file` names the file it came from, if any.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImageRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub format: ImageFormat,
    #[serde(flatten)]
    pub blob: BlobRef,
}

/// A stored text attachment. `file` names the file it came from, if any.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct TextRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(flatten)]
    pub blob: BlobRef,
}

/// An attachment as the journal records it; its content lives in the blob store.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttachmentRef {
    Text(TextRef),
    Image(ImageRef),
}

/// Blob contents a provider request references, loaded by the session store
/// and never serialized with the request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadedBlobs(HashMap<BlobDigest, Arc<[u8]>>);

impl LoadedBlobs {
    pub fn get(&self, blob: &BlobRef) -> Result<&[u8], MediaError> {
        self.0
            .get(&blob.sha256)
            .map(|bytes| &**bytes)
            .ok_or(MediaError::NotLoaded)
    }

    pub fn text(&self, text: &TextRef) -> Result<&str, MediaError> {
        std::str::from_utf8(self.get(&text.blob)?).map_err(|_| MediaError::InvalidText)
    }

    /// The stored image `image` names. The store admits only decoded images and
    /// checks their format when it loads them.
    pub fn image(&self, image: &ImageRef) -> Result<Image, MediaError> {
        let bytes = self
            .0
            .get(&image.blob.sha256)
            .ok_or(MediaError::NotLoaded)?;
        Ok(Image::stored(image.format, Arc::clone(bytes)))
    }

    pub(crate) fn contains(&self, blob: &BlobRef) -> bool {
        self.0.contains_key(&blob.sha256)
    }

    pub(crate) fn shared(&self, blob: &BlobRef) -> Option<Arc<[u8]>> {
        self.0.get(&blob.sha256).cloned()
    }

    pub(crate) fn insert(&mut self, blob: BlobRef, bytes: impl Into<Arc<[u8]>>) {
        self.0.insert(blob.sha256, bytes.into());
    }
}

/// Decode a standard padded payload without requesting an output allocation
/// larger than the caller's limit. Encoded strings have already been received.
pub(crate) fn decode_base64_bounded(data: &str, limit: usize) -> Result<Vec<u8>, MediaError> {
    // STANDARD requires padded complete quartets. Determine the exact output
    // size before allocating; decode_slice still validates every base64 byte.
    if !data.len().is_multiple_of(4) {
        return Err(MediaError::InvalidBase64);
    }
    let padding = data
        .as_bytes()
        .iter()
        .rev()
        .take_while(|&&byte| byte == b'=')
        .count();
    if padding > 2 {
        return Err(MediaError::InvalidBase64);
    }
    let length = (data.len() / 4 * 3)
        .checked_sub(padding)
        .ok_or(MediaError::InvalidBase64)?;
    if length > limit {
        return Err(MediaError::TooLarge);
    }
    let mut decoded = Vec::new();
    decoded
        .try_reserve_exact(length)
        .map_err(MediaError::Allocation)?;
    decoded.resize(length, 0);
    let written = STANDARD
        .decode_slice(data, &mut decoded)
        .map_err(|_| MediaError::InvalidBase64)?;
    decoded.truncate(written);
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    #[tokio::test]
    async fn classification_decodes_images_and_treats_nul_as_binary() {
        let png = crate::tests::png(b"\0").await;
        let classified = classify(png.bytes().to_vec()).await.unwrap();
        assert_eq!(classified, Classified::Image(png));
        assert_matches!(
            classify(b"\x89PNG\r\n\x1a\ntext".to_vec()).await,
            Err(MediaError::InvalidImage(_))
        );
        let text = classify(b"text".to_vec()).await.unwrap();
        assert_eq!(text, Classified::Text("text".into()));
        for binary in [&b"text\0"[..], b"\xff"] {
            assert_eq!(
                classify(binary.to_vec()).await.unwrap(),
                Classified::Binary(binary.to_vec())
            );
        }
    }

    #[test]
    fn digest_strict_canonical_serde_and_path_rejection() {
        let digest = BlobDigest::of(b"payload");
        let text = digest.to_string();
        assert_eq!(text.parse::<BlobDigest>().unwrap(), digest);
        for invalid in [
            "".to_owned(),
            "../outside".into(),
            "a".repeat(63),
            "a".repeat(65),
            "g".repeat(64),
            "é".repeat(32),
            text.to_uppercase(),
            format!("{}../x", "a".repeat(60)),
        ] {
            assert!(invalid.parse::<BlobDigest>().is_err(), "{invalid}");
            assert!(serde_json::from_value::<BlobDigest>(serde_json::json!(invalid)).is_err());
        }
    }

    #[test]
    fn bounded_base64_admits_exact_decoded_sizes_and_rejects_overflow() {
        // Every padding shape, including empty, is bounded by decoded length,
        // not the rounded-up encoded envelope or the decoder's size estimate.
        for length in 0..=8 {
            let bytes = vec![42; length];
            let encoded = STANDARD.encode(&bytes);
            assert_eq!(decode_base64_bounded(&encoded, length).unwrap(), bytes);
            if length > 0 {
                assert_matches!(
                    decode_base64_bounded(&encoded, length - 1),
                    Err(MediaError::TooLarge)
                );
            }
        }
        // Lengths and padding the bound reads; the alphabet is the decoder's.
        for invalid in ["YQ=", "====", "Y==="] {
            assert_matches!(
                decode_base64_bounded(invalid, 8),
                Err(MediaError::InvalidBase64),
                "{invalid}"
            );
        }
    }
}
