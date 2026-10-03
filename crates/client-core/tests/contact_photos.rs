//! Contact photo normalization: pre-decode bounds, format/animation rejection, orientation,
//! alpha flattening, metadata stripping, and the encrypted output actually decrypting to a
//! 256x256 JPEG.

use image::{ImageEncoder, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use peppy_client_core::{AttachmentInfo, Client, ClientConfig, DatabaseKey, Error};
use peppy_domain::{DeviceId, VaultId};
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn test_client() -> (Client, TempDir) {
    let temp = TempDir::new().unwrap();
    let config = ClientConfig {
        vault_id: VaultId::new(),
        device_id: DeviceId::new(),
        database_path: temp.path().join("test.db"),
    };
    let client = Client::open(config, DatabaseKey::new(&[42u8; 32]).unwrap()).unwrap();
    (client, temp)
}

fn prepare(client: &Client, dir: &Path, name: &str, bytes: &[u8]) -> Result<AttachmentInfo, Error> {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    client.prepare_contact_photo(&path)
}

/// Decrypts the stored attachment and returns its plaintext bytes.
fn decrypt(client: &Client, info: &AttachmentInfo) -> Vec<u8> {
    let plain = client.open_native_plaintext(info.attachment_id).unwrap();
    std::fs::read(plain.path()).unwrap()
}

fn jpeg(image: &RgbImage) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 95)
        .encode_image(image)
        .unwrap();
    out
}

fn png_rgb(image: &RgbImage) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    out
}

fn png_rgba(image: &RgbaImage) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
    out
}

fn webp_lossless(image: &RgbaImage) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    let mut crc_input = kind.to_vec();
    crc_input.extend_from_slice(data);
    chunk.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    chunk
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// A ~70 byte PNG whose IHDR declares `width`x`height` 8-bit RGBA, with a token IDAT.
fn png_header_only(width: u32, height: u32) -> Vec<u8> {
    let mut ihdr = width.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut out = PNG_SIGNATURE.to_vec();
    out.extend(png_chunk(b"IHDR", &ihdr));
    out.extend(png_chunk(
        b"IDAT",
        &[0x78, 0x9C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
    ));
    out.extend(png_chunk(b"IEND", &[]));
    out
}

/// Inserts an EXIF APP1 segment with the given Orientation tag right after the JPEG SOI.
fn with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    let mut tiff = b"II*\0".to_vec();
    tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 offset
    tiff.extend_from_slice(&1u16.to_le_bytes()); // one entry
    tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&orientation.to_le_bytes());
    tiff.extend_from_slice(&[0, 0]);
    tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend(tiff);
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    out.extend(payload);
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// Marker segment ids before the first SOS (APPn, COM, etc.).
fn jpeg_header_markers(bytes: &[u8]) -> Vec<u8> {
    let mut markers = Vec::new();
    let mut i = 2;
    while i + 4 <= bytes.len() && bytes[i] == 0xFF {
        let marker = bytes[i + 1];
        if marker == 0xDA {
            break;
        }
        markers.push(marker);
        i += 2 + usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
    }
    markers
}

fn assert_normalized_jpeg(bytes: &[u8]) -> RgbImage {
    assert!(bytes.len() <= 64 * 1024, "output {} bytes", bytes.len());
    assert_eq!(image::guess_format(bytes).unwrap(), ImageFormat::Jpeg);
    let markers = jpeg_header_markers(bytes);
    assert!(
        !markers
            .iter()
            .any(|m| (0xE1..=0xEF).contains(m) || *m == 0xFE),
        "metadata segments present: {markers:x?}"
    );
    let decoded = image::load_from_memory_with_format(bytes, ImageFormat::Jpeg)
        .unwrap()
        .into_rgb8();
    assert_eq!(decoded.dimensions(), (256, 256));
    decoded
}

fn near(actual: Rgb<u8>, expected: [u8; 3]) -> bool {
    actual
        .0
        .iter()
        .zip(expected)
        .all(|(a, e)| a.abs_diff(e) <= 40)
}

#[test]
fn header_declaring_huge_dimensions_is_rejected_without_decoding() {
    let (client, temp) = test_client();
    // 60000x60000 RGBA would need ~13 GiB; the file is tiny. Rejection must come from the header.
    let bomb = png_header_only(60_000, 60_000);
    assert!(bomb.len() < 128);
    let started = Instant::now();
    let err = prepare(&client, temp.path(), "bomb.png", &bomb).unwrap_err();
    assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));

    // Just over the bound in one axis, as a real, fully valid (and tiny) image.
    let wide = png_rgb(&RgbImage::from_pixel(4097, 1, Rgb([9, 9, 9])));
    assert!(matches!(
        prepare(&client, temp.path(), "wide.png", &wide),
        Err(Error::InvalidRequest(_))
    ));
    let tall = jpeg(&RgbImage::from_pixel(8, 4097, Rgb([9, 9, 9])));
    assert!(matches!(
        prepare(&client, temp.path(), "tall.jpg", &tall),
        Err(Error::InvalidRequest(_))
    ));
}

#[test]
fn boundary_dimension_is_accepted_and_output_decrypts_to_metadata_free_256_jpeg() {
    let (client, temp) = test_client();
    let source = with_exif_orientation(
        &jpeg(&RgbImage::from_pixel(4096, 16, Rgb([200, 30, 30]))),
        1,
    );
    let info = prepare(&client, temp.path(), "edge.jpg", &source).unwrap();
    assert_eq!(info.media_type, "image/jpeg");
    let plain = decrypt(&client, &info);
    assert_eq!(plain.len() as u64, info.plaintext_bytes);
    let decoded = assert_normalized_jpeg(&plain);
    assert!(near(*decoded.get_pixel(128, 128), [200, 30, 30]));
}

#[test]
fn exif_orientation_is_applied_before_crop() {
    let (client, temp) = test_client();
    // 64x32: left half red, right half blue. Orientation 6 (rotate 90 CW) makes it 32x64 with
    // red on top and blue at the bottom; the center square keeps that split horizontally.
    let source = RgbImage::from_fn(64, 32, |x, _| {
        if x < 32 {
            Rgb([255, 0, 0])
        } else {
            Rgb([0, 0, 255])
        }
    });
    let oriented = with_exif_orientation(&jpeg(&source), 6);
    let info = prepare(&client, temp.path(), "rotated.jpg", &oriented).unwrap();
    let out = assert_normalized_jpeg(&decrypt(&client, &info));
    assert!(
        near(*out.get_pixel(20, 20), [255, 0, 0]),
        "top-left {:?}",
        out.get_pixel(20, 20)
    );
    assert!(
        near(*out.get_pixel(236, 20), [255, 0, 0]),
        "top-right {:?}",
        out.get_pixel(236, 20)
    );
    assert!(
        near(*out.get_pixel(20, 236), [0, 0, 255]),
        "bottom-left {:?}",
        out.get_pixel(20, 236)
    );
    assert!(
        near(*out.get_pixel(236, 236), [0, 0, 255]),
        "bottom-right {:?}",
        out.get_pixel(236, 236)
    );
}

#[test]
fn rectangular_input_is_center_cropped() {
    let (client, temp) = test_client();
    // 300x100: green 100px band in the center, black elsewhere. Center crop is all green.
    let source = RgbImage::from_fn(300, 100, |x, _| {
        if (100..200).contains(&x) {
            Rgb([0, 255, 0])
        } else {
            Rgb([0, 0, 0])
        }
    });
    let info = prepare(&client, temp.path(), "wide.png", &png_rgb(&source)).unwrap();
    let out = assert_normalized_jpeg(&decrypt(&client, &info));
    for (x, y) in [(5, 5), (250, 5), (5, 250), (250, 250), (128, 128)] {
        assert!(
            near(*out.get_pixel(x, y), [0, 255, 0]),
            "({x},{y}) {:?}",
            out.get_pixel(x, y)
        );
    }
}

#[test]
fn transparent_pixels_flatten_to_white() {
    let (client, temp) = test_client();
    // Transparent black must become white, not black (premultiplication / halo regression).
    let source = RgbaImage::from_fn(64, 64, |x, _| {
        if x < 32 {
            Rgba([0, 0, 0, 0])
        } else {
            Rgba([0, 0, 255, 255])
        }
    });
    let info = prepare(&client, temp.path(), "alpha.png", &png_rgba(&source)).unwrap();
    let out = assert_normalized_jpeg(&decrypt(&client, &info));
    assert!(
        near(*out.get_pixel(20, 128), [255, 255, 255]),
        "{:?}",
        out.get_pixel(20, 128)
    );
    assert!(
        near(*out.get_pixel(236, 128), [0, 0, 255]),
        "{:?}",
        out.get_pixel(236, 128)
    );
}

#[test]
fn still_webp_is_accepted_but_animated_webp_is_rejected() {
    let (client, temp) = test_client();
    let frame = RgbaImage::from_pixel(16, 16, Rgba([10, 200, 10, 255]));
    let still = webp_lossless(&frame);
    let info = prepare(&client, temp.path(), "still.webp", &still).unwrap();
    assert_normalized_jpeg(&decrypt(&client, &info));

    // Re-wrap the still frame's VP8L chunk as a single-frame animation (VP8X + ANIM + ANMF).
    assert_eq!(&still[12..16], b"VP8L");
    let vp8l_chunk = &still[12..];
    let mut vp8x = vec![0x02, 0, 0, 0]; // animation flag
    vp8x.extend_from_slice(&15u32.to_le_bytes()[..3]);
    vp8x.extend_from_slice(&15u32.to_le_bytes()[..3]);
    let mut anmf = vec![0; 6]; // x, y offsets
    anmf.extend_from_slice(&15u32.to_le_bytes()[..3]);
    anmf.extend_from_slice(&15u32.to_le_bytes()[..3]);
    anmf.extend_from_slice(&100u32.to_le_bytes()[..3]); // duration
    anmf.push(0);
    anmf.extend_from_slice(vp8l_chunk);
    let riff_chunk = |kind: &[u8; 4], data: &[u8]| {
        let mut chunk = kind.to_vec();
        chunk.extend_from_slice(&(data.len() as u32).to_le_bytes());
        chunk.extend_from_slice(data);
        if data.len() % 2 == 1 {
            chunk.push(0);
        }
        chunk
    };
    let mut body = b"WEBP".to_vec();
    body.extend(riff_chunk(b"VP8X", &vp8x));
    body.extend(riff_chunk(b"ANIM", &[0, 0, 0, 0, 0, 0]));
    body.extend(riff_chunk(b"ANMF", &anmf));
    let mut animated = b"RIFF".to_vec();
    animated.extend_from_slice(&(body.len() as u32).to_le_bytes());
    animated.extend(body);
    assert!(matches!(
        prepare(&client, temp.path(), "animated.webp", &animated),
        Err(Error::InvalidRequest(_))
    ));
}

#[test]
fn still_png_is_accepted_but_apng_is_rejected() {
    let (client, temp) = test_client();
    let still = png_rgb(&RgbImage::from_pixel(16, 16, Rgb([10, 10, 200])));
    prepare(&client, temp.path(), "still.png", &still).unwrap();

    // Insert acTL (2 frames, loop forever) right after IHDR: signature(8) + IHDR chunk(25).
    let mut actl = 2u32.to_be_bytes().to_vec();
    actl.extend_from_slice(&0u32.to_be_bytes());
    let mut apng = still[..33].to_vec();
    apng.extend(png_chunk(b"acTL", &actl));
    apng.extend_from_slice(&still[33..]);
    assert!(matches!(
        prepare(&client, temp.path(), "animated.png", &apng),
        Err(Error::InvalidRequest(_))
    ));
}

#[test]
fn non_allowlisted_formats_are_rejected_even_when_compiled_in() {
    let (client, temp) = test_client();
    // GIF89a, 1x1, minimal valid image. The desktop crate enables image's `gif` feature.
    let gif: &[u8] = &[
        b'G', b'I', b'F', b'8', b'9', b'a', 1, 0, 1, 0, 0x80, 0, 0, 0, 0, 0, 255, 255, 255, 0x2C,
        0, 0, 0, 0, 1, 0, 1, 0, 0, 0x02, 0x02, 0x44, 0x01, 0, 0x3B,
    ];
    let bmp_header: &[u8] =
        b"BM\x3a\0\0\0\0\0\0\0\x36\0\0\0\x28\0\0\0\x01\0\0\0\x01\0\0\0\x01\0\x18\0";
    for (name, bytes) in [
        ("x.gif", gif),
        ("x.bmp", bmp_header),
        ("x.txt", b"not an image".as_slice()),
    ] {
        assert!(
            matches!(
                prepare(&client, temp.path(), name, bytes),
                Err(Error::InvalidRequest(_))
            ),
            "{name}"
        );
    }
}

#[test]
fn input_size_and_corruption_failures() {
    let (client, temp) = test_client();
    assert!(prepare(&client, temp.path(), "empty.jpg", b"").is_err());
    // A valid JPEG padded past 8 MiB is rejected by the byte bound, not by decoding.
    let mut big = jpeg(&RgbImage::from_pixel(8, 8, Rgb([1, 2, 3])));
    big.resize(8 * 1024 * 1024 + 1, 0);
    assert!(matches!(
        prepare(&client, temp.path(), "big.jpg", &big),
        Err(Error::InvalidRequest(_))
    ));
    let valid = jpeg(&RgbImage::from_pixel(64, 64, Rgb([1, 2, 3])));
    let truncated = &valid[..valid.len() / 3];
    assert!(prepare(&client, temp.path(), "trunc.jpg", truncated).is_err());
    assert!(
        client.prepare_contact_photo(temp.path()).is_err(),
        "directory source"
    );
}

#[test]
fn high_entropy_input_still_fits_64_kib() {
    let (client, temp) = test_client();
    let mut state = 0x1234_5678u32;
    let noise = RgbImage::from_fn(256, 256, |_, _| {
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        };
        Rgb([next(), next(), next()])
    });
    let info = prepare(&client, temp.path(), "noise.png", &png_rgb(&noise)).unwrap();
    assert!(info.plaintext_bytes <= 64 * 1024);
    assert_normalized_jpeg(&decrypt(&client, &info));
}

#[test]
fn no_plaintext_scratch_left_in_media_store() {
    let (client, temp) = test_client();
    let ok = jpeg(&RgbImage::from_pixel(32, 32, Rgb([5, 5, 5])));
    let info = prepare(&client, temp.path(), "ok.jpg", &ok).unwrap();
    let _ = prepare(
        &client,
        temp.path(),
        "bad.png",
        &png_header_only(60_000, 60_000),
    );
    let media = temp.path().join("test.db.media");
    for scratch in ["tmp", "plain"] {
        let leftovers: Vec<_> = std::fs::read_dir(media.join(scratch)).unwrap().collect();
        assert!(leftovers.is_empty(), "{scratch}: {leftovers:?}");
    }
    // The only stored object is ciphertext, and it is not the JPEG plaintext.
    let cipher: Vec<_> = std::fs::read_dir(media.join("cipher"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(cipher.len(), 1);
    let stored = std::fs::read(&cipher[0]).unwrap();
    assert_eq!(stored.len() as u64, info.ciphertext_bytes);
    assert!(
        image::guess_format(&stored).is_err(),
        "cipher store holds a recognizable image"
    );
}
