//! Image component: [`RawRegion`] annotation + skip cells + text fallback.

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;

use crate::component::{
    Component, DisplayRowContent, DisplayRowSpan, EventResult, RowSourceError, UiEvent,
};
use crate::frame::{RawRegion, push_raw_region};
use crate::image::{
    DocumentImage, DocumentImageOptions, ITerm2EncodeOptions, ImageDimensions, ImageError,
    KittyEncodeOptions, allocate_image_id, calculate_image_cell_size, encode_iterm2, encode_kitty,
    get_image_dimensions, image_fallback,
};
use crate::terminal::caps::{CellDimensions, ImageProtocol, TerminalCapabilities};

use super::util::{KeyedLine, paint_lines};

/// Theme for image fallback text.
#[derive(Clone)]
pub struct ImageTheme {
    /// Style fallback text.
    pub fallback_color: fn(&str) -> String,
}

impl Default for ImageTheme {
    fn default() -> Self {
        fn id(s: &str) -> String {
            s.to_owned()
        }
        Self { fallback_color: id }
    }
}

/// Options for image sizing and identity.
#[derive(Debug, Clone, Default)]
pub struct ImageOptions {
    /// Max width in cells (default 60).
    pub max_width_cells: Option<u16>,
    /// Max height in cells (default from aspect ratio).
    pub max_height_cells: Option<u16>,
    /// Optional filename for fallback text.
    pub filename: Option<String>,
    /// Optional Kitty image id reuse.
    pub image_id: Option<u32>,
}

/// Image component that emits protocol bytes via [`RawRegion`] annotations.
pub struct ImageComponent {
    base64_data: String,
    mime_type: String,
    dimensions: ImageDimensions,
    theme: ImageTheme,
    options: ImageOptions,
    image_id: Option<u32>,
    /// Capability snapshot.
    caps: TerminalCapabilities,
    cell: CellDimensions,
    cache: Option<Cache>,
    row_cache: Option<RowCache>,
}

struct Cache {
    width: u16,
    height: u16,
    fallback_lines: Vec<String>,
    raw_bytes: Option<Vec<u8>>,
    rows: u16,
    cols: u16,
    kitty_id: Option<u32>,
}

struct RowCache {
    width: u16,
    rows: usize,
    image: DocumentImage,
    fallback: Option<KeyedLine>,
}

impl ImageComponent {
    /// Create an image component.
    #[must_use]
    pub fn new(
        base64_data: impl Into<String>,
        mime_type: impl Into<String>,
        theme: ImageTheme,
        options: ImageOptions,
        dimensions: Option<ImageDimensions>,
        caps: TerminalCapabilities,
    ) -> Self {
        let base64_data = base64_data.into();
        let mime_type = mime_type.into();
        let dimensions = dimensions
            .or_else(|| get_image_dimensions(&base64_data, &mime_type))
            .unwrap_or(ImageDimensions {
                width_px: 800,
                height_px: 600,
            });
        let image_id = options.image_id;
        let cell = caps.cell;
        Self {
            base64_data,
            mime_type,
            dimensions,
            theme,
            options,
            image_id,
            caps,
            cell,
            cache: None,
            row_cache: None,
        }
    }

    /// Kitty image id if allocated.
    #[must_use]
    pub fn image_id(&self) -> Option<u32> {
        self.image_id
    }

    /// Prepare and borrow the opaque image used by the retained row source.
    ///
    /// Retained preparation deliberately treats iTerm2 as a fallback. The
    /// fullscreen lifecycle owns that mode-specific capability decision;
    /// regular [`Component::render`] remains unchanged below.
    ///
    /// # Errors
    ///
    /// Returns [`RowSourceError::InvalidImage`] when native image validation
    /// rejects the payload or placement metadata.
    pub fn document_image(&mut self, width: u16) -> Result<&DocumentImage, RowSourceError> {
        self.prepare_document_rows(width)?;
        self.row_cache
            .as_ref()
            .map(|cache| &cache.image)
            .ok_or(RowSourceError::NotPrepared)
    }

    fn prepare_document_rows(&mut self, width: u16) -> Result<usize, RowSourceError> {
        if width == 0 {
            self.row_cache = None;
            return Ok(0);
        }
        if let Some(cache) = self.row_cache.as_ref().filter(|cache| cache.width == width) {
            return Ok(cache.rows);
        }
        // Do not leave a previous-width image available after a failed
        // validation; retained row sources fail closed.
        self.row_cache = None;
        let max_width = width
            .saturating_sub(2)
            .min(self.options.max_width_cells.unwrap_or(60))
            .max(1);
        let image = DocumentImage::from_native(
            &self.base64_data,
            &self.mime_type,
            self.dimensions,
            &DocumentImageOptions {
                max_width_cells: Some(max_width),
                max_height_cells: self.options.max_height_cells,
                filename: self.options.filename.clone(),
                image_id: self.image_id,
                // iTerm2 is intentionally disabled in fullscreen retained
                // rows; its native fallback remains visible and searchable.
                protocol: (self.caps.images == Some(ImageProtocol::Kitty))
                    .then_some(ImageProtocol::Kitty),
                cell_dimensions: self.cell,
            },
        )
        .map_err(|_error: ImageError| RowSourceError::InvalidImage)?;
        self.image_id = image.image_id().or(self.image_id);
        let fallback = image.fallback_text().map(|line| {
            let styled = (self.theme.fallback_color)(line);
            KeyedLine::new(styled, width)
        });
        let rows = if image.is_fallback() {
            1
        } else {
            usize::from(image.rows())
        };
        self.row_cache = Some(RowCache {
            width,
            rows,
            image,
            fallback,
        });
        Ok(rows)
    }

    fn build_cache(&mut self, width: u16) -> Cache {
        let max_width = width
            .saturating_sub(2)
            .min(self.options.max_width_cells.unwrap_or(60))
            .max(1);
        let default_max_height = {
            let cell_w = u32::from(self.cell.width.max(1));
            let cell_h = u32::from(self.cell.height.max(1));
            // ceil(max_width * cell_w / cell_h) without float casts.
            let numer = u32::from(max_width).saturating_mul(cell_w);
            let h = numer.div_ceil(cell_h);
            u16::try_from(h).unwrap_or(u16::MAX).max(1)
        };
        let max_height = self.options.max_height_cells.unwrap_or(default_max_height);

        match self.caps.images {
            Some(ImageProtocol::Kitty) => {
                if self.image_id.is_none() {
                    self.image_id = Some(allocate_image_id());
                }
                let size = calculate_image_cell_size(
                    self.dimensions,
                    max_width,
                    Some(max_height),
                    self.cell,
                );
                let sequence = encode_kitty(
                    &self.base64_data,
                    KittyEncodeOptions {
                        columns: Some(size.columns),
                        rows: Some(size.rows),
                        image_id: self.image_id,
                        move_cursor: Some(false),
                    },
                );
                Cache {
                    width,
                    height: size.rows.max(1),
                    fallback_lines: Vec::new(),
                    raw_bytes: Some(sequence.into_bytes()),
                    rows: size.rows.max(1),
                    cols: size.columns.max(1),
                    kitty_id: self.image_id,
                }
            }
            Some(ImageProtocol::ITerm2) => {
                let size = calculate_image_cell_size(
                    self.dimensions,
                    max_width,
                    Some(max_height),
                    self.cell,
                );
                let sequence = encode_iterm2(
                    &self.base64_data,
                    ITerm2EncodeOptions {
                        width: Some(size.columns.to_string()),
                        height: Some("auto".to_owned()),
                        name: self.options.filename.clone(),
                        preserve_aspect_ratio: None,
                        inline: Some(true),
                    },
                );
                Cache {
                    width,
                    height: size.rows.max(1),
                    fallback_lines: Vec::new(),
                    raw_bytes: Some(sequence.into_bytes()),
                    rows: size.rows.max(1),
                    cols: size.columns.max(1),
                    kitty_id: None,
                }
            }
            None => {
                let fb = image_fallback(
                    &self.mime_type,
                    Some(self.dimensions),
                    self.options.filename.as_deref(),
                );
                let line = (self.theme.fallback_color)(&fb);
                Cache {
                    width,
                    height: 1,
                    fallback_lines: vec![line],
                    raw_bytes: None,
                    rows: 1,
                    cols: width,
                    kitty_id: None,
                }
            }
        }
    }

    fn ensure_cache(&mut self, width: u16) {
        let needs = !matches!(&self.cache, Some(c) if c.width == width);
        if needs {
            self.cache = Some(self.build_cache(width));
        }
    }
}

impl Component for ImageComponent {
    fn measure(&mut self, width: u16) -> u16 {
        self.ensure_cache(width);
        self.cache.as_ref().map_or(0, |c| c.height)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.ensure_cache(area.width);
        let Some(cache) = self.cache.as_ref() else {
            return;
        };
        if let Some(bytes) = cache.raw_bytes.clone() {
            let rows = cache.rows.min(area.height);
            let cols = cache.cols.min(area.width);
            let kitty_id = cache.kitty_id;
            let region_area = Rect {
                x: area.x,
                y: area.y,
                width: cols,
                height: rows,
            };
            for row in 0..rows {
                for col in 0..cols {
                    if let Some(cell) = buf.cell_mut((area.x + col, area.y + row)) {
                        cell.reset();
                        cell.set_diff_option(CellDiffOption::Skip);
                    }
                }
            }
            // Direct cell writer: claim the covered rows for damage scoping.
            crate::frame::claim_opaque_span(Rect {
                x: area.x,
                y: area.y,
                width: cols,
                height: rows,
            });
            push_raw_region(RawRegion {
                area: region_area,
                bytes,
                kitty_id,
            });
        } else {
            paint_lines(area, buf, &cache.fallback_lines);
        }
    }

    fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
        EventResult::Ignored
    }

    fn prepare_rows(&mut self, width: u16) -> Result<usize, RowSourceError> {
        self.prepare_document_rows(width)
    }

    fn visit_row(
        &self,
        row: usize,
        emit: &mut dyn FnMut(DisplayRowSpan<'_>),
    ) -> Result<(), RowSourceError> {
        let cache = self.row_cache.as_ref().ok_or(RowSourceError::NotPrepared)?;
        if row >= cache.rows {
            return Err(RowSourceError::RowOutOfBounds {
                row,
                rows: cache.rows,
            });
        }
        if let Some(line) = cache.fallback.as_ref() {
            emit(DisplayRowSpan {
                column: 0,
                width: cache.width,
                content: DisplayRowContent::Text(line),
            });
            return Ok(());
        }
        let row_in_image = u16::try_from(row).map_err(|_| RowSourceError::InvalidImage)?;
        emit(DisplayRowSpan {
            column: 0,
            width: cache.image.columns(),
            content: DisplayRowContent::Image {
                image: &cache.image,
                row_in_image,
            },
        });
        Ok(())
    }
    fn invalidate(&mut self) {
        self.cache = None;
        self.row_cache = None;
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "unit tests use contextual failure messages"
)]
mod tests {
    use super::*;
    use crate::components::util::{render_snapshot, strip_ansi};
    use crate::frame::{FrameAnnotations, with_annotations};
    use std::cell::RefCell;

    #[test]
    fn fallback_without_image_caps() {
        let caps = TerminalCapabilities {
            images: None,
            ..TerminalCapabilities::default()
        };
        let mut img = ImageComponent::new(
            "",
            "image/png",
            ImageTheme::default(),
            ImageOptions {
                filename: Some("pic.png".into()),
                ..Default::default()
            },
            Some(ImageDimensions {
                width_px: 100,
                height_px: 50,
            }),
            caps,
        );
        let snap = render_snapshot(&mut img, 60);
        assert_eq!(snap.len(), 1);
        let plain = strip_ansi(&snap[0]);
        assert!(plain.contains("Image") || plain.contains("png") || plain.contains("pic"));
    }

    #[test]
    fn kitty_emits_raw_region_and_skips() {
        let caps = TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            ..TerminalCapabilities::default()
        };
        let mut img = ImageComponent::new(
            "AAAA",
            "image/png",
            ImageTheme::default(),
            ImageOptions {
                max_width_cells: Some(10),
                max_height_cells: Some(4),
                ..Default::default()
            },
            Some(ImageDimensions {
                width_px: 90,
                height_px: 72,
            }),
            caps,
        );
        let height = img.measure(40);
        assert!(height >= 1);
        let annotations = RefCell::new(FrameAnnotations::new());
        with_annotations(&annotations, || {
            let area = Rect::new(0, 0, 40, height);
            let mut buf = Buffer::empty(area);
            img.render(area, &mut buf);
            assert!(
                buf.cell((0, 0))
                    .is_some_and(|c| c.diff_option == CellDiffOption::Skip)
            );
        });
        let ann = annotations.into_inner();
        assert!(!ann.raw_regions().is_empty());
        assert!(ann.raw_regions()[0].kitty_id.is_some());
    }
    #[test]
    fn widths_matrix_fallback() {
        // Fails iff measure/paint heights diverge (see render_snapshot contract).
        let caps = TerminalCapabilities::default();
        let mut img = ImageComponent::new(
            "",
            "image/png",
            ImageTheme::default(),
            ImageOptions::default(),
            Some(ImageDimensions {
                width_px: 10,
                height_px: 10,
            }),
            caps,
        );
        for w in [24_u16, 60, 80, 120] {
            let _ = render_snapshot(&mut img, w);
        }
    }

    #[test]
    fn retained_kitty_rows_borrow_document_image() {
        let caps = TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            ..TerminalCapabilities::default()
        };
        let mut img = ImageComponent::new(
            "AAAA",
            "image/png",
            ImageTheme::default(),
            ImageOptions {
                max_width_cells: Some(4),
                max_height_cells: Some(3),
                ..ImageOptions::default()
            },
            Some(ImageDimensions {
                width_px: 40,
                height_px: 60,
            }),
            caps,
        );
        let rows = Component::prepare_rows(&mut img, 20).expect("valid retained image");
        assert_eq!(rows, 3);
        let mut seen = None;
        Component::visit_row(&img, 1, &mut |span| {
            if let DisplayRowContent::Image {
                image,
                row_in_image,
            } = span.content
            {
                seen = Some((image.clone(), span.width, row_in_image));
            }
        })
        .expect("prepared row");
        let (image, width, row) = seen.expect("image span");
        assert_eq!(width, image.columns());
        assert_eq!(row, 1);
        let region = image
            .raw_region(
                Rect::new(0, 0, width, 2),
                1,
                2,
                crate::image::ImageEmission::Upload,
            )
            .expect("visible image slice");
        let text = String::from_utf8(region.bytes).expect("kitty bytes are UTF-8");
        assert!(text.contains("y=20"));
        assert!(text.contains("h=40"));
        assert!(text.contains("r=2"));
    }

    #[test]
    fn retained_iterm2_rows_use_fallback_only_in_document_mode() {
        let caps = TerminalCapabilities {
            images: Some(ImageProtocol::ITerm2),
            ..TerminalCapabilities::default()
        };
        let mut img = ImageComponent::new(
            "AAAA",
            "image/png",
            ImageTheme::default(),
            ImageOptions::default(),
            Some(ImageDimensions {
                width_px: 40,
                height_px: 20,
            }),
            caps,
        );
        let rows = Component::prepare_rows(&mut img, 20).expect("fallback preparation");
        assert_eq!(rows, 1);
        let mut text = false;
        Component::visit_row(&img, 0, &mut |span| {
            text = matches!(span.content, DisplayRowContent::Text(_));
        })
        .expect("fallback row");
        assert!(text);

        // The regular renderer still emits the native iTerm2 region.
        let height = img.measure(20);
        let annotations = RefCell::new(FrameAnnotations::new());
        with_annotations(&annotations, || {
            let area = Rect::new(0, 0, 20, height);
            let mut buf = Buffer::empty(area);
            img.render(area, &mut buf);
        });
        let ann = annotations.into_inner();
        assert!(
            ann.raw_regions()
                .iter()
                .any(|region| region.bytes.starts_with(b"\x1b]1337;File="))
        );
    }
}
