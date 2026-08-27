//! Shared helpers for component measure/render against Ratatui buffers.

use std::cell::RefCell;
use std::collections::HashMap;

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;

use crate::frame::{RawRegion, push_raw_region};
use crate::text::{extract_ansi_code, grapheme_width, parse_osc8_hyperlink, visible_width};

/// One recorded paint operation at a column offset, replayable at any buffer
/// position.
#[derive(Debug, Clone)]
enum PaintedOp {
    /// Grapheme cell: byte range into the source line plus reduced style.
    Sym {
        start: u32,
        end: u32,
        style: Style,
    },
    /// Continuation cell of a wider-than-one-column grapheme.
    Cont,
}

/// Derived paint result for one `(line, max_width)` pair.
#[derive(Debug, Default)]
struct DerivedLine {
    width: usize,
    /// `(column offset, op)` in paint order.
    ops: Vec<(u16, PaintedOp)>,
    /// Hyperlink region templates: `(start column, span columns, bytes)`.
    regions: Vec<(u16, u16, Vec<u8>)>,
}

// Painted-line memo. Derivation (ANSI scan + grapheme segmentation + width
// computation + SGR reduction) is a pure function of `(line, max_width)`,
// so unchanged lines — the overwhelming majority of every frame's rows —
// replay their recorded ops instead of re-deriving. Hit validation
// compares the full line, so a hash collision only costs a re-derivation.
thread_local! {
    static PAINT_CACHE: RefCell<HashMap<u64, (Box<str>, DerivedLine)>> =
        RefCell::new(HashMap::new());
}

/// Entry cap. The whole cache clears on overflow: one full re-derivation
/// frame amortized over `PAINT_CACHE_CAP` inserts, bounding the memo to a
/// few MB regardless of session length.
const PAINT_CACHE_CAP: usize = 1024;

/// FNV-1a over the line bytes with `max_width` folded into the final state.
fn paint_cache_key(line: &str, max_width: usize) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in line.as_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^ u64::try_from(max_width)
        .unwrap_or(u64::MAX)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// Active SGR attributes while painting a line.
#[derive(Debug, Clone, Default)]
struct PaintStyle {
    /// bit0 bold, bit1 dim, bit2 italic, bit3 underline, bit4 reverse, bit5 strike
    flags: u8,
    fg: Option<Color>,
    bg: Option<Color>,
}

impl PaintStyle {
    const BOLD: u8 = 1;
    const DIM: u8 = 1 << 1;
    const ITALIC: u8 = 1 << 2;
    const UNDERLINE: u8 = 1 << 3;
    const REVERSE: u8 = 1 << 4;
    const STRIKE: u8 = 1 << 5;

    fn set_flag(&mut self, bit: u8, on: bool) {
        if on {
            self.flags |= bit;
        } else {
            self.flags &= !bit;
        }
    }

    fn has(&self, bit: u8) -> bool {
        self.flags & bit != 0
    }

    fn process(&mut self, ansi_code: &str) {
        if !ansi_code.ends_with('m') {
            return;
        }
        let Some(rest) = ansi_code.strip_prefix("\u{1b}[") else {
            return;
        };
        let Some(params) = rest.strip_suffix('m') else {
            return;
        };
        if params.is_empty() || params == "0" {
            *self = Self::default();
            return;
        }
        let parts: Vec<&str> = params.split(';').collect();
        let mut i = 0usize;
        while i < parts.len() {
            let code = parts[i].parse::<u32>().unwrap_or(u32::MAX);
            if code == 38 || code == 48 {
                if parts.get(i + 1) == Some(&"5") {
                    if let Some(idx) = parts.get(i + 2).and_then(|s| s.parse::<u8>().ok()) {
                        let color = Color::Indexed(idx);
                        if code == 38 {
                            self.fg = Some(color);
                        } else {
                            self.bg = Some(color);
                        }
                    }
                    i = i.saturating_add(3);
                    continue;
                }
                if parts.get(i + 1) == Some(&"2")
                    && parts.get(i + 2).is_some()
                    && parts.get(i + 3).is_some()
                    && parts.get(i + 4).is_some()
                {
                    let r = parts[i + 2].parse::<u8>().unwrap_or(0);
                    let g = parts[i + 3].parse::<u8>().unwrap_or(0);
                    let b = parts[i + 4].parse::<u8>().unwrap_or(0);
                    let color = Color::Rgb(r, g, b);
                    if code == 38 {
                        self.fg = Some(color);
                    } else {
                        self.bg = Some(color);
                    }
                    i = i.saturating_add(5);
                    continue;
                }
            }
            match code {
                0 => *self = Self::default(),
                1 => self.set_flag(Self::BOLD, true),
                2 => self.set_flag(Self::DIM, true),
                3 => self.set_flag(Self::ITALIC, true),
                4 => self.set_flag(Self::UNDERLINE, true),
                7 => self.set_flag(Self::REVERSE, true),
                9 => self.set_flag(Self::STRIKE, true),
                21 | 22 => {
                    self.set_flag(Self::BOLD, false);
                    self.set_flag(Self::DIM, false);
                }
                23 => self.set_flag(Self::ITALIC, false),
                24 => self.set_flag(Self::UNDERLINE, false),
                27 => self.set_flag(Self::REVERSE, false),
                29 => self.set_flag(Self::STRIKE, false),
                39 => self.fg = None,
                49 => self.bg = None,
                30..=37 => self.fg = Some(basic_fg(code - 30)),
                90..=97 => self.fg = Some(basic_fg(code - 90 + 8)),
                40..=47 => self.bg = Some(basic_fg(code - 40)),
                100..=107 => self.bg = Some(basic_fg(code - 100 + 8)),
                _ => {}
            }
            i = i.saturating_add(1);
        }
    }

    fn to_style(&self) -> Style {
        let mut style = Style::default();
        if let Some(fg) = self.fg {
            style = style.fg(fg);
        }
        if let Some(bg) = self.bg {
            style = style.bg(bg);
        }
        let mut mods = Modifier::empty();
        if self.has(Self::BOLD) {
            mods |= Modifier::BOLD;
        }
        if self.has(Self::DIM) {
            mods |= Modifier::DIM;
        }
        if self.has(Self::ITALIC) {
            mods |= Modifier::ITALIC;
        }
        if self.has(Self::UNDERLINE) {
            mods |= Modifier::UNDERLINED;
        }
        if self.has(Self::REVERSE) {
            mods |= Modifier::REVERSED;
        }
        if self.has(Self::STRIKE) {
            mods |= Modifier::CROSSED_OUT;
        }
        style.add_modifier(mods)
    }

    /// Serialize the active style as one SGR sequence (empty when default).
    ///
    /// Used to prefix hyperlink region replays so the verbatim span re-
    /// establishes the style context that was active when the link opened.
    fn sgr_prefix(&self) -> String {
        let mut params = String::new();
        let mut push = |chunk: &str| {
            if !params.is_empty() {
                params.push(';');
            }
            params.push_str(chunk);
        };
        if let Some(fg) = self.fg {
            push(&color_params(fg, 30));
        }
        if let Some(bg) = self.bg {
            push(&color_params(bg, 40));
        }
        if self.has(Self::BOLD) {
            push("1");
        }
        if self.has(Self::DIM) {
            push("2");
        }
        if self.has(Self::ITALIC) {
            push("3");
        }
        if self.has(Self::UNDERLINE) {
            push("4");
        }
        if self.has(Self::REVERSE) {
            push("7");
        }
        if self.has(Self::STRIKE) {
            push("9");
        }
        if params.is_empty() {
            String::new()
        } else {
            format!("\u{1b}[{params}m")
        }
    }
}

/// SGR parameters for one color against base code 30 (fg) or 40 (bg).
fn color_params(color: Color, base: u32) -> String {
    match color {
        Color::Reset => {
            if base == 30 {
                "39".to_owned()
            } else {
                "49".to_owned()
            }
        }
        Color::Black => format!("{base}"),
        Color::Red => format!("{}", base + 1),
        Color::Green => format!("{}", base + 2),
        Color::Yellow => format!("{}", base + 3),
        Color::Blue => format!("{}", base + 4),
        Color::Magenta => format!("{}", base + 5),
        Color::Cyan => format!("{}", base + 6),
        Color::Gray => format!("{}", base + 7),
        Color::DarkGray => format!("{}", base + 60),
        Color::LightRed => format!("{}", base + 61),
        Color::LightGreen => format!("{}", base + 62),
        Color::LightYellow => format!("{}", base + 63),
        Color::LightBlue => format!("{}", base + 64),
        Color::LightMagenta => format!("{}", base + 65),
        Color::LightCyan => format!("{}", base + 66),
        Color::White => format!("{}", base + 67),
        Color::Rgb(r, g, b) => format!("{base};2;{r};{g};{b}"),
        Color::Indexed(n) => format!("{base};5;{n}"),
    }
}

fn basic_fg(idx: u32) -> Color {
    match idx {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        15 => Color::White,
        n => Color::Indexed(u8::try_from(n).unwrap_or(u8::MAX)),
    }
}

/// Paint pre-wrapped display lines into `area` (one line per row).
///
/// Styles never leak across rows: each row starts from a fresh style state.
pub fn paint_lines(area: Rect, buf: &mut Buffer, lines: &[String]) {
    let height = area.height as usize;
    let width = area.width as usize;
    for row in 0..height {
        let y = area
            .y
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        let content = lines.get(row).map_or("", String::as_str);
        paint_line(area.x, y, width, buf, content);
    }
}

/// Paint a single ANSI-capable line starting at `(x, y)`.
///
/// SGR sequences update the painted cell style. OSC 8 hyperlink sequences
/// cannot live in a cell buffer, so each balanced open/close span around
/// visible text is recorded as a [`RawRegion`] annotation; the writer replays
/// the verbatim bytes to the terminal (see `commit_frame`). Outside a frame
/// (`with_annotations` inactive, tests, settled-line painting) the push is a
/// no-op and the styled label cells stand alone.
pub fn paint_line(x: u16, y: u16, max_width: usize, buf: &mut Buffer, line: &str) {
    if max_width == 0 {
        return;
    }
    let key = paint_cache_key(line, max_width);
    let hit = PAINT_CACHE.with(|cache| {
        let cache = cache.borrow();
        let Some((hit_line, derived)) = cache.get(&key) else {
            return false;
        };
        if &**hit_line != line || derived.width != max_width {
            return false;
        }
        replay_derived(derived, line, x, y, buf);
        true
    });
    if hit {
        return;
    }
    let derived = derive_line(x, y, max_width, buf, line);
    // Byte ranges index into the validated line; lines beyond `u32::MAX`
    // bytes cannot carry faithful records and stay uncached.
    if line.len() <= u32::MAX as usize {
        PAINT_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.len() >= PAINT_CACHE_CAP {
                cache.clear();
            }
            cache.insert(key, (line.into(), derived));
        });
    }
}

/// Replay recorded ops at `(x, y)`: cell writes and region pushes identical
/// to a fresh derivation, translated to the target position.
fn replay_derived(derived: &DerivedLine, line: &str, x: u16, y: u16, buf: &mut Buffer) {
    for &(col, ref op) in &derived.ops {
        let cx = x.saturating_add(col);
        let Some(cell) = buf.cell_mut((cx, y)) else {
            continue;
        };
        match *op {
            PaintedOp::Sym { start, end, style } => {
                let (start, end) = (start as usize, end as usize);
                if end > start && end <= line.len() {
                    cell.set_symbol(&line[start..end]);
                    cell.set_style(style);
                }
            }
            PaintedOp::Cont => {
                cell.reset();
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }
    }
    for &(start_col, span, ref bytes) in &derived.regions {
        let rx = x.saturating_add(start_col);
        push_raw_region(RawRegion {
            area: Rect::new(rx, y, span, 1),
            bytes: bytes.clone(),
            kitty_id: None,
        });
    }
}

/// Derive (and paint) one line at `(x, y)`, recording the ops for replay.
fn derive_line(x: u16, y: u16, max_width: usize, buf: &mut Buffer, line: &str) -> DerivedLine {
    let mut derived = DerivedLine {
        width: max_width,
        ops: Vec::with_capacity(32),
        regions: Vec::new(),
    };
    let mut col = 0usize;
    let mut i = 0usize;
    let mut style = PaintStyle::default();
    // Open OSC 8 hyperlink: (byte offset of the open sequence, first painted
    // column, SGR context active when the link opened).
    let mut link: Option<(usize, usize, String)> = None;
    // Visible graphemes are gated by `max_width`, but the trailing ANSI tail
    // is always consumed: wrapped rows that fill the line exactly to the
    // margin carry their OSC 8 close at `col == max_width` and must still
    // close the region.
    while i < line.len() {
        if let Some(ansi) = extract_ansi_code(line, i) {
            match parse_osc8_hyperlink(ansi.code) {
                Some(Some(_)) => {
                    let prefix = style.sgr_prefix();
                    link = Some((i, col, prefix));
                }
                Some(None) => {
                    if let Some((open_at, start_col, prefix)) = link.take()
                        && col > start_col
                    {
                        let width = u16::try_from(col - start_col).unwrap_or(u16::MAX);
                        let region_x =
                            x.saturating_add(u16::try_from(start_col).unwrap_or(u16::MAX));
                        let mut bytes = prefix.into_bytes();
                        bytes.extend_from_slice(line[open_at..i + ansi.len].as_bytes());
                        // Reset guard: the verbatim span may set SGR without
                        // restoring it, and the replayed bytes must not leak
                        // attributes into subsequent payload writes.
                        bytes.extend_from_slice(b"\x1b[0m");
                        push_raw_region(RawRegion {
                            area: Rect::new(region_x, y, width, 1),
                            bytes: bytes.clone(),
                            kitty_id: None,
                        });
                        derived.regions.push((
                            u16::try_from(start_col).unwrap_or(u16::MAX),
                            width,
                            bytes,
                        ));
                    }
                }
                None => style.process(ansi.code),
            }
            i += ansi.len;
            continue;
        }
        if col >= max_width {
            break;
        }
        let rest = &line[i..];
        let Some(grapheme) = rest.graphemes(true).next() else {
            break;
        };
        if grapheme.is_empty() {
            break;
        }
        let gw = grapheme_width(grapheme);
        if gw == 0 {
            i += grapheme.len();
            continue;
        }
        if col + gw > max_width {
            break;
        }
        let cell_style = style.to_style();
        let cell_x = x.saturating_add(u16::try_from(col).unwrap_or(u16::MAX));
        if let Some(cell) = buf.cell_mut((cell_x, y)) {
            cell.set_symbol(grapheme);
            cell.set_style(cell_style);
        }
        derived.ops.push((
            u16::try_from(col).unwrap_or(u16::MAX),
            PaintedOp::Sym {
                start: u32::try_from(i).unwrap_or(u32::MAX),
                end: u32::try_from(i + grapheme.len()).unwrap_or(u32::MAX),
                style: cell_style,
            },
        ));
        for extra in 1..gw {
            let cx = x.saturating_add(u16::try_from(col + extra).unwrap_or(u16::MAX));
            if let Some(cell) = buf.cell_mut((cx, y)) {
                cell.reset();
                cell.set_diff_option(CellDiffOption::Skip);
            }
            derived.ops.push((
                u16::try_from(col + extra).unwrap_or(u16::MAX),
                PaintedOp::Cont,
            ));
        }
        col = col.saturating_add(gw);
        i = i.saturating_add(grapheme.len());
    }
    derived
}

/// Pad a line to exactly `width` visible columns with trailing spaces.
#[must_use]
pub fn pad_to_width(line: &str, width: usize) -> String {
    let vis = visible_width(line);
    if vis >= width {
        return line.to_owned();
    }
    format!("{line}{}", " ".repeat(width - vis))
}

/// Empty (space-filled) line of `width` columns.
#[must_use]
pub fn empty_line(width: usize) -> String {
    " ".repeat(width)
}

/// Apply an optional background function that wraps a full-width line.
#[must_use]
pub fn apply_background(line: &str, width: usize, bg: Option<&dyn Fn(&str) -> String>) -> String {
    match bg {
        Some(f) => f(&pad_to_width(line, width)),
        None => pad_to_width(line, width),
    }
}

/// Strip ANSI for plain-text snapshot comparison.
#[must_use]
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        if let Some(ansi) = extract_ansi_code(s, i) {
            i += ansi.len;
            continue;
        }
        let ch = s[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&s[i..i + ch]);
        i += ch;
    }
    out
}

/// Snapshot painted cell symbols for a rectangle (visible symbols only).
#[cfg(test)]
#[must_use]
pub fn snapshot_area(buf: &Buffer, area: Rect) -> Vec<String> {
    let mut out = Vec::with_capacity(area.height as usize);
    for row in 0..area.height {
        let y = area.y + row;
        let mut line = String::new();
        let mut x = area.x;
        while x < area.x + area.width {
            if let Some(cell) = buf.cell((x, y)) {
                if cell.diff_option == CellDiffOption::Skip {
                    x = x.saturating_add(1);
                    continue;
                }
                line.push_str(cell.symbol());
            } else {
                line.push(' ');
            }
            x = x.saturating_add(1);
        }
        out.push(line);
    }
    out
}

/// Render into a fresh buffer and return painted rows; assert height contract.
///
/// # Panics
///
/// Panics when measure height does not equal the painted row count.
#[cfg(test)]
pub fn render_snapshot<C: crate::component::Component + ?Sized>(
    component: &mut C,
    width: u16,
) -> Vec<String> {
    let height = component.measure(width);
    let area = Rect::new(0, 0, width, height.max(1));
    let mut buf = Buffer::empty(area);
    if height == 0 {
        // Zero-height components must not paint into a non-zero area.
        component.render(Rect::new(0, 0, width, 0), &mut buf);
        return Vec::new();
    }
    component.render(area, &mut buf);
    let painted = snapshot_area(&buf, Rect::new(0, 0, width, height));
    assert_eq!(
        u16::try_from(painted.len()).unwrap_or(u16::MAX),
        height,
        "measure height must equal rendered row count at width {width}"
    );
    painted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{FrameAnnotations, with_annotations};
    use crate::link::{format_link_close_bel, format_link_open_bel, hyperlink_capped};
    use std::cell::RefCell;

    fn paint_with_annotations(
        x: u16,
        y: u16,
        max_width: usize,
        line: &str,
    ) -> (Buffer, Vec<crate::frame::RawRegion>) {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 4));
        let annotations = RefCell::new(FrameAnnotations::new());
        with_annotations(&annotations, || {
            paint_line(x, y, max_width, &mut buf, line);
        });
        let regions = annotations.into_inner().into_parts().1;
        (buf, regions)
    }

    #[test]
    fn paint_line_records_hyperlink_region() {
        let styled_label = "\u{1b}[34mexample\u{1b}[0m";
        let line = format!("see {}", hyperlink_capped(styled_label, "https://example.com", None));
        let (buf, regions) = paint_with_annotations(5, 2, 80, &line);
        assert_eq!(regions.len(), 1, "one region per balanced link span");
        let region = &regions[0];
        // "see " occupies columns 0..3; label starts at column 4 of the line,
        // which is screen column 5 + 4 = 9 and spans 7 visible columns.
        assert_eq!(region.area, Rect::new(9, 2, 7, 1));
        assert!(region.bytes.starts_with(b"\x1b]8;;https://example.com\x1b\\"));
        assert!(region.bytes.ends_with(b"\x1b]8;;\x1b\\\x1b[0m"));
        let rendered = String::from_utf8_lossy(&region.bytes).into_owned();
        assert!(rendered.contains("example"), "label text rides in the region");
        // Label cells are still painted for non-raw consumers (tests, fallback).
        assert_eq!(buf.cell((9, 2)).map(|c| c.symbol()), Some("e"));
        assert_eq!(buf.cell((15, 2)).map(|c| c.symbol()), Some("e"));
    }

    #[test]
    fn paint_line_records_hyperlink_region_bel_terminator() {
        let line = format!(
            "{}label{}",
            format_link_open_bel("https://example.com", None).unwrap(),
            format_link_close_bel()
        );
        let (_, regions) = paint_with_annotations(0, 0, 80, &line);
        assert_eq!(regions.len(), 1);
        assert!(regions[0].bytes.starts_with(b"\x1b]8;;https://example.com\x07"));
        assert!(regions[0].bytes.ends_with(b"\x1b]8;;\x07\x1b[0m"));
    }

    #[test]
    fn paint_line_drops_region_when_label_truncated() {
        let line = hyperlink_capped("example", "https://example.com", None);
        // max_width cuts inside the label: the close sequence is never seen.
        let (_, regions) = paint_with_annotations(0, 0, 3, &line);
        assert!(regions.is_empty(), "no dangling open on truncation");
    }

    #[test]
    fn paint_line_plain_line_records_no_region() {
        let (_, regions) = paint_with_annotations(0, 0, 80, "plain \u{1b}[1mbold\u{1b}[0m text");
        assert!(regions.is_empty());
    }

    #[test]
    fn paint_line_outside_frame_still_paints_label() {
        // No with_annotations: push is a no-op, label cells must still paint.
        let line = hyperlink_capped("example", "https://example.com", None);
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 1));
        paint_line(0, 0, 80, &mut buf, &line);
        assert_eq!(buf.cell((0, 0)).map(|c| c.symbol()), Some("e"));
        assert_eq!(buf.cell((6, 0)).map(|c| c.symbol()), Some("e"));
    }

    #[test]
    fn paint_line_records_region_when_label_fills_line_exactly() {
        // Wrapped rows fill interior lines to exactly the margin and carry
        // the close at col == max_width; the region must survive.
        let line = hyperlink_capped("abcdef", "https://example.com", None);
        let (_, regions) = paint_with_annotations(0, 0, 6, &line);
        assert_eq!(regions.len(), 1, "close at the margin still closes the span");
        assert_eq!(regions[0].area, Rect::new(0, 0, 6, 1));
    }

    #[test]
    fn paint_line_region_carries_outer_style_context() {
        // Italic set before the open: the replay must re-establish it so the
        // overwritten label cells keep their painted style (blockquote case).
        let line = format!("\u{1b}[3m{}", hyperlink_capped("label", "https://example.com", None));
        let (_, regions) = paint_with_annotations(0, 0, 80, &line);
        assert_eq!(regions.len(), 1);
        assert!(
            regions[0].bytes.starts_with(b"\x1b[3m\x1b]8;;https://example.com\x1b\\"),
            "style context at open prefixes the region: {:?}",
            String::from_utf8_lossy(&regions[0].bytes)
        );
    }

    #[test]
    fn paint_line_replay_is_identical_to_first_paint() {
        // Second paint of unchanged content takes the memo replay path; it
        // must reproduce the derived buffer and regions exactly, including
        // wide-grapheme skip flags and hyperlink region bytes.
        let line = format!(
            "\u{1b}[1mbold\u{1b}[0m 日本語 {} tail",
            hyperlink_capped("label", "https://example.com", None)
        );
        let (first, first_regions) = paint_with_annotations(0, 0, 40, &line);
        let (second, second_regions) = paint_with_annotations(0, 0, 40, &line);
        assert_eq!(first, second, "replayed cells must equal derived cells");
        assert_eq!(
            first_regions, second_regions,
            "replayed regions must equal derived regions"
        );
        // "bold" spans cols 0..4, space at 4, 日 at 5, continuation at 6.
        assert_eq!(
            first.cell((6, 0)).map(|c| c.diff_option),
            Some(CellDiffOption::Skip)
        );
    }

    #[test]
    fn paint_line_replay_translates_position() {
        let line = format!(
            "pad {}",
            hyperlink_capped("label", "https://example.com", None)
        );
        let (base, base_regions) = paint_with_annotations(0, 0, 40, &line);
        let (shifted, shifted_regions) = paint_with_annotations(2, 3, 40, &line);
        // Same painted row content, offset by (+2, +3).
        for x in 0..38u16 {
            assert_eq!(
                base.cell((x, 0)).map(|c| (c.symbol(), c.diff_option)),
                shifted.cell((x + 2, 3)).map(|c| (c.symbol(), c.diff_option)),
                "column {x} must replay identically when translated"
            );
        }
        assert_eq!(shifted_regions.len(), 1);
        let base_area = base_regions[0].area;
        assert_eq!(
            shifted_regions[0].area,
            Rect::new(base_area.x + 2, base_area.y + 3, base_area.width, 1),
            "replayed region area must translate with the paint origin"
        );
        assert_eq!(base_regions[0].bytes, shifted_regions[0].bytes);
    }

    #[test]
    fn paint_line_cache_keys_on_width() {
        // The same line at a different max_width derives (and replays)
        // independently: truncation at the narrower width must hold.
        let line = "abcdefghij".to_owned();
        let (wide, _) = paint_with_annotations(0, 0, 10, &line);
        let (narrow, _) = paint_with_annotations(0, 0, 4, &line);
        let (narrow_again, _) = paint_with_annotations(0, 0, 4, &line);
        assert_eq!(wide.cell((9, 0)).map(|c| c.symbol()), Some("j"));
        assert_eq!(narrow.cell((4, 0)).map(|c| c.symbol()), Some(" "), "cut at width 4");
        assert_eq!(narrow, narrow_again, "narrow replay stays self-consistent");
    }
}
