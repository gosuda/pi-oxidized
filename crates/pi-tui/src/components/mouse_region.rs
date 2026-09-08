//! Child-first mouse handling without changing component rendering.

use ratatui::layout::Rect;

use crate::component::{
    Component, MouseResponse, MouseTarget, TuiMouseEvent, dispatch_mouse_event,
};
use crate::focus::FocusId;

/// Mouse callback used by [`MouseRegion`].
pub type MouseRegionHandler =
    Box<dyn FnMut(&TuiMouseEvent) -> Option<MouseResponse> + Send>;

/// Adds mouse handling to an existing component without changing its rendering.
pub struct MouseRegion {
    child: Box<dyn Component>,
    handler: MouseRegionHandler,
    id: FocusId,
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
}

impl Component for MouseRegion {
    fn measure(&mut self, width: u16) -> u16 {
        self.child.measure(width)
    }

    fn render(&mut self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        self.child.render(area, buf);
    }

    fn handle_event(&mut self, event: &crate::component::UiEvent) -> crate::component::EventResult {
        self.child.handle_event(event)
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        let origin_x = i32::from(event.screen_x).saturating_sub(event.x);
        let origin_y = i32::from(event.screen_y).saturating_sub(event.y);
        let (rect_x, rect_width) = clipped_axis(origin_x, event.width);
        let (rect_y, rect_height) = clipped_axis(origin_y, event.height);
        let rect = Rect::new(rect_x, rect_y, rect_width, rect_height);
        let target = MouseTarget::new(
            self.id,
            origin_x,
            origin_y,
            event.width,
            event.height,
            rect,
        );
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
    let width = u16::try_from((visible_end - visible_start).min(i64::from(u16::MAX)))
        .unwrap_or(u16::MAX);
    (
        u16::try_from(visible_start).unwrap_or(u16::MAX),
        width,
    )
}
