//! Padded container (TS `Box`) — padding and optional background, no borders.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::component::{
    Component, EventResult, MouseResponse, MouseTarget, TuiMouseEvent, UiEvent,
    dispatch_mouse_event,
};
use crate::focus::FocusId;

use super::util::{apply_background, empty_line, paint_line};

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
        }
    }

    /// Add a child component.
    pub fn add_child(&mut self, child: impl Component + 'static) {
        self.children.push(Box::new(child));
    }

    /// Remove all children.
    pub fn clear(&mut self) {
        self.children.clear();
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
    }

    fn background_blank(&self, width: u16) -> String {
        let bg = self
            .bg
            .as_ref()
            .map(|f| f.as_ref() as &dyn Fn(&str) -> String);
        apply_background(&empty_line(usize::from(width)), usize::from(width), bg)
    }
}

impl Default for Padded {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Padded {
    fn measure(&mut self, width: u16) -> u16 {
        if width == 0 || self.children.is_empty() {
            return 0;
        }
        let content_width = width
            .saturating_sub(self.padding_x.saturating_mul(2))
            .max(1);
        let content: u32 = self
            .children
            .iter_mut()
            .map(|child| u32::from(child.measure(content_width)))
            .sum();
        content
            .saturating_add(u32::from(self.padding_y).saturating_mul(2))
            .try_into()
            .unwrap_or(u16::MAX)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || self.children.is_empty() {
            return;
        }
        let pad_line = self.background_blank(area.width);
        let content_width = area
            .width
            .saturating_sub(self.padding_x.saturating_mul(2))
            .max(1);
        let mut y = area.y;
        for _ in 0..self.padding_y {
            if y >= area.bottom() {
                return;
            }
            paint_line(area.x, y, usize::from(area.width), buf, &pad_line);
            y += 1;
        }
        for child in &mut self.children {
            let height = child.measure(content_width);
            if height == 0 {
                continue;
            }
            let visible = area.bottom().saturating_sub(y).min(height);
            if visible > 0 {
                child.render(
                    Rect::new(
                        area.x.saturating_add(self.padding_x),
                        y,
                        content_width,
                        visible,
                    ),
                    buf,
                );
            }
            y = y.saturating_add(height);
        }
        while y < area.bottom() {
            paint_line(area.x, y, usize::from(area.width), buf, &pad_line);
            y += 1;
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

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        let content_width = event
            .width
            .saturating_sub(self.padding_x.saturating_mul(2))
            .max(1);
        let origin_x = i32::from(event.screen_x).saturating_sub(event.x);
        let origin_y = i32::from(event.screen_y).saturating_sub(event.y);
        let padding_x = i32::from(self.padding_x);
        let content_origin_x = origin_x + padding_x;
        let mut child_y = i32::from(self.padding_y);
        for child in &mut self.children {
            let height = child.measure(content_width);
            if height == 0 {
                continue;
            }
            let inside = event.y >= child_y
                && event.y < child_y + i32::from(height)
                && event.x >= padding_x
                && event.x < padding_x + i32::from(content_width);
            if inside {
                let child_origin_y = origin_y + child_y;
                let rect = Rect::new(
                    u16::try_from(content_origin_x.max(0)).unwrap_or(0),
                    u16::try_from(child_origin_y.max(0)).unwrap_or(u16::MAX),
                    content_width,
                    height,
                );
                let target = MouseTarget::new(
                    FocusId::new(),
                    content_origin_x,
                    child_origin_y,
                    content_width,
                    height,
                    rect,
                );
                let result = dispatch_mouse_event(child.as_mut(), event, target)?;
                return Some(MouseResponse::Forwarded(result));
            }
            child_y += i32::from(height);
        }
        None
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "unit tests use contextual failure messages"
)]
mod tests {
    use super::*;
    use crate::components::text::Text;
    use crate::components::util::render_snapshot;
    use ratatui::style::{Color, Style};

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

    /// A child rendering styled cells keeps its style: the container renders
    /// children into the target buffer sub-area instead of round-tripping
    /// symbols through strings.
    #[test]
    fn child_styles_survive_render() {
        struct StyledLine;
        impl Component for StyledLine {
            fn measure(&mut self, _width: u16) -> u16 {
                1
            }
            fn render(&mut self, area: Rect, buf: &mut Buffer) {
                for x in area.x..area.width.min(area.x + area.width) {
                    if let Some(cell) = buf.cell_mut((x, area.y)) {
                        cell.set_symbol("X");
                        cell.set_style(Style::default().fg(Color::Red));
                    }
                }
            }
            fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
                EventResult::Ignored
            }
            fn invalidate(&mut self) {}
        }

        let mut p = Padded::with_padding(1, 1);
        p.add_child(StyledLine);
        let height = p.measure(10);
        let area = Rect::new(0, 0, 10, height);
        let mut buf = Buffer::empty(area);
        p.render(area, &mut buf);
        let styled = (0..height)
            .filter_map(|y| buf.cell((1, y)))
            .find(|cell| cell.symbol() == "X")
            .expect("styled cell rendered");
        assert_eq!(styled.style().fg, Some(Color::Red), "child style lost");
    }

    #[test]
    fn measure_includes_vertical_padding() {
        let mut p = Padded::with_padding(0, 2);
        p.add_child(Text::with_padding("hi", 0, 0));
        // one text row + 2*padding_y
        assert_eq!(p.measure(20), 5);
    }

    /// A press inside a child band forwards coordinates local to the child's
    /// own band, and a press in the padding gutter declines instead of
    /// forwarding an out-of-band column.
    #[test]
    fn mouse_dispatch_offsets_child_by_padding() {
        use std::sync::{Arc, Mutex};

        use crate::component::{MouseResult, TuiMouseButton, TuiMouseEventType};

        struct Probe {
            events: Arc<Mutex<Vec<(i32, i32)>>>,
        }
        impl Component for Probe {
            fn measure(&mut self, _width: u16) -> u16 {
                1
            }
            fn render(&mut self, _area: Rect, _buf: &mut Buffer) {}
            fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
                EventResult::Ignored
            }
            fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
                self.events
                    .lock()
                    .expect("probe events")
                    .push((event.x, event.y));
                Some(MouseResponse::Local(MouseResult {
                    handled: true,
                    capture: false,
                    focus: false,
                    render: None,
                }))
            }
            fn invalidate(&mut self) {}
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        let mut p = Padded::with_padding(2, 1);
        p.add_child(Probe {
            events: Arc::clone(&events),
        });
        // Container at screen (10, 5), 20x3: pad row, child row, pad row.
        let event = TuiMouseEvent {
            kind: TuiMouseEventType::Press,
            button: TuiMouseButton::Left,
            x: 5,
            y: 1,
            screen_x: 15,
            screen_y: 6,
            width: 20,
            height: 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
            wheel_delta: None,
            click_count: 1,
        };
        let response = p.handle_mouse(&event).expect("in-band press reaches child");
        assert!(matches!(response, MouseResponse::Forwarded(_)));
        let recorded = events.lock().expect("probe events").clone();
        assert_eq!(
            recorded,
            vec![(3, 0)],
            "child must see its own band's local coordinates"
        );

        // A press in the left padding gutter declines without forwarding.
        let gutter = TuiMouseEvent {
            x: 1,
            screen_x: 11,
            ..event
        };
        assert!(p.handle_mouse(&gutter).is_none());
        assert_eq!(events.lock().expect("probe events").len(), 1);
    }
}
