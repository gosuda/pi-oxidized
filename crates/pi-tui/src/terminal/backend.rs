//! Backend wrapper that suppresses screen clears and serves caches.

use std::ops::Range;

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// Ratatui backend decorator enforcing the no-clear / cached-query contract.
#[derive(Debug)]
pub struct GuardedBackend<B> {
    inner: B,
    size: Size,
    cursor: Position,
    window_size: WindowSize,
    full_rows: Option<Range<u16>>,
    byte_audit: bool,
    suppressed_clears: u64,
}

impl<B> GuardedBackend<B> {
    /// Wrap `inner` with the given cached size/cursor.
    pub fn new(inner: B, size: Size, cursor: Position) -> Self {
        let window_size = WindowSize {
            columns_rows: size,
            pixels: Size {
                width: size.width.saturating_mul(9),
                height: size.height.saturating_mul(18),
            },
        };
        Self {
            inner,
            size,
            cursor,
            window_size,
            full_rows: None,
            byte_audit: false,
            suppressed_clears: 0,
        }
    }

    /// Borrow the inner backend.
    pub fn inner(&self) -> &B {
        &self.inner
    }

    /// Borrow the inner backend mutably.
    pub fn inner_mut(&mut self) -> &mut B {
        &mut self.inner
    }

    /// Consume the guard and return the inner backend.
    pub fn into_inner(self) -> B {
        self.inner
    }

    /// Update the cached terminal size (from resize events / ioctl).
    pub fn set_size(&mut self, size: Size) {
        self.size = size;
        self.window_size.columns_rows = size;
    }

    /// Update the cached cursor position.
    pub fn set_cursor_cache(&mut self, cursor: Position) {
        self.cursor = cursor;
    }

    /// Update pixel window size when known.
    pub fn set_window_size(&mut self, window_size: WindowSize) {
        self.window_size = window_size;
        self.size = window_size.columns_rows;
    }

    /// Enable or disable full-row redraw mode used by re-anchor transactions.
    pub fn set_full_rows(&mut self, enabled: bool) {
        self.full_rows = enabled.then_some(0..self.size.height);
    }

    /// Set the exact terminal row range for a full-row redraw.
    pub fn set_full_row_region(&mut self, region: Range<u16>) {
        self.full_rows = Some(region);
    }

    /// Whether full-row redraw mode is active.
    #[must_use]
    pub fn full_rows(&self) -> bool {
        self.full_rows.is_some()
    }

    /// Enable outgoing byte audit (tests / `PI_TUI_AUDIT`).
    pub fn set_byte_audit(&mut self, enabled: bool) {
        self.byte_audit = enabled;
    }

    /// Number of screen-wide clear attempts suppressed.
    #[must_use]
    pub fn suppressed_clears(&self) -> u64 {
        self.suppressed_clears
    }

    /// Cached size.
    #[must_use]
    pub fn cached_size(&self) -> Size {
        self.size
    }

    /// Cached cursor.
    #[must_use]
    pub fn cached_cursor(&self) -> Position {
        self.cursor
    }
}

impl<B> Backend for GuardedBackend<B>
where
    B: Backend,
{
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        if let Some(region) = self.full_rows.clone() {
            let mut rows: Vec<(u16, Vec<(u16, &'a Cell)>)> = Vec::new();
            for (x, y, cell) in content {
                match rows.last_mut() {
                    Some((row_y, cells)) if *row_y == y => cells.push((x, cell)),
                    _ => rows.push((y, vec![(x, cell)])),
                }
            }
            let mut rows = rows.into_iter().peekable();
            for y in region {
                self.inner.set_cursor_position(Position { x: 0, y })?;
                self.inner.clear_region(ClearType::CurrentLine)?;
                if rows.peek().is_some_and(|(row_y, _)| *row_y == y)
                    && let Some((_, cells)) = rows.next()
                {
                    self.inner
                        .draw(cells.into_iter().map(|(x, cell)| (x, y, cell)))?;
                }
            }
            Ok(())
        } else {
            self.inner.draw(content)
        }
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let position = position.into();
        self.cursor = position;
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.suppressed_clears = self.suppressed_clears.saturating_add(1);
        Ok(())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        match clear_type {
            ClearType::All | ClearType::BeforeCursor | ClearType::AfterCursor => {
                // Screen-wide / bulk clears are banned. Re-anchor emits row-local EL2.
                self.suppressed_clears = self.suppressed_clears.saturating_add(1);
                Ok(())
            }
            ClearType::CurrentLine | ClearType::UntilNewLine => self.inner.clear_region(clear_type),
        }
    }

    fn size(&self) -> Result<Size, Self::Error> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        Ok(self.window_size)
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }

    // `ratatui` is always built with `scrolling-regions` in this crate.
    fn scroll_region_up(&mut self, region: Range<u16>, line_count: u16) -> Result<(), Self::Error> {
        self.inner.scroll_region_up(region, line_count)
    }

    fn scroll_region_down(
        &mut self,
        region: Range<u16>,
        line_count: u16,
    ) -> Result<(), Self::Error> {
        self.inner.scroll_region_down(region, line_count)
    }
}

/// Emit full-row redraw sequences into `out`: for each row, CUP + EL2.
///
/// Used by re-anchor transactions so row-local erase is immediately followed by
/// the reflowed content in the same stage-3 write.
pub fn encode_full_row_prefix(out: &mut Vec<u8>, row: u16) {
    // CUP is 1-based.
    let y = row.saturating_add(1);
    out.extend_from_slice(format!("\x1b[{y};1H").as_bytes());
    out.extend_from_slice(b"\x1b[2K"); // EL2
}

/// Scan composed bytes for banned clear / unbalanced synchronized-output markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteAuditReport {
    /// Count of `CSI 2J` occurrences.
    pub clear_2j: usize,
    /// Count of `CSI 3J` occurrences.
    pub clear_3j: usize,
    /// Count of `CSI ? 2026 h`.
    pub sync_begin: usize,
    /// Count of `CSI ? 2026 l`.
    pub sync_end: usize,
}

impl ByteAuditReport {
    /// Returns true when no banned clears are present and 2026 markers balance.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.clear_2j == 0 && self.clear_3j == 0 && self.sync_begin == self.sync_end
    }
}

/// Audit a composed byte stream for clear and synchronized-output invariants.
#[must_use]
pub fn audit_bytes(bytes: &[u8]) -> ByteAuditReport {
    ByteAuditReport {
        clear_2j: count_seq(bytes, b"\x1b[2J"),
        clear_3j: count_seq(bytes, b"\x1b[3J"),
        sync_begin: count_seq(bytes, b"\x1b[?2026h"),
        sync_end: count_seq(bytes, b"\x1b[?2026l"),
    }
}

fn count_seq(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    let mut count = 0;
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            count += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    count
}

/// Wrap composed payload in DEC synchronized output when enabled.
#[must_use]
pub fn wrap_synchronized(payload: &[u8], enabled: bool) -> Vec<u8> {
    if !enabled || payload.is_empty() {
        return payload.to_vec();
    }
    let mut out = Vec::with_capacity(payload.len() + 20);
    out.extend_from_slice(b"\x1b[?2026h");
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\x1b[?2026l");
    out
}

#[cfg(test)]
mod tests {
    use super::{GuardedBackend, audit_bytes, encode_full_row_prefix, wrap_synchronized};
    use ratatui::backend::{Backend, ClearType, TestBackend};
    use ratatui::layout::{Position, Size};

    #[test]
    fn suppresses_screen_clears() -> Result<(), std::convert::Infallible> {
        let inner = TestBackend::new(10, 5);
        let mut backend = GuardedBackend::new(inner, Size::new(10, 5), Position::ORIGIN);
        backend.clear()?;
        backend.clear_region(ClearType::All)?;
        backend.clear_region(ClearType::AfterCursor)?;
        assert!(backend.suppressed_clears() >= 3);
        backend.clear_region(ClearType::CurrentLine)?;
        Ok(())
    }

    #[test]
    fn size_and_cursor_come_from_cache() -> Result<(), std::convert::Infallible> {
        let inner = TestBackend::new(40, 20);
        let mut backend = GuardedBackend::new(inner, Size::new(80, 24), Position { x: 3, y: 4 });
        assert_eq!(backend.size()?, Size::new(80, 24));
        assert_eq!(backend.get_cursor_position()?, Position { x: 3, y: 4 });
        backend.set_size(Size::new(100, 30));
        assert_eq!(backend.size()?, Size::new(100, 30));
        Ok(())
    }

    #[test]
    fn audit_detects_clears_and_balanced_sync() {
        let dirty = b"\x1b[2J\x1b[3J\x1b[?2026hpayload\x1b[?2026l";
        let report = audit_bytes(dirty);
        assert_eq!(report.clear_2j, 1);
        assert_eq!(report.clear_3j, 1);
        assert_eq!(report.sync_begin, 1);
        assert_eq!(report.sync_end, 1);
        assert!(!report.is_clean());

        let clean = wrap_synchronized(b"abc", true);
        let report = audit_bytes(&clean);
        assert!(report.is_clean());
        assert_eq!(report.sync_begin, 1);
        assert_eq!(report.sync_end, 1);
    }

    #[test]
    fn full_row_prefix_is_cup_then_el2() {
        let mut out = Vec::new();
        encode_full_row_prefix(&mut out, 0);
        assert_eq!(out, b"\x1b[1;1H\x1b[2K");
        out.clear();
        encode_full_row_prefix(&mut out, 5);
        assert_eq!(out, b"\x1b[6;1H\x1b[2K");
    }

    #[test]
    fn wrap_synchronized_can_be_disabled() {
        let out = wrap_synchronized(b"payload", false);
        assert_eq!(out, b"payload");
        assert_eq!(audit_bytes(&out).sync_begin, 0);
    }
}
