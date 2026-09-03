//! Image MIME sniffing and the inline-image pipeline facade.
//!
//! Ports the *surface* of `.references/pi-2.0/packages/coding-agent/src/utils/`
//! `{mime.ts, image-process.ts, image-convert.ts}`. The decoder, EXIF
//! orientation, resize, and encode ladder live in
//! [`crate::core::tools::read`] (the read-tool pipeline, which is the single
//! canonical home for image decoding in this crate). This module only exposes
//! the clipboard- and display-facing helpers and delegates every decode to
//! that pipeline, so the `image` decoder is never duplicated.

use crate::core::tools::read::{
    ProcessImageResult as ReadProcessImageResult, convert_image_bytes_to_png,
    detect_supported_image_mime_type, process_image_bytes,
};

/// Supported inline image MIME kinds, keyed by magic bytes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ImageMime {
    /// `image/jpeg`
    Jpeg,
    /// `image/png`
    Png,
    /// `image/gif`
    Gif,
    /// `image/webp`
    Webp,
    /// `image/bmp`
    Bmp,
}

impl ImageMime {
    /// The canonical MIME type string.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Bmp => "image/bmp",
        }
    }

    /// Parse a canonical MIME string into a kind.
    #[must_use]
    pub fn from_canonical(mime: &str) -> Option<Self> {
        Some(match mime {
            "image/jpeg" | "image/jpg" => Self::Jpeg,
            "image/png" => Self::Png,
            "image/gif" => Self::Gif,
            "image/webp" => Self::Webp,
            "image/bmp" => Self::Bmp,
            _ => return None,
        })
    }
}

/// File extension for an image MIME, matching the TypeScript
/// `extensionForImageMimeType`. Returns `"jpg"` (not `"jpeg"`) for JPEG.
///
/// This is a pure string mapping with no decoding, so it is safe to keep here
/// alongside the delegating helpers.
#[must_use]
pub fn extension_for_image_mime(mime: &str) -> Option<&'static str> {
    let base = base_mime(mime);
    match base.as_str() {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        "image/bmp" => Some("bmp"),
        _ => None,
    }
}

/// Detect a supported image MIME from magic bytes.
///
/// Delegates to [`crate::core::tools::read::detect_supported_image_mime_type`]
/// (the read-tool sniffer), rejecting animated PNG (acTL) and the JPEG Hi/Co
/// variant (third byte `0xF7`).
#[must_use]
pub fn detect_supported_image_mime(bytes: &[u8]) -> Option<ImageMime> {
    detect_supported_image_mime_type(bytes).and_then(|mime| ImageMime::from_canonical(&mime))
}

/// Convert an image byte stream to PNG bytes (with orientation applied).
///
/// Delegates to [`crate::core::tools::read::convert_image_bytes_to_png`] so the
/// decoder is not duplicated. Returns `None` when the bytes cannot be decoded.
#[must_use]
pub fn convert_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    convert_image_bytes_to_png(bytes)
}

/// Outcome of [`process_image`].
#[derive(Clone, Debug)]
pub enum ProcessImageResult {
    /// Image was normalized (and optionally resized) successfully.
    Ok {
        /// Base64-encoded payload.
        data: String,
        /// Canonical MIME type.
        mime: String,
        /// Human-readable annotations (conversion note, dimension note).
        hints: Vec<String>,
    },
    /// Image could not be converted or resized; a model-readable omission note.
    Omitted(String),
}

/// Full image pipeline mirroring `processImage` in `image-process.ts`.
///
/// Delegates to [`crate::core::tools::read::process_image_bytes`]: normalize
/// the MIME (keep supported types, else convert to PNG), then resize when
/// `auto_resize` is on. Produces conversion and dimension hints in the same
/// order as the reference.
#[must_use]
pub fn process_image(bytes: &[u8], mime: &str, auto_resize: bool) -> ProcessImageResult {
    match process_image_bytes(bytes, mime, auto_resize) {
        ReadProcessImageResult::Ok(processed) => ProcessImageResult::Ok {
            data: processed.data,
            mime: processed.mime_type,
            hints: processed.hints,
        },
        ReadProcessImageResult::Failed(error) => ProcessImageResult::Omitted(error.message),
    }
}

/// Normalize a MIME string to its base form (lowercased, parameters stripped).
fn base_mime(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, ImageFormat};
    use std::io::Cursor;
    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn solid_png(width: u32, height: u32) -> TestResult<Vec<u8>> {
        let img = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)?;
        Ok(buf)
    }

    fn solid_jpeg(width: u32, height: u32) -> TestResult<Vec<u8>> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb([10, 20, 30]));
        let mut buf = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 90);
        encoder.encode_image(&image::DynamicImage::ImageRgb8(img))?;
        Ok(buf)
    }

    #[test]
    fn detects_png_jpeg_gif_webp_signatures() -> TestResult {
        let png = solid_png(2, 2)?;
        assert_eq!(detect_supported_image_mime(&png), Some(ImageMime::Png));
        let jpeg = solid_jpeg(2, 2)?;
        assert_eq!(detect_supported_image_mime(&jpeg), Some(ImageMime::Jpeg));
        let gif = b"GIF89a...";
        assert_eq!(detect_supported_image_mime(gif), Some(ImageMime::Gif));
        let mut webp = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        webp.extend_from_slice(b"VP8 ");
        assert_eq!(detect_supported_image_mime(&webp), Some(ImageMime::Webp));
        Ok(())
    }

    #[test]
    fn rejects_jpeg_hico_marker() {
        // FF D8 FF F7 is the Hi/Co JPEG variant; rejected.
        assert_eq!(detect_supported_image_mime(&[0xFF, 0xD8, 0xFF, 0xF7]), None);
    }

    #[test]
    fn extension_for_mime_matches_ts() {
        assert_eq!(extension_for_image_mime("image/png"), Some("png"));
        assert_eq!(extension_for_image_mime("image/jpeg"), Some("jpg"));
        assert_eq!(
            extension_for_image_mime("image/jpeg; charset=binary"),
            Some("jpg")
        );
        assert_eq!(extension_for_image_mime("image/x-foo"), None);
    }

    #[test]
    fn convert_to_png_roundtrips_dimensions() -> TestResult {
        let png = solid_png(3, 5)?;
        let out = convert_to_png(&png)
            .ok_or_else(|| std::io::Error::other("PNG conversion was omitted"))?;
        let img = image::load_from_memory(&out)?;
        assert_eq!(img.dimensions(), (3, 5));
        Ok(())
    }

    #[test]
    fn process_image_normalizes_bmp_to_png() -> TestResult {
        let mut bmp_buf = Vec::new();
        let img = image::RgbImage::from_pixel(4, 4, image::Rgb([5, 6, 7]));
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut bmp_buf), ImageFormat::Bmp)?;
        match process_image(&bmp_buf, "image/bmp", false) {
            ProcessImageResult::Ok { data, mime, hints } => {
                assert_eq!(mime, "image/png");
                assert!(!data.is_empty());
                assert!(
                    hints
                        .iter()
                        .any(|h| h.contains("converted from image/bmp to image/png"))
                );
                Ok(())
            }
            ProcessImageResult::Omitted(reason) => {
                Err(std::io::Error::other(format!("expected Ok, got Omitted({reason})")).into())
            }
        }
    }

    #[test]
    fn process_image_keeps_supported_png() -> TestResult {
        let png = solid_png(2, 2)?;
        match process_image(&png, "image/png", false) {
            ProcessImageResult::Ok { mime, .. } => {
                assert_eq!(mime, "image/png");
                Ok(())
            }
            ProcessImageResult::Omitted(reason) => {
                Err(std::io::Error::other(format!("expected Ok, got Omitted({reason})")).into())
            }
        }
    }
}
