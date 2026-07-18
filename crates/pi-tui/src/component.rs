//! Product-agnostic component contract for the native TUI.

use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

/// Closed UI event set consumed by components and the terminal event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    /// Keyboard event from the sole [`crossterm::event::EventStream`] owner.
    Key(KeyEvent),
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

/// Height-for-width terminal component.
///
/// `render` must emit exactly the number of rows previously returned by
/// [`Component::measure`] for the same width.
pub trait Component: Send {
    /// Measure the height required to render at `width`.
    fn measure(&mut self, width: u16) -> u16;

    /// Render into `area` of `buf`.
    fn render(&mut self, area: Rect, buf: &mut Buffer);

    /// Handle an input event.
    fn handle_event(&mut self, event: &UiEvent) -> EventResult;

    /// Drop width/theme-sensitive caches after resize or theme change.
    fn invalidate(&mut self);
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
