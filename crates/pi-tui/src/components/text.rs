//! Multi-line word-wrapped text with padding and optional background.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::component::{Component, EventResult, UiEvent};
use crate::text::{visible_width, wrap_text_with_ansi};

use super::util::{apply_background, empty_line, paint_lines};

/// Background applicator for a full-width line.
type TextBgFn = Box<dyn Fn(&str) -> String + Send>;

/// Multi-line text component with horizontal/vertical padding and wrap cache.
pub struct Text {
    content: String,
    padding_x: u16,
    padding_y: u16,
    custom_bg: Option<TextBgFn>,
    cache: Option<RenderCache>,
}

#[derive(Clone)]
struct RenderCache {
    content: String,
    width: u16,
    lines: Vec<String>,
}

impl Text {
    /// Create a text component. Defaults match TS: `padding_x=1`, `padding_y=1`.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            content: text.into(),
            padding_x: 1,
            padding_y: 1,
            custom_bg: None,
            cache: None,
        }
    }

    /// Create with explicit padding.
    #[must_use]
    pub fn with_padding(text: impl Into<String>, padding_x: u16, padding_y: u16) -> Self {
        Self {
            content: text.into(),
            padding_x,
            padding_y,
            custom_bg: None,
            cache: None,
        }
    }

    /// Replace displayed text and drop the width cache.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.content = text.into();
        self.cache = None;
    }

    /// Borrow the current text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.content
    }

    /// Set horizontal padding.
    pub fn set_padding_x(&mut self, padding_x: u16) {
        self.padding_x = padding_x;
        self.cache = None;
    }

    /// Set vertical padding.
    pub fn set_padding_y(&mut self, padding_y: u16) {
        self.padding_y = padding_y;
        self.cache = None;
    }

    /// Optional background applicator receiving a full-width line.
    pub fn set_custom_bg<F>(&mut self, custom_bg: Option<F>)
    where
        F: Fn(&str) -> String + Send + 'static,
    {
        self.custom_bg = custom_bg.map(|f| Box::new(f) as TextBgFn);
        self.cache = None;
    }

    fn lines_for_width(&mut self, width: u16) -> Vec<String> {
        if let Some(cache) = &self.cache
            && cache.width == width
            && cache.content == self.content
        {
            return cache.lines.clone();
        }
        let lines = self.render_lines(width);
        self.cache = Some(RenderCache {
            content: self.content.clone(),
            width,
            lines: lines.clone(),
        });
        lines
    }

    fn render_lines(&self, width: u16) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        if self.content.trim().is_empty() {
            return Vec::new();
        }

        let normalized = self.content.replace('\t', "   ");
        let content_width =
            usize::from(width.saturating_sub(self.padding_x.saturating_mul(2))).max(1);
        let wrapped = wrap_text_with_ansi(&normalized, content_width);
        let left = " ".repeat(usize::from(self.padding_x));
        let right = left.clone();
        let bg = self
            .custom_bg
            .as_ref()
            .map(|f| f.as_ref() as &dyn Fn(&str) -> String);

        let mut content_lines = Vec::with_capacity(wrapped.len());
        for line in wrapped {
            let with_margins = format!("{left}{line}{right}");
            // If margins already exceed width (tiny terminal), still clamp.
            let clamped = if visible_width(&with_margins) > usize::from(width) {
                crate::text::truncate_to_width(&with_margins, usize::from(width), "", false)
            } else {
                with_margins
            };
            content_lines.push(apply_background(&clamped, usize::from(width), bg));
        }

        let empty = match bg {
            Some(f) => f(&empty_line(usize::from(width))),
            None => empty_line(usize::from(width)),
        };
        let mut result =
            Vec::with_capacity(content_lines.len() + usize::from(self.padding_y).saturating_mul(2));
        for _ in 0..self.padding_y {
            result.push(empty.clone());
        }
        result.extend(content_lines);
        for _ in 0..self.padding_y {
            result.push(empty.clone());
        }
        if result.is_empty() {
            vec![String::new()]
        } else {
            result
        }
    }
}

impl Component for Text {
    fn measure(&mut self, width: u16) -> u16 {
        let lines = self.lines_for_width(width);
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let lines = self.lines_for_width(area.width);
        paint_lines(area, buf, &lines);
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

    #[test]
    fn empty_text_measures_zero() {
        let mut t = Text::new("");
        assert_eq!(t.measure(80), 0);
        let snap = render_snapshot(&mut t, 80);
        assert!(snap.is_empty());
    }

    #[test]
    fn wrap_and_padding_heights() {
        let mut t = Text::with_padding("hello world from text", 1, 1);
        for width in [24_u16, 60, 80, 120] {
            let snap = render_snapshot(&mut t, width);
            assert!(!snap.is_empty());
            // top padding blank
            assert!(strip_ansi(&snap[0]).chars().all(|c| c == ' '));
        }
    }

    #[test]
    fn cache_invalidation_on_set_text() {
        let mut t = Text::with_padding("one", 0, 0);
        let h1 = t.measure(40);
        t.set_text("one\ntwo\nthree");
        let h2 = t.measure(40);
        assert!(h2 >= h1);
        assert_eq!(t.measure(40), h2);
    }

    #[test]
    fn no_style_leak_between_rows() {
        let mut t = Text::with_padding("\u{1b}[31mred\u{1b}[0m\nplain", 0, 0);
        let snap = render_snapshot(&mut t, 40);
        assert!(strip_ansi(&snap[0]).contains("red"));
        assert!(strip_ansi(&snap[1]).contains("plain"));
    }

    #[test]
    fn tabs_expand_to_three_spaces() {
        let mut t = Text::with_padding("a\tb", 0, 0);
        let snap = render_snapshot(&mut t, 40);
        let plain = strip_ansi(&snap[0]);
        assert!(plain.contains("a   b") || plain.starts_with("a   b"));
    }
}
