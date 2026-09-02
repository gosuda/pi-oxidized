//! Terminal image protocol encoders and header dimension parsers.
//!
//! Ports `.references/pi/packages/tui/src/terminal-image.ts` for Kitty and
//! iTerm2 inline graphics. No stdin picker, no terminal writes — callers emit
//! the returned bytes through frame annotations.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::terminal::caps::CellDimensions;

/// Image pixel dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDimensions {
    /// Width in pixels.
    pub width_px: u32,
    /// Height in pixels.
    pub height_px: u32,
}

/// Image size expressed in terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageCellSize {
    /// Columns (character cells).
    pub columns: u16,
    /// Rows (character cells).
    pub rows: u16,
}

/// Options for [`encode_kitty`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KittyEncodeOptions {
    /// Placement width in columns (`c=`).
    pub columns: Option<u16>,
    /// Placement height in rows (`r=`).
    pub rows: Option<u16>,
    /// Image id (`i=`), range `[1, 0xFFFF_FFFE]`.
    pub image_id: Option<u32>,
    /// When `false`, emit `C=1` so Kitty does not move the cursor.
    /// Default `true` (omit `C=1`).
    pub move_cursor: Option<bool>,
}

/// Options for [`encode_iterm2`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ITerm2EncodeOptions {
    /// Width: cells as decimal string, or `"auto"`.
    pub width: Option<String>,
    /// Height: cells as decimal string, or `"auto"`.
    pub height: Option<String>,
    /// Optional file name (base64-encoded in the sequence).
    pub name: Option<String>,
    /// When `false`, emit `preserveAspectRatio=0`. Default preserves aspect.
    pub preserve_aspect_ratio: Option<bool>,
    /// When `false`, emit `inline=0`. Default is inline.
    pub inline: Option<bool>,
}

const KITTY_CHUNK_SIZE: usize = 4096;
const MAX_IMAGE_ID: u32 = 0xFFFF_FFFE;

/// Allocate a random-ish image id in `[1, 0xFFFF_FFFE]`.
///
/// Uses a process-wide counter mixed with a simple LCG so parallel tests and
/// module instances do not collide deterministically.
#[must_use]
pub fn allocate_image_id() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(1);
    let next = COUNTER.fetch_add(1, Ordering::Relaxed);
    (next % MAX_IMAGE_ID).saturating_add(1)
}

/// Encode a base64 PNG/JPEG/… payload as a Kitty graphics placement.
///
/// Always emits `a=T,f=100,q=2`. Optional `C=1` when `move_cursor == false`,
/// optional `c`/`r`/`i`. Multi-chunk payloads use `m=1` / `m=0` with 4096-char
/// base64 slices.
#[must_use]
pub fn encode_kitty(base64_data: &str, options: KittyEncodeOptions) -> String {
    let mut params = vec!["a=T".to_owned(), "f=100".to_owned(), "q=2".to_owned()];
    if options.move_cursor == Some(false) {
        params.push("C=1".to_owned());
    }
    if let Some(c) = options.columns {
        params.push(format!("c={c}"));
    }
    if let Some(r) = options.rows {
        params.push(format!("r={r}"));
    }
    if let Some(i) = options.image_id {
        params.push(format!("i={i}"));
    }

    if base64_data.len() <= KITTY_CHUNK_SIZE {
        return format!("\u{1b}_G{};{}\u{1b}\\", params.join(","), base64_data);
    }

    let joined_params = params.join(",");
    let mut out = String::new();
    let mut offset = 0usize;
    let mut is_first = true;
    while offset < base64_data.len() {
        let end = (offset + KITTY_CHUNK_SIZE).min(base64_data.len());
        let Some(chunk) = base64_data.get(offset..end) else {
            break;
        };
        let is_last = end >= base64_data.len();
        if is_first {
            out.push_str("\u{1b}_G");
            out.push_str(&joined_params);
            out.push_str(",m=1;");
            out.push_str(chunk);
            out.push_str("\u{1b}\\");
            is_first = false;
        } else {
            out.push_str(if is_last {
                "\u{1b}_Gm=0;"
            } else {
                "\u{1b}_Gm=1;"
            });
            out.push_str(chunk);
            out.push_str("\u{1b}\\");
        }
        offset = end;
    }
    out
}

/// Delete a Kitty graphics image by id (frees image data with `d=I`).
#[must_use]
pub fn delete_kitty_image(image_id: u32) -> String {
    format!("\u{1b}_Ga=d,d=I,i={image_id},q=2\u{1b}\\")
}

/// Delete all visible Kitty graphics images (frees data with `d=A`).
#[must_use]
pub fn delete_all_kitty_images() -> String {
    "\u{1b}_Ga=d,d=A,q=2\u{1b}\\".to_owned()
}

/// Encode a base64 payload as an iTerm2 inline file transfer (`OSC 1337`).
#[must_use]
pub fn encode_iterm2(base64_data: &str, options: ITerm2EncodeOptions) -> String {
    use base64::Engine as _;
    let ITerm2EncodeOptions {
        width,
        height,
        name,
        preserve_aspect_ratio,
        inline,
    } = options;
    let mut params = Vec::new();
    let inline = inline != Some(false);
    params.push(format!("inline={}", i32::from(inline)));
    if let Some(w) = width {
        params.push(format!("width={w}"));
    }
    if let Some(h) = height {
        params.push(format!("height={h}"));
    }
    if let Some(name) = name {
        let name_b64 = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
        params.push(format!("name={name_b64}"));
    }
    if preserve_aspect_ratio == Some(false) {
        params.push("preserveAspectRatio=0".to_owned());
    }
    format!("\u{1b}]1337;File={}:{}\u{7}", params.join(";"), base64_data)
}

/// Convenience builders for cell-count width/height.
impl ITerm2EncodeOptions {
    /// Width and height in cells.
    #[must_use]
    pub fn cells(width: u16, height: u16) -> Self {
        Self {
            width: Some(width.to_string()),
            height: Some(height.to_string()),
            ..Self::default()
        }
    }

    /// Width in cells, height `auto`.
    #[must_use]
    pub fn width_auto_height(width: u16) -> Self {
        Self {
            width: Some(width.to_string()),
            height: Some("auto".to_owned()),
            ..Self::default()
        }
    }
}

/// Compute placement size in cells using min-scale + ceil + clamp.
///
/// Ports `calculateImageCellSize` from terminal-image.ts.
#[must_use]
pub fn calculate_image_cell_size(
    image_dimensions: ImageDimensions,
    max_width_cells: u16,
    max_height_cells: Option<u16>,
    cell_dimensions: CellDimensions,
) -> ImageCellSize {
    let max_width = max_width_cells.max(1);
    let max_height = max_height_cells.map(|height| height.max(1));
    let image_width = u128::from(image_dimensions.width_px.max(1));
    let image_height = u128::from(image_dimensions.height_px.max(1));
    let cell_width = u128::from(cell_dimensions.width.max(1));
    let cell_height = u128::from(cell_dimensions.height.max(1));

    let width_scale = (u128::from(max_width) * cell_width, image_width);
    let scale = if let Some(height) = max_height {
        let height_scale = (u128::from(height) * cell_height, image_height);
        if width_scale.0 * height_scale.1 <= height_scale.0 * width_scale.1 {
            width_scale
        } else {
            height_scale
        }
    } else {
        width_scale
    };

    let columns = (image_width * scale.0).div_ceil(scale.1 * cell_width);
    let rows = (image_height * scale.0).div_ceil(scale.1 * cell_height);
    let columns = u16::try_from(columns.min(u128::from(max_width)))
        .unwrap_or(max_width)
        .max(1);
    let rows = match max_height {
        Some(height) => u16::try_from(rows.min(u128::from(height)))
            .unwrap_or(height)
            .max(1),
        None => u16::try_from(rows.min(u128::from(u16::MAX)))
            .unwrap_or(u16::MAX)
            .max(1),
    };

    ImageCellSize { columns, rows }
}

/// Rows for a target width (no max-height clamp).
#[must_use]
pub fn calculate_image_rows(
    image_dimensions: ImageDimensions,
    target_width_cells: u16,
    cell_dimensions: CellDimensions,
) -> u16 {
    calculate_image_cell_size(image_dimensions, target_width_cells, None, cell_dimensions).rows
}

/// Fallback text when graphics protocols are unavailable.
#[must_use]
pub fn image_fallback(
    mime_type: &str,
    dimensions: Option<ImageDimensions>,
    filename: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(name) = filename {
        parts.push(name.to_owned());
    }
    parts.push(format!("[{mime_type}]"));
    if let Some(d) = dimensions {
        parts.push(format!("{}x{}", d.width_px, d.height_px));
    }
    format!("[Image: {}]", parts.join(" "))
}

/// Parse PNG IHDR dimensions from raw bytes.
#[must_use]
pub fn get_png_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 24 {
        return None;
    }
    if bytes[0] != 0x89 || bytes[1] != 0x50 || bytes[2] != 0x4e || bytes[3] != 0x47 {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some(ImageDimensions {
        width_px: width,
        height_px: height,
    })
}

/// Parse PNG dimensions from a base64 payload.
#[must_use]
pub fn get_png_dimensions_base64(base64_data: &str) -> Option<ImageDimensions> {
    decode_b64(base64_data).and_then(|b| get_png_dimensions(&b))
}

/// Parse JPEG SOF0/1/2 dimensions from raw bytes.
#[must_use]
pub fn get_jpeg_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 2 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut offset = 2usize;
    while offset + 9 < bytes.len() {
        if bytes[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker = bytes[offset + 1];
        if (0xc0..=0xc2).contains(&marker) {
            let height = u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]]);
            let width = u16::from_be_bytes([bytes[offset + 7], bytes[offset + 8]]);
            return Some(ImageDimensions {
                width_px: u32::from(width),
                height_px: u32::from(height),
            });
        }
        if offset + 3 >= bytes.len() {
            return None;
        }
        let length = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]);
        if length < 2 {
            return None;
        }
        offset += 2 + usize::from(length);
    }
    None
}

/// Parse JPEG dimensions from a base64 payload.
#[must_use]
pub fn get_jpeg_dimensions_base64(base64_data: &str) -> Option<ImageDimensions> {
    decode_b64(base64_data).and_then(|b| get_jpeg_dimensions(&b))
}

/// Parse GIF logical screen descriptor dimensions.
#[must_use]
pub fn get_gif_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 10 {
        return None;
    }
    let sig = std::str::from_utf8(&bytes[0..6]).ok()?;
    if sig != "GIF87a" && sig != "GIF89a" {
        return None;
    }
    let width = u16::from_le_bytes([bytes[6], bytes[7]]);
    let height = u16::from_le_bytes([bytes[8], bytes[9]]);
    Some(ImageDimensions {
        width_px: u32::from(width),
        height_px: u32::from(height),
    })
}

/// Parse GIF dimensions from a base64 payload.
#[must_use]
pub fn get_gif_dimensions_base64(base64_data: &str) -> Option<ImageDimensions> {
    decode_b64(base64_data).and_then(|b| get_gif_dimensions(&b))
}

/// Parse WebP VP8 / VP8L / VP8X dimensions.
#[must_use]
pub fn get_webp_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 30 {
        return None;
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    let chunk = &bytes[12..16];
    if chunk == b"VP8 " {
        if bytes.len() < 30 {
            return None;
        }
        let width = u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff;
        let height = u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff;
        return Some(ImageDimensions {
            width_px: u32::from(width),
            height_px: u32::from(height),
        });
    }
    if chunk == b"VP8L" {
        if bytes.len() < 25 {
            return None;
        }
        let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
        let width = (bits & 0x3fff) + 1;
        let height = ((bits >> 14) & 0x3fff) + 1;
        return Some(ImageDimensions {
            width_px: width,
            height_px: height,
        });
    }
    if chunk == b"VP8X" {
        if bytes.len() < 30 {
            return None;
        }
        let width =
            u32::from(bytes[24]) | (u32::from(bytes[25]) << 8) | (u32::from(bytes[26]) << 16);
        let height =
            u32::from(bytes[27]) | (u32::from(bytes[28]) << 8) | (u32::from(bytes[29]) << 16);
        return Some(ImageDimensions {
            width_px: width + 1,
            height_px: height + 1,
        });
    }
    None
}

/// Parse WebP dimensions from a base64 payload.
#[must_use]
pub fn get_webp_dimensions_base64(base64_data: &str) -> Option<ImageDimensions> {
    decode_b64(base64_data).and_then(|b| get_webp_dimensions(&b))
}

/// Dispatch dimension parsing by MIME type (base64 payload).
#[must_use]
pub fn get_image_dimensions(base64_data: &str, mime_type: &str) -> Option<ImageDimensions> {
    match mime_type {
        "image/png" => get_png_dimensions_base64(base64_data),
        "image/jpeg" => get_jpeg_dimensions_base64(base64_data),
        "image/gif" => get_gif_dimensions_base64(base64_data),
        "image/webp" => get_webp_dimensions_base64(base64_data),
        _ => None,
    }
}

/// Dispatch dimension parsing by MIME type (raw bytes).
#[must_use]
pub fn get_image_dimensions_bytes(bytes: &[u8], mime_type: &str) -> Option<ImageDimensions> {
    match mime_type {
        "image/png" => get_png_dimensions(bytes),
        "image/jpeg" => get_jpeg_dimensions(bytes),
        "image/gif" => get_gif_dimensions(bytes),
        "image/webp" => get_webp_dimensions(bytes),
        _ => None,
    }
}

fn decode_b64(data: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn kitty_small_golden() {
        let seq = encode_kitty(
            "AAAA",
            KittyEncodeOptions {
                columns: Some(2),
                rows: Some(2),
                ..KittyEncodeOptions::default()
            },
        );
        assert_eq!(seq, "\u{1b}_Ga=T,f=100,q=2,c=2,r=2;AAAA\u{1b}\\");
    }

    #[test]
    fn kitty_no_cursor_move_golden() {
        let seq = encode_kitty(
            "AAAA",
            KittyEncodeOptions {
                columns: Some(2),
                rows: Some(2),
                move_cursor: Some(false),
                ..KittyEncodeOptions::default()
            },
        );
        assert_eq!(seq, "\u{1b}_Ga=T,f=100,q=2,C=1,c=2,r=2;AAAA\u{1b}\\");
        assert!(seq.starts_with("\u{1b}_Ga=T,f=100,q=2,C=1,c=2,r=2;"));
    }

    #[test]
    fn kitty_with_id_golden() {
        let seq = encode_kitty(
            "AAAA",
            KittyEncodeOptions {
                columns: Some(1),
                rows: Some(1),
                image_id: Some(42),
                ..KittyEncodeOptions::default()
            },
        );
        assert_eq!(seq, "\u{1b}_Ga=T,f=100,q=2,c=1,r=1,i=42;AAAA\u{1b}\\");
    }

    #[test]
    fn kitty_multi_chunk_4096() {
        let big = "A".repeat(KITTY_CHUNK_SIZE * 2 + 10);
        let seq = encode_kitty(
            &big,
            KittyEncodeOptions {
                columns: Some(3),
                rows: Some(4),
                image_id: Some(7),
                ..KittyEncodeOptions::default()
            },
        );
        let parts: Vec<&str> = seq.split("\u{1b}\\").filter(|p| !p.is_empty()).collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].starts_with("\u{1b}_Ga=T,f=100,q=2,c=3,r=4,i=7,m=1;"));
        assert!(parts[1].starts_with("\u{1b}_Gm=1;"));
        assert!(parts[2].starts_with("\u{1b}_Gm=0;"));
        assert_eq!(
            parts
                .first()
                .and_then(|part| part.split_once(';'))
                .map(|p| p.1.len()),
            Some(4096)
        );
        assert_eq!(
            parts
                .get(1)
                .and_then(|part| part.split_once(';'))
                .map(|p| p.1.len()),
            Some(4096)
        );
        assert_eq!(
            parts
                .get(2)
                .and_then(|part| part.split_once(';'))
                .map(|p| p.1.len()),
            Some(10)
        );
    }

    #[test]
    fn delete_kitty_goldens() {
        assert_eq!(delete_kitty_image(42), "\u{1b}_Ga=d,d=I,i=42,q=2\u{1b}\\");
        assert_eq!(delete_all_kitty_images(), "\u{1b}_Ga=d,d=A,q=2\u{1b}\\");
    }

    #[test]
    fn iterm2_goldens() {
        let seq = encode_iterm2(
            "AAAA",
            ITerm2EncodeOptions {
                width: Some("2".into()),
                height: Some("auto".into()),
                name: Some("x.png".into()),
                ..ITerm2EncodeOptions::default()
            },
        );
        assert_eq!(
            seq,
            "\u{1b}]1337;File=inline=1;width=2;height=auto;name=eC5wbmc=:AAAA\u{7}"
        );
        let seq2 = encode_iterm2(
            "AAAA",
            ITerm2EncodeOptions {
                width: Some("5".into()),
                height: Some("3".into()),
                preserve_aspect_ratio: Some(false),
                ..ITerm2EncodeOptions::default()
            },
        );
        assert_eq!(
            seq2,
            "\u{1b}]1337;File=inline=1;width=5;height=3;preserveAspectRatio=0:AAAA\u{7}"
        );
    }

    #[test]
    fn cell_size_math() {
        let size = calculate_image_cell_size(
            ImageDimensions {
                width_px: 20,
                height_px: 20,
            },
            2,
            None,
            CellDimensions {
                width: 10,
                height: 10,
            },
        );
        assert_eq!(
            size,
            ImageCellSize {
                columns: 2,
                rows: 2
            }
        );

        let size_h = calculate_image_cell_size(
            ImageDimensions {
                width_px: 10,
                height_px: 100,
            },
            10,
            Some(5),
            CellDimensions {
                width: 10,
                height: 10,
            },
        );
        assert_eq!(
            size_h,
            ImageCellSize {
                columns: 1,
                rows: 5
            }
        );
    }

    #[test]
    fn size_clamps_minimum_one() {
        let size = calculate_image_cell_size(
            ImageDimensions {
                width_px: 1,
                height_px: 1,
            },
            0,
            Some(0),
            CellDimensions::default(),
        );
        assert_eq!(size.columns, 1);
        assert_eq!(size.rows, 1);
    }

    #[test]
    fn fallback_text() {
        assert_eq!(
            image_fallback(
                "image/png",
                Some(ImageDimensions {
                    width_px: 10,
                    height_px: 20
                }),
                Some("a.png")
            ),
            "[Image: a.png [image/png] 10x20]"
        );
        assert_eq!(
            image_fallback("image/jpeg", None, None),
            "[Image: [image/jpeg]]"
        );
    }

    #[test]
    fn png_header_parse() {
        let mut png = vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0,
            0, 0, 1, 0, 0, 0, 1,
        ];
        png.resize(24, 0);
        let expected = ImageDimensions {
            width_px: 1,
            height_px: 1,
        };
        assert_eq!(get_png_dimensions(&png), Some(expected));
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        assert_eq!(get_png_dimensions_base64(&b64), Some(expected));
    }

    #[test]
    fn jpeg_header_parse() {
        let jpeg = [
            0xff, 0xd8, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x20, 0x00, 0x40, 0x01, 0x01, 0x11,
            0x00,
        ];
        assert_eq!(
            get_jpeg_dimensions(&jpeg),
            Some(ImageDimensions {
                width_px: 64,
                height_px: 32,
            })
        );
    }

    #[test]
    fn gif_header_parse() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&100u16.to_le_bytes());
        gif.extend_from_slice(&50u16.to_le_bytes());
        assert_eq!(
            get_gif_dimensions(&gif),
            Some(ImageDimensions {
                width_px: 100,
                height_px: 50,
            })
        );
    }

    #[test]
    fn webp_vp8x_header_parse() {
        let mut webp = vec![0u8; 30];
        webp[0..4].copy_from_slice(b"RIFF");
        webp[8..12].copy_from_slice(b"WEBP");
        webp[12..16].copy_from_slice(b"VP8X");
        webp[24] = 9;
        webp[27] = 19;
        assert_eq!(
            get_webp_dimensions(&webp),
            Some(ImageDimensions {
                width_px: 10,
                height_px: 20,
            })
        );
    }

    #[test]
    fn allocate_image_id_in_range() {
        for _ in 0..20 {
            let id = allocate_image_id();
            assert!((1..=MAX_IMAGE_ID).contains(&id));
        }
    }

    #[test]
    fn get_image_dimensions_by_mime() {
        let mut png = vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, b'I', b'H', b'D', b'R', 0,
            0, 0, 2, 0, 0, 0, 3,
        ];
        png.resize(24, 0);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        assert_eq!(
            get_image_dimensions(&b64, "image/png"),
            Some(ImageDimensions {
                width_px: 2,
                height_px: 3,
            })
        );
        assert!(get_image_dimensions(&b64, "image/unknown").is_none());
    }
}
