//! Overlay compositing surface: wide-cell-aware buffer writes and string-level
//! CJK-boundary compositing.
//!
//! The z-order/focus/capture overlay stack that previously lived here was
//! removed in issue #175 (zero live consumers; both registries unpublished).
//! What remains is the compositing trio used by hosts to paint overlay text
//! over a base [`ratatui::buffer::Buffer`]: [`composite_into_buffer`],
//! [`write_overlay_cells`], and [`composite_overlay_line`]. Layout math lives
//! in [`crate::layout`]; CJK boundary overwrite uses Ratatui `Buffer` wide-cell
//! semantics (and the string-level [`crate::text::composite_line_at`] helper
//! for regression tests).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::layout::ResolvedOverlayLayout;

/// Composite overlay text into a Ratatui buffer region using wide-cell overwrite.
///
/// `area` is the full terminal/frame area. Overlay content is clipped to
/// `layout` and written with [`write_overlay_cells`], which blanks both
/// halves of a straddled wide grapheme.
pub fn composite_into_buffer(
    buf: &mut Buffer,
    area: Rect,
    layout: ResolvedOverlayLayout,
    lines: &[String],
) {
    let height = lines.len().min(usize::from(
        layout
            .max_height
            .unwrap_or(u16::try_from(lines.len()).unwrap_or(u16::MAX)),
    ));
    let height = height.min(usize::from(area.height.saturating_sub(layout.row)));
    for (i, line) in lines.iter().take(height).enumerate() {
        let row = layout.row.saturating_add(u16::try_from(i).unwrap_or(0));
        if row >= area.y.saturating_add(area.height) {
            break;
        }
        let overlay_area = Rect {
            x: area.x.saturating_add(layout.col),
            y: area.y.saturating_add(row),
            width: layout.width.min(area.width.saturating_sub(layout.col)),
            height: 1,
        };
        write_overlay_cells(buf, overlay_area, line);
    }
}

/// Write `text` into `area` (single row), overwriting wide-cell pairs cleanly.
///
/// When a write starts on the trailing half of a wide character, the leading
/// half is blanked. When a wide character would overflow the right edge of
/// `area`, it is replaced with spaces so column count is conserved.
pub fn write_overlay_cells(buf: &mut Buffer, area: Rect, text: &str) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Overlays composite over other components' rows: claim the rows as
    // foreign so base paint lines cannot skip-repaint over them and stale
    // cells survive an overlay close (PERF-T11 Design B).
    crate::frame::claim_foreign_span(area);

    // If the first cell of the overlay region is the trailing half of a wide
    // grapheme, blank the leading cell so the pair is not left half-stale.
    if area.x > 0 {
        let origin = buf
            .cell((area.x, area.y))
            .map(|cell| cell.symbol().to_owned());
        if origin.as_deref() == Some("") {
            // Trailing half of a wide char: clear the previous cell too.
            if let Some(prev) = buf.cell_mut((area.x - 1, area.y)) {
                prev.set_symbol(" ");
            }
            if let Some(cell) = buf.cell_mut((area.x, area.y)) {
                cell.set_symbol(" ");
            }
        }
    }

    let mut col = 0u16;
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(text, true) {
        let w = u16::try_from(crate::text::grapheme_width(grapheme)).unwrap_or(u16::MAX);
        if w == 0 {
            continue;
        }
        if col >= area.width {
            break;
        }
        if col.saturating_add(w) > area.width {
            // Wide char would overflow: pad with spaces for remaining columns.
            while col < area.width {
                let x = area.x.saturating_add(col);
                if let Some(cell) = buf.cell_mut((x, area.y)) {
                    cell.set_symbol(" ");
                }
                col = col.saturating_add(1);
            }
            break;
        }
        let x = area.x.saturating_add(col);
        if let Some(cell) = buf.cell_mut((x, area.y)) {
            cell.set_symbol(grapheme);
        }
        // Clear trailing half for wide glyphs.
        if w == 2
            && let Some(cell) = buf.cell_mut((x + 1, area.y))
        {
            cell.set_symbol("");
        }
        col = col.saturating_add(w);
    }

    // Fill remaining overlay width with spaces so base content under the
    // declared width is fully replaced (matches declared-width compositing).
    while col < area.width {
        let x = area.x.saturating_add(col);
        if let Some(cell) = buf.cell_mut((x, area.y)) {
            cell.set_symbol(" ");
        }
        col = col.saturating_add(1);
    }
}

/// String-level CJK-boundary composite (delegates to text module).
///
/// Exposed for overlay regression tests that port
/// `regression-overlay-cjk-boundary.test.ts`.
#[must_use]
pub fn composite_overlay_line(
    base_line: &str,
    overlay_line: &str,
    start_col: usize,
    overlay_width: usize,
    total_width: usize,
) -> String {
    crate::text::composite_line_at(
        base_line,
        overlay_line,
        start_col,
        overlay_width,
        total_width,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::visible_width;

    #[test]
    fn cjk_boundary_string_composite_inside_wide_grapheme() {
        // "abcd让EFGH" — 让 is a wide char spanning cols 4-5.
        // Overlay starting at col 5 (inside 让) must drop 让 and keep width.
        let out = composite_overlay_line("abcd让EFGH", "│XX│", 5, 4, 20);
        assert!(!out.contains('让'));
        assert_eq!(visible_width(&out), 20);
        let overlay = crate::text::slice_by_column(&out, 5, 4, true);
        assert_eq!(visible_width(&overlay), 4);
        assert!(overlay.contains("│XX│"));
    }

    #[test]
    fn cjk_boundary_string_composite_at_wide_boundary() {
        let out = composite_overlay_line("abcd让EFGH", "│XX│", 4, 4, 20);
        assert!(!out.contains('让'));
        assert_eq!(visible_width(&out), 20);
        let overlay = crate::text::slice_by_column(&out, 4, 4, true);
        assert_eq!(visible_width(&overlay), 4);
        assert!(overlay.contains("│XX│"));
    }

    #[test]
    fn buffer_wide_char_overwrite_blanks_pair() {
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        // Base: "ab让cd" starting at col 0 → 让 at cols 2-3
        write_overlay_cells(&mut buf, area, "ab让cd");
        assert_eq!(
            buf.cell((2, 0)).map(ratatui::buffer::Cell::symbol),
            Some("让")
        );
        assert_eq!(
            buf.cell((3, 0)).map(ratatui::buffer::Cell::symbol),
            Some("")
        );

        // Overlay starts at col 3 (trailing half of 让) with "XY"
        let overlay_area = Rect::new(3, 0, 2, 1);
        write_overlay_cells(&mut buf, overlay_area, "XY");
        // Leading half of 让 must be blanked
        let lead = buf.cell((2, 0)).map(|c| c.symbol().to_owned());
        assert_eq!(lead.as_deref(), Some(" "));
        assert_eq!(
            buf.cell((3, 0)).map(ratatui::buffer::Cell::symbol),
            Some("X")
        );
        assert_eq!(
            buf.cell((4, 0)).map(ratatui::buffer::Cell::symbol),
            Some("Y")
        );
    }

    #[test]
    fn buffer_wide_char_overflow_pads_spaces() {
        let area = Rect::new(0, 0, 3, 1);
        let mut buf = Buffer::empty(area);
        // Two CJK chars need 4 cols; area is 3 → pad, no overflow symbol.
        write_overlay_cells(&mut buf, area, "中文");
        // First CJK fits at 0-1, second would overflow → spaces
        assert_eq!(
            buf.cell((0, 0)).map(ratatui::buffer::Cell::symbol),
            Some("中")
        );
        let c2 = buf.cell((2, 0)).map(|c| c.symbol().to_owned());
        assert_eq!(c2.as_deref(), Some(" "));
    }

    #[test]
    fn buffer_vs16_does_not_consume_cell() {
        // VS16 (U+FE0F) is zero-width via grapheme_width — it does not
        // consume an extra cell.  unicode-segmentation groups "B\u{FE0F}"
        // as one grapheme, so the cell symbol is "B\u{FE0F}" but it
        // occupies only one column.
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        write_overlay_cells(&mut buf, area, "AB\u{FE0F}CD");
        assert_eq!(
            buf.cell((0, 0)).map(ratatui::buffer::Cell::symbol),
            Some("A")
        );
        assert_eq!(
            buf.cell((1, 0)).map(ratatui::buffer::Cell::symbol),
            Some("B\u{FE0F}")
        );
        assert_eq!(
            buf.cell((2, 0)).map(ratatui::buffer::Cell::symbol),
            Some("C")
        );
        assert_eq!(
            buf.cell((3, 0)).map(ratatui::buffer::Cell::symbol),
            Some("D")
        );
    }

    #[test]
    fn buffer_ri_pair_one_grapheme_two_cells() {
        // Regional indicator pair 🇺🇸 is ONE grapheme cluster, width 2.
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        write_overlay_cells(&mut buf, area, "\u{1F1FA}\u{1F1F8}");
        assert_eq!(
            buf.cell((0, 0)).map(ratatui::buffer::Cell::symbol),
            Some("\u{1F1FA}\u{1F1F8}")
        );
        assert_eq!(
            buf.cell((1, 0)).map(ratatui::buffer::Cell::symbol),
            Some("")
        );
    }

    #[test]
    fn buffer_ri_singleton_two_cells() {
        // A single regional indicator is width 2.
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        write_overlay_cells(&mut buf, area, "\u{1F1FA}X");
        assert_eq!(
            buf.cell((0, 0)).map(ratatui::buffer::Cell::symbol),
            Some("\u{1F1FA}")
        );
        assert_eq!(
            buf.cell((1, 0)).map(ratatui::buffer::Cell::symbol),
            Some("")
        );
        assert_eq!(
            buf.cell((2, 0)).map(ratatui::buffer::Cell::symbol),
            Some("X")
        );
    }

    #[test]
    fn buffer_spacing_mark_agrees_with_layout() {
        // "का" (U+0915 + U+093E) is one grapheme, width 2 (base + spacing mark).
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        write_overlay_cells(&mut buf, area, "काX");
        assert_eq!(
            buf.cell((0, 0)).map(ratatui::buffer::Cell::symbol),
            Some("का")
        );
        assert_eq!(
            buf.cell((1, 0)).map(ratatui::buffer::Cell::symbol),
            Some("")
        );
        assert_eq!(
            buf.cell((2, 0)).map(ratatui::buffer::Cell::symbol),
            Some("X")
        );
    }
}
