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

pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_IMAGES_PER_SUBMISSION: usize = 8;
pub const MAX_IMAGE_BYTES_PER_SUBMISSION: u64 = 32 * 1024 * 1024;

/// A blob identity, never a path supplied by the caller. Canonical lowercase
/// matches the stored blob filenames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
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

    #[must_use]
    pub const fn to_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl FromStr for BlobDigest {
    type Err = MediaError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(MediaError::InvalidDigest);
        }
        let mut digest = [0; 32];
        fn nibble(value: u8) -> Result<u8, MediaError> {
            match value {
                b'0'..=b'9' => Ok(value - b'0'),
                b'a'..=b'f' => Ok(value - b'a' + 10),
                _ => Err(MediaError::InvalidDigest),
            }
        }
        for (slot, pair) in digest.iter_mut().zip(value.as_bytes().as_chunks::<2>().0) {
            *slot = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Ok(Self(digest))
    }
}

impl TryFrom<String> for BlobDigest {
    type Error = MediaError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<BlobDigest> for String {
    fn from(value: BlobDigest) -> Self {
        value.to_string()
    }
}

impl fmt::Display for BlobDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("invalid blob digest: expected 64 lowercase hexadecimal characters")]
    InvalidDigest,
    #[error("data is not a supported image (PNG, JPEG, GIF or WebP)")]
    UnsupportedImage,
    #[error("blob was not loaded for this request")]
    NotLoaded,
    #[error("text attachment is not valid UTF-8")]
    InvalidText,
    #[error("image has invalid base64")]
    InvalidBase64,
    #[error("blob exceeds its byte limit")]
    TooLarge,
    #[error("blob does not match its byte length")]
    LengthMismatch,
    #[error("blob does not match its digest")]
    HashMismatch,
    #[error("blob allocation failed: {0}")]
    Allocation(#[source] std::collections::TryReserveError),
}

/// A supported image encoding, identified from the image bytes themselves.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
pub enum ImageFormat {
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/jpeg")]
    Jpeg,
    #[serde(rename = "image/gif")]
    Gif,
    #[serde(rename = "image/webp")]
    WebP,
}

impl ImageFormat {
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(Self::WebP)
        } else {
            None
        }
    }

    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::WebP => "image/webp",
        }
    }
}

/// An image kept in its source encoding. Construction proves the bytes are a
/// supported format, so producers never pair bytes with a separate media type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    format: ImageFormat,
    bytes: Vec<u8>,
}

impl Image {
    pub fn new(bytes: Vec<u8>) -> Result<Self, MediaError> {
        let format = ImageFormat::sniff(&bytes).ok_or(MediaError::UnsupportedImage)?;
        Ok(Self { format, bytes })
    }

    pub const fn format(&self) -> ImageFormat {
        self.format
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
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
    #[schemars(with = "String")]
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

    pub fn base64(&self, blob: &BlobRef) -> Result<String, MediaError> {
        Ok(STANDARD.encode(self.get(blob)?))
    }

    pub(crate) fn contains(&self, blob: &BlobRef) -> bool {
        self.0.contains_key(&blob.sha256)
    }

    pub(crate) fn insert(&mut self, blob: BlobRef, bytes: Vec<u8>) {
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
    use crate::{
        provider::protocol::{Message, ModelRequest, ToolResult, UserContent},
        session::SessionStore,
    };

    #[test]
    fn digest_strict_canonical_serde_and_path_rejection() {
        let digest = BlobDigest::of(b"payload");
        let text = digest.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(text, crate::sha256_hex(b"payload"));
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
                assert!(matches!(
                    decode_base64_bounded(&encoded, length - 1),
                    Err(MediaError::TooLarge)
                ));
            }
        }
        for invalid in ["!Q==", "YR==", "YQ=", "====", "Y===", "=Q==", "AA=A"] {
            assert!(
                matches!(
                    decode_base64_bounded(invalid, 8),
                    Err(MediaError::InvalidBase64)
                ),
                "{invalid}"
            );
        }
    }

    #[tokio::test]
    async fn stored_attachments_load_by_digest_and_reject_corrupt_blobs() {
        for durable in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let store = if durable {
                SessionStore::create(root.path()).await.unwrap()
            } else {
                SessionStore::create_ephemeral(root.path()).await.unwrap()
            };
            let png = Image::new(b"\x89PNG\r\n\x1a\npixels".to_vec()).unwrap();
            let image = Attachment::Image {
                file: Some("shot.png".into()),
                image: png.clone(),
            };
            let AttachmentRef::Image(image) = store.store_attachment(&image).await.unwrap() else {
                panic!("image attachment")
            };
            let text = Attachment::Text {
                file: None,
                content: "notes".into(),
            };
            let AttachmentRef::Text(text) = store.store_attachment(&text).await.unwrap() else {
                panic!("text attachment")
            };
            assert_eq!(image.file.as_deref(), Some("shot.png"));
            assert_eq!(image.format, ImageFormat::Png);
            let mut request = ModelRequest {
                model: "fixture".into(),
                system: vec![],
                tail: Vec::new(),
                history_lifetime: Default::default(),
                history: vec![
                    Message::User(vec![UserContent::Attachment {
                        attachment: AttachmentRef::Text(text.clone()),
                    }]),
                    Message::Tool(vec![ToolResult {
                        call_id: "call".into(),
                        name: "read".into(),
                        result: serde_json::json!({}),
                        images: vec![image.clone()],
                        is_error: false,
                    }]),
                ],
                tools: vec![],
                response_schema: None,
                reasoning: None,
                max_output_tokens: None,
                correlation: None,
                blobs: LoadedBlobs::default(),
            };
            assert!(matches!(
                request.blobs.get(&image.blob),
                Err(MediaError::NotLoaded)
            ));
            store.load_blobs(&mut request).await.unwrap();
            assert_eq!(request.blobs.text(&text).unwrap(), "notes");
            assert_eq!(request.blobs.get(&image.blob).unwrap(), png.bytes());
            assert!(!serde_json::to_string(&request).unwrap().contains("blobs"));
            if durable {
                let limit = MAX_IMAGE_BYTES as usize;
                let digest = image.blob.sha256;
                store.corrupt_blob(digest, Some(b"changed".to_vec())).await;
                assert!(store.read_blob(&image.blob, limit).await.is_err());
                store.corrupt_blob(digest, None).await;
                assert!(store.read_blob(&image.blob, limit).await.is_err());
            }
        }
    }

    #[test]
    fn images_keep_their_source_format_and_reject_other_bytes() {
        for (bytes, format) in [
            (&b"\x89PNG\r\n\x1a\nrest"[..], ImageFormat::Png),
            (b"\xff\xd8\xffrest", ImageFormat::Jpeg),
            (b"GIF89arest", ImageFormat::Gif),
            (b"RIFF\0\0\0\0WEBPrest", ImageFormat::WebP),
        ] {
            let image = Image::new(bytes.to_vec()).unwrap();
            assert_eq!((image.format(), image.bytes()), (format, bytes));
        }
        assert!(Image::new(b"not an image".to_vec()).is_err());
    }
}
