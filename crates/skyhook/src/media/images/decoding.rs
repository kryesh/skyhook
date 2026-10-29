//! Decoding supported images within [`MAX_DECODED_BYTES`].

use std::io::Cursor;

use image::{
    DynamicImage, ImageDecoder, ImageReader, ImageResult, Limits,
    codecs::{png::PngDecoder, webp::WebPDecoder},
    metadata::Orientation,
};

use super::ImageFormat;

/// The most memory one image's decoded pixels may take; larger images are refused
/// before any pixel is allocated.
const MAX_DECODED_BYTES: u64 = 512 * 1024 * 1024;

fn limits(budget: u64) -> Limits {
    let mut limits = Limits::default();
    limits.max_alloc = Some(budget);
    limits
}

/// Every pixel of `bytes`, with the EXIF orientation that turns them upright: an
/// animation's first frame on its canvas (an APNG's default image), the only frame
/// decoded.
pub(super) fn admit(format: ImageFormat, bytes: &[u8]) -> ImageResult<(DynamicImage, Orientation)> {
    admit_within(format, bytes, MAX_DECODED_BYTES)
}

/// [`admit`] within `budget` bytes of decoded pixels, as each decoder's declared
/// dimensions size them: a WebP's EXIF read, and planes sized from inner
/// dimensions that contradict its canvas, fall outside it.
fn admit_within(
    format: ImageFormat,
    bytes: &[u8],
    budget: u64,
) -> ImageResult<(DynamicImage, Orientation)> {
    let mut limits = limits(budget);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format.codec());
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder()?;
    // As `ImageReader::decode` does, which `into_decoder` leaves to us: the output
    // is reserved, and the decoder's own buffers (a GIF frame that does not span
    // its canvas) take what it leaves.
    limits.reserve(decoder.total_bytes())?;
    // `image` passes WebP no limits. Its temporaries come to at most two RGBA
    // canvases: an animation composes its first frame, decoded as RGBA, on a
    // canvas of its own; an opaque lossless still decodes through one RGBA copy,
    // and a lossy one through its YUV planes and an alpha plane decoded as RGBA
    // (macroblock padding outgrows that only in images far too small to matter).
    if format == ImageFormat::WebP {
        let (width, height) = decoder.dimensions();
        limits.reserve(u64::from(width) * u64::from(height) * 8)?;
    }
    decoder.set_limits(limits)?;
    // `image` does not pass `Limits` to WebP, so its EXIF read reserves the chunk's
    // declared length (untouched virtual memory, up to 4 GiB). Drop this note once
    // upstream forwards limits.
    let orientation = decoder.orientation()?;
    Ok((DynamicImage::from_decoder(decoder)?, orientation))
}

/// Whether `bytes` are stored as they are: not an animation, which is stored as
/// its first frame (an APNG's default image). Any GIF may animate, so none is kept.
pub(super) fn kept(format: ImageFormat, bytes: &[u8]) -> ImageResult<bool> {
    Ok(match format {
        ImageFormat::Jpeg => true,
        ImageFormat::Png => {
            !PngDecoder::with_limits(Cursor::new(bytes), limits(MAX_DECODED_BYTES))?.is_apng()?
        }
        ImageFormat::Gif => false,
        ImageFormat::WebP => !WebPDecoder::new(Cursor::new(bytes))?.has_animation(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::noise_bytes;

    fn refused(format: ImageFormat, bytes: &[u8], budget: u64) -> bool {
        match admit_within(format, bytes, budget) {
            Ok(_) => false,
            Err(image::ImageError::Limits(_)) => true,
            Err(error) => panic!("{error}"),
        }
    }

    /// The least budget that admits `bytes`, as `needed` bytes are: one byte
    /// less is refused.
    fn admits_from(format: ImageFormat, bytes: &[u8], needed: u64) {
        assert!(refused(format, bytes, needed - 1));
        assert!(!refused(format, bytes, needed));
    }

    /// A GIF frame that does not span its canvas decodes through a buffer of its
    /// own, beside the output.
    #[test]
    fn a_gif_frame_off_its_canvas_shares_the_budget_with_the_output() {
        let pixels = noise_bytes().take(9 * 10 * 4).collect();
        let frame = image::Frame::new(image::RgbaImage::from_vec(9, 10, pixels).unwrap());
        let mut bytes = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut bytes)
            .encode_frame(frame)
            .unwrap();
        // Widen the canvas by a column, and move the frame right onto it.
        bytes[6..8].copy_from_slice(&10u16.to_le_bytes());
        let descriptor = [0x2c, 0, 0, 0, 0, 9, 0, 10, 0];
        let at = bytes
            .windows(9)
            .position(|window| window == descriptor)
            .unwrap();
        bytes[at + 1] = 1;
        admits_from(ImageFormat::Gif, &bytes, 10 * 10 * 4 + 9 * 10 * 4);
    }

    /// A WebP reserves two RGBA canvases beside its output: an animation's
    /// canvas and frame, or a still's temporaries.
    #[test]
    fn a_webp_reserves_two_rgba_canvases_beside_its_output() {
        let pixels = noise_bytes().take(4 * 3 * 3).collect();
        let opaque = image::RgbImage::from_vec(4, 3, pixels).unwrap();
        let mut still = Cursor::new(Vec::new());
        opaque
            .write_to(&mut still, image::ImageFormat::WebP)
            .unwrap();
        let animation = super::super::tests::animated_webp(4, 3);
        for bytes in [still.into_inner(), animation] {
            admits_from(ImageFormat::WebP, &bytes, 4 * 3 * (3 + 8));
        }
    }
}
