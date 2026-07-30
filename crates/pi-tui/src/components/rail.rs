//! Rail container — a painted glyph in the left gutter beside a child.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::component::{Component, EventResult, UiEvent};

use super::util::paint_line;

/// Columns consumed by the rail: glyph at column 0, then one space.
pub const RAIL_WIDTH: u16 = 2;

/// Glyph painter for the rail cell.
type RailPaintFn = Box<dyn Fn(&str) -> String + Send>;

/// Container that draws a one-column rail glyph beside its child.
pub struct Rail {
    glyph: String,
    paint: RailPaintFn,
    children: Vec<Box<dyn Component>>,
}

impl Rail {
    /// Rail glyph drawn in column 0 of every rendered row, then one space,
    /// then the child. `paint` applies ANSI styling to the glyph.
    #[must_use]
    pub fn with_glyph(
        glyph: impl Into<String>,
        paint: impl Fn(&str) -> String + Send + 'static,
    ) -> Self {
        Self {
            glyph: glyph.into(),
            paint: Box::new(paint),
            children: Vec::new(),
        }
    }

    /// Add a child component.
    pub fn add_child(&mut self, child: impl Component + 'static) {
        self.children.push(Box::new(child));
    }

    fn draw_glyph(&self, area: Rect, buf: &mut Buffer) {
        let painted = (self.paint)(&self.glyph);
        for row in 0..area.height {
            let y = area.y.saturating_add(row);
            paint_line(area.x, y, 1, buf, &painted);
        }
    }
}

impl Component for Rail {
    fn measure(&mut self, width: u16) -> u16 {
        let content_width = width.saturating_sub(RAIL_WIDTH);
        if content_width == 0 {
            return 0;
        }
        let mut height = 0u16;
        for child in &mut self.children {
            height = height.saturating_add(child.measure(content_width));
        }
        height
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let content_width = area.width.saturating_sub(RAIL_WIDTH);
        if content_width == 0 {
            self.draw_glyph(area, buf);
            return;
        }
        self.draw_glyph(area, buf);
        let mut y = area.y;
        for child in &mut self.children {
            // pi-tui components render into shared buffers, so a component
            // must never paint outside its assigned Rect. Clamp each child to
            // the rows remaining inside `area` and stop once none are left,
            // so a parent that hands Rail a rectangle shorter than its
            // measured children cannot let a child overwrite the components
            // below it.
            let remaining = area.bottom().saturating_sub(y);
            if remaining == 0 {
                break;
            }
            let h = child.measure(content_width);
            if h == 0 {
                continue;
            }
            let h = h.min(remaining);
            let child_area = Rect::new(area.x.saturating_add(RAIL_WIDTH), y, content_width, h);
            child.render(child_area, buf);
            y = y.saturating_add(h);
        }
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
    fn rail_glyph_on_every_row() {
        let mut rail = Rail::with_glyph("|", str::to_owned);
        rail.add_child(Text::with_padding("one\ntwo\nthree", 0, 0));
        let snap = render_snapshot(&mut rail, 40);
        assert_eq!(snap.len(), 3);
        for row in &snap {
            assert!(
                row.starts_with('|'),
                "row must start with rail glyph: {row:?}"
            );
        }
    }

    #[test]
    fn measure_matches_child_at_reduced_width() {
        let mut child = Text::with_padding("one\ntwo\nthree", 0, 0);
        let child_height = child.measure(38);
        let mut rail = Rail::with_glyph("|", str::to_owned);
        rail.add_child(Text::with_padding("one\ntwo\nthree", 0, 0));
        assert_eq!(rail.measure(40), child_height);
    }

    #[test]
    fn width_one_draws_glyph_without_child() {
        use ratatui::buffer::Buffer;
        let mut rail = Rail::with_glyph("|", str::to_owned);
        rail.add_child(Text::with_padding("hi", 0, 0));
        assert_eq!(rail.measure(1), 0);
        // Defensive: even if a caller violates the contract and hands render a
        // non-zero area at width 1, the glyph is drawn and the child is not.
        let area = Rect::new(0, 0, 1, 2);
        let mut buf = Buffer::empty(area);
        rail.render(area, &mut buf);
        for row in 0..2 {
            assert_eq!(
                buf.cell((0, row)).map(ratatui::buffer::Cell::symbol),
                Some("|")
            );
        }
    }

    #[test]
    fn zero_height_child_draws_nothing() {
        let mut rail = Rail::with_glyph("|", str::to_owned);
        rail.add_child(Text::with_padding("", 0, 0));
        assert_eq!(rail.measure(40), 0);
        let snap = render_snapshot(&mut rail, 40);
        assert!(snap.is_empty());
    }
    #[test]
    fn clips_children_to_assigned_area() {
        use ratatui::buffer::Buffer;
        // Children measuring taller than the assigned area must be clipped:
        // nothing may paint at or below `area.bottom()` into the shared
        // buffer. Render into a buffer taller than the area so any overflow
        // shows up as non-default cells below the rail.
        let mut rail = Rail::with_glyph("|", str::to_owned);
        rail.add_child(Text::with_padding("a\nb\nc\nd", 0, 0));
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 6));
        rail.render(area, &mut buf);
        for y in area.bottom()..6 {
            for x in 0..10 {
                assert_eq!(
                    buf.cell((x, y)).map(ratatui::buffer::Cell::symbol),
                    Some(" "),
                    "row {y} (>= area.bottom) must stay default/empty"
                );
            }
        }
    }
}
