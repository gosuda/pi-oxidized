//! Read tool: text files and supported images.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/tools/read.ts` plus the
//! image pipeline from `utils/{mime,image-process,image-resize-core}.ts`.

use std::fmt::Write as _;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use pi_agent::{AgentTool, AgentToolResult, ToolError, ToolUpdates};
use pi_ai::types::{ImageContent, Model, ModelInput, TextContent, ToolResultContent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::task;
use tokio_util::sync::CancellationToken;

use super::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, PathResolveError, TruncationOptions, TruncationResult,
    format_size, resolve_read_path_async, truncate_head,
};

/// Default max image edge (TypeScript `ImageResizeOptions.maxWidth/maxHeight`).
const IMAGE_MAX_DIMENSION: u32 = 2000;
/// Default max base64 payload size (4.5 MiB).
const IMAGE_MAX_BASE64_BYTES: usize = 4_718_592;
/// Default JPEG quality for the first encode candidate.
const IMAGE_JPEG_QUALITY: u8 = 80;
/// Bytes sniffed for MIME detection (TypeScript `IMAGE_TYPE_SNIFF_BYTES`).
const IMAGE_TYPE_SNIFF_BYTES: usize = 4100;

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// TypeBox-compatible read arguments (fixture `read.json`).
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ReadToolInput {
    /// Path to the file to read (relative or absolute).
    #[schemars(description = "Path to the file to read (relative or absolute)")]
    pub path: String,
    /// Line number to start reading from (1-indexed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Line number to start reading from (1-indexed)")]
    pub offset: Option<f64>,
    /// Maximum number of lines to read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Maximum number of lines to read")]
    pub limit: Option<f64>,
}

/// Optional structured details returned by the read tool.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadToolDetails {
    /// Truncation metadata when head truncation applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
}

/// Options for [`ReadTool`].
#[derive(Clone, Debug)]
pub struct ReadToolOptions {
    /// Working directory used to resolve relative paths.
    pub cwd: PathBuf,
    /// Whether to auto-resize images to provider inline limits. Default: true.
    pub auto_resize_images: bool,
    /// When `Some` and the model does not accept images, append the non-vision note.
    pub model: Option<Model>,
}

impl ReadToolOptions {
    /// Builds options for `cwd` with default image resizing and no model.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            auto_resize_images: true,
            model: None,
        }
    }

    /// Sets the model used for non-vision image omission notes.
    #[must_use]
    pub fn with_model(mut self, model: Option<Model>) -> Self {
        self.model = model;
        self
    }

    /// Enables or disables automatic image resizing.
    #[must_use]
    pub fn with_auto_resize_images(mut self, auto_resize_images: bool) -> Self {
        self.auto_resize_images = auto_resize_images;
        self
    }
}

/// Agent tool that reads text and supported image files.
#[derive(Clone, Debug)]
pub struct ReadTool {
    cwd: PathBuf,
    auto_resize_images: bool,
    model: Option<Model>,
    parameters: Value,
    description: String,
}

impl ReadTool {
    /// Creates a read tool rooted at `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_options(ReadToolOptions::new(cwd))
    }

    /// Creates a read tool from explicit options.
    #[must_use]
    pub fn with_options(options: ReadToolOptions) -> Self {
        let description = format!(
            "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
            DEFAULT_MAX_BYTES / 1024
        );
        Self {
            cwd: options.cwd,
            auto_resize_images: options.auto_resize_images,
            model: options.model,
            parameters: read_parameters_schema(),
            description,
        }
    }

    /// Returns the JSON Schema for read arguments (normalized `TypeBox` shape).
    #[must_use]
    pub fn parameters_schema() -> Value {
        read_parameters_schema()
    }

    /// Validates raw tool arguments into [`ReadToolInput`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when required fields are missing or mistyped.
    pub fn parse_input(args: &Map<String, Value>) -> Result<ReadToolInput, ToolError> {
        serde_json::from_value(Value::Object(args.clone()))
            .map_err(|error| ToolError::new(format!("Read tool input is invalid. {error}")))
    }
}

impl AgentTool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn label(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn validate_arguments(
        &self,
        args: &Map<String, Value>,
    ) -> Result<Map<String, Value>, ToolError> {
        let _ = Self::parse_input(args)?;
        Ok(args.clone())
    }

    fn execute(
        &self,
        _tool_call_id: &str,
        args: Map<String, Value>,
        cancel: CancellationToken,
        _updates: ToolUpdates,
    ) -> BoxFuture<'static, Result<AgentToolResult, ToolError>> {
        let cwd = self.cwd.clone();
        let auto_resize_images = self.auto_resize_images;
        let model = self.model.clone();
        async move {
            throw_if_cancelled(&cancel)?;
            let input = ReadTool::parse_input(&args)?;
            let absolute_path =
                resolve_read_path_async(&input.path, cwd.to_string_lossy().as_ref())
                    .await
                    .map_err(|error| path_error(&error))?;
            throw_if_cancelled(&cancel)?;

            // Readable access check (TypeScript fs.access R_OK).
            ensure_readable(&absolute_path).await?;
            throw_if_cancelled(&cancel)?;

            let bytes = tokio::fs::read(&absolute_path).await.map_err(|error| {
                ToolError::new(format!("Could not read file {absolute_path}: {error}"))
            })?;
            throw_if_cancelled(&cancel)?;

            let sniff_len = bytes.len().min(IMAGE_TYPE_SNIFF_BYTES);
            if let Some(mime) = detect_supported_image_mime_type(&bytes[..sniff_len]) {
                let non_vision = non_vision_image_note(model.as_ref());
                let processed = {
                    let cancel = cancel.clone();
                    task::spawn_blocking(move || {
                        throw_if_cancelled(&cancel)?;
                        Ok::<ProcessImageResult, ToolError>(process_image_bytes(
                            &bytes,
                            &mime,
                            auto_resize_images,
                        ))
                    })
                    .await
                    .map_err(|error| {
                        ToolError::new(format!("image processing failed: {error}"))
                    })??
                };
                throw_if_cancelled(&cancel)?;
                return Ok(image_tool_result(processed, non_vision.as_deref()));
            }

            // Byte-preserving UTF-8 decode (lossy only at invalid sequences, matching
            // Node Buffer.toString("utf-8") replacement behavior for invalid bytes).
            let text_content = String::from_utf8_lossy(&bytes).into_owned();
            let result = build_text_result(&input, &text_content)?;
            throw_if_cancelled(&cancel)?;
            Ok(result)
        }
        .boxed()
    }
}

fn build_text_result(
    input: &ReadToolInput,
    text_content: &str,
) -> Result<AgentToolResult, ToolError> {
    // TypeScript split("\n") keeps a trailing empty entry in totalFileLines.
    let all_lines: Vec<&str> = text_content.split('\n').collect();
    let total_file_lines = all_lines.len();
    let offset = input.offset.map(floored_nonnegative_usize);
    let limit = input.limit.map(floored_nonnegative_usize);

    let start_line = offset.map_or(0, |value| value.saturating_sub(1));

    if start_line >= all_lines.len() {
        return Err(ToolError::new(format!(
            "Offset {} is beyond end of file ({total_file_lines} lines total)",
            offset.unwrap_or(0)
        )));
    }
    let start_line_display = start_line + 1;

    let (selected_content, user_limited_lines) = if let Some(limit) = limit {
        let end_line = start_line.saturating_add(limit).min(all_lines.len());
        let selected = all_lines[start_line..end_line].join("\n");
        (selected, Some(end_line - start_line))
    } else {
        (all_lines[start_line..].join("\n"), None)
    };

    let truncation = truncate_head(
        &selected_content,
        TruncationOptions {
            max_lines: Some(DEFAULT_MAX_LINES),
            max_bytes: Some(DEFAULT_MAX_BYTES),
        },
    );

    let (output_text, details) = if truncation.first_line_exceeds_limit {
        let first_line_size =
            format_size(u64::try_from(all_lines[start_line].len()).unwrap_or(u64::MAX));
        let output = format!(
            "[Line {start_line_display} is {first_line_size}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {} | head -c {DEFAULT_MAX_BYTES}]",
            format_size(u64::try_from(DEFAULT_MAX_BYTES).unwrap_or(u64::MAX)),
            input.path
        );
        (
            output,
            Some(ReadToolDetails {
                truncation: Some(truncation),
            }),
        )
    } else if truncation.truncated {
        let end_line_display = start_line_display + truncation.output_lines - 1;
        let next_offset = end_line_display + 1;
        let mut output = truncation.content.clone();
        match truncation.truncated_by {
            Some(super::TruncatedBy::Lines) => {
                write!(
                    output,
                    "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. Use offset={next_offset} to continue.]"
                )
                .map_err(|error| ToolError::new(format!("Could not format read output: {error}")))?;
            }
            _ => {
                write!(
                    output,
                    "\n\n[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                    format_size(u64::try_from(DEFAULT_MAX_BYTES).unwrap_or(u64::MAX))
                )
                .map_err(|error| ToolError::new(format!("Could not format read output: {error}")))?;
            }
        }
        (
            output,
            Some(ReadToolDetails {
                truncation: Some(truncation),
            }),
        )
    } else if let Some(user_limited) = user_limited_lines {
        if start_line + user_limited < all_lines.len() {
            let remaining = all_lines.len() - (start_line + user_limited);
            let next_offset = start_line + user_limited + 1;
            (
                format!(
                    "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                    truncation.content
                ),
                None,
            )
        } else {
            (truncation.content, None)
        }
    } else {
        (truncation.content, None)
    };

    Ok(AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent::new(output_text))],
        details: details_value(details),
        added_tool_names: None,
        terminate: None,
    })
}

fn details_value(details: Option<ReadToolDetails>) -> Value {
    match details {
        Some(details) => serde_json::to_value(details).unwrap_or_else(|_| json!({})),
        None => Value::Null,
    }
}

fn image_tool_result(processed: ProcessImageResult, non_vision: Option<&str>) -> AgentToolResult {
    match processed {
        ProcessImageResult::Failed(error) => {
            let mut text_note = format!("Read image file [{}]\n{}", error.mime_type, error.message);
            if let Some(note) = non_vision {
                text_note.push('\n');
                text_note.push_str(note);
            }
            AgentToolResult {
                content: vec![ToolResultContent::Text(TextContent::new(text_note))],
                details: Value::Null,
                added_tool_names: None,
                terminate: None,
            }
        }
        ProcessImageResult::Ok(processed) => {
            let ProcessedImage {
                data,
                mime_type,
                hints,
                ..
            } = processed;
            let mut text_note = format!("Read image file [{mime_type}]");
            if !hints.is_empty() {
                text_note.push('\n');
                text_note.push_str(&hints.join("\n"));
            }
            if let Some(note) = non_vision {
                text_note.push('\n');
                text_note.push_str(note);
            }
            let mut content = vec![ToolResultContent::Text(TextContent::new(text_note))];
            if non_vision.is_none() {
                content.push(ToolResultContent::Image(ImageContent::new(data, mime_type)));
            }
            AgentToolResult {
                content,
                details: Value::Null,
                added_tool_names: None,
                terminate: None,
            }
        }
    }
}

fn non_vision_image_note(model: Option<&Model>) -> Option<String> {
    let model = model?;
    if model.input.contains(&ModelInput::Image) {
        None
    } else {
        Some(
            "[Current model does not support images. The image will be omitted from this request.]"
                .to_owned(),
        )
    }
}

async fn ensure_readable(path: &str) -> Result<(), ToolError> {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_file() || meta.is_dir() => {
            // Open for read to mirror access(R_OK).
            tokio::fs::File::open(path)
                .await
                .map_err(|error| ToolError::new(format!("Could not read file {path}: {error}")))?;
            Ok(())
        }
        Ok(_) => Err(ToolError::new(format!(
            "Could not read file {path}: not a regular file"
        ))),
        Err(error) => Err(ToolError::new(format!(
            "Could not read file {path}: {error}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// MIME sniffing (mime.ts)
// ---------------------------------------------------------------------------

/// Detects a supported inline image MIME type from magic bytes.
#[must_use]
pub fn detect_supported_image_mime_type(buffer: &[u8]) -> Option<String> {
    if starts_with(buffer, &[0xff, 0xd8, 0xff]) {
        return if buffer.get(3) == Some(&0xf7) {
            None
        } else {
            Some("image/jpeg".to_owned())
        };
    }
    if starts_with(buffer, &PNG_SIGNATURE) {
        return if is_png(buffer) && !is_animated_png(buffer) {
            Some("image/png".to_owned())
        } else {
            None
        };
    }
    if starts_with_ascii(buffer, 0, b"GIF") {
        return Some("image/gif".to_owned());
    }
    if starts_with_ascii(buffer, 0, b"RIFF") && starts_with_ascii(buffer, 8, b"WEBP") {
        return Some("image/webp".to_owned());
    }
    if starts_with_ascii(buffer, 0, b"BM") && is_bmp(buffer) {
        return Some("image/bmp".to_owned());
    }
    None
}

fn is_png(buffer: &[u8]) -> bool {
    buffer.len() >= 16
        && read_u32_be(buffer, PNG_SIGNATURE.len()) == Some(13)
        && starts_with_ascii(buffer, 12, b"IHDR")
}

fn is_animated_png(buffer: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= buffer.len() {
        let Some(chunk_length) = read_u32_be(buffer, offset) else {
            return false;
        };
        let chunk_type_offset = offset + 4;
        if starts_with_ascii(buffer, chunk_type_offset, b"acTL") {
            return true;
        }
        if starts_with_ascii(buffer, chunk_type_offset, b"IDAT") {
            return false;
        }
        let next_offset = usize::try_from(chunk_length)
            .ok()
            .and_then(|chunk_length| offset.checked_add(8 + chunk_length))
            .and_then(|v| v.checked_add(4));
        let Some(next_offset) = next_offset else {
            return false;
        };
        if next_offset <= offset || next_offset > buffer.len() {
            return false;
        }
        offset = next_offset;
    }
    false
}

fn is_bmp(buffer: &[u8]) -> bool {
    if buffer.len() < 26 {
        return false;
    }
    let Some(declared_file_size) = read_u32_le(buffer, 2) else {
        return false;
    };
    let Some(pixel_data_offset) = read_u32_le(buffer, 10) else {
        return false;
    };
    let Some(dib_header_size) = read_u32_le(buffer, 14) else {
        return false;
    };
    if declared_file_size != 0 && declared_file_size < 26 {
        return false;
    }
    if pixel_data_offset < 14 + dib_header_size {
        return false;
    }
    if declared_file_size != 0 && pixel_data_offset >= declared_file_size {
        return false;
    }

    let (color_planes, bits_per_pixel) = if dib_header_size == 12 {
        (
            read_u16_le(buffer, 22).unwrap_or(0),
            read_u16_le(buffer, 24).unwrap_or(0),
        )
    } else if (40..=124).contains(&dib_header_size) {
        if buffer.len() < 30 {
            return false;
        }
        (
            read_u16_le(buffer, 26).unwrap_or(0),
            read_u16_le(buffer, 28).unwrap_or(0),
        )
    } else {
        return false;
    };

    color_planes == 1 && matches!(bits_per_pixel, 1 | 4 | 8 | 16 | 24 | 32)
}

fn read_u16_le(buffer: &[u8], offset: usize) -> Option<u16> {
    let bytes = buffer.get(offset..)?.get(..2)?.try_into().ok()?;
    Some(u16::from_le_bytes(bytes))
}

fn read_u32_be(buffer: &[u8], offset: usize) -> Option<u32> {
    let bytes = buffer.get(offset..)?.get(..4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn read_u32_le(buffer: &[u8], offset: usize) -> Option<u32> {
    let bytes = buffer.get(offset..)?.get(..4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn starts_with(buffer: &[u8], bytes: &[u8]) -> bool {
    buffer.len() >= bytes.len() && buffer[..bytes.len()] == *bytes
}

fn starts_with_ascii(buffer: &[u8], offset: usize, text: &[u8]) -> bool {
    buffer
        .get(offset..offset + text.len())
        .is_some_and(|slice| slice == text)
}

// ---------------------------------------------------------------------------
// Image processing (image-process.ts + image-resize-core.ts)
// ---------------------------------------------------------------------------

/// Successfully processed image data ready for an inline attachment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessedImage {
    /// Base64-encoded attachment bytes.
    pub data: String,
    /// MIME type of the encoded attachment.
    pub mime_type: String,
    /// Human-readable conversion and resize hints.
    pub hints: Vec<String>,
    /// Original and delivered dimensions when the resize pipeline decoded them.
    pub dimensions: Option<ProcessedImageDimensions>,
}

/// Original and delivered image dimensions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessedImageDimensions {
    /// Width after applying EXIF orientation, before resizing.
    pub original_width: u32,
    /// Height after applying EXIF orientation, before resizing.
    pub original_height: u32,
    /// Delivered attachment width.
    pub width: u32,
    /// Delivered attachment height.
    pub height: u32,
    /// Whether the delivered bytes were re-encoded by the resize pipeline.
    pub was_resized: bool,
}

/// Failure produced when image bytes cannot become a supported attachment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessImageError {
    /// MIME type detected for the source bytes.
    pub mime_type: String,
    /// Exact user-facing omission notice.
    pub message: String,
}

/// Result of [`process_image_bytes`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessImageResult {
    /// Supported attachment data.
    Ok(ProcessedImage),
    /// Conversion or resizing failure.
    Failed(ProcessImageError),
}

struct NormalizedImage {
    bytes: Vec<u8>,
    mime_type: String,
    converted_from: Option<String>,
}

struct ResizedImage {
    data: String,
    mime_type: String,
    original_width: u32,
    original_height: u32,
    width: u32,
    height: u32,
    was_resized: bool,
}

/// Processes detected image bytes using the same pipeline as [`ReadTool`].
///
/// Supported JPEG/PNG/GIF/WebP bytes are preserved when already within
/// provider limits. BMP and other decodable inputs are normalized to PNG.
/// With `auto_resize_images`, EXIF orientation is applied and output is kept
/// below the 2000×2000 and 4.5 MiB base64 limits.
#[must_use]
pub fn process_image_bytes(
    bytes: &[u8],
    mime_type: &str,
    auto_resize_images: bool,
) -> ProcessImageResult {
    let Some(normalized) = normalize_image(bytes, mime_type) else {
        return ProcessImageResult::Failed(ProcessImageError {
            mime_type: mime_type.to_owned(),
            message: "[Image omitted: could not be converted to a supported inline image format.]"
                .to_owned(),
        });
    };

    if auto_resize_images {
        let Some(resized) = resize_image(&normalized.bytes, &normalized.mime_type) else {
            return ProcessImageResult::Failed(ProcessImageError {
                mime_type: normalized.mime_type,
                message: "[Image omitted: could not be resized below the inline image size limit.]"
                    .to_owned(),
            });
        };
        let mut hints = Vec::new();
        if let Some(hint) =
            conversion_hint(normalized.converted_from.as_deref(), &resized.mime_type)
        {
            hints.push(hint);
        }
        if let Some(note) = format_dimension_note(&resized) {
            hints.push(note);
        }
        let dimensions = ProcessedImageDimensions {
            original_width: resized.original_width,
            original_height: resized.original_height,
            width: resized.width,
            height: resized.height,
            was_resized: resized.was_resized,
        };
        return ProcessImageResult::Ok(ProcessedImage {
            data: resized.data,
            mime_type: resized.mime_type,
            hints,
            dimensions: Some(dimensions),
        });
    }

    let mut hints = Vec::new();
    if let Some(hint) = conversion_hint(normalized.converted_from.as_deref(), &normalized.mime_type)
    {
        hints.push(hint);
    }
    ProcessImageResult::Ok(ProcessedImage {
        data: BASE64.encode(&normalized.bytes),
        mime_type: normalized.mime_type,
        hints,
        dimensions: None,
    })
}

fn base_mime_type(mime_type: &str) -> String {
    mime_type
        .split(';')
        .next()
        .unwrap_or(mime_type)
        .trim()
        .to_ascii_lowercase()
}

fn normalize_supported_image_mime_type(mime_type: &str) -> Option<&'static str> {
    match base_mime_type(mime_type).as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

fn normalize_image(bytes: &[u8], mime_type: &str) -> Option<NormalizedImage> {
    if let Some(normalized) = normalize_supported_image_mime_type(mime_type) {
        return Some(NormalizedImage {
            bytes: bytes.to_vec(),
            mime_type: normalized.to_owned(),
            converted_from: None,
        });
    }

    let png_bytes = convert_image_bytes_to_png(bytes)?;
    Some(NormalizedImage {
        bytes: png_bytes,
        mime_type: "image/png".to_owned(),
        converted_from: Some(base_mime_type(mime_type)),
    })
}

fn conversion_hint(from: Option<&str>, to: &str) -> Option<String> {
    let from = from?;
    if from == to {
        None
    } else {
        Some(format!("[Image converted from {from} to {to}.]"))
    }
}

/// Decode bytes into PNG, applying the decoder's reported orientation.
///
/// Shared with the clipboard/Kitty display paths so the image decoder is not
/// duplicated. Returns `None` when the bytes cannot be decoded.
#[must_use]
pub fn convert_image_bytes_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.no_limits();
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .ok()?;
    Some(out)
}

fn resize_image(input_bytes: &[u8], mime_type: &str) -> Option<ResizedImage> {
    let input_base64_size = input_bytes.len().div_ceil(3) * 4;

    let mut reader = ImageReader::new(Cursor::new(input_bytes))
        .with_guessed_format()
        .ok()?;
    reader.no_limits();
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);

    let original_width = image.width();
    let original_height = image.height();
    let format = mime_type.split('/').nth(1).unwrap_or("png");

    if original_width <= IMAGE_MAX_DIMENSION
        && original_height <= IMAGE_MAX_DIMENSION
        && input_base64_size < IMAGE_MAX_BASE64_BYTES
    {
        return Some(ResizedImage {
            data: BASE64.encode(input_bytes),
            mime_type: if mime_type.is_empty() {
                format!("image/{format}")
            } else {
                mime_type.to_owned()
            },
            original_width,
            original_height,
            width: original_width,
            height: original_height,
            was_resized: false,
        });
    }

    let mut target_width = original_width;
    let mut target_height = original_height;
    if target_width > IMAGE_MAX_DIMENSION {
        target_height = rounded_scaled_dimension(target_height, IMAGE_MAX_DIMENSION, target_width);
        target_width = IMAGE_MAX_DIMENSION;
    }
    if target_height > IMAGE_MAX_DIMENSION {
        target_width = rounded_scaled_dimension(target_width, IMAGE_MAX_DIMENSION, target_height);
        target_height = IMAGE_MAX_DIMENSION;
    }
    target_width = target_width.max(1);
    target_height = target_height.max(1);

    let mut quality_steps = vec![IMAGE_JPEG_QUALITY, 85, 70, 55, 40];
    quality_steps.sort_unstable();
    quality_steps.dedup();
    // Preserve first-try order: preferred quality first, then the rest descending-ish.
    let mut ordered = vec![IMAGE_JPEG_QUALITY];
    for q in [85_u8, 70, 55, 40] {
        if q != IMAGE_JPEG_QUALITY {
            ordered.push(q);
        }
    }

    let mut current_width = target_width;
    let mut current_height = target_height;

    loop {
        let candidates = try_encodings(&image, current_width, current_height, &ordered);
        for candidate in candidates {
            if candidate.encoded_size < IMAGE_MAX_BASE64_BYTES {
                return Some(ResizedImage {
                    data: candidate.data,
                    mime_type: candidate.mime_type,
                    original_width,
                    original_height,
                    width: current_width,
                    height: current_height,
                    was_resized: true,
                });
            }
        }

        if current_width == 1 && current_height == 1 {
            break;
        }
        let next_width = if current_width == 1 {
            1
        } else {
            current_width.saturating_mul(3) / 4
        };
        let next_height = if current_height == 1 {
            1
        } else {
            current_height.saturating_mul(3) / 4
        };
        if next_width == current_width && next_height == current_height {
            break;
        }
        current_width = next_width;
        current_height = next_height;
    }

    None
}

struct EncodedCandidate {
    data: String,
    encoded_size: usize,
    mime_type: String,
}

fn try_encodings(
    image: &DynamicImage,
    width: u32,
    height: u32,
    jpeg_qualities: &[u8],
) -> Vec<EncodedCandidate> {
    let resized = if image.width() == width && image.height() == height {
        image.clone()
    } else {
        image.resize_exact(width, height, FilterType::Lanczos3)
    };

    let mut candidates = Vec::new();
    if let Some(png) = encode_png(&resized) {
        candidates.push(encode_candidate(&png, "image/png"));
    }
    for &quality in jpeg_qualities {
        if let Some(jpeg) = encode_jpeg(&resized, quality) {
            candidates.push(encode_candidate(&jpeg, "image/jpeg"));
        }
    }
    candidates
}

fn encode_candidate(bytes: &[u8], mime_type: &str) -> EncodedCandidate {
    let data = BASE64.encode(bytes);
    let encoded_size = data.len();
    EncodedCandidate {
        data,
        encoded_size,
        mime_type: mime_type.to_owned(),
    }
}

fn encode_png(image: &DynamicImage) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .ok()?;
    Some(out)
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let rgb = image.to_rgb8();
    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut out, quality);
    encoder
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .ok()?;
    Some(out)
}

fn format_dimension_note(result: &ResizedImage) -> Option<String> {
    if !result.was_resized {
        return None;
    }
    let scale = f64::from(result.original_width) / f64::from(result.width.max(1));
    Some(format!(
        "[Image: original {}x{}, displayed at {}x{}. Multiply coordinates by {:.2} to map to original image.]",
        result.original_width, result.original_height, result.width, result.height, scale
    ))
}

// ---------------------------------------------------------------------------
// Schema helpers
// ---------------------------------------------------------------------------

fn read_parameters_schema() -> Value {
    normalize_tool_schema(schemars::schema_for!(ReadToolInput))
}

fn normalize_tool_schema(schema: schemars::Schema) -> Value {
    let mut value = serde_json::to_value(schema).unwrap_or_else(|_| Value::Object(Map::new()));
    if let Value::Object(map) = &mut value {
        map.remove("$schema");
        map.remove("title");
        map.remove("description");
        map.remove("additionalProperties");
        normalize_schema_node(map);
    }
    value
}

fn normalize_schema_node(map: &mut Map<String, Value>) {
    map.remove("format");
    // schemars represents Option<T> as ["number","null"]; TypeBox optional
    // numbers are just "number".
    if let Some(Value::Array(types)) = map.get("type").cloned() {
        let non_null: Vec<Value> = types
            .into_iter()
            .filter(|t| t.as_str() != Some("null"))
            .collect();
        if non_null.len() == 1 {
            map.insert("type".to_owned(), non_null[0].clone());
        } else if !non_null.is_empty() {
            map.insert("type".to_owned(), Value::Array(non_null));
        }
    }
    let keys: Vec<String> = map.keys().cloned().collect();
    for key in keys {
        match map.get_mut(&key) {
            Some(Value::Object(child)) => normalize_schema_node(child),
            Some(Value::Array(items)) => {
                for item in items {
                    if let Value::Object(child) = item {
                        normalize_schema_node(child);
                    }
                }
            }
            _ => {}
        }
    }
}

fn throw_if_cancelled(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError::new("Operation aborted"))
    } else {
        Ok(())
    }
}

fn path_error(error: &PathResolveError) -> ToolError {
    ToolError::new(error.to_string())
}

/// Builds an [`Arc<dyn AgentTool>`] read tool for `cwd`.
#[must_use]
pub fn create_read_tool(cwd: impl Into<PathBuf>) -> Arc<dyn AgentTool> {
    Arc::new(ReadTool::new(cwd))
}

fn floored_nonnegative_usize(value: f64) -> usize {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    value.floor().to_string().parse().unwrap_or(usize::MAX)
}

fn rounded_scaled_dimension(value: u32, numerator: u32, denominator: u32) -> u32 {
    let denominator = u64::from(denominator);
    let scaled = u64::from(value) * u64::from(numerator);
    u32::try_from((scaled + denominator / 2) / denominator).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::types::ModelCost;
    use serde_json::json;
    use tempfile::tempdir;

    fn fixture_schema() -> Result<Value, serde_json::Error> {
        let text = include_str!("../../../tests/fixtures/tool-schemas/read.json");
        serde_json::from_str(text)
    }

    fn text_model() -> Model {
        Model {
            id: "text-only".to_owned(),
            name: "text-only".to_owned(),
            api: "test".to_owned(),
            provider: "test".to_owned(),
            base_url: String::new(),
            reasoning: false,
            input: vec![ModelInput::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                tiers: None,
            },
            context_window: 8_000,
            max_tokens: 1_024,
            headers: None,
            compat: None,
            thinking_level_map: None,
            extra: std::collections::BTreeMap::default(),
        }
    }

    fn vision_model() -> Model {
        let mut model = text_model();
        model.id = "vision".to_owned();
        model.name = "vision".to_owned();
        model.input = vec![ModelInput::Text, ModelInput::Image];
        model
    }

    fn json_map(value: Value) -> Result<Map<String, Value>, &'static str> {
        if let Value::Object(map) = value {
            Ok(map)
        } else {
            Err("expected JSON object")
        }
    }

    fn text_of(result: &AgentToolResult) -> String {
        match result.content.first() {
            Some(ToolResultContent::Text(text)) => text.text.clone(),
            _ => String::new(),
        }
    }

    fn tiny_bmp_1x1_red() -> Vec<u8> {
        let mut buffer = vec![0_u8; 58];
        buffer[0] = b'B';
        buffer[1] = b'M';
        buffer[2..6].copy_from_slice(&58_u32.to_le_bytes());
        buffer[10..14].copy_from_slice(&54_u32.to_le_bytes());
        buffer[14..18].copy_from_slice(&40_u32.to_le_bytes());
        buffer[18..22].copy_from_slice(&1_i32.to_le_bytes());
        buffer[22..26].copy_from_slice(&1_i32.to_le_bytes());
        buffer[26..28].copy_from_slice(&1_u16.to_le_bytes());
        buffer[28..30].copy_from_slice(&24_u16.to_le_bytes());
        buffer[30..34].copy_from_slice(&0_u32.to_le_bytes());
        buffer[34..38].copy_from_slice(&4_u32.to_le_bytes());
        buffer[56] = 0xff;
        buffer
    }

    fn solid_png(width: u32, height: u32) -> Result<Vec<u8>, image::ImageError> {
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            width,
            height,
            image::Rgb([10, 20, 30]),
        ));
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)?;
        Ok(out)
    }

    #[test]
    fn schema_matches_typebox_fixture() -> Result<(), Box<dyn std::error::Error>> {
        let schema = ReadTool::parameters_schema();
        assert_eq!(schema, fixture_schema()?);
        Ok(())
    }

    #[test]
    fn detects_bmp_and_png_magic() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            detect_supported_image_mime_type(&tiny_bmp_1x1_red()).as_deref(),
            Some("image/bmp")
        );
        let png = solid_png(2, 2)?;
        assert_eq!(
            detect_supported_image_mime_type(&png).as_deref(),
            Some("image/png")
        );
        Ok(())
    }

    #[test]
    fn public_image_helper_preserves_small_supported_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let png = solid_png(3, 2)?;
        let result = process_image_bytes(&png, "image/png", true);
        let ProcessImageResult::Ok(processed) = result else {
            return Err("expected processed image".into());
        };
        assert_eq!(processed.mime_type, "image/png");
        assert_eq!(
            BASE64.decode(&processed.data).ok().as_deref(),
            Some(png.as_slice())
        );
        assert!(processed.hints.is_empty());
        assert_eq!(
            processed.dimensions,
            Some(ProcessedImageDimensions {
                original_width: 3,
                original_height: 2,
                width: 3,
                height: 2,
                was_resized: false,
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn offset_beyond_end_errors() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("lines.txt");
        tokio::fs::write(&path, "a\nb\nc").await?;
        let tool = ReadTool::new(dir.path());
        let result = tool
            .execute(
                "1",
                json_map(json!({"path": "lines.txt", "offset": 10}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await;
        let Err(err) = result else {
            return Err("expected offset error".into());
        };
        assert_eq!(
            err.message(),
            "Offset 10 is beyond end of file (3 lines total)"
        );
        Ok(())
    }

    #[tokio::test]
    async fn first_huge_line_notice() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("huge.txt");
        let huge = "x".repeat(DEFAULT_MAX_BYTES + 10);
        tokio::fs::write(&path, &huge).await?;
        let tool = ReadTool::new(dir.path());
        let result = tool
            .execute(
                "1",
                json_map(json!({"path": "huge.txt"}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await?;
        let text = text_of(&result);
        assert!(
            text.contains("exceeds") && text.contains("sed -n '1p'"),
            "unexpected: {text}"
        );
        assert!(result.details.get("truncation").is_some());
        Ok(())
    }

    #[tokio::test]
    async fn continuation_notices_for_limit_and_truncation()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("many.txt");
        let mut content = String::new();
        for i in 1..=10 {
            writeln!(content, "line{i}")?;
        }
        tokio::fs::write(&path, &content).await?;
        let tool = ReadTool::new(dir.path());
        let result = tool
            .execute(
                "1",
                json_map(json!({"path": "many.txt", "offset": 1, "limit": 3}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await?;
        let text = text_of(&result);
        assert!(
            text.contains("more lines in file. Use offset=4 to continue."),
            "{text}"
        );

        // Force line truncation via many short lines.
        let path2 = dir.path().join("lots.txt");
        let mut lots = String::new();
        for i in 0..(DEFAULT_MAX_LINES + 50) {
            writeln!(lots, "L{i}")?;
        }
        tokio::fs::write(&path2, &lots).await?;
        let result = tool
            .execute(
                "2",
                json_map(json!({"path": "lots.txt"}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await?;
        let text = text_of(&result);
        assert!(
            text.contains("Showing lines 1-") && text.contains("Use offset="),
            "{text}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_file_errors() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = ReadTool::new(dir.path());
        let result = tool
            .execute(
                "1",
                json_map(json!({"path": "nope.txt"}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await;
        let Err(err) = result else {
            return Err("expected missing-file error".into());
        };
        assert!(
            err.message().contains("Could not read file")
                || err.message().contains("No such file")
                || err.message().contains("not found")
                || err.message().contains("os error"),
            "{}",
            err.message()
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_wins() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("a.txt");
        tokio::fs::write(&path, "hi").await?;
        let tool = ReadTool::new(dir.path());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = tool
            .execute(
                "1",
                json_map(json!({"path": "a.txt"}))?,
                cancel,
                ToolUpdates::noop(),
            )
            .await;
        let Err(err) = result else {
            return Err("expected cancellation error".into());
        };
        assert_eq!(err.message(), "Operation aborted");
        Ok(())
    }

    #[tokio::test]
    async fn supported_image_passthrough_and_bmp_conversion()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let png_path = dir.path().join("tiny.png");
        let png = solid_png(4, 4)?;
        tokio::fs::write(&png_path, &png).await?;
        let tool = ReadTool::with_options(
            ReadToolOptions::new(dir.path()).with_model(Some(vision_model())),
        );
        let result = tool
            .execute(
                "1",
                json_map(json!({"path": "tiny.png"}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await?;
        assert!(matches!(
            result.content.get(1),
            Some(ToolResultContent::Image(_))
        ));
        let text = text_of(&result);
        assert!(text.starts_with("Read image file [image/png]"), "{text}");

        let bmp_path = dir.path().join("tiny.bmp");
        tokio::fs::write(&bmp_path, tiny_bmp_1x1_red()).await?;
        let result = tool
            .execute(
                "2",
                json_map(json!({"path": "tiny.bmp"}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await?;
        let text = text_of(&result);
        assert!(
            text.contains("[Image converted from image/bmp to image/png.]"),
            "{text}"
        );
        match result.content.get(1) {
            Some(ToolResultContent::Image(image)) => {
                assert_eq!(image.mime_type, "image/png");
                let decoded = BASE64.decode(&image.data)?;
                assert_eq!(&decoded[..4], &[0x89, b'P', b'N', b'G']);
            }
            other => return Err(format!("expected image content, got {other:?}").into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn resize_dimensions_and_size() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("big.png");
        // Large solid image forces dimension reduction.
        let png = solid_png(2500, 100)?;
        tokio::fs::write(&path, &png).await?;
        let tool = ReadTool::with_options(
            ReadToolOptions::new(dir.path()).with_model(Some(vision_model())),
        );
        let result = tool
            .execute(
                "1",
                json_map(json!({"path": "big.png"}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await?;
        let text = text_of(&result);
        assert!(
            text.contains("original 2500x100") && text.contains("displayed at"),
            "{text}"
        );
        match result.content.get(1) {
            Some(ToolResultContent::Image(image)) => {
                assert!(image.data.len() < IMAGE_MAX_BASE64_BYTES);
            }
            other => return Err(format!("expected image, got {other:?}").into()),
        }
        Ok(())
    }

    #[tokio::test]
    async fn non_vision_note_omits_image() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("pic.png");
        tokio::fs::write(&path, solid_png(8, 8)?).await?;
        let tool =
            ReadTool::with_options(ReadToolOptions::new(dir.path()).with_model(Some(text_model())));
        let result = tool
            .execute(
                "1",
                json_map(json!({"path": "pic.png"}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await?;
        assert_eq!(result.content.len(), 1);
        let text = text_of(&result);
        assert!(
            text.contains(
                "[Current model does not support images. The image will be omitted from this request.]"
            ),
            "{text}"
        );
        Ok(())
    }

    #[test]
    fn sniffs_only_first_chunk_size_constant() {
        // Guard the constant used by file sniffing parity.
        assert_eq!(IMAGE_TYPE_SNIFF_BYTES, 4100);
    }
}
