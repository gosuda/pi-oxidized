//! Shared helpers for component measure/render against Ratatui buffers.

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;

use crate::text::{extract_ansi_code, grapheme_width, visible_width};

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
pub fn paint_line(x: u16, y: u16, max_width: usize, buf: &mut Buffer, line: &str) {
    if max_width == 0 {
        return;
    }
    let mut col = 0usize;
    let mut i = 0usize;
    let mut style = PaintStyle::default();
    while i < line.len() && col < max_width {
        if let Some(ansi) = extract_ansi_code(line, i) {
            style.process(ansi.code);
            i += ansi.len;
            continue;
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
        let cell_x = x.saturating_add(u16::try_from(col).unwrap_or(u16::MAX));
        if let Some(cell) = buf.cell_mut((cell_x, y)) {
            cell.set_symbol(grapheme);
            cell.set_style(style.to_style());
        }
        for extra in 1..gw {
            let cx = x.saturating_add(u16::try_from(col + extra).unwrap_or(u16::MAX));
            if let Some(cell) = buf.cell_mut((cx, y)) {
                cell.reset();
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }
        col = col.saturating_add(gw);
        i = i.saturating_add(grapheme.len());
    }
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
