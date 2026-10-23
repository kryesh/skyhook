//! Images in their source encoding: proving bytes decode as a supported format,
//! and fitting a copy within a provider's limits.

use std::{
    borrow::Cow,
    fmt,
    io::Cursor,
    num::{NonZeroU32, NonZeroU64},
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{
    DynamicImage, ImageReader, Rgba, RgbaImage, codecs::jpeg::JpegEncoder, imageops,
    metadata::Orientation,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{MAX_IMAGE_BYTES, MediaError};
use crate::named_enum::named_enum;

mod decoding;

/// What the session stores of one image.
const STORED: ImageLimits = ImageLimits::new(u32::MAX, MAX_IMAGE_BYTES);
/// The quality of JPEGs re-encoded to fit a byte limit.
const FIT_JPEG_QUALITY: u8 = 85;

named_enum! {
    /// A supported image encoding, identified from the image bytes themselves and
    /// spelled as its media type.
    #[derive(
        Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema,
    )]
    pub enum ImageFormat {
        Png = "image/png",
        Jpeg = "image/jpeg",
        Gif = "image/gif",
        WebP = "image/webp",
    }
}

impl ImageFormat {
    /// The supported format the bytes' signature names; the rest is not inspected.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        match image::guess_format(bytes).ok()? {
            image::ImageFormat::Png => Some(Self::Png),
            image::ImageFormat::Jpeg => Some(Self::Jpeg),
            image::ImageFormat::Gif => Some(Self::Gif),
            image::ImageFormat::WebP => Some(Self::WebP),
            _ => None,
        }
    }

    const fn codec(self) -> image::ImageFormat {
        match self {
            Self::Png => image::ImageFormat::Png,
            Self::Jpeg => image::ImageFormat::Jpeg,
            Self::Gif => image::ImageFormat::Gif,
            Self::WebP => image::ImageFormat::WebP,
        }
    }

    /// The conventional file extension for names shown to the model.
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::WebP => "webp",
        }
    }
}

/// The longest edge, in pixels, a fitted copy may have: no more than a JPEG holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "NonZeroU32", into = "NonZeroU32")]
pub struct ImageEdge(NonZeroU32);

impl ImageEdge {
    const MAX: NonZeroU32 = NonZeroU32::new(65_535).expect("a positive edge");

    pub const fn new(edge: NonZeroU32) -> Self {
        Self(if edge.get() > Self::MAX.get() {
            Self::MAX
        } else {
            edge
        })
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

impl From<NonZeroU32> for ImageEdge {
    fn from(edge: NonZeroU32) -> Self {
        Self::new(edge)
    }
}

impl From<ImageEdge> for NonZeroU32 {
    fn from(edge: ImageEdge) -> Self {
        edge.0
    }
}

/// The encoded size, in bytes, a fitted copy may have: no more than the session
/// stores of an image, [`MAX_IMAGE_BYTES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "NonZeroU64", into = "NonZeroU64")]
pub struct ImageBytes(NonZeroU64);

impl ImageBytes {
    const MAX: NonZeroU64 = NonZeroU64::new(MAX_IMAGE_BYTES).expect("a positive size");

    pub const fn new(bytes: NonZeroU64) -> Self {
        Self(if bytes.get() > Self::MAX.get() {
            Self::MAX
        } else {
            bytes
        })
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl From<NonZeroU64> for ImageBytes {
    fn from(bytes: NonZeroU64) -> Self {
        Self::new(bytes)
    }
}

impl From<ImageBytes> for NonZeroU64 {
    fn from(bytes: ImageBytes) -> Self {
        bytes.0
    }
}

impl fmt::Display for ImageBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The largest image a provider accepts: its longest edge in pixels and its
/// encoded size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ImageLimits {
    pub max_edge: ImageEdge,
    pub max_bytes: ImageBytes,
}

impl ImageLimits {
    pub(crate) const fn new(max_edge: u32, max_bytes: u64) -> Self {
        Self {
            max_edge: ImageEdge::new(NonZeroU32::new(max_edge).expect("a positive edge")),
            max_bytes: ImageBytes::new(NonZeroU64::new(max_bytes).expect("a positive size")),
        }
    }

    fn fits_bytes(self, image: &Image) -> bool {
        image.bytes.len() as u64 <= self.max_bytes.get()
    }
}

/// Why no copy of an image could be fitted.
#[derive(Debug, thiserror::Error)]
pub enum FitError {
    #[error("image data is invalid: {0}")]
    Invalid(#[source] image::ImageError),
    #[error("image cannot be re-encoded within {0} bytes")]
    Unfittable(ImageBytes),
}

/// What a provider is sent of one stored image.
#[derive(Debug)]
pub enum Prepared {
    Fitted(Image),
    /// Sent as a note of why in its place; the stored original is kept.
    Omitted(FitError),
}

/// An image kept in its source encoding. Construction proves the bytes decode as
/// a supported format within [`MAX_IMAGE_BYTES`], so producers never pair bytes
/// with a separate media type and the session stores whatever they produce.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    format: ImageFormat,
    bytes: Arc<[u8]>,
}

impl Image {
    /// Decode `bytes` whole, off the async workers. A still image keeps its bytes;
    /// an animation, or any GIF, becomes an upright PNG of its first frame (an
    /// APNG's default image), the only frame decoded, or the JPEG [`Image::fit`] makes of it when that PNG exceeds
    /// [`MAX_IMAGE_BYTES`].
    pub async fn decode(bytes: Vec<u8>) -> Result<Self, MediaError> {
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            return Err(MediaError::TooLarge);
        }
        let format = ImageFormat::sniff(&bytes).ok_or(MediaError::UnsupportedImage)?;
        crate::blocking(move || {
            let (pixels, orientation) =
                decoding::admit(format, &bytes).map_err(MediaError::InvalidImage)?;
            if decoding::kept(format, &bytes).map_err(MediaError::InvalidImage)? {
                return Ok(Self {
                    format,
                    bytes: bytes.into(),
                });
            }
            Ok(fit_pixels(pixels, orientation, format, STORED)?)
        })
        .await
    }

    /// An image the session store admitted, which decoded when it was stored.
    pub(super) fn stored(format: ImageFormat, bytes: Arc<[u8]>) -> Self {
        Self { format, bytes }
    }

    pub const fn format(&self) -> ImageFormat {
        self.format
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn base64(&self) -> String {
        STANDARD.encode(&self.bytes)
    }

    /// This image within `limits`: itself when it already is, else a copy
    /// downscaled to the edge limit, then a PNG if one fits (never of a JPEG),
    /// else a JPEG stepped down until it fits. A copy is upright, as its EXIF
    /// orientation showed the original. Deterministic, and CPU-bound when it copies.
    pub fn fit(&self, limits: ImageLimits) -> Result<Cow<'_, Self>, FitError> {
        let reader = ImageReader::with_format(Cursor::new(&self.bytes[..]), self.format.codec());
        // Turning upright swaps the edges at most, so the longest stays the same.
        let (width, height) = reader.into_dimensions().map_err(FitError::Invalid)?;
        if width.max(height) <= limits.max_edge.get() && limits.fits_bytes(self) {
            return Ok(Cow::Borrowed(self));
        }
        let (pixels, orientation) =
            decoding::admit(self.format, &self.bytes).map_err(FitError::Invalid)?;
        fit_pixels(pixels, orientation, self.format, limits).map(Cow::Owned)
    }
}

/// Decoded `pixels` of a `source` image as [`Image::fit`] fits them.
fn fit_pixels(
    mut pixels: DynamicImage,
    orientation: Orientation,
    source: ImageFormat,
    limits: ImageLimits,
) -> Result<Image, FitError> {
    let max_edge = limits.max_edge.get();
    if pixels.width().max(pixels.height()) > max_edge {
        // Area averaging allocates only its output, never more than its input.
        pixels = pixels.thumbnail(max_edge, max_edge);
    }
    pixels.apply_orientation(orientation);
    if source != ImageFormat::Jpeg {
        let fitted = png(&pixels)?;
        if limits.fits_bytes(&fitted) {
            return Ok(fitted);
        }
    }
    let mut pixels = flatten(pixels);
    loop {
        let fitted = jpeg(&pixels)?;
        if limits.fits_bytes(&fitted) {
            return Ok(fitted);
        }
        let edge = pixels.width().max(pixels.height());
        if edge == 1 {
            return Err(FitError::Unfittable(limits.max_bytes));
        }
        // Encoded size grows about with area; the margin usually lands the next try.
        let scale = (limits.max_bytes.get() as f64 / fitted.bytes.len() as f64).sqrt() * 0.9;
        let edge = ((f64::from(edge) * scale) as u32).clamp(1, edge - 1);
        pixels = pixels.thumbnail(edge, edge);
    }
}

fn png(pixels: &DynamicImage) -> Result<Image, FitError> {
    let mut bytes = Cursor::new(Vec::new());
    pixels
        .write_to(&mut bytes, image::ImageFormat::Png)
        .map_err(FitError::Invalid)?;
    Ok(Image {
        format: ImageFormat::Png,
        bytes: bytes.into_inner().into(),
    })
}

fn jpeg(pixels: &DynamicImage) -> Result<Image, FitError> {
    let mut bytes = Vec::new();
    pixels
        .write_with_encoder(JpegEncoder::new_with_quality(&mut bytes, FIT_JPEG_QUALITY))
        .map_err(FitError::Invalid)?;
    Ok(Image {
        format: ImageFormat::Jpeg,
        bytes: bytes.into(),
    })
}

/// Composite onto white, since JPEG has no alpha.
fn flatten(pixels: DynamicImage) -> DynamicImage {
    if !pixels.color().has_alpha() {
        return pixels.into_rgb8().into();
    }
    let mut white = RgbaImage::from_pixel(pixels.width(), pixels.height(), Rgba([255; 4]));
    imageops::overlay(&mut white, &pixels.into_rgba8(), 0, 0);
    DynamicImage::from(white).into_rgb8().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::noise;

    #[tokio::test]
    async fn still_images_keep_their_source_bytes() {
        for (codec, format) in [
            (image::ImageFormat::Png, ImageFormat::Png),
            (image::ImageFormat::Jpeg, ImageFormat::Jpeg),
            (image::ImageFormat::WebP, ImageFormat::WebP),
        ] {
            let bytes = noise(codec, 2, 2);
            let image = Image::decode(bytes.clone()).await.unwrap();
            assert_eq!((image.format(), image.bytes()), (format, &bytes[..]));
        }
        // Any GIF may animate, so even a still one is stored as a PNG.
        let image = Image::decode(gif_still(2, 2)).await.unwrap();
        assert_eq!(image.format(), ImageFormat::Png);
    }

    #[tokio::test]
    async fn animations_are_stored_as_their_first_frame() {
        // Only the first frame decodes, so a second one cut short is never kept.
        let animation = crate::tests::animated_gif();
        let truncated = animation[..animation.len() - 4].to_vec();
        let red_then_blue = apng(4, 2, false, &[&[0; 8], &[1; 8]]);
        // An APNG keeps the image viewers without animation show: its default
        // one, outside the animation here.
        let blue_then_red = apng(4, 2, true, &[&[1; 8], &[0; 8]]);
        let red = [255, 0, 0, 255];
        let blue = [0, 0, 255, 255];
        for (bytes, color) in [
            (animation, red),
            (truncated, red),
            (red_then_blue, red),
            (blue_then_red, blue),
        ] {
            let image = Image::decode(bytes).await.unwrap();
            assert_eq!(image.format(), ImageFormat::Png);
            let (still, _) = decoding::admit(image.format(), image.bytes()).unwrap();
            assert_eq!(still.to_rgba8().get_pixel(3, 1).0, color);
        }
    }

    #[tokio::test]
    async fn decoding_refuses_what_it_cannot_store() {
        let oversized = vec![0; MAX_IMAGE_BYTES as usize + 1];
        assert!(matches!(
            Image::decode(oversized).await,
            Err(MediaError::TooLarge)
        ));
        assert!(matches!(
            Image::decode(b"not an image".to_vec()).await,
            Err(MediaError::UnsupportedImage)
        ));
        // A canvas is admitted before any frame is read, however small its frame,
        // and a decompression bomb before its pixels are inflated.
        let refused = async |bytes| {
            matches!(
                Image::decode(bytes).await,
                Err(MediaError::InvalidImage(image::ImageError::Limits(_)))
            )
        };
        for canvas in [animated_webp, gif_still] {
            assert!(Image::decode(canvas(1, 1)).await.is_ok());
            assert!(refused(canvas(20_000, 20_000)).await);
        }
        // An opaque animation's RGB output fits, but not beside its RGBA canvas.
        assert!(refused(animated_webp(10_000, 10_000)).await);
        assert!(refused(png_bomb(20_000, 20_000)).await);
    }

    /// A one-pixel frame on a `width`×`height` canvas.
    fn gif_still(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = noise(image::ImageFormat::Gif, 1, 1);
        let [width, height] = [width, height].map(|edge| u16::try_from(edge).unwrap());
        bytes[6..8].copy_from_slice(&width.to_le_bytes());
        bytes[8..10].copy_from_slice(&height.to_le_bytes());
        bytes
    }

    /// A `width`×`height` RGBA PNG whose pixel data is missing.
    fn png_bomb(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        let mut writer = encoder.write_header().unwrap();
        writer.write_chunk(png::chunk::IDAT, &[]).unwrap();
        drop(writer);
        bytes
    }

    /// An APNG of `images`: its default image, then its frames, the first of them
    /// also the default unless it is `separate`; palette index 0 is red, 1 blue.
    fn apng(width: u32, height: u32, separate: bool, images: &[&[u8]]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_palette(vec![255, 0, 0, 0, 0, 255]);
        let frames = u32::try_from(images.len() - usize::from(separate)).unwrap();
        encoder.set_animated(frames, 0).unwrap();
        encoder.set_sep_def_img(separate).unwrap();
        let mut writer = encoder.write_header().unwrap();
        for image in images {
            writer.write_image_data(image).unwrap();
        }
        writer.finish().unwrap();
        bytes
    }

    /// An animated WebP of one 1×1 frame on a `width`×`height` canvas.
    pub(super) fn animated_webp(width: u32, height: u32) -> Vec<u8> {
        let still = noise(image::ImageFormat::WebP, 1, 1);
        let u24 = |value: u32| value.to_le_bytes()[..3].to_vec();
        let chunk = |name: &[u8], body: Vec<u8>| {
            let length = u32::try_from(body.len()).unwrap().to_le_bytes();
            [name, &length, &body].concat()
        };
        let canvas = [vec![0x02, 0, 0, 0], u24(width - 1), u24(height - 1)].concat();
        // Origin, 1×1, 100 ms, then the still's own image chunk.
        let frame = [vec![0; 12], u24(100), vec![0], still[12..].to_vec()].concat();
        let body = [
            b"WEBP".to_vec(),
            chunk(b"VP8X", canvas),
            chunk(b"ANIM", vec![0; 6]),
            chunk(b"ANMF", frame),
        ]
        .concat();
        let length = u32::try_from(body.len()).unwrap().to_le_bytes();
        [b"RIFF".as_slice(), &length, &body].concat()
    }

    /// A JPEG whose EXIF orientation turns it a quarter clockwise.
    fn rotated_jpeg(width: u32, height: u32) -> Vec<u8> {
        let jpeg = noise(image::ImageFormat::Jpeg, width, height);
        let mut tiff = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01".to_vec();
        tiff.extend_from_slice(b"\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0");
        let length = u16::try_from(tiff.len() + 2).unwrap().to_be_bytes();
        [&jpeg[..2], b"\xff\xe1", &length, &tiff, &jpeg[2..]].concat()
    }

    /// Only an image over a limit is copied; the copy is upright and keeps its
    /// aspect, and its format unless only a JPEG fits.
    #[tokio::test]
    async fn fit_copies_only_images_over_a_limit() {
        use image::{GenericImageView as _, ImageFormat as Codec};
        let mut wide = Cursor::new(Vec::new());
        image::GrayImage::new(16_000_000, 1)
            .write_to(&mut wide, Codec::Png)
            .unwrap();
        let roomy = 1 << 20;
        for (source, limits, expected) in [
            (noise(Codec::Png, 40, 20), ImageLimits::new(40, roomy), None),
            (
                noise(Codec::Png, 40, 20),
                ImageLimits::new(10, roomy),
                Some((ImageFormat::Png, Some((10, 5)))),
            ),
            (
                noise(Codec::Png, 64, 64),
                ImageLimits::new(64, 4000),
                Some((ImageFormat::Jpeg, None)),
            ),
            (
                wide.into_inner(),
                ImageLimits::new(8000, roomy),
                Some((ImageFormat::Png, Some((8000, 1)))),
            ),
            (
                rotated_jpeg(64, 32),
                ImageLimits::new(16, roomy),
                Some((ImageFormat::Jpeg, Some((8, 16)))),
            ),
        ] {
            let source = Image::decode(source).await.unwrap();
            let fitted = source.fit(limits).unwrap();
            let Some((format, dimensions)) = expected else {
                assert!(matches!(fitted, Cow::Borrowed(image) if *image == source));
                continue;
            };
            assert_eq!(fitted.format(), format);
            assert!(limits.fits_bytes(&fitted));
            let (decoded, _) = decoding::admit(fitted.format(), fitted.bytes()).unwrap();
            if let Some(dimensions) = dimensions {
                assert_eq!(decoded.dimensions(), dimensions);
            }
        }
        let tiny = ImageLimits::new(64, 1);
        let image = Image::decode(noise(Codec::Png, 8, 8)).await.unwrap();
        assert!(matches!(image.fit(tiny), Err(FitError::Unfittable(_))));
    }

    /// A lossless source within the edge limit but over the byte limit is
    /// re-encoded as a PNG when one fits.
    #[tokio::test]
    async fn fit_prefers_a_png_to_a_jpeg() {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, 8, 8);
        encoder.set_color(png::ColorType::Rgba);
        encoder
            .add_text_chunk("Comment".into(), "metadata ".repeat(110))
            .unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0; 8 * 8 * 4]).unwrap();
        writer.finish().unwrap();
        let limits = ImageLimits::new(64, 200);
        let source = Image::decode(bytes).await.unwrap();
        assert!(!limits.fits_bytes(&source));
        let fitted = source.fit(limits).unwrap();
        assert_eq!(fitted.format(), ImageFormat::Png);
        assert!(limits.fits_bytes(&fitted));
    }
}
