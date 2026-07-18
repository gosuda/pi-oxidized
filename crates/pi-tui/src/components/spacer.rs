//! Empty vertical spacer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::component::{Component, EventResult, UiEvent};

/// Renders `lines` empty rows (ignores width for content).
#[derive(Debug, Clone)]
pub struct Spacer {
    lines: u16,
}

impl Spacer {
    /// Create a spacer with the given number of empty lines (default 1).
    #[must_use]
    pub fn new(lines: u16) -> Self {
        Self { lines }
    }

    /// Update the number of empty lines.
    pub fn set_lines(&mut self, lines: u16) {
        self.lines = lines;
    }

    /// Current line count.
    #[must_use]
    pub fn lines(&self) -> u16 {
        self.lines
    }
}

impl Default for Spacer {
    fn default() -> Self {
        Self::new(1)
    }
}

impl Component for Spacer {
    fn measure(&mut self, _width: u16) -> u16 {
        self.lines
    }

    fn render(&mut self, _area: Rect, _buf: &mut Buffer) {
        // Empty rows: leave buffer cells as default blanks.
    }

    fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
        EventResult::Ignored
    }

    fn invalidate(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::util::render_snapshot;

    #[test]
    fn measures_lines() {
        let mut s = Spacer::new(3);
        assert_eq!(s.measure(80), 3);
        let snap = render_snapshot(&mut s, 24);
        assert_eq!(snap.len(), 3);
    }
}
