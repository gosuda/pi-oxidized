//! Child-first mouse handling without changing component rendering.

use crossterm::event::{MouseButton as CrosstermMouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::component::{
    Component, EventResult, MouseResponse, MouseTarget, TuiMouseButton, TuiMouseEvent,
    TuiMouseEventType, UiEvent, dispatch_mouse_event,
};
use crate::focus::FocusId;

/// Mouse callback used by [`MouseRegion`].
pub type MouseRegionHandler = Box<dyn FnMut(&TuiMouseEvent) -> Option<MouseResponse> + Send>;

/// Adds mouse handling to an existing component without changing its rendering.
pub struct MouseRegion {
    child: Box<dyn Component>,
    handler: MouseRegionHandler,
    id: FocusId,
    /// Screen rectangle from the most recent render; the hit region for
    /// routing [`UiEvent::Mouse`] through [`Component::handle_mouse`].
    area: Option<Rect>,
}

impl MouseRegion {
    /// Wrap `child`; the callback runs only when the child declines the event.
    #[must_use]
    pub fn new(
        child: impl Component + 'static,
        handler: impl FnMut(&TuiMouseEvent) -> Option<MouseResponse> + Send + 'static,
    ) -> Self {
        Self {
            child: Box::new(child),
            handler: Box::new(handler),
            id: FocusId::new(),
            area: None,
        }
    }

    /// Stable identity of this wrapper's local target.
    #[must_use]
    pub const fn focus_id(&self) -> FocusId {
        self.id
    }

    /// Borrow the wrapped child.
    #[must_use]
    pub fn child(&self) -> &dyn Component {
        &*self.child
    }

    /// Mutably borrow the wrapped child.
    pub fn child_mut(&mut self) -> &mut dyn Component {
        &mut *self.child
    }

    /// Route a raw pointer event through the wrapper's mouse path.
    ///
    /// Returns `None` when the event cannot belong to this region — no
    /// rendered rectangle yet, or the point lies outside it — or when the
    /// mouse path declined, so the caller keeps the plain [`UiEvent`]
    /// forwarding contract.
    fn route_mouse(&mut self, mouse: MouseEvent) -> Option<EventResult> {
        let area = self.area?;
        let target = MouseTarget::new(
            self.id,
            i32::from(area.x),
            i32::from(area.y),
            area.width,
            area.height,
            area,
        );
        if !target.contains_screen_point(mouse.column, mouse.row) {
            return None;
        }
        let event = normalize_region_mouse(mouse, area);
        let response = self.handle_mouse(&event)?;
        let (handled, render) = match response {
            MouseResponse::Local(result) => (result.is_handled(), result.should_render(event.kind)),
            MouseResponse::Forwarded(result) => {
                (result.is_handled(), result.should_render(event.kind))
            }
        };
        if !handled {
            return None;
        }
        Some(if render {
            EventResult::Render
        } else {
            EventResult::Consumed
        })
    }
}

impl Component for MouseRegion {
    fn measure(&mut self, width: u16) -> u16 {
        self.child.measure(width)
    }

    fn render(&mut self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        self.area = Some(area);
        self.child.render(area, buf);
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        if let UiEvent::Mouse(mouse) = event
            && let Some(result) = self.route_mouse(*mouse)
        {
            return result;
        }
        self.child.handle_event(event)
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        let origin_x = i32::from(event.screen_x).saturating_sub(event.x);
        let origin_y = i32::from(event.screen_y).saturating_sub(event.y);
        let (rect_x, rect_width) = clipped_axis(origin_x, event.width);
        let (rect_y, rect_height) = clipped_axis(origin_y, event.height);
        let rect = Rect::new(rect_x, rect_y, rect_width, rect_height);
        let target = MouseTarget::new(self.id, origin_x, origin_y, event.width, event.height, rect);
        if let Some(result) = dispatch_mouse_event(&mut *self.child, event, target) {
            return Some(MouseResponse::Forwarded(result));
        }
        (self.handler)(event)
    }

    fn invalidate(&mut self) {
        self.child.invalidate();
    }
}

fn clipped_axis(origin: i32, length: u16) -> (u16, u16) {
    let start = i64::from(origin);
    let end = start.saturating_add(i64::from(length));
    let visible_start = start.max(0);
    let visible_end = end.min(i64::from(u16::MAX) + 1);
    if visible_start > i64::from(u16::MAX) || visible_end <= visible_start {
        return (u16::MAX, 0);
    }
    let width =
        u16::try_from((visible_end - visible_start).min(i64::from(u16::MAX))).unwrap_or(u16::MAX);
    (u16::try_from(visible_start).unwrap_or(u16::MAX), width)
}

/// Normalize a raw terminal pointer event into the region-local shape the
/// [`Component::handle_mouse`] path expects: local coordinates relative to
/// the rendered rectangle and the rectangle as the target size.
///
/// The kind/button/wheel mapping mirrors the alt-screen viewport's
/// normalization, minus the viewport-owned click-cycle and wheel-scaling
/// state this wrapper does not carry.
fn normalize_region_mouse(mouse: MouseEvent, area: Rect) -> TuiMouseEvent {
    let (kind, button, wheel_delta) = match mouse.kind {
        MouseEventKind::Down(button) => {
            (TuiMouseEventType::Press, region_mouse_button(button), None)
        }
        MouseEventKind::Up(button) => (
            TuiMouseEventType::Release,
            region_mouse_button(button),
            None,
        ),
        MouseEventKind::Drag(button) => {
            (TuiMouseEventType::Drag, region_mouse_button(button), None)
        }
        MouseEventKind::Moved => (TuiMouseEventType::Move, TuiMouseButton::None, None),
        MouseEventKind::ScrollUp => (TuiMouseEventType::Wheel, TuiMouseButton::None, Some(-1)),
        MouseEventKind::ScrollDown => (TuiMouseEventType::Wheel, TuiMouseButton::None, Some(1)),
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
            (TuiMouseEventType::Wheel, TuiMouseButton::None, None)
        }
    };
    TuiMouseEvent {
        kind,
        button,
        x: i32::from(mouse.column).saturating_sub(i32::from(area.x)),
        y: i32::from(mouse.row).saturating_sub(i32::from(area.y)),
        screen_x: mouse.column,
        screen_y: mouse.row,
        width: area.width,
        height: area.height,
        modifiers: mouse.modifiers,
        wheel_delta,
        click_count: u8::from(kind == TuiMouseEventType::Press),
    }
}

fn region_mouse_button(button: CrosstermMouseButton) -> TuiMouseButton {
    match button {
        CrosstermMouseButton::Left => TuiMouseButton::Left,
        CrosstermMouseButton::Middle => TuiMouseButton::Middle,
        CrosstermMouseButton::Right => TuiMouseButton::Right,
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "unit tests use contextual failure messages"
)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crossterm::event::{KeyModifiers, MouseButton as CrosstermButton};
    use ratatui::buffer::Buffer;

    use super::*;
    use crate::component::MouseResult;

    /// Probe child that logs the channels it sees and replies with a fixed
    /// mouse response, so tests can observe child-first dispatch and raw
    /// [`UiEvent`] forwarding independently.
    struct Probe {
        log: Arc<Mutex<Vec<String>>>,
        mouse_response: Option<MouseResponse>,
    }

    impl Component for Probe {
        fn measure(&mut self, _width: u16) -> u16 {
            1
        }

        fn render(&mut self, _area: Rect, _buf: &mut Buffer) {}

        fn handle_event(&mut self, event: &UiEvent) -> EventResult {
            if let UiEvent::Mouse(mouse) = event {
                self.log
                    .lock()
                    .expect("probe log")
                    .push(format!("event {},{}", mouse.column, mouse.row));
            }
            EventResult::Ignored
        }

        fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
            self.log
                .lock()
                .expect("probe log")
                .push(format!("mouse {},{}", event.x, event.y));
            self.mouse_response
        }

        fn invalidate(&mut self) {}
    }

    fn press(column: u16, row: u16) -> UiEvent {
        UiEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(CrosstermButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn handled(render: Option<bool>) -> MouseResponse {
        MouseResponse::Local(MouseResult {
            handled: true,
            capture: false,
            focus: false,
            render,
        })
    }

    #[test]
    fn ui_event_mouse_inside_region_invokes_fallback_handler() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let handler_log = Arc::new(Mutex::new(Vec::new()));
        let mut region = MouseRegion::new(
            Probe {
                log: Arc::clone(&log),
                mouse_response: None,
            },
            {
                let handler_log = Arc::clone(&handler_log);
                move |event: &TuiMouseEvent| {
                    handler_log
                        .lock()
                        .expect("handler log")
                        .push((event.x, event.y));
                    Some(handled(None))
                }
            },
        );
        let area = Rect::new(10, 5, 8, 3);
        region.render(area, &mut Buffer::empty(area));

        let result = region.handle_event(&press(12, 6));

        // Press defaults to a repaint request.
        assert_eq!(result, EventResult::Render);
        // Child-first dispatch ran with region-local coordinates…
        assert_eq!(*log.lock().expect("probe log"), vec!["mouse 2,1"]);
        // …and the wrapper fallback saw the same normalized event.
        assert_eq!(*handler_log.lock().expect("handler log"), vec![(2, 1)]);
    }

    #[test]
    fn child_first_dispatch_preempts_fallback_handler() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let handler_log = Arc::new(Mutex::new(Vec::new()));
        let mut region = MouseRegion::new(
            Probe {
                log: Arc::clone(&log),
                mouse_response: Some(handled(Some(false))),
            },
            {
                let handler_log = Arc::clone(&handler_log);
                move |event: &TuiMouseEvent| {
                    handler_log
                        .lock()
                        .expect("handler log")
                        .push((event.x, event.y));
                    Some(handled(None))
                }
            },
        );
        let area = Rect::new(10, 5, 8, 3);
        region.render(area, &mut Buffer::empty(area));

        let result = region.handle_event(&press(12, 6));

        // The child claimed the event with an explicit no-render decision.
        assert_eq!(result, EventResult::Consumed);
        assert_eq!(*log.lock().expect("probe log"), vec!["mouse 2,1"]);
        assert!(handler_log.lock().expect("handler log").is_empty());
    }

    #[test]
    fn mouse_outside_region_keeps_plain_forwarding() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let handler_log = Arc::new(Mutex::new(Vec::new()));
        let mut region = MouseRegion::new(
            Probe {
                log: Arc::clone(&log),
                mouse_response: Some(handled(Some(false))),
            },
            {
                let handler_log = Arc::clone(&handler_log);
                move |event: &TuiMouseEvent| {
                    handler_log
                        .lock()
                        .expect("handler log")
                        .push((event.x, event.y));
                    Some(handled(None))
                }
            },
        );
        let area = Rect::new(10, 5, 8, 3);
        region.render(area, &mut Buffer::empty(area));

        let result = region.handle_event(&press(0, 0));

        // Neither mouse channel ran; the child kept the raw event contract.
        assert_eq!(result, EventResult::Ignored);
        assert_eq!(*log.lock().expect("probe log"), vec!["event 0,0"]);
        assert!(handler_log.lock().expect("handler log").is_empty());
    }

    #[test]
    fn mouse_without_rendered_geometry_keeps_plain_forwarding() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let handler_log = Arc::new(Mutex::new(Vec::new()));
        let mut region = MouseRegion::new(
            Probe {
                log: Arc::clone(&log),
                mouse_response: Some(handled(Some(false))),
            },
            {
                let handler_log = Arc::clone(&handler_log);
                move |event: &TuiMouseEvent| {
                    handler_log
                        .lock()
                        .expect("handler log")
                        .push((event.x, event.y));
                    Some(handled(None))
                }
            },
        );

        let result = region.handle_event(&press(12, 6));

        assert_eq!(result, EventResult::Ignored);
        assert_eq!(*log.lock().expect("probe log"), vec!["event 12,6"]);
        assert!(handler_log.lock().expect("handler log").is_empty());
    }
}
