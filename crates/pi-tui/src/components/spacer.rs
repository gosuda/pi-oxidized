//! Empty vertical spacer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::component::{Component, DisplayRowSpan, EventResult, RowSourceError, UiEvent};

/// Renders `lines` empty rows (ignores width for content).
#[derive(Debug, Clone)]
pub struct Spacer {
    lines: u16,
    prepared: Option<(u16, usize)>,
}

impl Spacer {
    /// Create a spacer with the given number of empty lines (default 1).
    #[must_use]
    pub fn new(lines: u16) -> Self {
        Self {
            lines,
            prepared: None,
        }
    }

    /// Update the number of empty lines.
    pub fn set_lines(&mut self, lines: u16) {
        self.lines = lines;
        self.prepared = None;
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
    fn prepare_rows(&mut self, width: u16) -> Result<usize, RowSourceError> {
        let rows = if width == 0 {
            0
        } else {
            usize::from(self.lines)
        };
        self.prepared = Some((width, rows));
        Ok(rows)
    }

    fn visit_row(
        &self,
        row: usize,
        _emit: &mut dyn FnMut(DisplayRowSpan<'_>),
    ) -> Result<(), RowSourceError> {
        let Some((_, rows)) = self.prepared else {
            return Err(RowSourceError::NotPrepared);
        };
        if row >= rows {
            return Err(RowSourceError::RowOutOfBounds { row, rows });
        }
        Ok(())
    }

    fn measure(&mut self, width: u16) -> u16 {
        if self
            .prepared
            .as_ref()
            .is_some_and(|(prepared_width, _)| *prepared_width != width)
        {
            self.prepared = None;
        }
        self.lines
    }

    fn render(&mut self, area: Rect, _buf: &mut Buffer) {
        if self
            .prepared
            .as_ref()
            .is_some_and(|(prepared_width, _)| *prepared_width != area.width)
        {
            self.prepared = None;
        }
        // Empty rows: leave buffer cells as default blanks.
    }

    fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
        EventResult::Ignored
    }

    fn invalidate(&mut self) {
        self.prepared = None;
    }
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
    #[test]
    fn retained_rows_count_blanks_without_spans() -> Result<(), RowSourceError> {
        let mut spacer = Spacer::new(2);
        assert_eq!(spacer.prepare_rows(0)?, 0);
        assert_eq!(
            spacer.visit_row(0, &mut |_| {}),
            Err(RowSourceError::RowOutOfBounds { row: 0, rows: 0 })
        );

        assert_eq!(spacer.prepare_rows(8)?, 2);
        let mut spans = 0;
        spacer.visit_row(1, &mut |_| spans += 1)?;
        assert_eq!(spans, 0);
        Ok(())
    }
}
