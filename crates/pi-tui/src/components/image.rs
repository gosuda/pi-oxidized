//! Image component: [`RawRegion`] annotation + skip cells + text fallback.

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;

use crate::component::{Component, EventResult, UiEvent};
use crate::frame::{RawRegion, push_raw_region};
use crate::image::{
    ITerm2EncodeOptions, ImageDimensions, KittyEncodeOptions, allocate_image_id,
    calculate_image_cell_size, encode_iterm2, encode_kitty, get_image_dimensions, image_fallback,
};
use crate::terminal::caps::{CellDimensions, ImageProtocol, TerminalCapabilities};

use super::util::paint_lines;

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
        }
    }

    /// Kitty image id if allocated.
    #[must_use]
    pub fn image_id(&self) -> Option<u32> {
        self.image_id
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

    fn invalidate(&mut self) {
        self.cache = None;
    }
}

#[cfg(test)]
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
}
