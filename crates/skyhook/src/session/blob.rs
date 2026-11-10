//! Blob storage: content-addressed attachments and images, saved with the entries that
//! reference them and loaded for the requests that send them.

use thiserror::Error;

use super::{Command, SessionError, SessionStore, core, db};
use crate::provider::protocol::ModelRequest;

pub(super) fn read_blob(
    db: &crate::db::Db,
    blob: skyhook_media::BlobRef,
    limit: usize,
) -> Result<Vec<u8>, SessionError> {
    usize::try_from(blob.bytes)
        .ok()
        .filter(|&length| length <= limit)
        .ok_or(BlobError::TooLarge.at(blob.sha256))?;
    let bytes = db::verified_blob(db, blob.sha256)?;
    let bytes = bytes.map_err(|reason| reason.at(blob.sha256))?;
    if bytes.len() as u64 != blob.bytes {
        return Err(BlobError::LengthMismatch.at(blob.sha256));
    }
    Ok(bytes)
}

/// Why a stored blob cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum BlobError {
    #[error("is missing")]
    Missing,
    #[error("failed its content hash check")]
    HashMismatch,
    #[error("exceeds its byte limit")]
    TooLarge,
    #[error("does not match its byte length")]
    LengthMismatch,
    #[error("is not the image format it was stored as")]
    UnsupportedImage,
}

impl BlobError {
    fn at(self, digest: skyhook_media::BlobDigest) -> SessionError {
        SessionError::Blob {
            digest,
            reason: self,
        }
    }
}

impl SessionStore {
    /// Store bytes in the content-addressed blob store.
    pub(crate) async fn store_blob(
        &self,
        bytes: &[u8],
    ) -> Result<skyhook_media::BlobRef, SessionError> {
        let blob = skyhook_media::BlobRef::of(bytes);
        let bytes = bytes.to_vec();
        // A blob the journal cannot take stops the session, as an append does.
        let insert = move |writer: &mut core::Writer| {
            writer.shared.read().phase().admits(Command::Journal)?;
            writer.write(|db, _| Ok(db::insert_blob(db, blob.sha256, &bytes)?))
        };
        self.inner.core.call(insert).await??;
        Ok(blob)
    }
    pub(crate) async fn store_image(
        &self,
        file: Option<String>,
        image: &skyhook_media::Image,
    ) -> Result<skyhook_media::ImageRef, SessionError> {
        Ok(skyhook_media::ImageRef {
            file,
            format: image.format(),
            blob: self.store_blob(image.bytes()).await?,
        })
    }
    pub(crate) async fn store_attachment(
        &self,
        attachment: &skyhook_media::Attachment,
    ) -> Result<skyhook_media::AttachmentRef, SessionError> {
        use skyhook_media::{Attachment, AttachmentRef, TextRef};
        let file = attachment.file().map(|file| file.display().to_string());
        Ok(match attachment {
            Attachment::Text { content, .. } => AttachmentRef::Text(TextRef {
                file,
                blob: self.store_blob(content.as_bytes()).await?,
            }),
            Attachment::Image { image, .. } => {
                AttachmentRef::Image(self.store_image(file, image).await?)
            }
        })
    }
    /// Load every blob a request references so providers can encode it. Blobs in
    /// `cache` are not read again; afterwards it holds exactly this request's blobs.
    pub async fn load_blobs(
        &self,
        request: &mut ModelRequest,
        cache: &mut skyhook_media::LoadedBlobs,
    ) -> Result<(), SessionError> {
        use skyhook_media::{AttachmentRef, ImageFormat, MAX_IMAGE_BYTES};
        // Image blobs carry the format they must sniff as; text blobs carry none.
        let mut blobs = Vec::new();
        use crate::provider::protocol::{Message, UserContent};
        for message in request.messages() {
            match message {
                Message::User(content) => {
                    blobs.extend(content.iter().filter_map(|item| match item {
                        UserContent::Attachment {
                            attachment: AttachmentRef::Text(text),
                        } => Some((text.blob, None)),
                        UserContent::Attachment {
                            attachment: AttachmentRef::Image(image),
                        } => Some((image.blob, Some(image.format))),
                        _ => None,
                    }));
                }
                Message::Tool(results) => blobs.extend(
                    results
                        .iter()
                        .flat_map(|result| &result.images)
                        .map(|image| (image.blob, Some(image.format))),
                ),
                Message::Assistant(_) => {}
            }
        }
        // A blob a text also references is exempt from sniffing.
        blobs.sort_by_key(|(_, format)| format.is_none());
        let blobs: std::collections::BTreeMap<_, _> = blobs.into_iter().collect();
        let missing: Vec<_> = blobs
            .keys()
            .filter(|blob| !cache.contains(blob))
            .copied()
            .collect();
        if !missing.is_empty() {
            // One bound for every blob loaded into a request, text or image.
            let limit = MAX_IMAGE_BYTES as usize;
            let read = move |db: &crate::db::Db| {
                let read = |blob| Ok((blob, read_blob(db, blob, limit)?));
                missing.into_iter().map(read).collect::<Result<Vec<_>, _>>()
            };
            for (blob, bytes) in self.snapshot(read).await? {
                cache.insert(blob, bytes);
            }
        }
        let mut loaded = skyhook_media::LoadedBlobs::default();
        for (blob, format) in blobs {
            let bytes = cache.shared(&blob).expect("referenced blobs are loaded");
            if format.is_some_and(|format| ImageFormat::sniff(&bytes) != Some(format)) {
                return Err(BlobError::UnsupportedImage.at(blob.sha256));
            }
            loaded.insert(blob, bytes);
        }
        cache.clone_from(&loaded);
        request.blobs = loaded;
        Ok(())
    }
}
