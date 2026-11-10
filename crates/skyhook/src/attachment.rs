//! Workspace files attached to a prompt: from the command line or the composer.
use skyhook_base::limits::MAX_TEXT_BYTES;
use skyhook_media::{Attachment, DecodeError, Finished, MAX_IMAGE_BYTES, classify};
use skyhook_worker::op::FsError;
use std::path::{Path, PathBuf};

/// Why a file could not be attached to a prompt.
#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    #[error("{0}: {1}")]
    Read(PathBuf, #[source] FsError),
    #[error("File reference leaves the workspace")]
    OutsideWorkspace,
    #[error("File is too large to attach; ask the agent to read it instead")]
    TooLarge,
    #[error("File is larger than {} MiB; ask the agent to read it instead", MAX_TEXT_BYTES >> 20)]
    TextTooLarge,
    #[error("{0} is neither a supported image nor UTF-8 text")]
    Unsupported(PathBuf),
    #[error("{0} is not a supported image")]
    NotImage(PathBuf),
    #[error("{0}: {1}")]
    InvalidImage(PathBuf, #[source] DecodeError),
}

/// Read a workspace file as a prompt attachment: an image when its bytes are a
/// supported image, otherwise UTF-8 text.
pub async fn read_attachment(workspace: &Path, path: &Path) -> Result<Attachment, AttachmentError> {
    let path = tokio::fs::canonicalize(workspace.join(path))
        .await
        .map_err(|error| AttachmentError::Read(path.to_owned(), error.into()))?;
    if !path.starts_with(workspace) {
        return Err(AttachmentError::OutsideWorkspace);
    }
    let bytes = skyhook_worker::fs::read_regular(&path, MAX_IMAGE_BYTES)
        .await
        .map_err(|error| match error {
            FsError::TooLarge { .. } => AttachmentError::TooLarge,
            error => AttachmentError::Read(path.clone(), error),
        })?;
    let classified = classify(bytes)
        .await
        .map_err(|error| AttachmentError::InvalidImage(path.clone(), error))?;
    match classified {
        Finished::Image(image) => Ok(Attachment::Image {
            file: Some(path),
            image,
        }),
        Finished::Text(content) if content.len() as u64 > MAX_TEXT_BYTES => {
            Err(AttachmentError::TextTooLarge)
        }
        Finished::Text(content) => Ok(Attachment::Text {
            file: Some(path),
            content,
        }),
        Finished::Binary => Err(AttachmentError::Unsupported(path)),
    }
}

/// Read the images named on the command line.
pub async fn read_images(
    workspace: &Path,
    paths: &[PathBuf],
) -> Result<Vec<Attachment>, AttachmentError> {
    let mut images = Vec::with_capacity(paths.len());
    for path in paths {
        match read_attachment(workspace, path).await? {
            image @ Attachment::Image { .. } => images.push(image),
            Attachment::Text { .. } => return Err(AttachmentError::NotImage(path.clone())),
        }
    }
    Ok(images)
}
