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
use skyhook_base::hex::Sha256;

mod classify;
mod images;
pub use classify::{Classifier, Finished, Step, classify};
pub use images::{
    DecodeError, FitError, Image, ImageBytes, ImageEdge, ImageFormat, ImageLimits, Prepared,
};

pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// A blob's identity. Canonical lowercase matches the stored blob filenames.
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
pub struct BlobDigest(Sha256);

impl BlobDigest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::of(bytes))
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Sha256::from_bytes(bytes))
    }

    pub const fn to_bytes(self) -> [u8; 32] {
        *self.0.as_bytes()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid blob digest: expected 64 lowercase hexadecimal characters")]
pub struct DigestError;

impl FromStr for BlobDigest {
    type Err = DigestError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        (Sha256::from_lowercase_hex(value).map(Self)).ok_or(DigestError)
    }
}

impl fmt::Display for BlobDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
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

/// Why a request's blob could not be read.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("blob was not loaded for this request")]
    NotLoaded,
    #[error("text attachment is not valid UTF-8")]
    InvalidText,
}

/// Why a base64 payload did not decode within its limit.
#[derive(Debug, thiserror::Error)]
pub enum Base64Error {
    #[error("image has invalid base64")]
    Invalid,
    #[error("blob exceeds its byte limit")]
    TooLarge,
    #[error("blob allocation failed: {0}")]
    Allocation(#[source] std::collections::TryReserveError),
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

/// Blob contents a provider request references, loaded by the session store.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadedBlobs(HashMap<BlobDigest, Arc<[u8]>>);

impl LoadedBlobs {
    pub fn get(&self, blob: &BlobRef) -> Result<&[u8], LoadError> {
        self.0
            .get(&blob.sha256)
            .map(|bytes| &**bytes)
            .ok_or(LoadError::NotLoaded)
    }

    pub fn text(&self, text: &TextRef) -> Result<&str, LoadError> {
        std::str::from_utf8(self.get(&text.blob)?).map_err(|_| LoadError::InvalidText)
    }

    /// The stored image `image` names. The store admits only decoded images and
    /// checks their format when it loads them.
    pub fn image(&self, image: &ImageRef) -> Result<Image, LoadError> {
        let bytes = self.0.get(&image.blob.sha256).ok_or(LoadError::NotLoaded)?;
        Ok(Image::stored(image.format, Arc::clone(bytes)))
    }

    pub fn contains(&self, blob: &BlobRef) -> bool {
        self.0.contains_key(&blob.sha256)
    }

    pub fn shared(&self, blob: &BlobRef) -> Option<Arc<[u8]>> {
        self.0.get(&blob.sha256).cloned()
    }

    pub fn insert(&mut self, blob: BlobRef, bytes: impl Into<Arc<[u8]>>) {
        self.0.insert(blob.sha256, bytes.into());
    }
}

/// Decode a standard padded payload without requesting an output allocation
/// larger than the caller's limit. Encoded strings have already been received.
pub fn decode_base64_bounded(data: &str, limit: usize) -> Result<Vec<u8>, Base64Error> {
    // STANDARD requires padded complete quartets. Determine the exact output
    // size before allocating; decode_slice still validates every base64 byte.
    if !data.len().is_multiple_of(4) {
        return Err(Base64Error::Invalid);
    }
    let padding = data
        .as_bytes()
        .iter()
        .rev()
        .take_while(|&&byte| byte == b'=')
        .count();
    if padding > 2 {
        return Err(Base64Error::Invalid);
    }
    let length = (data.len() / 4 * 3)
        .checked_sub(padding)
        .ok_or(Base64Error::Invalid)?;
    if length > limit {
        return Err(Base64Error::TooLarge);
    }
    let mut decoded = Vec::new();
    decoded
        .try_reserve_exact(length)
        .map_err(Base64Error::Allocation)?;
    decoded.resize(length, 0);
    let written = STANDARD
        .decode_slice(data, &mut decoded)
        .map_err(|_| Base64Error::Invalid)?;
    decoded.truncate(written);
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    /// A 1x1 PNG followed by `tail`, which decoding ignores: distinct tails make
    /// distinct blobs.
    pub(crate) fn png_bytes(tail: &[u8]) -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(1, 1)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        [bytes.into_inner(), tail.to_vec()].concat()
    }

    /// [`png_bytes`] as an image.
    pub(crate) async fn png(tail: &[u8]) -> crate::Image {
        crate::Image::decode(png_bytes(tail)).await.unwrap()
    }

    /// Deterministic bytes that do not compress.
    pub(crate) fn noise_bytes() -> impl Iterator<Item = u8> {
        let mut seed = 0x2545_f491_u32;
        std::iter::repeat_with(move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed.to_le_bytes()
        })
        .flatten()
    }

    /// Deterministic noise encoded as `format`, so encoded sizes grow with the image.
    pub(crate) fn noise(format: image::ImageFormat, width: u32, height: u32) -> Vec<u8> {
        let pixels = noise_bytes().take(width as usize * height as usize * 4);
        let pixels = image::RgbaImage::from_vec(width, height, pixels.collect()).unwrap();
        let pixels = match format {
            image::ImageFormat::Jpeg => image::DynamicImage::from(pixels).into_rgb8().into(),
            _ => image::DynamicImage::from(pixels),
        };
        let mut bytes = std::io::Cursor::new(Vec::new());
        pixels.write_to(&mut bytes, format).unwrap();
        bytes.into_inner()
    }

    /// A GIF of a red frame, then a blue one.
    pub(crate) fn animated_gif() -> Vec<u8> {
        let mut animation = Vec::new();
        let frame = |color| image::Frame::new(image::RgbaImage::from_pixel(4, 2, color));
        image::codecs::gif::GifEncoder::new(&mut animation)
            .encode_frames([
                frame(image::Rgba([255, 0, 0, 255])),
                frame(image::Rgba([0, 0, 255, 255])),
            ])
            .unwrap();
        animation
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
                    Err(Base64Error::TooLarge)
                );
            }
        }
        // Lengths and padding the bound reads; the alphabet is the decoder's.
        for invalid in ["YQ=", "====", "Y==="] {
            assert_matches!(
                decode_base64_bounded(invalid, 8),
                Err(Base64Error::Invalid),
                "{invalid}"
            );
        }
    }
}
