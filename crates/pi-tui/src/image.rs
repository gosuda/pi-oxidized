//! Terminal image protocol encoders and header dimension parsers.
//!
//! Ports `.references/pi-2.0/packages/tui/src/terminal-image.ts` for Kitty and
//! iTerm2 inline graphics. No stdin picker, no terminal writes — callers emit
//! the returned bytes through frame annotations.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use ratatui::layout::Rect;

use crate::frame::RawRegion;
use crate::terminal::caps::{CellDimensions, ImageProtocol};

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

/// How a retained image should be emitted to the terminal.
///
/// `Upload` carries the complete image transmission. `Placement` references a
/// Kitty image that the terminal already has in its image store and carries
/// only a placement command. iTerm2 has no placement-only equivalent, so it
/// always uses `Upload`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageEmission {
    /// Upload image data and place it.
    Upload,
    /// Place an image that was uploaded previously.
    Placement,
}

/// Errors raised while turning a native image or retained image line into a
/// document image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageError {
    /// The image payload is not valid standard base64.
    InvalidEncoding,
    /// The protocol controls are malformed or incomplete.
    InvalidProtocol,
    /// The image dimensions or cell placement are not positive.
    InvalidDimensions,
    /// The protocol sequence does not have a complete terminator.
    InvalidTermination,
    /// Kitty placement metadata was not sufficient to identify the image.
    MissingMetadata,
    /// The image's dimensions or memory accounting overflowed.
    Overflow,
}

/// Metadata retained for a Kitty image upload.
///
/// This is the native equivalent of the metadata table used by the upstream
/// terminal-image module. It lets retained raw lines recover pixel dimensions
/// for checked viewport cropping without copying the image into a text row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyImageMetadata {
    /// Kitty image identity.
    pub image_id: u32,
    /// Placement width in terminal columns.
    pub columns: u16,
    /// Placement height in terminal rows.
    pub rows: u16,
    /// Source image width in pixels.
    pub width_px: u32,
    /// Source image height in pixels.
    pub height_px: u32,
    /// Monotonic transmission generation.
    pub transmission_generation: u64,
}

/// Options used by native image components when preparing a retained image.
///
/// This type is deliberately about validated native image preparation. There
/// is no public constructor taking arbitrary encoded bytes; retained raw lines
/// must go through [`DocumentImage::try_from_line`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DocumentImageOptions {
    /// Maximum placement width in cells.
    pub max_width_cells: Option<u16>,
    /// Maximum placement height in cells.
    pub max_height_cells: Option<u16>,
    /// Optional filename for an image fallback.
    pub filename: Option<String>,
    /// Optional stable Kitty image id.
    pub image_id: Option<u32>,
    /// Native terminal image protocol selected for this preparation.
    pub protocol: Option<ImageProtocol>,
    /// Pixel dimensions of one terminal cell.
    pub cell_dimensions: CellDimensions,
}

/// One validated, retained terminal image.
///
/// The payload, protocol sequence, placement metadata, and fallback are
/// private by design. Consumers can borrow dimensions and ask this type for a
/// checked [`RawRegion`], but cannot manufacture a region from arbitrary bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentImage {
    protocol: Option<ImageProtocol>,
    dimensions: ImageDimensions,
    columns: u16,
    rows: u16,
    image_id: Option<u32>,
    sequence: Arc<[u8]>,
    fallback: Option<Arc<str>>,
    transmission_generation: u64,
    transmission_bytes: usize,
    estimated_decoded_bytes: usize,
}

impl DocumentImage {
    /// Prepare a document image from native component data.
    ///
    /// This is crate-visible rather than public: image components are the
    /// authority allowed to introduce a payload into a retained document.
    pub(crate) fn from_native(
        base64_data: &str,
        mime_type: &str,
        dimensions: ImageDimensions,
        options: &DocumentImageOptions,
    ) -> Result<Self, ImageError> {
        if options.protocol.is_some()
            && (base64_data.is_empty() || decode_b64(base64_data).is_none())
        {
            return Err(ImageError::InvalidEncoding);
        }
        if mime_type.is_empty() || dimensions.width_px == 0 || dimensions.height_px == 0 {
            return Err(ImageError::InvalidDimensions);
        }
        let cell = options.cell_dimensions;
        let max_width = options.max_width_cells.unwrap_or(60).max(1);
        let default_max_height = u32::from(max_width)
            .checked_mul(u32::from(cell.width.max(1)))
            .ok_or(ImageError::Overflow)?
            .div_ceil(u32::from(cell.height.max(1)));
        let default_max_height = u16::try_from(default_max_height.max(1))
            .unwrap_or(u16::MAX)
            .max(1);
        let max_height = options
            .max_height_cells
            .unwrap_or(default_max_height)
            .max(1);
        let size = calculate_image_cell_size(dimensions, max_width, Some(max_height), cell);
        let decoded_bytes = usize::try_from(
            u128::from(dimensions.width_px)
                .checked_mul(u128::from(dimensions.height_px))
                .and_then(|v| v.checked_mul(4))
                .ok_or(ImageError::Overflow)?,
        )
        .map_err(|_| ImageError::Overflow)?;
        let transmission_generation = next_transmission_generation();
        let (protocol, image_id, sequence) = match options.protocol {
            Some(ImageProtocol::Kitty) => {
                let image_id = options.image_id.unwrap_or_else(allocate_image_id);
                validate_image_id(image_id)?;
                let sequence = encode_kitty(
                    base64_data,
                    KittyEncodeOptions {
                        columns: Some(size.columns),
                        rows: Some(size.rows),
                        image_id: Some(image_id),
                        move_cursor: Some(false),
                    },
                );
                register_kitty_image_metadata(KittyImageMetadata {
                    image_id,
                    columns: size.columns,
                    rows: size.rows,
                    width_px: dimensions.width_px,
                    height_px: dimensions.height_px,
                    transmission_generation,
                });
                (Some(ImageProtocol::Kitty), Some(image_id), sequence)
            }
            Some(ImageProtocol::ITerm2) => {
                let sequence = encode_iterm2(
                    base64_data,
                    ITerm2EncodeOptions {
                        width: Some(size.columns.to_string()),
                        height: Some("auto".to_owned()),
                        name: options.filename.clone(),
                        preserve_aspect_ratio: None,
                        inline: Some(true),
                    },
                );
                (Some(ImageProtocol::ITerm2), None, sequence)
            }
            None => (None, None, String::new()),
        };
        let fallback = if protocol.is_none() {
            Some(Arc::<str>::from(image_fallback(
                mime_type,
                Some(dimensions),
                options.filename.as_deref(),
            )))
        } else {
            None
        };
        Ok(Self {
            protocol,
            dimensions,
            columns: size.columns,
            rows: if protocol.is_none() {
                1
            } else {
                size.rows.max(1)
            },
            image_id,
            sequence: Arc::<[u8]>::from(sequence.into_bytes()),
            fallback,
            transmission_generation,
            transmission_bytes: 0,
            estimated_decoded_bytes: decoded_bytes,
        }
        .with_transmission_length())
    }

    /// Parse a complete retained image line using default cell dimensions.
    pub(crate) fn try_from_line(line: &str) -> Result<Option<Self>, ImageError> {
        Self::try_from_line_with_cell(line, CellDimensions::default())
    }

    /// Parse a complete retained image line with the measured cell size.
    /// A non-image line returns `Ok(None)`. A line that starts or contains an
    /// image introducer but fails protocol, payload, termination, or registered
    /// Kitty metadata validation returns an error instead of becoming
    /// searchable text.
    pub(crate) fn try_from_line_with_cell(
        line: &str,
        cell: CellDimensions,
    ) -> Result<Option<Self>, ImageError> {
        if let Some(start) = line.find(KITTY_PREFIX) {
            return parse_kitty_document_image(line, start).map(Some);
        }
        if let Some(start) = line.find(ITERM2_PREFIX) {
            return parse_iterm2_document_image(line, start, cell).map(Some);
        }
        Ok(None)
    }

    /// Image protocol, or `None` for a validated text fallback.
    #[must_use]
    pub const fn protocol(&self) -> Option<ImageProtocol> {
        self.protocol
    }

    /// Whether this image is rendered through its text fallback.
    #[must_use]
    pub const fn is_fallback(&self) -> bool {
        self.protocol.is_none()
    }

    /// Borrow the validated fallback text, if this image has one.
    #[must_use]
    pub fn fallback_text(&self) -> Option<&str> {
        self.fallback.as_deref()
    }

    /// Pixel dimensions of the source image.
    #[must_use]
    pub const fn dimensions(&self) -> ImageDimensions {
        self.dimensions
    }

    /// Placement width in terminal columns.
    #[must_use]
    pub const fn columns(&self) -> u16 {
        self.columns
    }

    /// Placement height in terminal rows.
    #[must_use]
    pub const fn rows(&self) -> u16 {
        self.rows
    }

    /// Kitty image id, when this image has one.
    #[must_use]
    pub const fn image_id(&self) -> Option<u32> {
        self.image_id
    }

    /// Native transmission generation used for cache identity.
    #[must_use]
    pub const fn transmission_generation(&self) -> u64 {
        self.transmission_generation
    }

    /// Encoded transmission size used by the offscreen cache.
    #[must_use]
    pub const fn transmission_bytes(&self) -> usize {
        self.transmission_bytes
    }

    /// Estimated decoded RGBA memory used by the offscreen cache.
    #[must_use]
    pub const fn estimated_decoded_bytes(&self) -> usize {
        self.estimated_decoded_bytes
    }

    /// Build a frame annotation for a visible image slice.
    ///
    /// `hidden_rows` is the number of logical image rows above the viewport.
    /// The source pixel rectangle uses checked floor/ceil mapping and the
    /// placement controls are replaced atomically, never duplicated.
    #[must_use]
    pub fn raw_region(
        &self,
        area: Rect,
        hidden_rows: usize,
        visible_rows: usize,
        emission: ImageEmission,
    ) -> Option<RawRegion> {
        if area.width == 0
            || area.height == 0
            || self.protocol.is_none()
            || visible_rows == 0
            || hidden_rows >= usize::from(self.rows)
        {
            return None;
        }
        let visible_rows = visible_rows.min(usize::from(self.rows) - hidden_rows);
        let bytes = self.sequence_for_rows(hidden_rows, visible_rows, emission)?;
        Some(RawRegion {
            area,
            bytes,
            kitty_id: (self.protocol == Some(ImageProtocol::Kitty))
                .then_some(self.image_id)
                .flatten(),
        })
    }

    /// Return a checked Kitty sequence for a logical image slice.
    #[must_use]
    pub(crate) fn sequence_for_rows(
        &self,
        hidden_rows: usize,
        visible_rows: usize,
        emission: ImageEmission,
    ) -> Option<Vec<u8>> {
        let protocol = self.protocol?;
        if visible_rows == 0 || hidden_rows >= usize::from(self.rows) {
            return None;
        }
        let visible_rows = visible_rows.min(usize::from(self.rows) - hidden_rows);
        match protocol {
            ImageProtocol::Kitty => {
                let full = std::str::from_utf8(&self.sequence).ok()?;
                let cropped = crop_kitty_image_line(full, hidden_rows, visible_rows);
                if emission == ImageEmission::Upload {
                    Some(cropped.into_bytes())
                } else {
                    Some(kitty_placement_from_line(&cropped, self.image_id?)?.into_bytes())
                }
            }
            // iTerm2 carries no source-rect control, so a partial slice cannot
            // be cropped. Refuse the region; the caller paints the text
            // fallback instead of emitting the whole bitmap over the band.
            ImageProtocol::ITerm2 if hidden_rows == 0 => Some(self.sequence.to_vec()),
            ImageProtocol::ITerm2 => None,
        }
    }

    fn with_transmission_length(mut self) -> Self {
        self.transmission_bytes = self.sequence.len();
        self
    }
}

/// An explicit deletion produced when a cached offscreen image is evicted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageCacheEviction {
    /// Kitty image id to release.
    pub image_id: u32,
    /// Protocol bytes consumed by the writer's existing output stage.
    pub deletion: Vec<u8>,
    /// Transmission generation that was evicted.
    pub transmission_generation: u64,
}

/// An upload required when a visible image generation is not cached.
///
/// The visible [`DocumentImage`] remains the single owner of its immutable
/// sequence. The caller emits that sequence through `raw_region(Upload)`,
/// which applies viewport cropping; this record only carries cache identity
/// and admission bookkeeping, avoiding a second full-sequence allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageCacheUpload {
    /// Kitty image id being uploaded.
    pub image_id: u32,
    /// Transmission generation represented by the upload.
    pub transmission_generation: u64,
    /// Encoded transmission length used for admission accounting.
    pub transmission_bytes: usize,
    /// Whether the image was admitted to the bounded cache.
    pub cached: bool,
}

/// Output from one image-cache operation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImageCacheOutput {
    /// Upload identities required by the current frame. Emit the corresponding
    /// bytes once through [`DocumentImage::raw_region`] with `Upload`.
    pub uploads: Vec<ImageCacheUpload>,
    /// Deletions that must be staged before replacement/current-frame image
    /// regions so an evicted id cannot delete a newly uploaded generation.
    pub evictions: Vec<ImageCacheEviction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CachedKittyImage {
    image_id: u32,
    transmission_generation: u64,
    transmission_bytes: usize,
    estimated_decoded_bytes: usize,
}

/// Bounded cache for Kitty image uploads that are no longer visible.
///
/// The cache deliberately stores metadata only. The immutable upload remains
/// owned by `DocumentImage`; the frame caller emits it through the checked
/// [`DocumentImage::raw_region`] path.
#[derive(Debug, Clone, Default)]
pub struct KittyImageCache {
    entries: VecDeque<CachedKittyImage>,
    transmission_bytes: usize,
    decoded_bytes: usize,
}

/// Maximum number of cached offscreen Kitty images.
pub const MAX_CACHED_OFFSCREEN_KITTY_IMAGES: usize = 16;
/// Maximum total encoded transmission bytes for cached offscreen images.
pub const MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES: usize = 32 * 1024 * 1024;
/// Maximum total estimated decoded bytes for cached offscreen images.
pub const MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES: usize = 64 * 1024 * 1024;

impl KittyImageCache {
    /// Create an empty bounded cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of image generations retained for upload/placement decisions.
    ///
    /// Visible entries remain tracked while they are on screen. The bounded
    /// limits apply to entries that become offscreen, matching the upstream
    /// cache rather than imposing a limit on one visible frame.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no image generation is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Current encoded-byte total for retained offscreen entries.
    #[must_use]
    pub const fn transmission_bytes(&self) -> usize {
        self.transmission_bytes
    }

    /// Current estimated decoded-byte total for retained offscreen entries.
    #[must_use]
    pub const fn decoded_bytes(&self) -> usize {
        self.decoded_bytes
    }

    /// Prepare visible Kitty images and evict old offscreen entries.
    ///
    /// The iterator is the complete set of images visible in the next frame.
    /// Existing uploads become placement-only at the caller's paint step;
    /// unseen generations produce an upload. Entries are touched in iterator
    /// order. Only entries that are not visible count toward the C-compatible
    /// bounds and are eligible for eviction.
    #[must_use]
    pub fn prepare_frame<'a, I>(&mut self, visible: I) -> ImageCacheOutput
    where
        I: IntoIterator<Item = &'a DocumentImage>,
    {
        let visible: Vec<&DocumentImage> = visible
            .into_iter()
            .filter(|image| image.protocol == Some(ImageProtocol::Kitty))
            .collect();
        let visible_ids: Vec<u32> = visible.iter().filter_map(|image| image.image_id).collect();
        let mut output = ImageCacheOutput::default();
        let mut seen_ids = Vec::with_capacity(visible_ids.len());
        for image in visible {
            let Some(image_id) = image.image_id else {
                continue;
            };
            if seen_ids.contains(&image_id) {
                continue;
            }
            seen_ids.push(image_id);
            let index = self
                .entries
                .iter()
                .position(|entry| entry.image_id == image_id);
            let current = index.and_then(|index| self.entries.get(index).copied());
            if current
                .is_some_and(|entry| entry.transmission_generation == image.transmission_generation)
            {
                if let Some(index) = index
                    && let Some(entry) = self.entries.remove(index)
                {
                    self.entries.push_back(entry);
                }
                continue;
            }
            if let Some(index) = index
                && let Some(old) = self.entries.remove(index)
            {
                output.evictions.push(cache_eviction(old));
            }
            let transmission_bytes = image.transmission_bytes;
            let decoded_bytes = image.estimated_decoded_bytes;
            let admissible = transmission_bytes <= MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES
                && decoded_bytes <= MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES;
            output.uploads.push(ImageCacheUpload {
                image_id,
                transmission_generation: image.transmission_generation,
                transmission_bytes,
                cached: admissible,
            });
            if admissible {
                self.entries.push_back(CachedKittyImage {
                    image_id,
                    transmission_generation: image.transmission_generation,
                    transmission_bytes,
                    estimated_decoded_bytes: decoded_bytes,
                });
            }
        }
        self.evict_offscreen(&visible_ids, &mut output.evictions);
        output
    }

    /// Prepare one visible image.
    #[must_use]
    pub fn prepare_image(&mut self, image: &DocumentImage) -> ImageCacheOutput {
        self.prepare_frame(std::iter::once(image))
    }

    /// Whether this exact image generation is currently uploaded/cached.
    #[must_use]
    pub fn is_uploaded(&self, image: &DocumentImage) -> bool {
        image.protocol == Some(ImageProtocol::Kitty)
            && image.image_id.is_some_and(|image_id| {
                self.entries.iter().any(|entry| {
                    entry.image_id == image_id
                        && entry.transmission_generation == image.transmission_generation
                })
            })
    }

    /// Select upload versus placement for an image after [`Self::prepare_frame`].
    ///
    /// A visible image included in this output must use `Upload` even though
    /// it is now retained in cache metadata; only a prior generation may use
    /// a placement-only command.
    #[must_use]
    pub fn emission_for(&self, image: &DocumentImage, output: &ImageCacheOutput) -> ImageEmission {
        if image.protocol != Some(ImageProtocol::Kitty)
            || output.uploads.iter().any(|upload| {
                Some(upload.image_id) == image.image_id
                    && upload.transmission_generation == image.transmission_generation
            })
            || !self.is_uploaded(image)
        {
            ImageEmission::Upload
        } else {
            ImageEmission::Placement
        }
    }

    /// Release all cached image ids for a mode/session transition.
    #[must_use]
    pub fn clear(&mut self) -> Vec<ImageCacheEviction> {
        let evictions = self.entries.iter().copied().map(cache_eviction).collect();
        self.entries.clear();
        self.transmission_bytes = 0;
        self.decoded_bytes = 0;
        evictions
    }

    fn evict_offscreen(&mut self, visible_ids: &[u32], evictions: &mut Vec<ImageCacheEviction>) {
        loop {
            let mut offscreen_count: usize = 0;
            let mut transmission_bytes: usize = 0;
            let mut decoded_bytes: usize = 0;
            for entry in &self.entries {
                if visible_ids.contains(&entry.image_id) {
                    continue;
                }
                offscreen_count = offscreen_count.saturating_add(1);
                transmission_bytes = transmission_bytes.saturating_add(entry.transmission_bytes);
                decoded_bytes = decoded_bytes.saturating_add(entry.estimated_decoded_bytes);
            }
            self.transmission_bytes = transmission_bytes;
            self.decoded_bytes = decoded_bytes;
            if offscreen_count <= MAX_CACHED_OFFSCREEN_KITTY_IMAGES
                && transmission_bytes <= MAX_CACHED_OFFSCREEN_KITTY_TRANSMISSION_BYTES
                && decoded_bytes <= MAX_CACHED_OFFSCREEN_KITTY_DECODED_BYTES
            {
                break;
            }
            let Some(index) = self
                .entries
                .iter()
                .position(|entry| !visible_ids.contains(&entry.image_id))
            else {
                break;
            };
            if let Some(old) = self.entries.remove(index) {
                evictions.push(cache_eviction(old));
            }
        }
    }
}

fn cache_eviction(entry: CachedKittyImage) -> ImageCacheEviction {
    ImageCacheEviction {
        image_id: entry.image_id,
        deletion: delete_kitty_image(entry.image_id).into_bytes(),
        transmission_generation: entry.transmission_generation,
    }
}

fn validate_image_id(image_id: u32) -> Result<(), ImageError> {
    if (1..=MAX_IMAGE_ID).contains(&image_id) {
        Ok(())
    } else {
        Err(ImageError::InvalidProtocol)
    }
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

/// Delete all visible Kitty placements while retaining uploaded image data.
#[must_use]
pub fn delete_all_kitty_placements() -> String {
    "\u{1b}_Ga=d,d=a,q=2\u{1b}\\".to_owned()
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

const KITTY_PREFIX: &str = "\u{1b}_G";
const ITERM2_PREFIX: &str = "\u{1b}]1337;File=";
const ST: &str = "\u{1b}\\";
const BEL: char = '\u{7}';

static KITTY_METADATA: LazyLock<Mutex<Vec<KittyImageMetadata>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
static NEXT_TRANSMISSION_GENERATION: AtomicU64 = AtomicU64::new(1);

fn metadata_table() -> &'static Mutex<Vec<KittyImageMetadata>> {
    &KITTY_METADATA
}

fn next_transmission_generation() -> u64 {
    NEXT_TRANSMISSION_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Register metadata for a native Kitty upload.
///
/// The table is bounded and process-local. It exists only so a retained line
/// can recover the source pixel dimensions needed by viewport cropping.
pub(crate) fn register_kitty_image_metadata(metadata: KittyImageMetadata) {
    if validate_image_id(metadata.image_id).is_err()
        || metadata.columns == 0
        || metadata.rows == 0
        || metadata.width_px == 0
        || metadata.height_px == 0
    {
        return;
    }
    let Ok(mut table) = metadata_table().lock() else {
        return;
    };
    table.retain(|entry| entry.image_id != metadata.image_id);
    table.push(metadata);
    if table.len() > 1024 {
        let excess = table.len().saturating_sub(1024);
        table.drain(..excess);
    }
}

fn kitty_metadata(image_id: u32) -> Option<KittyImageMetadata> {
    metadata_table().lock().ok().and_then(|table| {
        table
            .iter()
            .rev()
            .find(|entry| entry.image_id == image_id)
            .copied()
    })
}

#[derive(Debug, Clone, Copy)]
struct KittyHeader {
    controls_start: usize,
    controls_end: usize,
    sequence_end: usize,
    columns: u16,
    rows: u16,
    image_id: u32,
    dimensions: ImageDimensions,
    transmission_generation: u64,
}

#[expect(
    clippy::too_many_lines,
    reason = "Kitty header validation keeps chunk and metadata invariants together"
)]
fn parse_kitty_header(line: &str, start: usize) -> Result<KittyHeader, ImageError> {
    if !line
        .get(start..)
        .is_some_and(|tail| tail.starts_with(KITTY_PREFIX))
    {
        return Err(ImageError::InvalidProtocol);
    }
    let controls_start = start
        .checked_add(KITTY_PREFIX.len())
        .ok_or(ImageError::Overflow)?;
    let controls_end = line
        .get(controls_start..)
        .and_then(|rest| rest.find(';'))
        .and_then(|offset| controls_start.checked_add(offset))
        .ok_or(ImageError::InvalidTermination)?;
    let controls = line
        .get(controls_start..controls_end)
        .ok_or(ImageError::InvalidProtocol)?;
    let parsed = parse_controls(controls)?;
    if !parsed
        .iter()
        .any(|(key, value)| *key == "a" && *value == "T")
    {
        return Err(ImageError::InvalidProtocol);
    }
    let _line_columns = parse_positive_u16(find_control(&parsed, "c"))?;
    let _line_rows = parse_positive_u16(find_control(&parsed, "r"))?;
    let image_id = parse_positive_u32(find_control(&parsed, "i"))?;
    if let Some(marker) = find_control(&parsed, "m")
        && marker != "0"
        && marker != "1"
    {
        return Err(ImageError::InvalidProtocol);
    }
    let metadata = kitty_metadata(image_id).ok_or(ImageError::MissingMetadata)?;
    let columns = metadata.columns;
    let rows = metadata.rows;

    let mut sequence_end = controls_end.checked_add(1).ok_or(ImageError::Overflow)?;
    let mut current_controls = controls;
    loop {
        let terminator_offset = line
            .get(sequence_end..)
            .and_then(|rest| rest.find(ST))
            .ok_or(ImageError::InvalidTermination)?;
        let terminator = sequence_end
            .checked_add(terminator_offset)
            .ok_or(ImageError::Overflow)?;
        let payload = line
            .get(sequence_end..terminator)
            .ok_or(ImageError::InvalidProtocol)?;
        if payload.is_empty() || decode_b64(payload).is_none() {
            return Err(ImageError::InvalidEncoding);
        }
        sequence_end = terminator
            .checked_add(ST.len())
            .ok_or(ImageError::Overflow)?;
        if find_control_in_str(current_controls, "m") != Some("1") {
            break;
        }
        if !line
            .get(sequence_end..)
            .is_some_and(|tail| tail.starts_with(KITTY_PREFIX))
        {
            return Err(ImageError::InvalidTermination);
        }
        let continuation_controls_start = sequence_end
            .checked_add(KITTY_PREFIX.len())
            .ok_or(ImageError::Overflow)?;
        let continuation_controls_end = line
            .get(continuation_controls_start..)
            .and_then(|rest| rest.find(';'))
            .and_then(|offset| continuation_controls_start.checked_add(offset))
            .ok_or(ImageError::InvalidTermination)?;
        current_controls = line
            .get(continuation_controls_start..continuation_controls_end)
            .ok_or(ImageError::InvalidProtocol)?;
        let continuation = parse_controls(current_controls)?;
        let marker = find_control(&continuation, "m").ok_or(ImageError::InvalidProtocol)?;
        if marker != "0" && marker != "1" {
            return Err(ImageError::InvalidProtocol);
        }
        sequence_end = continuation_controls_end
            .checked_add(1)
            .ok_or(ImageError::Overflow)?;
    }
    let dimensions = ImageDimensions {
        width_px: metadata.width_px,
        height_px: metadata.height_px,
    };
    if dimensions.width_px == 0 || dimensions.height_px == 0 {
        return Err(ImageError::InvalidDimensions);
    }
    Ok(KittyHeader {
        controls_start,
        controls_end,
        sequence_end,
        columns,
        rows,
        image_id,
        dimensions,
        transmission_generation: metadata.transmission_generation,
    })
}

fn parse_kitty_document_image(line: &str, start: usize) -> Result<DocumentImage, ImageError> {
    let header = parse_kitty_header(line, start)?;
    let sequence = line
        .get(start..header.sequence_end)
        .ok_or(ImageError::InvalidProtocol)?;
    let decoded_bytes = usize::try_from(
        u128::from(header.dimensions.width_px)
            .checked_mul(u128::from(header.dimensions.height_px))
            .and_then(|v| v.checked_mul(4))
            .ok_or(ImageError::Overflow)?,
    )
    .map_err(|_| ImageError::Overflow)?;
    Ok(DocumentImage {
        protocol: Some(ImageProtocol::Kitty),
        dimensions: header.dimensions,
        columns: header.columns,
        rows: header.rows,
        image_id: Some(header.image_id),
        sequence: Arc::<[u8]>::from(sequence.as_bytes()),
        fallback: None,
        transmission_generation: header.transmission_generation,
        transmission_bytes: sequence.len(),
        estimated_decoded_bytes: decoded_bytes,
    })
}

fn parse_iterm2_document_image(
    line: &str,
    start: usize,
    cell: CellDimensions,
) -> Result<DocumentImage, ImageError> {
    let payload_start = start
        .checked_add(ITERM2_PREFIX.len())
        .ok_or(ImageError::Overflow)?;
    let controls_end = line
        .get(payload_start..)
        .and_then(|rest| rest.find(':'))
        .and_then(|offset| payload_start.checked_add(offset))
        .ok_or(ImageError::InvalidProtocol)?;
    let terminator = line
        .get(controls_end.checked_add(1).ok_or(ImageError::Overflow)?..)
        .and_then(|rest| {
            let bel = rest.find(BEL);
            let st = rest.find(ST);
            match (bel, st) {
                (Some(bel), Some(st)) => Some(bel.min(st)),
                (Some(bel), None) => Some(bel),
                (None, Some(st)) => Some(st),
                (None, None) => None,
            }
        })
        .and_then(|offset| controls_end.checked_add(1 + offset))
        .ok_or(ImageError::InvalidTermination)?;
    let controls = line
        .get(payload_start..controls_end)
        .ok_or(ImageError::InvalidProtocol)?;
    let payload = line
        .get(controls_end.checked_add(1).ok_or(ImageError::Overflow)?..terminator)
        .ok_or(ImageError::InvalidProtocol)?;
    if payload.is_empty() || decode_b64(payload).is_none() {
        return Err(ImageError::InvalidEncoding);
    }
    let parsed = parse_iterm_controls(controls)?;
    let width_cells = parse_cell_control(find_control(&parsed, "width"))?.unwrap_or(1);
    let height_cells = parse_cell_control(find_control(&parsed, "height"))?.unwrap_or(1);
    let dimensions = ImageDimensions {
        width_px: width_cells
            .checked_mul(u32::from(cell.width.max(1)))
            .ok_or(ImageError::Overflow)?,
        height_px: height_cells
            .checked_mul(u32::from(cell.height.max(1)))
            .ok_or(ImageError::Overflow)?,
    };
    let filename = match find_control(&parsed, "name") {
        Some(name) => {
            let filename = String::from_utf8(decode_b64(name).ok_or(ImageError::InvalidEncoding)?)
                .map_err(|_| ImageError::InvalidEncoding)?;
            if filename.chars().any(char::is_control) {
                return Err(ImageError::InvalidProtocol);
            }
            Some(filename)
        }
        None => None,
    };
    let fallback = image_fallback("image/unknown", Some(dimensions), filename.as_deref());
    let decoded_bytes = usize::try_from(
        u128::from(dimensions.width_px)
            .checked_mul(u128::from(dimensions.height_px))
            .and_then(|v| v.checked_mul(4))
            .ok_or(ImageError::Overflow)?,
    )
    .map_err(|_| ImageError::Overflow)?;
    // Fullscreen deliberately disables iTerm2 images. Keep the actual native
    // fallback as a retained image instead of classifying the sequence as text.
    Ok(DocumentImage {
        protocol: None,
        dimensions,
        columns: u16::try_from(width_cells).unwrap_or(u16::MAX).max(1),
        rows: 1,
        image_id: None,
        sequence: Arc::<[u8]>::from([]),
        fallback: Some(Arc::<str>::from(fallback)),
        transmission_generation: next_transmission_generation(),
        transmission_bytes: 0,
        estimated_decoded_bytes: decoded_bytes,
    })
}

fn parse_controls(controls: &str) -> Result<Vec<(&str, &str)>, ImageError> {
    if controls.is_empty() {
        return Err(ImageError::InvalidProtocol);
    }
    controls
        .split(',')
        .map(|control| {
            let (key, value) = control.split_once('=').ok_or(ImageError::InvalidProtocol)?;
            if key.is_empty() || value.is_empty() {
                return Err(ImageError::InvalidProtocol);
            }
            Ok((key, value))
        })
        .collect()
}

fn parse_iterm_controls(controls: &str) -> Result<Vec<(&str, &str)>, ImageError> {
    if controls.is_empty() {
        return Err(ImageError::InvalidProtocol);
    }
    controls
        .split(';')
        .map(|control| {
            let (key, value) = control.split_once('=').ok_or(ImageError::InvalidProtocol)?;
            if key.is_empty() || value.is_empty() {
                return Err(ImageError::InvalidProtocol);
            }
            Ok((key, value))
        })
        .collect()
}

fn find_control<'a>(controls: &'a [(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    controls
        .iter()
        .find_map(|(candidate, value)| (*candidate == key).then_some(*value))
}

fn find_control_in_str<'a>(controls: &'a str, key: &str) -> Option<&'a str> {
    controls.split(',').find_map(|control| {
        let (candidate, value) = control.split_once('=')?;
        (candidate == key).then_some(value)
    })
}

fn parse_positive_u16(value: Option<&str>) -> Result<u16, ImageError> {
    let value = value.ok_or(ImageError::MissingMetadata)?;
    let parsed = value
        .parse::<u16>()
        .map_err(|_| ImageError::InvalidDimensions)?;
    (parsed > 0)
        .then_some(parsed)
        .ok_or(ImageError::InvalidDimensions)
}

fn parse_positive_u32(value: Option<&str>) -> Result<u32, ImageError> {
    let value = value.ok_or(ImageError::MissingMetadata)?;
    let parsed = value
        .parse::<u32>()
        .map_err(|_| ImageError::InvalidProtocol)?;
    validate_image_id(parsed)?;
    Ok(parsed)
}

fn parse_cell_control(value: Option<&str>) -> Result<Option<u32>, ImageError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value == "auto" {
        return Ok(None);
    }
    let parsed = value
        .parse::<u32>()
        .map_err(|_| ImageError::InvalidDimensions)?;
    (parsed > 0)
        .then_some(Some(parsed))
        .ok_or(ImageError::InvalidDimensions)
}

///
/// The returned line is unchanged when it is not a complete Kitty upload, the
/// requested rows are already fully visible, or the slice is outside the
/// image. This mirrors the upstream helper while using checked `u128`
/// arithmetic for floor/ceil source-pixel mapping.
#[must_use]
pub fn crop_kitty_image_line(line: &str, hidden_rows: usize, visible_rows: usize) -> String {
    let Some(start) = line.find(KITTY_PREFIX) else {
        return line.to_owned();
    };
    let Ok(header) = parse_kitty_header(line, start) else {
        return line.to_owned();
    };
    let image_rows = usize::from(header.rows);
    if hidden_rows >= image_rows || visible_rows == 0 {
        return line.to_owned();
    }
    let cropped_rows = visible_rows.min(image_rows - hidden_rows);
    if hidden_rows == 0 && cropped_rows == image_rows {
        return line.to_owned();
    }
    let image_height = u128::from(header.dimensions.height_px);
    let hidden = u128::try_from(hidden_rows).unwrap_or(u128::MAX);
    let rows = u128::from(header.rows);
    let Some(source_y) = image_height
        .checked_mul(hidden)
        .map(|numerator| numerator / rows)
    else {
        return line.to_owned();
    };
    let visible_end = hidden_rows.saturating_add(cropped_rows);
    let visible_end = u128::try_from(visible_end).unwrap_or(u128::MAX);
    let Some(source_end) = image_height
        .checked_mul(visible_end)
        .map(|numerator| numerator.div_ceil(rows))
    else {
        return line.to_owned();
    };
    let source_y = source_y.min(image_height);
    let source_end = source_end.min(image_height);
    let source_height = source_end.saturating_sub(source_y).max(1);
    let Some(controls) = line.get(header.controls_start..header.controls_end) else {
        return line.to_owned();
    };
    let mut replaced: Vec<String> = controls
        .split(',')
        .filter(|control| {
            let key = control.split_once('=').map_or(*control, |(key, _)| key);
            !matches!(key, "y" | "h" | "r")
        })
        .map(str::to_owned)
        .collect();
    replaced.push(format!("y={source_y}"));
    replaced.push(format!("h={source_height}"));
    replaced.push(format!("r={cropped_rows}"));
    let replacement = replaced.join(",");
    let mut output = String::with_capacity(line.len() + replacement.len());
    output.push_str(&line[..header.controls_start]);
    output.push_str(&replacement);
    output.push_str(&line[header.controls_end..]);
    output
}

fn kitty_placement_from_line(line: &str, image_id: u32) -> Option<String> {
    let start = line.find(KITTY_PREFIX)?;
    let controls_start = start.checked_add(KITTY_PREFIX.len())?;
    let controls_end = line
        .get(controls_start..)?
        .find(';')?
        .checked_add(controls_start)?;
    let controls = line.get(controls_start..controls_end)?;
    let mut placement = vec!["a=p".to_owned(), "q=2".to_owned()];
    for control in controls.split(',') {
        let key = control.split_once('=').map_or(control, |(key, _)| key);
        if matches!(
            key,
            "i" | "p"
                | "x"
                | "y"
                | "w"
                | "h"
                | "X"
                | "Y"
                | "c"
                | "r"
                | "C"
                | "U"
                | "z"
                | "P"
                | "Q"
                | "H"
                | "V"
        ) {
            placement.push(control.to_owned());
        }
    }
    if !placement.iter().any(|control| control.starts_with("i=")) {
        placement.push(format!("i={image_id}"));
    }
    Some(format!("{KITTY_PREFIX}{}\u{1b}\\", placement.join(",")))
}
#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "unit tests use contextual failure messages"
)]
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
        assert_eq!(delete_all_kitty_placements(), "\u{1b}_Ga=d,d=a,q=2\u{1b}\\");
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

    #[test]
    fn kitty_crop_replaces_source_controls() {
        let image_id = 0x1020;
        register_kitty_image_metadata(KittyImageMetadata {
            image_id,
            columns: 4,
            rows: 4,
            width_px: 80,
            height_px: 100,
            transmission_generation: 1,
        });
        let line = encode_kitty(
            "AAAA",
            KittyEncodeOptions {
                columns: Some(4),
                rows: Some(4),
                image_id: Some(image_id),
                ..KittyEncodeOptions::default()
            },
        );
        let cropped = crop_kitty_image_line(&line, 1, 2);
        assert!(cropped.contains("y=25"));
        assert!(cropped.contains("h=50"));
        assert!(cropped.contains("r=2"));
        assert_eq!(cropped.matches(",r=").count(), 1);
        assert_eq!(cropped.matches(",h=").count(), 1);
        let parsed = DocumentImage::try_from_line(&cropped)
            .expect("cropped Kitty line remains valid")
            .expect("cropped image line");
        assert_eq!(parsed.rows(), 4);
    }

    #[test]
    fn kitty_cache_emits_bounded_evictions() {
        let mut cache = KittyImageCache::new();
        let images: Vec<DocumentImage> = (1..=17)
            .map(|image_id| {
                DocumentImage::from_native(
                    "AAAA",
                    "image/png",
                    ImageDimensions {
                        width_px: 8,
                        height_px: 8,
                    },
                    &DocumentImageOptions {
                        max_width_cells: Some(2),
                        max_height_cells: Some(1),
                        image_id: Some(image_id),
                        protocol: Some(ImageProtocol::Kitty),
                        ..DocumentImageOptions::default()
                    },
                )
                .expect("valid native image")
            })
            .collect();
        let first = cache.prepare_frame(images.iter());
        assert_eq!(first.uploads.len(), images.len());
        assert!(first.evictions.is_empty());
        assert_eq!(cache.transmission_bytes(), 0);
        let second = cache.prepare_frame(images.iter());
        assert!(second.uploads.is_empty());
        assert_eq!(
            cache.emission_for(&images[0], &second),
            ImageEmission::Placement
        );
        let evictions = cache
            .prepare_frame(std::iter::empty::<&DocumentImage>())
            .evictions;
        assert_eq!(cache.len(), MAX_CACHED_OFFSCREEN_KITTY_IMAGES);
        assert_eq!(evictions[0].deletion, delete_kitty_image(1).into_bytes());
    }

    #[test]
    fn retained_iterm2_line_becomes_validated_fallback() {
        let line = encode_iterm2(
            "AAAA",
            ITerm2EncodeOptions {
                width: Some("2".to_owned()),
                height: Some("auto".to_owned()),
                name: Some("pic.png".to_owned()),
                ..ITerm2EncodeOptions::default()
            },
        );
        let image = DocumentImage::try_from_line(&line)
            .expect("valid iTerm2 line")
            .expect("image line");
        assert!(image.is_fallback());
        assert_eq!(image.protocol(), None);
        assert_eq!(image.rows(), 1);
        assert_eq!(image.transmission_bytes(), 0);
        assert!(
            image
                .fallback_text()
                .is_some_and(|text| text.contains("pic.png"))
        );
    }

    #[test]
    fn retained_image_line_requires_protocol_metadata_and_termination() {
        register_kitty_image_metadata(KittyImageMetadata {
            image_id: 0x2222,
            columns: 2,
            rows: 3,
            width_px: 18,
            height_px: 54,
            transmission_generation: 7,
        });
        let line = encode_kitty(
            "AAAA",
            KittyEncodeOptions {
                columns: Some(2),
                rows: Some(3),
                image_id: Some(0x2222),
                ..KittyEncodeOptions::default()
            },
        );
        let image = DocumentImage::try_from_line(&line)
            .expect("valid Kitty line")
            .expect("image line");
        assert_eq!(image.columns(), 2);
        assert_eq!(image.rows(), 3);
        assert_eq!(image.image_id(), Some(0x2222));
        assert!(
            DocumentImage::try_from_line("plain text")
                .expect("plain text classification")
                .is_none()
        );
        let malformed = line.trim_end_matches("\u{1b}\\");
        assert_eq!(
            DocumentImage::try_from_line(malformed),
            Err(ImageError::InvalidTermination)
        );
    }
}
