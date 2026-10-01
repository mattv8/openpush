//! Image handling for previews and public copies. Decoding is bounded (dimensions and
//! allocation), only raster PNG/JPEG/WebP/GIF are accepted, and every output is a fresh
//! re-encode, so EXIF/XMP/ICC/text metadata from the source is never carried over. SVG/HTML and
//! other formats are never decoded or rendered.
use crate::error::{BridgeError, BridgeResult};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use image::{
    codecs::jpeg::JpegEncoder, imageops::FilterType, DynamicImage, ImageDecoder, ImageFormat,
    Limits,
};
use std::{
    fs,
    io::{Cursor, Read},
    path::Path,
};

const MAX_DECODE_EDGE: u32 = 8192;
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
/// Largest plaintext read for a preview or public copy.
pub const MAX_IMAGE_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const PREVIEW_EDGE: u32 = 320;
const PUBLIC_EDGE: u32 = 2048;
/// Server limit for public copies (`MAX_PUBLIC_COPY_BYTES`).
pub const MAX_PUBLIC_COPY_BYTES: usize = 10 * 1024 * 1024;

/// Media type from magic bytes, never from the file name.
pub fn sniff_media_type(prefix: &[u8]) -> &'static str {
    match image::guess_format(prefix) {
        Ok(ImageFormat::Png) => "image/png",
        Ok(ImageFormat::Jpeg) => "image/jpeg",
        Ok(ImageFormat::WebP) => "image/webp",
        Ok(ImageFormat::Gif) => "image/gif",
        _ if prefix.starts_with(b"%PDF-") => "application/pdf",
        _ => "application/octet-stream",
    }
}

pub fn is_previewable(media_type: &str) -> bool {
    matches!(
        media_type,
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    )
}

/// Reads the first bytes of a file for sniffing.
pub fn read_prefix(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut prefix = Vec::with_capacity(32);
    fs::File::open(path)?.take(32).read_to_end(&mut prefix)?;
    Ok(prefix)
}

/// Reads a native plaintext file with a hard cap.
pub fn read_capped(path: &Path) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_IMAGE_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_IMAGE_SOURCE_BYTES).then_some(bytes)
}

fn decode_limited(bytes: &[u8]) -> Option<(DynamicImage, ImageFormat)> {
    let format = image::guess_format(bytes).ok()?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif
    ) {
        return None;
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_EDGE);
    limits.max_image_height = Some(MAX_DECODE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    Some((image, format))
}

fn encode_png(image: &DynamicImage) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .ok()?;
    Some(out)
}

/// A small re-encoded PNG thumbnail as a data URL, or `None` for anything not safely decodable.
pub fn preview_data_url(bytes: &[u8]) -> Option<String> {
    let (image, _) = decode_limited(bytes)?;
    let png = encode_png(&image.thumbnail(PREVIEW_EDGE, PREVIEW_EDGE))?;
    Some(format!("data:image/png;base64,{}", STANDARD.encode(png)))
}

pub struct PublicImage {
    pub bytes: Vec<u8>,
    pub extension: &'static str,
    pub width: u32,
    pub height: u32,
}

fn unsupported() -> BridgeError {
    BridgeError::new(
        "public-copy-unsupported",
        "Only PNG, JPEG, WebP or GIF images can be shared as a public copy.",
    )
}

/// Decodes and re-encodes an image into a new, metadata-free derivative (JPEG for JPEG sources,
/// PNG otherwise), downscaled to at most 2048 px and within the server's size limit.
pub fn reencode_public(bytes: &[u8]) -> BridgeResult<PublicImage> {
    let (image, format) = decode_limited(bytes).ok_or_else(unsupported)?;
    for edge in [PUBLIC_EDGE, 1024, 512] {
        let scaled = if image.width() > edge || image.height() > edge {
            image.resize(edge, edge, FilterType::Triangle)
        } else {
            image.clone()
        };
        let (bytes, extension) = if format == ImageFormat::Jpeg {
            let mut out = Vec::new();
            DynamicImage::ImageRgb8(scaled.to_rgb8())
                .write_with_encoder(JpegEncoder::new_with_quality(&mut out, 85))
                .map_err(|_| unsupported())?;
            (out, "jpg")
        } else {
            (encode_png(&scaled).ok_or_else(unsupported)?, "png")
        };
        if bytes.len() <= MAX_PUBLIC_COPY_BYTES {
            return Ok(PublicImage {
                bytes,
                extension,
                width: scaled.width(),
                height: scaled.height(),
            });
        }
    }
    Err(BridgeError::new(
        "public-copy-too-large",
        "The image is too large for a public copy even after downscaling.",
    ))
}

/// Server-safe public filename (`[A-Za-z0-9._-]`, at most 100 bytes) with the derivative's
/// extension; the original name is not otherwise exposed.
pub fn public_name(display_name: &str, extension: &str) -> String {
    let stem = display_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .rsplit_once('.')
        .map_or(display_name, |(stem, _)| stem);
    let cleaned: String = stem
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(60)
        .collect();
    format!(
        "{}.{extension}",
        if cleaned.is_empty() {
            "image"
        } else {
            &cleaned
        }
    )
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn sample_png(width: u32, height: u32) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([(x % 255) as u8, (y % 255) as u8, 90, 255])
        }));
        encode_png(&image).unwrap()
    }

    fn jpeg_with_exif() -> Vec<u8> {
        let mut jpeg = Vec::new();
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            40,
            30,
            image::Rgb([200, 10, 10]),
        ))
        .write_with_encoder(JpegEncoder::new_with_quality(&mut jpeg, 90))
        .unwrap();
        let payload = b"Exif\0\0GPS-SECRET-LOCATION-MARKER";
        let mut segment = vec![0xff, 0xe1];
        segment.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        segment.extend_from_slice(payload);
        let mut out = jpeg[..2].to_vec();
        out.extend_from_slice(&segment);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn public_copy_is_reencoded_without_source_metadata() {
        let source = jpeg_with_exif();
        assert!(contains(&source, b"GPS-SECRET"));
        assert!(decode_limited(&source).is_some());
        let public = reencode_public(&source).unwrap();
        assert_eq!(public.extension, "jpg");
        assert!(!contains(&public.bytes, b"GPS-SECRET"));
        assert!(!contains(&public.bytes, b"Exif"));
        assert_eq!(
            image::guess_format(&public.bytes).unwrap(),
            ImageFormat::Jpeg
        );

        let png = reencode_public(&sample_png(3000, 20)).unwrap();
        assert_eq!(png.extension, "png");
        let decoded = image::load_from_memory(&png.bytes).unwrap();
        assert_eq!(decoded.width(), 2048);
    }

    #[test]
    fn non_raster_and_oversized_inputs_are_refused() {
        assert_eq!(
            reencode_public(
                b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>"
            )
            .err()
            .unwrap()
            .code,
            "public-copy-unsupported"
        );
        assert!(reencode_public(b"<html><body>x</body></html>").is_err());
        assert!(preview_data_url(b"%PDF-1.7 not an image").is_none());
        assert!(decode_limited(&sample_png(MAX_DECODE_EDGE + 1, 2)).is_none());
    }

    #[test]
    fn previews_are_png_data_urls_and_names_are_sanitized() {
        let preview = preview_data_url(&sample_png(900, 600)).unwrap();
        assert!(preview.starts_with("data:image/png;base64,"));
        let decoded = image::load_from_memory(
            &STANDARD
                .decode(&preview["data:image/png;base64,".len()..])
                .unwrap(),
        )
        .unwrap();
        assert!(decoded.width() <= PREVIEW_EDGE && decoded.height() <= PREVIEW_EDGE);
        assert_eq!(sniff_media_type(&sample_png(2, 2)), "image/png");
        assert_eq!(sniff_media_type(b"<svg"), "application/octet-stream");
        assert_eq!(public_name("../My Photo (1).HEIC", "jpg"), "MyPhoto1.jpg");
        assert_eq!(public_name("???", "png"), "image.png");
    }
}
