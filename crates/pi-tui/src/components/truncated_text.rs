//! First-line truncated text with optional padding.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::component::{Component, EventResult, UiEvent};
use crate::text::{truncate_to_width, visible_width};

use super::util::{empty_line, pad_to_width, paint_lines};

/// Single-line truncated text (first line only) with padding.
pub struct TruncatedText {
    text: String,
    padding_x: u16,
    padding_y: u16,
}

impl TruncatedText {
    /// Create truncated text. Defaults match TS: `padding_x=0`, `padding_y=0`.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            padding_x: 0,
            padding_y: 0,
        }
    }

    /// Create with padding.
    #[must_use]
    pub fn with_padding(text: impl Into<String>, padding_x: u16, padding_y: u16) -> Self {
        Self {
            text: text.into(),
            padding_x,
            padding_y,
        }
    }

    /// Replace text.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
    }

    fn render_lines(&self, width: u16) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        let empty = empty_line(usize::from(width));
        let mut result = Vec::new();
        for _ in 0..self.padding_y {
            result.push(empty.clone());
        }

        let available = usize::from(width.saturating_sub(self.padding_x.saturating_mul(2))).max(1);
        let single = self
            .text
            .split_once('\n')
            .map_or(self.text.as_str(), |(first, _)| first);
        let display = truncate_to_width(single, available, "...", false);
        let left = " ".repeat(usize::from(self.padding_x));
        let right = left.clone();
        let with_pad = format!("{left}{display}{right}");
        let final_line = if visible_width(&with_pad) < usize::from(width) {
            pad_to_width(&with_pad, usize::from(width))
        } else {
            truncate_to_width(&with_pad, usize::from(width), "", false)
        };
        result.push(final_line);

        for _ in 0..self.padding_y {
            result.push(empty.clone());
        }
        result
    }
}

impl Component for TruncatedText {
    fn measure(&mut self, width: u16) -> u16 {
        if width == 0 {
            return 0;
        }
        1u16.saturating_add(self.padding_y.saturating_mul(2))
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let lines = self.render_lines(area.width);
        paint_lines(area, buf, &lines);
    }

    fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
        EventResult::Ignored
    }

    fn invalidate(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::util::{render_snapshot, strip_ansi};

    #[test]
    fn first_line_only() {
        let mut t = TruncatedText::new("hello\nworld");
        let snap = render_snapshot(&mut t, 80);
        assert_eq!(snap.len(), 1);
        assert!(strip_ansi(&snap[0]).contains("hello"));
        assert!(!strip_ansi(&snap[0]).contains("world"));
    }

    #[test]
    fn ellipsis_at_narrow_width() {
        let mut t = TruncatedText::new("abcdefghijklmnopqrstuvwxyz");
        let snap = render_snapshot(&mut t, 10);
        let plain = strip_ansi(&snap[0]);
        assert!(plain.contains("..."));
        assert!(visible_width(&plain) <= 10);
    }

    #[test]
    fn padding_y_adds_rows() {
        let mut t = TruncatedText::with_padding("hi", 0, 1);
        assert_eq!(t.measure(40), 3);
        let snap = render_snapshot(&mut t, 40);
        assert_eq!(snap.len(), 3);
    }

    #[test]
    fn widths_matrix() {
        let mut t = TruncatedText::new("the quick brown fox jumps over the lazy dog");
        for width in [24_u16, 60, 80, 120] {
            let snap = render_snapshot(&mut t, width);
            assert_eq!(snap.len(), 1);
            assert!(visible_width(&strip_ansi(&snap[0])) <= usize::from(width));
        }
    }
}
