//! The images one provider context sends, fitted to its model's limits.

use std::{borrow::Cow, collections::BTreeMap};

use crate::{
    media::{BlobDigest, ImageLimits, Prepared},
    provider::{
        ProviderError, ProviderErrorKind,
        codec::common::{PreparedImages, blob_error},
        protocol::ModelRequest,
    },
};

/// Prepared copies by stored original. A context sends to one model, so each
/// original is fitted once while the context still references it, and one that
/// cannot be fitted stays omitted rather than failing every request.
#[derive(Debug, Default)]
pub(crate) struct FittedImages(BTreeMap<BlobDigest, Prepared>);

impl FittedImages {
    /// Fit each image `request` references that is not fitted yet, and forget the
    /// ones it no longer references.
    pub(crate) async fn refresh(
        &mut self,
        request: &ModelRequest,
        limits: ImageLimits,
    ) -> Result<(), ProviderError> {
        let referenced: BTreeMap<_, _> = request
            .images()
            .map(|image| (image.blob.sha256, image))
            .collect();
        self.0.retain(|digest, _| referenced.contains_key(digest));
        for (digest, image) in referenced {
            if self.0.contains_key(&digest) {
                continue;
            }
            let original = request.blobs.image(image).map_err(blob_error)?;
            let fitted =
                tokio::task::spawn_blocking(move || original.fit(limits).map(Cow::into_owned));
            let prepared = match fitted.await.map_err(|error| {
                ProviderErrorKind::Internal.error(format!("image fitting stopped: {error}"))
            })? {
                Ok(image) => Prepared::Fitted(image),
                Err(error) => Prepared::Omitted(error),
            };
            self.0.insert(digest, prepared);
        }
        Ok(())
    }

    pub(crate) fn prepared(&self) -> PreparedImages<'_> {
        PreparedImages::new(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        media::FitError,
        provider::{
            codec::common::tests::{LIMITS, image, request, tool_result},
            protocol::entries,
        },
    };

    #[tokio::test]
    async fn images_fit_once_while_referenced() {
        let png = image();
        let mut request = request("model");
        let result = tool_result(
            "call",
            "read",
            serde_json::json!({}),
            vec![png.clone()],
            false,
        );
        request.context = entries(vec![result]);
        request.blobs.insert(png.blob, crate::tests::png_bytes(b""));
        let mut images = FittedImages::default();
        // Whether the image is omitted as undecodable, once prepared.
        let omitted = |images: &FittedImages| {
            images.prepared().get(&png).map(|prepared| match prepared {
                Prepared::Fitted(_) => false,
                Prepared::Omitted(reason) => matches!(reason, FitError::Invalid(_)),
            })
        };
        images.refresh(&request, LIMITS).await.unwrap();
        assert_eq!(omitted(&images), Ok(false));
        // A referenced original is never fitted again, so bytes it no longer
        // decodes from are not read.
        request
            .blobs
            .insert(png.blob, b"\x89PNG\r\n\x1a\nbroken".to_vec());
        images.refresh(&request, LIMITS).await.unwrap();
        assert_eq!(omitted(&images), Ok(false));
        // An unreferenced original is forgotten, and fitted afresh once referenced again.
        let referenced = std::mem::take(&mut request.context);
        images.refresh(&request, LIMITS).await.unwrap();
        assert!(omitted(&images).is_err());
        request.context = referenced;
        images.refresh(&request, LIMITS).await.unwrap();
        assert_eq!(omitted(&images), Ok(true));
    }
}
