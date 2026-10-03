//! Contact photo normalization: bounded decode, orientation, crop/resize, metadata-free JPEG.
//!
//! * Input: 1 B..=8 MiB, JPEG/PNG/WebP by content sniffing (any other compiled-in `image`
//!   format, e.g. GIF enabled by the desktop crate, is rejected). Animated PNG/WebP are rejected,
//!   and animation probes fail closed.
//! * Dimensions are read from the format header and checked against 4096x4096 before any pixel
//!   buffer is allocated; the decoder also carries strict width/height limits and a `max_alloc`
//!   budget sized for a 4096x4096 image at 8 bytes per pixel.
//! * EXIF orientation is applied, then the image is center-cropped, flattened onto white,
//!   resized to 256x256 and re-encoded as a fresh JPEG (no EXIF/ICC/XMP) of at most 64 KiB.
//! * The normalized plaintext only exists in memory; callers encrypt it with
//!   `media::encrypt_bytes_into_store`.

use crate::Error;
use image::codecs::{jpeg::JpegDecoder, png::PngDecoder, webp::WebPDecoder};
use image::{DynamicImage, ExtendedColorType, ImageBuffer, ImageDecoder, ImageFormat, Limits, Rgb};
use std::io::{Cursor, Read};
use std::path::Path;

const MAX_PHOTO_INPUT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PHOTO_INPUT_DIM: u32 = 4096;
/// Decoded-buffer budget: the largest accepted image (4096x4096) in the widest decoded color
/// type these codecs emit (16-bit RGBA, 8 bytes per pixel).
const MAX_PHOTO_DECODE_BYTES: u64 = MAX_PHOTO_INPUT_DIM as u64 * MAX_PHOTO_INPUT_DIM as u64 * 8;
const PHOTO_OUTPUT_DIM: u32 = 256;
const MAX_PHOTO_OUTPUT_BYTES: usize = 64 * 1024;
const JPEG_QUALITIES: [u8; 4] = [75, 60, 45, 30];

fn photo_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_PHOTO_INPUT_DIM);
    limits.max_image_height = Some(MAX_PHOTO_INPUT_DIM);
    limits.max_alloc = Some(MAX_PHOTO_DECODE_BYTES);
    limits
}

fn invalid(_: image::ImageError) -> Error {
    Error::InvalidRequest("invalid or unsupported photo")
}

/// Returns the normalized 256x256 JPEG bytes for the photo at `input_path`.
pub(crate) fn normalize_photo(input_path: &Path) -> Result<Vec<u8>, Error> {
    let input = read_bounded(input_path)?;
    let mut image = decode_bounded(&input)?;
    drop(input);

    let (width, height) = (image.width(), image.height());
    let side = width.min(height);
    image = image.crop_imm((width - side) / 2, (height - side) / 2, side, side);
    let flattened = flatten_onto_white(image);
    let resized = image::imageops::resize(
        &flattened,
        PHOTO_OUTPUT_DIM,
        PHOTO_OUTPUT_DIM,
        image::imageops::FilterType::Lanczos3,
    );
    encode_jpeg(&resized)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, Error> {
    let file = std::fs::File::open(path).map_err(|_| Error::InvalidRequest("photo source"))?;
    let meta = file
        .metadata()
        .map_err(|_| Error::InvalidRequest("photo source"))?;
    if !meta.is_file() {
        return Err(Error::InvalidRequest("photo source"));
    }
    let mut input = Vec::new();
    // Read one byte past the limit so growth after `metadata` is still caught.
    file.take(MAX_PHOTO_INPUT_BYTES + 1)
        .read_to_end(&mut input)
        .map_err(|_| Error::InvalidRequest("photo source"))?;
    if input.is_empty() || input.len() as u64 > MAX_PHOTO_INPUT_BYTES {
        return Err(Error::InvalidRequest(
            "photo must be between 1 byte and 8 MiB",
        ));
    }
    Ok(input)
}

fn decode_bounded(input: &[u8]) -> Result<DynamicImage, Error> {
    let format = image::guess_format(input).map_err(invalid)?;
    let cursor = Cursor::new(input);
    match format {
        ImageFormat::Jpeg => decode_with(JpegDecoder::new(cursor).map_err(invalid)?),
        ImageFormat::Png => {
            let decoder = PngDecoder::with_limits(cursor, photo_limits()).map_err(invalid)?;
            // Fail closed: an unreadable animation-control probe is a rejection.
            if decoder.is_apng().map_err(invalid)? {
                return Err(Error::InvalidRequest("animated photos are not supported"));
            }
            decode_with(decoder)
        }
        ImageFormat::WebP => {
            let decoder = WebPDecoder::new(cursor).map_err(invalid)?;
            if decoder.has_animation() {
                return Err(Error::InvalidRequest("animated photos are not supported"));
            }
            decode_with(decoder)
        }
        _ => Err(Error::InvalidRequest("photo must be JPEG, PNG, or WebP")),
    }
}

/// Checks header dimensions and the decoded-buffer size before `from_decoder` allocates pixels.
fn decode_with(mut decoder: impl ImageDecoder) -> Result<DynamicImage, Error> {
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > MAX_PHOTO_INPUT_DIM || height > MAX_PHOTO_INPUT_DIM {
        return Err(Error::InvalidRequest("photo dimensions exceed 4096x4096"));
    }
    if decoder.total_bytes() > MAX_PHOTO_DECODE_BYTES {
        return Err(Error::InvalidRequest("photo dimensions exceed 4096x4096"));
    }
    decoder.set_limits(photo_limits()).map_err(invalid)?;
    let orientation = decoder.orientation().map_err(invalid)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(invalid)?;
    image.apply_orientation(orientation);
    Ok(image)
}

fn flatten_onto_white(image: DynamicImage) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
    if !image.color().has_alpha() {
        return image.into_rgb8();
    }
    let rgba = image.into_rgba8();
    ImageBuffer::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let blend =
            |c: u8| ((u16::from(c) * u16::from(a) + 255 * u16::from(255 - a) + 127) / 255) as u8;
        Rgb([blend(r), blend(g), blend(b)])
    })
}

/// Encodes at descending quality until the output fits in 64 KiB.
fn encode_jpeg(image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> Result<Vec<u8>, Error> {
    for quality in JPEG_QUALITIES {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
            .encode(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgb8,
            )
            .map_err(|_| Error::Storage)?;
        if out.len() <= MAX_PHOTO_OUTPUT_BYTES {
            return Ok(out);
        }
    }
    Err(Error::InvalidRequest(
        "photo cannot be encoded within 64 KiB",
    ))
}

/// Verifies that received, decrypted bytes really are a normalized contact photo: a JPEG of
/// at most 64 KiB whose header declares exactly 256x256, fully decodable under a strict
/// allocation budget (256*256*4). The header is checked before any pixel buffer exists. Pure
/// in-memory check; no files.
pub(crate) fn validate_normalized(bytes: &[u8]) -> Result<(), Error> {
    let reject = || Error::InvalidMedia;
    if bytes.is_empty() || bytes.len() > MAX_PHOTO_OUTPUT_BYTES {
        return Err(reject());
    }
    if image::guess_format(bytes).map_err(|_| reject())? != ImageFormat::Jpeg {
        return Err(reject());
    }
    let mut decoder = JpegDecoder::new(Cursor::new(bytes)).map_err(|_| reject())?;
    if decoder.dimensions() != (PHOTO_OUTPUT_DIM, PHOTO_OUTPUT_DIM) {
        return Err(reject());
    }
    let budget = u64::from(PHOTO_OUTPUT_DIM) * u64::from(PHOTO_OUTPUT_DIM) * 4;
    if decoder.total_bytes() > budget {
        return Err(reject());
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(PHOTO_OUTPUT_DIM);
    limits.max_image_height = Some(PHOTO_OUTPUT_DIM);
    limits.max_alloc = Some(budget);
    decoder.set_limits(limits).map_err(|_| reject())?;
    DynamicImage::from_decoder(decoder).map_err(|_| reject())?;
    Ok(())
}
