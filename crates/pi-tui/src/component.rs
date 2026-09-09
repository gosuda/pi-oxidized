//! Product-agnostic component contract for the native TUI.

use crossterm::event::{KeyEvent, KeyModifiers, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::focus::FocusId;

/// Closed UI event set consumed by components and the terminal event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    /// Keyboard event from the sole [`crossterm::event::EventStream`] owner.
    Key(KeyEvent),
    /// Mouse event from the sole [`crossterm::event::EventStream`] owner.
    ///
    /// Coordinates remain the parsed, zero-based terminal coordinates. Native
    /// components normalize them into signed local coordinates at dispatch.
    Mouse(MouseEvent),
    /// Bracketed-paste payload with OS newlines normalized by the input task.
    Paste(String),
    /// Terminal gained focus.
    FocusGained,
    /// Terminal lost focus.
    FocusLost,
    /// Terminal size changed.
    Resize {
        /// New column count.
        width: u16,
        /// New row count.
        height: u16,
    },
}

/// One positioned row span exposed by a prepared component.
///
/// The span borrows the component's existing styled row cache. Consumers must
/// use the declared `width` when painting it; a row key is not a component or
/// message identity.
pub struct DisplayRowSpan<'a> {
    /// Column at which the span begins in its parent row.
    pub column: u16,
    /// Exact width used to build the row content.
    pub width: u16,
    /// Borrowed row content.
    pub content: DisplayRowContent<'a>,
}

/// Borrowed content for one prepared display-row span.
pub enum DisplayRowContent<'a> {
    /// A styled, keyed text row from a native component cache.
    Text(&'a crate::components::util::KeyedLine),
    /// A validated image placement row.
    Image {
        /// Opaque, validated image data and placement metadata.
        image: &'a crate::image::DocumentImage,
        /// Zero-based row within the logical image placement.
        row_in_image: u16,
    },
}

/// Failure while preparing or visiting a retained component row source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RowSourceError {
    /// The component does not expose a retained row source.
    #[error("component row source is unsupported: {component}")]
    Unsupported {
        /// Component name identifying the unsupported row source.
        component: &'static str,
    },
    /// A row was requested before a successful preparation.
    #[error("component rows are not prepared")]
    NotPrepared,
    /// The requested row is outside the prepared source.
    #[error("row {row} is outside {rows} prepared rows")]
    RowOutOfBounds {
        /// Requested zero-based row index.
        row: usize,
        /// Number of rows produced by the last successful preparation.
        rows: usize,
    },
    /// A checked row count or cumulative extent overflowed `usize`.
    #[error("row count overflow")]
    RowCountOverflow,
    /// A source advertised an image row that cannot be represented safely.
    #[error("invalid image row")]
    InvalidImage,
}

/// Normalized native mouse event kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMouseEventType {
    /// Button press.
    Press,
    /// Button release.
    Release,
    /// Pointer motion without a button.
    Move,
    /// Pointer motion while a button is held.
    Drag,
    /// A completed click synthesized by the native dispatcher.
    Click,
    /// Logical wheel movement.
    Wheel,
}

impl TuiMouseEventType {
    /// Whether this event kind requests a repaint when the handler does not
    /// provide an explicit `render` decision.
    #[must_use]
    pub const fn default_render(self) -> bool {
        matches!(self, Self::Press | Self::Click | Self::Drag | Self::Wheel)
    }
}

/// Normalized native mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMouseButton {
    /// Primary/left button.
    Left,
    /// Middle button.
    Middle,
    /// Secondary/right button.
    Right,
    /// No button, used for motion and wheel events.
    None,
}

/// Native mouse event with both screen and target-local coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TuiMouseEvent {
    /// Normalized event kind.
    pub kind: TuiMouseEventType,
    /// Normalized button.
    pub button: TuiMouseButton,
    /// Signed coordinate local to the receiving target.
    pub x: i32,
    /// Signed coordinate local to the receiving target.
    pub y: i32,
    /// Zero-based terminal column.
    pub screen_x: u16,
    /// Zero-based terminal row.
    pub screen_y: u16,
    /// Target's logical width.
    pub width: u16,
    /// Target's logical height.
    pub height: u16,
    /// Crossterm modifier flags.
    pub modifiers: KeyModifiers,
    /// Logical wheel movement; negative values scroll toward the document
    /// start and positive values scroll toward its end.
    pub wheel_delta: Option<i32>,
    /// Consecutive click number (normally 1 through 3; zero for non-clicks).
    pub click_count: u8,
}

/// Stable geometry and identity for a mouse dispatch target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseTarget {
    /// Stable focus identity for this live target.
    pub id: FocusId,
    /// Signed screen-space origin of the target.
    pub origin_x: i32,
    /// Signed screen-space origin of the target.
    pub origin_y: i32,
    /// Target's logical width.
    pub width: u16,
    /// Target's logical height.
    pub height: u16,
    /// Target rectangle clipped to the currently visible viewport.
    pub rect: Rect,
}

impl MouseTarget {
    /// Construct a target from its logical geometry and clipped hit rectangle.
    #[must_use]
    pub const fn new(
        id: FocusId,
        origin_x: i32,
        origin_y: i32,
        width: u16,
        height: u16,
        rect: Rect,
    ) -> Self {
        Self {
            id,
            origin_x,
            origin_y,
            width,
            height,
            rect,
        }
    }

    /// True when a screen point lies in the clipped, half-open hit rectangle.
    #[must_use]
    pub const fn contains_screen_point(self, screen_x: u16, screen_y: u16) -> bool {
        screen_x >= self.rect.x
            && screen_x.saturating_sub(self.rect.x) < self.rect.width
            && screen_y >= self.rect.y
            && screen_y.saturating_sub(self.rect.y) < self.rect.height
    }
}

/// Local response from a component mouse handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseResult {
    /// Stop propagation.
    pub handled: bool,
    /// Capture subsequent drag/release events for this target.
    pub capture: bool,
    /// Request keyboard focus for this target.
    pub focus: bool,
    /// Explicit repaint decision; `None` uses the event-kind default.
    pub render: Option<bool>,
}

impl MouseResult {
    /// True when the response stops propagation or requests capture/focus.
    #[must_use]
    pub const fn is_handled(self) -> bool {
        self.handled || self.capture || self.focus
    }

    /// Resolve the repaint default for an event kind.
    #[must_use]
    pub const fn should_render(self, kind: TuiMouseEventType) -> bool {
        match self.render {
            Some(render) => render,
            None => kind.default_render(),
        }
    }
}

/// Result of dispatching a normalized event to a concrete target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseDispatchResult {
    /// Stop propagation. Capture/focus also imply this value.
    pub handled: bool,
    /// Capture subsequent drag/release events for `target`.
    pub capture: bool,
    /// Request keyboard focus.
    pub focus: bool,
    /// Explicit repaint decision; `None` uses the event-kind default.
    pub render: Option<bool>,
    /// Actual target that handled the event.
    pub target: MouseTarget,
    /// Optional focus target, which may differ from `target` for delegation.
    pub focus_target: Option<FocusId>,
}

impl MouseDispatchResult {
    /// True when the dispatch stopped propagation or requests capture/focus.
    #[must_use]
    pub const fn is_handled(self) -> bool {
        self.handled || self.capture || self.focus
    }

    /// Resolve the repaint default for an event kind.
    #[must_use]
    pub const fn should_render(self, kind: TuiMouseEventType) -> bool {
        match self.render {
            Some(render) => render,
            None => kind.default_render(),
        }
    }
}

/// Component-level mouse response, either local or forwarded to a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseResponse {
    /// Handle the event as the current target.
    Local(MouseResult),
    /// Preserve a child dispatch result and its target/focus identity.
    Forwarded(MouseDispatchResult),
}

/// Retarget a screen-coordinate event to a target without narrowing signed
/// local coordinates. This is required for captured drags outside the target.
#[must_use]
pub fn retarget_mouse_event(event: &TuiMouseEvent, target: &MouseTarget) -> TuiMouseEvent {
    let mut retargeted = *event;
    retargeted.x = i32::from(event.screen_x) - target.origin_x;
    retargeted.y = i32::from(event.screen_y) - target.origin_y;
    retargeted.width = target.width;
    retargeted.height = target.height;
    retargeted
}

/// Dispatch an event to a component at a target's geometry.
///
/// A local response receives retargeted coordinates and is attributed to the
/// supplied target. A forwarded response is returned unchanged so a child
/// target remains the capture/focus owner rather than being replaced by a
/// wrapper.
#[must_use]
pub fn dispatch_mouse_event(
    component: &mut dyn Component,
    event: &TuiMouseEvent,
    target: MouseTarget,
) -> Option<MouseDispatchResult> {
    match component.handle_mouse(&retarget_mouse_event(event, &target))? {
        MouseResponse::Local(result) if result.is_handled() => Some(MouseDispatchResult {
            handled: result.is_handled(),
            capture: result.capture,
            focus: result.focus,
            render: result.render,
            target,
            focus_target: result.focus.then_some(target.id),
        }),
        MouseResponse::Forwarded(result) if result.is_handled() => Some(MouseDispatchResult {
            handled: result.is_handled(),
            ..result
        }),
        MouseResponse::Local(_) | MouseResponse::Forwarded(_) => None,
    }
}

/// Result of dispatching a [`UiEvent`] to a component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventResult {
    /// Event was not handled; fall through to the next handler.
    Ignored,
    /// Event was handled; no immediate repaint is required.
    Consumed,
    /// Event was handled and the component needs a repaint on this loop turn.
    Render,
}

impl EventResult {
    /// Combine two results, preferring the stronger outcome.
    ///
    /// Strength order: `Render` > `Consumed` > `Ignored`.
    #[must_use]
    pub const fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Render, _) | (_, Self::Render) => Self::Render,
            (Self::Consumed, _) | (_, Self::Consumed) => Self::Consumed,
            (Self::Ignored, Self::Ignored) => Self::Ignored,
        }
    }

    /// Returns true when the event was not ignored.
    #[must_use]
    pub const fn is_handled(self) -> bool {
        !matches!(self, Self::Ignored)
    }

    /// Returns true when a repaint should run on the current loop turn.
    #[must_use]
    pub const fn needs_render(self) -> bool {
        matches!(self, Self::Render)
    }
}

/// Terminal content that measures, paints, and handles input in its assigned area.
pub trait Component: Send {
    /// Measure the height required to render at `width`.
    fn measure(&mut self, width: u16) -> u16;

    /// Render into `area` of `buf`.
    fn render(&mut self, area: Rect, buf: &mut Buffer);

    /// Handle an input event.
    fn handle_event(&mut self, event: &UiEvent) -> EventResult;

    /// Drop width/theme-sensitive caches after resize or theme change.
    fn invalidate(&mut self);

    /// Prepare retained rows for exactly one width.
    ///
    /// The default is deliberately fail-closed: components that are not
    /// transcript row sources cannot silently become empty fullscreen blocks.
    ///
    /// # Errors
    ///
    /// The default implementation returns [`RowSourceError::Unsupported`].
    fn prepare_rows(&mut self, _width: u16) -> Result<usize, RowSourceError> {
        Err(RowSourceError::Unsupported {
            component: std::any::type_name::<Self>(),
        })
    }

    /// Visit one immutable, prepared row as ordered positioned spans.
    ///
    /// The callback is synchronous; no emitted borrow can outlive this call.
    /// The default is deliberately fail-closed.
    ///
    /// # Errors
    ///
    /// The default implementation returns [`RowSourceError::Unsupported`].
    fn visit_row(
        &self,
        _row: usize,
        _emit: &mut dyn FnMut(DisplayRowSpan<'_>),
    ) -> Result<(), RowSourceError> {
        Err(RowSourceError::Unsupported {
            component: std::any::type_name::<Self>(),
        })
    }

    /// Handle a normalized native mouse event.
    ///
    /// Components that do not participate in native pointer routing decline
    /// by default.
    fn handle_mouse(&mut self, _event: &TuiMouseEvent) -> Option<MouseResponse> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::EventResult;

    #[test]
    fn event_result_merge_prefers_render() {
        assert_eq!(
            EventResult::Ignored.merge(EventResult::Consumed),
            EventResult::Consumed
        );
        assert_eq!(
            EventResult::Consumed.merge(EventResult::Render),
            EventResult::Render
        );
        assert_eq!(
            EventResult::Render.merge(EventResult::Ignored),
            EventResult::Render
        );
    }
}
