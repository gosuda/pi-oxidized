//! Overlay stack: z-order, capture, focus transfer, layout compositing.
//!
//! Ports `showOverlay` / `OverlayHandle` / stack focus restore interplay from
//! `.references/pi/packages/tui/src/tui.ts`. Layout math lives in
//! [`crate::layout`]; CJK boundary overwrite uses Ratatui `Buffer` wide-cell
//! semantics (and the string-level [`crate::text::composite_line_at`] helper
//! for regression tests).

use std::sync::{Arc, Mutex};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Span;

use crate::focus::{FocusId, FocusManager, OverlayFocusRestorePolicy};
use crate::layout::{OverlaySpec, ResolvedOverlayLayout, SizeValue, resolve_overlay_layout};

/// Native overlay options: serializable [`OverlaySpec`] plus host-side
/// visibility callback (never serialized — Phase 6 host owns the callback).
#[derive(Default)]
pub struct OverlayOptions {
    /// Serializable layout / capture flags.
    pub spec: OverlaySpec,
    /// Optional visibility predicate `(term_width, term_height) -> bool`.
    pub visible: Option<Box<dyn Fn(u16, u16) -> bool + Send + Sync>>,
}

impl std::fmt::Debug for OverlayOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayOptions")
            .field("spec", &self.spec)
            .field("visible", &self.visible.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

impl OverlayOptions {
    /// Options from a pure layout spec (always visible).
    #[must_use]
    pub fn from_spec(spec: OverlaySpec) -> Self {
        Self {
            spec,
            visible: None,
        }
    }

    /// Convenience: non-capturing overlay.
    #[must_use]
    pub fn non_capturing(mut self) -> Self {
        self.spec.non_capturing = true;
        self
    }

    /// Attach a visibility callback.
    #[must_use]
    pub fn with_visible(mut self, f: impl Fn(u16, u16) -> bool + Send + Sync + 'static) -> Self {
        self.visible = Some(Box::new(f));
        self
    }
}

/// Options for [`OverlayHandle::unfocus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayUnfocusOptions {
    /// Explicit target to focus after releasing this overlay.
    pub target: Option<FocusId>,
}

/// Shared mutable entry inside the overlay stack.
#[derive(Debug)]
struct OverlayEntry {
    id: FocusId,
    options: OverlayOptions,
    pre_focus: Option<FocusId>,
    hidden: bool,
    focus_order: u64,
    /// Measured content height last resolved (for layout tests / paint).
    content_height: u16,
}

/// Handle returned by [`OverlayStack::show`] for controlling one overlay.
#[derive(Clone)]
pub struct OverlayHandle {
    id: FocusId,
    stack: Arc<Mutex<OverlayStackInner>>,
}

impl std::fmt::Debug for OverlayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayHandle")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl OverlayHandle {
    /// Overlay focus id.
    #[must_use]
    pub fn id(&self) -> FocusId {
        self.id
    }

    /// Permanently remove the overlay.
    pub fn hide(&self) {
        let mut stack = self
            .stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stack.hide_id(self.id);
    }

    /// Temporarily hide or show the overlay.
    pub fn set_hidden(&self, hidden: bool) {
        let mut stack = self
            .stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stack.set_hidden(self.id, hidden);
    }

    /// Whether the overlay is temporarily hidden.
    #[must_use]
    pub fn is_hidden(&self) -> bool {
        let stack = self
            .stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stack
            .entries
            .iter()
            .find(|e| e.id == self.id)
            .is_none_or(|e| e.hidden)
    }

    /// Focus this overlay and bring it to the visual front.
    pub fn focus(&self) {
        let mut stack = self
            .stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stack.focus_id(self.id);
    }

    /// Release focus to the next capturing overlay / pre-focus / explicit target.
    pub fn unfocus(&self, options: Option<OverlayUnfocusOptions>) {
        let mut stack = self
            .stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stack.unfocus_id(self.id, options);
    }

    /// Whether this overlay currently has focus.
    #[must_use]
    pub fn is_focused(&self) -> bool {
        let stack = self
            .stack
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stack.focus.focused() == Some(self.id)
    }
}

#[derive(Debug, Default)]
struct OverlayStackInner {
    entries: Vec<OverlayEntry>,
    focus_order_counter: u64,
    focus: FocusManager,
    term_width: u16,
    term_height: u16,
}

impl OverlayStackInner {
    fn is_visible(&self, entry: &OverlayEntry) -> bool {
        if entry.hidden {
            return false;
        }
        if let Some(ref visible) = entry.options.visible {
            return visible(self.term_width, self.term_height);
        }
        true
    }

    fn topmost_visible_capturing(&self) -> Option<FocusId> {
        let mut top: Option<&OverlayEntry> = None;
        for entry in &self.entries {
            if entry.options.spec.non_capturing || !self.is_visible(entry) {
                continue;
            }
            if top.is_none_or(|t| entry.focus_order > t.focus_order) {
                top = Some(entry);
            }
        }
        top.map(|e| e.id)
    }

    fn show(&mut self, id: FocusId, options: OverlayOptions) -> FocusId {
        self.focus_order_counter = self.focus_order_counter.saturating_add(1);
        let pre_focus = self.focus.focused();
        let entry = OverlayEntry {
            id,
            options,
            pre_focus,
            hidden: false,
            focus_order: self.focus_order_counter,
            content_height: 0,
        };
        self.focus.register_overlay_pre_focus(id, pre_focus);
        let capture = !entry.options.spec.non_capturing && self.is_visible(&entry);
        self.entries.push(entry);
        if capture {
            self.focus.set_focus(Some(id));
        }
        id
    }

    fn hide_id(&mut self, id: FocusId) {
        let Some(index) = self.entries.iter().position(|e| e.id == id) else {
            return;
        };
        let entry = self.entries.remove(index);
        for remaining in &mut self.entries {
            if remaining.pre_focus == Some(id) {
                remaining.pre_focus = entry.pre_focus;
            }
        }
        self.focus.clear_overlay_focus_restore_for(id);
        self.focus.retarget_overlay_pre_focus(id, entry.pre_focus);
        self.focus.unregister_overlay(id);
        if self.focus.focused() == Some(id) {
            let top = self.topmost_visible_capturing();
            self.focus.set_focus(top.or(entry.pre_focus));
        }
    }

    fn set_hidden(&mut self, id: FocusId, hidden: bool) {
        let Some(index) = self.entries.iter().position(|e| e.id == id) else {
            return;
        };
        if self.entries[index].hidden == hidden {
            return;
        }
        self.entries[index].hidden = hidden;
        if hidden {
            self.focus.clear_overlay_focus_restore_for(id);
            if self.focus.focused() == Some(id) {
                let pre = self.entries[index].pre_focus;
                let top = self.topmost_visible_capturing();
                self.focus.set_focus(top.or(pre));
            }
        } else {
            let non_capturing = self.entries[index].options.spec.non_capturing;
            let visible = self.is_visible(&self.entries[index]);
            if !non_capturing && visible {
                self.focus_order_counter = self.focus_order_counter.saturating_add(1);
                self.entries[index].focus_order = self.focus_order_counter;
                self.focus.set_focus(Some(id));
            }
        }
    }

    fn focus_id(&mut self, id: FocusId) {
        let Some(index) = self.entries.iter().position(|e| e.id == id) else {
            return;
        };
        if !self.is_visible(&self.entries[index]) {
            return;
        }
        self.focus_order_counter = self.focus_order_counter.saturating_add(1);
        self.entries[index].focus_order = self.focus_order_counter;
        self.focus.set_focus(Some(id));
    }

    fn unfocus_id(&mut self, id: FocusId, options: Option<OverlayUnfocusOptions>) {
        let is_focused = self.focus.focused() == Some(id);
        let restore = self.focus.overlay_focus_restore();
        let has_pending = matches!(
            restore,
            crate::focus::OverlayFocusRestoreState::Eligible { overlay }
            | crate::focus::OverlayFocusRestoreState::Blocked { overlay, .. }
                if overlay == id
        );
        if !is_focused && !has_pending {
            return;
        }

        if let crate::focus::OverlayFocusRestoreState::Blocked {
            overlay,
            blocked_by,
            ..
        } = restore
            && overlay == id
            && self.focus.focused() == Some(blocked_by)
        {
            if let Some(options) = options {
                let _updated = self.focus.set_blocked_resume_target(id, options.target);
            } else {
                self.focus.clear_overlay_focus_restore();
            }
            return;
        }

        self.focus.clear_overlay_focus_restore_for(id);
        if is_focused || options.is_some() {
            let top = self.topmost_visible_capturing().filter(|t| *t != id);
            let pre = self
                .entries
                .iter()
                .find(|e| e.id == id)
                .and_then(|e| e.pre_focus);
            let fallback = top.or(pre);
            let target = options.map_or(fallback, |o| o.target);
            self.focus.set_focus(target);
        }
    }

    fn hide_topmost(&mut self) {
        let Some(last) = self.entries.last().map(|e| e.id) else {
            return;
        };
        self.hide_id(last);
    }

    fn has_visible(&self) -> bool {
        self.entries.iter().any(|e| self.is_visible(e))
    }

    fn resolve_entry_layout(
        &self,
        entry: &OverlayEntry,
        overlay_height: u16,
    ) -> ResolvedOverlayLayout {
        resolve_overlay_layout(
            &entry.options.spec,
            overlay_height,
            self.term_width,
            self.term_height,
        )
    }

    /// Visible overlays sorted by `focus_order` ascending (topmost last).
    fn visible_sorted(&self) -> Vec<&OverlayEntry> {
        let mut v: Vec<&OverlayEntry> =
            self.entries.iter().filter(|e| self.is_visible(e)).collect();
        v.sort_by_key(|e| e.focus_order);
        v
    }
}

/// Public overlay stack coordinating z-order and focus with a [`FocusManager`].
#[derive(Clone, Default)]
pub struct OverlayStack {
    inner: Arc<Mutex<OverlayStackInner>>,
}

impl std::fmt::Debug for OverlayStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f.debug_struct("OverlayStack")
            .field("count", &inner.entries.len())
            .field("focused", &inner.focus.focused())
            .finish()
    }
}

impl OverlayStack {
    /// Create an empty stack.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Update terminal dimensions used by visibility and layout.
    pub fn set_terminal_size(&self, width: u16, height: u16) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.term_width = width;
        inner.term_height = height;
    }

    /// Show an overlay identified by `id` with `options`.
    #[must_use]
    pub fn show(&self, id: FocusId, options: OverlayOptions) -> OverlayHandle {
        {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner.show(id, options);
        }
        OverlayHandle {
            id,
            stack: Arc::clone(&self.inner),
        }
    }

    /// Hide the topmost overlay.
    pub fn hide_topmost(&self) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.hide_topmost();
    }

    /// Whether any overlay is currently visible.
    #[must_use]
    pub fn has_overlay(&self) -> bool {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.has_visible()
    }

    /// Current focused id according to the stack's focus manager.
    #[must_use]
    pub fn focused(&self) -> Option<FocusId> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.focus.focused()
    }

    /// Borrow-free snapshot of the focus manager focused id + restore state.
    #[must_use]
    pub fn focus_manager_snapshot(
        &self,
    ) -> (Option<FocusId>, crate::focus::OverlayFocusRestoreState) {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (inner.focus.focused(), inner.focus.overlay_focus_restore())
    }

    /// Set base (non-overlay) focus through the stack's manager.
    pub fn set_base_focus(&self, id: Option<FocusId>) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.focus.set_focus(id);
    }

    /// Set base focus preserving overlay restore state.
    pub fn set_base_focus_preserve(&self, id: Option<FocusId>) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner
            .focus
            .set_focus_internal(id, OverlayFocusRestorePolicy::Preserve);
    }

    /// Resolve layout for a visible overlay given its measured content height.
    #[must_use]
    pub fn resolve_layout(
        &self,
        id: FocusId,
        content_height: u16,
    ) -> Option<ResolvedOverlayLayout> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = inner.entries.iter().position(|e| e.id == id)?;
        inner.entries[index].content_height = content_height;
        let entry = &inner.entries[index];
        if !inner.is_visible(entry) {
            return None;
        }
        Some(inner.resolve_entry_layout(entry, content_height))
    }

    /// Visible overlay ids in paint order (lowest `focus_order` first).
    #[must_use]
    pub fn paint_order(&self) -> Vec<FocusId> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.visible_sorted().into_iter().map(|e| e.id).collect()
    }

    /// Composite overlay text into a Ratatui buffer region using wide-cell overwrite.
    ///
    /// `area` is the full terminal/frame area. Overlay content is clipped to
    /// `layout` and written with [`write_overlay_cells`], which blanks both
    /// halves of a straddled wide grapheme.
    pub fn composite_into_buffer(
        buf: &mut Buffer,
        area: Rect,
        layout: ResolvedOverlayLayout,
        lines: &[String],
    ) {
        let height = lines.len().min(usize::from(
            layout
                .max_height
                .unwrap_or(u16::try_from(lines.len()).unwrap_or(u16::MAX)),
        ));
        let height = height.min(usize::from(area.height.saturating_sub(layout.row)));
        for (i, line) in lines.iter().take(height).enumerate() {
            let row = layout.row.saturating_add(u16::try_from(i).unwrap_or(0));
            if row >= area.y.saturating_add(area.height) {
                break;
            }
            let overlay_area = Rect {
                x: area.x.saturating_add(layout.col),
                y: area.y.saturating_add(row),
                width: layout.width.min(area.width.saturating_sub(layout.col)),
                height: 1,
            };
            write_overlay_cells(buf, overlay_area, line);
        }
    }
}

/// Write `text` into `area` (single row), overwriting wide-cell pairs cleanly.
///
/// When a write starts on the trailing half of a wide character, the leading
/// half is blanked. When a wide character would overflow the right edge of
/// `area`, it is replaced with spaces so column count is conserved.
pub fn write_overlay_cells(buf: &mut Buffer, area: Rect, text: &str) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Overlays composite over other components' rows: claim the rows as
    // foreign so base paint lines cannot skip-repaint over them and stale
    // cells survive an overlay close (PERF-T11 Design B).
    crate::frame::claim_foreign_span(area);


    // If the first cell of the overlay region is the trailing half of a wide
    // grapheme, blank the leading cell so the pair is not left half-stale.
    if area.x > 0 {
        let origin = buf
            .cell((area.x, area.y))
            .map(|cell| cell.symbol().to_owned());
        if origin.as_deref() == Some("") {
            // Trailing half of a wide char: clear the previous cell too.
            if let Some(prev) = buf.cell_mut((area.x - 1, area.y)) {
                prev.set_symbol(" ");
            }
            if let Some(cell) = buf.cell_mut((area.x, area.y)) {
                cell.set_symbol(" ");
            }
        }
    }

    let mut col = 0u16;
    for grapheme in unicode_segmentation::UnicodeSegmentation::graphemes(text, true) {
        let w = u16::try_from(unicode_width::UnicodeWidthStr::width(grapheme)).unwrap_or(u16::MAX);
        if w == 0 {
            continue;
        }
        if col >= area.width {
            break;
        }
        if col.saturating_add(w) > area.width {
            // Wide char would overflow: pad with spaces for remaining columns.
            while col < area.width {
                let x = area.x.saturating_add(col);
                if let Some(cell) = buf.cell_mut((x, area.y)) {
                    cell.set_symbol(" ");
                }
                col = col.saturating_add(1);
            }
            break;
        }
        let x = area.x.saturating_add(col);
        if let Some(cell) = buf.cell_mut((x, area.y)) {
            cell.set_symbol(grapheme);
        }
        // Clear trailing half for wide glyphs.
        if w == 2
            && let Some(cell) = buf.cell_mut((x + 1, area.y))
        {
            cell.set_symbol("");
        }
        col = col.saturating_add(w);
    }

    // Fill remaining overlay width with spaces so base content under the
    // declared width is fully replaced (matches declared-width compositing).
    while col < area.width {
        let x = area.x.saturating_add(col);
        if let Some(cell) = buf.cell_mut((x, area.y)) {
            cell.set_symbol(" ");
        }
        col = col.saturating_add(1);
    }
}

/// String-level CJK-boundary composite (delegates to text module).
///
/// Exposed for overlay regression tests that port
/// `regression-overlay-cjk-boundary.test.ts`.
#[must_use]
pub fn composite_overlay_line(
    base_line: &str,
    overlay_line: &str,
    start_col: usize,
    overlay_width: usize,
    total_width: usize,
) -> String {
    crate::text::composite_line_at(
        base_line,
        overlay_line,
        start_col,
        overlay_width,
        total_width,
    )
}

/// Helper to build a default-centered overlay of a given cell width.
#[must_use]
pub fn centered_width(width: u16) -> OverlayOptions {
    OverlayOptions::from_spec(OverlaySpec {
        width: Some(SizeValue::cells(width)),
        ..OverlaySpec::default()
    })
}

/// Measure helper: return spans' total width for layout callers.
#[must_use]
pub fn spans_width(spans: &[Span<'_>]) -> u16 {
    spans.iter().fold(0u16, |total, span| {
        let width = u16::try_from(unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
            .unwrap_or(u16::MAX);
        total.saturating_add(width)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::visible_width;

    #[test]
    fn non_capturing_preserves_focus() {
        let stack = OverlayStack::new();
        stack.set_terminal_size(80, 24);
        let editor = FocusId::new();
        stack.set_base_focus(Some(editor));
        let overlay = FocusId::new();
        let handle = stack.show(
            overlay,
            OverlayOptions::from_spec(OverlaySpec::default()).non_capturing(),
        );
        assert_eq!(stack.focused(), Some(editor));
        assert!(!handle.is_focused());
    }

    #[test]
    fn capturing_overlay_takes_focus() {
        let stack = OverlayStack::new();
        stack.set_terminal_size(80, 24);
        let editor = FocusId::new();
        stack.set_base_focus(Some(editor));
        let overlay = FocusId::new();
        let handle = stack.show(overlay, OverlayOptions::default());
        assert_eq!(stack.focused(), Some(overlay));
        assert!(handle.is_focused());
    }

    #[test]
    fn focus_transfer_and_unfocus_restore() {
        let stack = OverlayStack::new();
        stack.set_terminal_size(80, 24);
        let editor = FocusId::new();
        stack.set_base_focus(Some(editor));
        let overlay = FocusId::new();
        let handle = stack.show(
            overlay,
            OverlayOptions::from_spec(OverlaySpec::default()).non_capturing(),
        );
        handle.focus();
        assert!(handle.is_focused());
        handle.unfocus(None);
        assert_eq!(stack.focused(), Some(editor));
    }

    #[test]
    fn hide_restores_pre_focus() {
        let stack = OverlayStack::new();
        stack.set_terminal_size(80, 24);
        let editor = FocusId::new();
        stack.set_base_focus(Some(editor));
        let overlay = FocusId::new();
        let handle = stack.show(overlay, OverlayOptions::default());
        assert_eq!(stack.focused(), Some(overlay));
        handle.hide();
        assert_eq!(stack.focused(), Some(editor));
        assert!(!stack.has_overlay());
    }

    #[test]
    fn nested_overlay_hide_restores_capturing_parent_then_base() {
        let stack = OverlayStack::new();
        stack.set_terminal_size(80, 24);
        let editor = FocusId::new();
        let parent = FocusId::new();
        let child = FocusId::new();
        stack.set_base_focus(Some(editor));
        let parent_handle = stack.show(parent, OverlayOptions::default());
        let child_handle = stack.show(child, OverlayOptions::default());

        assert_eq!(stack.focused(), Some(child));
        child_handle.hide();
        assert_eq!(stack.focused(), Some(parent));
        parent_handle.hide();
        assert_eq!(stack.focused(), Some(editor));
    }

    #[test]
    fn blocked_overlay_unfocus_restores_explicit_target() {
        let stack = OverlayStack::new();
        stack.set_terminal_size(80, 24);
        let editor = FocusId::new();
        let overlay = FocusId::new();
        let blocker = FocusId::new();
        let explicit_target = FocusId::new();
        stack.set_base_focus(Some(editor));
        let handle = stack.show(overlay, OverlayOptions::default());

        stack.set_base_focus(Some(blocker));
        handle.unfocus(Some(OverlayUnfocusOptions {
            target: Some(explicit_target),
        }));
        assert_eq!(stack.focused(), Some(blocker));

        stack.set_base_focus(None);
        assert_eq!(stack.focused(), Some(explicit_target));
    }

    #[test]
    fn z_order_follows_focus_order() {
        let stack = OverlayStack::new();
        stack.set_terminal_size(80, 24);
        let a = FocusId::new();
        let b = FocusId::new();
        let ha = stack.show(a, OverlayOptions::default().non_capturing());
        let hb = stack.show(b, OverlayOptions::default().non_capturing());
        let order = stack.paint_order();
        assert_eq!(order, vec![a, b]);
        ha.focus();
        let order = stack.paint_order();
        assert_eq!(order.last().copied(), Some(a));
        hb.focus();
        let order = stack.paint_order();
        assert_eq!(order.last().copied(), Some(b));
    }

    #[test]
    fn set_hidden_moves_focus_and_restores() {
        let stack = OverlayStack::new();
        stack.set_terminal_size(80, 24);
        let editor = FocusId::new();
        stack.set_base_focus(Some(editor));
        let overlay = FocusId::new();
        let handle = stack.show(overlay, OverlayOptions::default());
        handle.set_hidden(true);
        assert!(handle.is_hidden());
        assert_eq!(stack.focused(), Some(editor));
        handle.set_hidden(false);
        assert!(handle.is_focused());
    }

    #[test]
    fn cjk_boundary_string_composite_inside_wide_grapheme() {
        // "abcd让EFGH" — 让 is a wide char spanning cols 4-5.
        // Overlay starting at col 5 (inside 让) must drop 让 and keep width.
        let out = composite_overlay_line("abcd让EFGH", "│XX│", 5, 4, 20);
        assert!(!out.contains('让'));
        assert_eq!(visible_width(&out), 20);
        let overlay = crate::text::slice_by_column(&out, 5, 4, true);
        assert_eq!(visible_width(&overlay), 4);
        assert!(overlay.contains("│XX│"));
    }

    #[test]
    fn cjk_boundary_string_composite_at_wide_boundary() {
        let out = composite_overlay_line("abcd让EFGH", "│XX│", 4, 4, 20);
        assert!(!out.contains('让'));
        assert_eq!(visible_width(&out), 20);
        let overlay = crate::text::slice_by_column(&out, 4, 4, true);
        assert_eq!(visible_width(&overlay), 4);
        assert!(overlay.contains("│XX│"));
    }

    #[test]
    fn buffer_wide_char_overwrite_blanks_pair() {
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        // Base: "ab让cd" starting at col 0 → 让 at cols 2-3
        write_overlay_cells(&mut buf, area, "ab让cd");
        assert_eq!(
            buf.cell((2, 0)).map(ratatui::buffer::Cell::symbol),
            Some("让")
        );
        assert_eq!(
            buf.cell((3, 0)).map(ratatui::buffer::Cell::symbol),
            Some("")
        );

        // Overlay starts at col 3 (trailing half of 让) with "XY"
        let overlay_area = Rect::new(3, 0, 2, 1);
        write_overlay_cells(&mut buf, overlay_area, "XY");
        // Leading half of 让 must be blanked
        let lead = buf.cell((2, 0)).map(|c| c.symbol().to_owned());
        assert_eq!(lead.as_deref(), Some(" "));
        assert_eq!(
            buf.cell((3, 0)).map(ratatui::buffer::Cell::symbol),
            Some("X")
        );
        assert_eq!(
            buf.cell((4, 0)).map(ratatui::buffer::Cell::symbol),
            Some("Y")
        );
    }

    #[test]
    fn buffer_wide_char_overflow_pads_spaces() {
        let area = Rect::new(0, 0, 3, 1);
        let mut buf = Buffer::empty(area);
        // Two CJK chars need 4 cols; area is 3 → pad, no overflow symbol.
        write_overlay_cells(&mut buf, area, "中文");
        // First CJK fits at 0-1, second would overflow → spaces
        assert_eq!(
            buf.cell((0, 0)).map(ratatui::buffer::Cell::symbol),
            Some("中")
        );
        let c2 = buf.cell((2, 0)).map(|c| c.symbol().to_owned());
        assert_eq!(c2.as_deref(), Some(" "));
    }

    #[test]
    fn resolve_layout_uses_stack_terminal_size() {
        let stack = OverlayStack::new();
        stack.set_terminal_size(80, 24);
        let id = FocusId::new();
        let _ = stack.show(
            id,
            OverlayOptions::from_spec(OverlaySpec {
                width: Some(SizeValue::cells(20)),
                ..OverlaySpec::default()
            }),
        );
        assert_eq!(
            stack.resolve_layout(id, 5),
            Some(ResolvedOverlayLayout {
                width: 20,
                row: 9,
                col: 30,
                max_height: None,
            })
        );
    }

    #[test]
    fn visibility_callback_hides_overlay() {
        let stack = OverlayStack::new();
        stack.set_terminal_size(80, 24);
        let id = FocusId::new();
        let _ = stack.show(
            id,
            OverlayOptions::from_spec(OverlaySpec::default()).with_visible(|w, _| w >= 100),
        );
        assert!(!stack.has_overlay());
        stack.set_terminal_size(120, 24);
        assert!(stack.has_overlay());
    }

    #[test]
    fn removing_middle_overlay_retargets_nested_pre_focus() {
        let stack = OverlayStack::new();
        stack.set_terminal_size(80, 24);
        let editor = FocusId::new();
        let parent = FocusId::new();
        let child = FocusId::new();
        stack.set_base_focus(Some(editor));
        let parent_handle = stack.show(parent, OverlayOptions::default());
        let child_handle = stack.show(child, OverlayOptions::default());

        assert_eq!(stack.focused(), Some(child));
        parent_handle.hide();
        assert_eq!(stack.focused(), Some(child));
        child_handle.hide();
        assert_eq!(stack.focused(), Some(editor));
        assert!(!stack.has_overlay());
    }
}
