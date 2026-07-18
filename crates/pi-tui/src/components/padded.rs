//! Padded container (TS `Box`) — padding and optional background, no borders.

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;

use crate::component::{Component, EventResult, UiEvent};

use super::util::{apply_background, empty_line, paint_lines};

/// Background applicator for a full-width padded line.
type PaddedBgFn = Box<dyn Fn(&str) -> String + Send>;

/// Container that applies padding and optional background to children.
///
/// Named `Padded` because the TS `Box` draws **no** border characters.
pub struct Padded {
    children: Vec<Box<dyn Component>>,
    padding_x: u16,
    padding_y: u16,
    bg: Option<PaddedBgFn>,
    cache: Option<RenderCache>,
}

struct RenderCache {
    width: u16,
    lines: Vec<String>,
}

impl Padded {
    /// Create a padded container. Defaults: `padding_x=1`, `padding_y=1`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
            padding_x: 1,
            padding_y: 1,
            bg: None,
            cache: None,
        }
    }

    /// Create with padding.
    #[must_use]
    pub fn with_padding(padding_x: u16, padding_y: u16) -> Self {
        Self {
            children: Vec::new(),
            padding_x,
            padding_y,
            bg: None,
            cache: None,
        }
    }

    /// Add a child component.
    pub fn add_child(&mut self, child: impl Component + 'static) {
        self.children.push(Box::new(child));
        self.cache = None;
    }

    /// Remove all children.
    pub fn clear(&mut self) {
        self.children.clear();
        self.cache = None;
    }

    /// Number of children.
    #[must_use]
    pub fn len(&self) -> usize {
        self.children.len()
    }

    /// True when there are no children.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// Set background applicator.
    pub fn set_bg<F>(&mut self, bg: Option<F>)
    where
        F: Fn(&str) -> String + Send + 'static,
    {
        self.bg = bg.map(|f| Box::new(f) as PaddedBgFn);
        self.cache = None;
    }

    fn render_lines(&mut self, width: u16) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        if self.children.is_empty() {
            return Vec::new();
        }

        let content_width = width.saturating_sub(self.padding_x.saturating_mul(2));
        let left = " ".repeat(usize::from(self.padding_x));
        let bg = self
            .bg
            .as_ref()
            .map(|f| f.as_ref() as &dyn Fn(&str) -> String);

        let mut content_lines: Vec<String> = Vec::new();
        for child in &mut self.children {
            let h = child.measure(content_width);
            if h == 0 {
                continue;
            }
            let area = Rect::new(0, 0, content_width.max(1), h);
            let mut buf = Buffer::empty(area);
            child.render(area, &mut buf);
            for row in 0..h {
                let mut line = String::new();
                let mut x = 0u16;
                while x < content_width.max(1) {
                    if let Some(cell) = buf.cell((x, row))
                        && cell.diff_option != CellDiffOption::Skip
                    {
                        line.push_str(cell.symbol());
                    }
                    x = x.saturating_add(1);
                }
                let with_pad = format!("{left}{line}");
                content_lines.push(apply_background(&with_pad, usize::from(width), bg));
            }
        }

        let empty = match bg {
            Some(f) => f(&empty_line(usize::from(width))),
            None => empty_line(usize::from(width)),
        };
        let mut result = Vec::new();
        for _ in 0..self.padding_y {
            result.push(empty.clone());
        }
        result.extend(content_lines);
        for _ in 0..self.padding_y {
            result.push(empty.clone());
        }
        result
    }
}

impl Default for Padded {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Padded {
    fn measure(&mut self, width: u16) -> u16 {
        let lines = self.render_lines(width);
        // Cache for render.
        self.cache = Some(RenderCache {
            width,
            lines: lines.clone(),
        });
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let lines = if let Some(cache) = &self.cache {
            if cache.width == area.width {
                cache.lines.clone()
            } else {
                self.render_lines(area.width)
            }
        } else {
            self.render_lines(area.width)
        };
        paint_lines(area, buf, &lines);
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        let mut result = EventResult::Ignored;
        for child in &mut self.children {
            result = result.merge(child.handle_event(event));
            if result.is_handled() {
                break;
            }
        }
        result
    }

    fn invalidate(&mut self) {
        self.cache = None;
        for child in &mut self.children {
            child.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::text::Text;
    use crate::components::util::render_snapshot;

    #[test]
    fn empty_children_zero_height() {
        let mut p = Padded::new();
        assert_eq!(p.measure(80), 0);
    }

    #[test]
    fn pads_child() {
        let mut p = Padded::with_padding(2, 1);
        p.add_child(Text::with_padding("hi", 0, 0));
        let snap = render_snapshot(&mut p, 40);
        // top pad + content + bottom pad
        assert!(snap.len() >= 3);
    }
}
