//! Select list with two-column layout, half-window scroll, and prefix filter.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::component::{Component, EventResult, UiEvent};
use crate::keybindings::get_keybindings;
use crate::text::{truncate_to_width, visible_width};

use super::util::paint_lines;

const DEFAULT_PRIMARY_COLUMN_WIDTH: usize = 32;
const PRIMARY_COLUMN_GAP: usize = 2;
const MIN_DESCRIPTION_WIDTH: usize = 10;

/// One selectable item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem {
    /// Stable value returned on confirm.
    pub value: String,
    /// Display label (falls back to `value` when empty).
    pub label: String,
    /// Optional description shown in the secondary column.
    pub description: Option<String>,
}

impl SelectItem {
    /// Create an item with value and label.
    #[must_use]
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }

    /// Attach a description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Theme hooks for select list rendering.
#[derive(Clone)]
pub struct SelectListTheme {
    /// Style the selected-prefix glyph region (unused for literal `→ `, retained for parity).
    pub selected_prefix: fn(&str) -> String,
    /// Style the full selected row.
    pub selected_text: fn(&str) -> String,
    /// Style description text on unselected rows.
    pub description: fn(&str) -> String,
    /// Style the `(i/n)` scroll footer.
    pub scroll_info: fn(&str) -> String,
    /// Style the empty/no-match message.
    pub no_match: fn(&str) -> String,
}

impl Default for SelectListTheme {
    fn default() -> Self {
        fn id(s: &str) -> String {
            s.to_owned()
        }
        Self {
            selected_prefix: id,
            selected_text: id,
            description: id,
            scroll_info: id,
            no_match: id,
        }
    }
}

/// Context for a custom primary-column truncator.
pub struct SelectListTruncatePrimaryContext<'a> {
    /// Full display text.
    pub text: &'a str,
    /// Max width for the truncated primary.
    pub max_width: usize,
    /// Allocated primary column width.
    pub column_width: usize,
    /// Item being rendered.
    pub item: &'a SelectItem,
    /// Whether the item is selected.
    pub is_selected: bool,
}

/// Layout knobs for the primary column.
#[derive(Default)]
pub struct SelectListLayoutOptions {
    /// Minimum primary column width.
    pub min_primary_column_width: Option<usize>,
    /// Maximum primary column width.
    pub max_primary_column_width: Option<usize>,
    /// Optional custom primary truncator.
    pub truncate_primary: Option<SelectTruncatePrimary>,
}

type SelectItemCallback = Box<dyn FnMut(&SelectItem) + Send>;
type SelectCancelCallback = Box<dyn FnMut() + Send>;
type SelectTruncatePrimary = Box<dyn Fn(SelectListTruncatePrimaryContext<'_>) -> String + Send>;

/// Selectable list with filter, scroll window, and two-column layout.
pub struct SelectList {
    items: Vec<SelectItem>,
    filtered: Vec<SelectItem>,
    selected_index: usize,
    max_visible: usize,
    theme: SelectListTheme,
    layout: SelectListLayoutOptions,
    /// Called when the user confirms a selection.
    pub on_select: Option<SelectItemCallback>,
    /// Called when the user cancels.
    pub on_cancel: Option<SelectCancelCallback>,
    /// Called when the selection index changes.
    pub on_selection_change: Option<SelectItemCallback>,
    cache: Option<(u16, Vec<String>)>,
}

impl SelectList {
    /// Create a select list.
    #[must_use]
    pub fn new(items: Vec<SelectItem>, max_visible: usize, theme: SelectListTheme) -> Self {
        let filtered = items.clone();
        Self {
            items,
            filtered,
            selected_index: 0,
            max_visible: max_visible.max(1),
            theme,
            layout: SelectListLayoutOptions::default(),
            on_select: None,
            on_cancel: None,
            on_selection_change: None,
            cache: None,
        }
    }

    /// Attach layout options.
    #[must_use]
    pub fn with_layout(mut self, layout: SelectListLayoutOptions) -> Self {
        self.layout = layout;
        self
    }

    /// Replace items and reset filter/selection.
    pub fn set_items(&mut self, items: Vec<SelectItem>) {
        self.items = items;
        self.filtered = self.items.clone();
        self.selected_index = 0;
        self.cache = None;
    }

    /// Filter by `value` prefix (case-insensitive). Resets selection to 0.
    pub fn set_filter(&mut self, filter: &str) {
        let needle = filter.to_ascii_lowercase();
        self.filtered = self
            .items
            .iter()
            .filter(|item| item.value.to_ascii_lowercase().starts_with(&needle))
            .cloned()
            .collect();
        self.selected_index = 0;
        self.cache = None;
    }

    /// Set selected index (clamped).
    pub fn set_selected_index(&mut self, index: usize) {
        if self.filtered.is_empty() {
            self.selected_index = 0;
        } else {
            self.selected_index = index.min(self.filtered.len() - 1);
        }
        self.cache = None;
    }

    /// Currently selected item, if any.
    #[must_use]
    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.filtered.get(self.selected_index)
    }

    fn display_value(item: &SelectItem) -> &str {
        if item.label.is_empty() {
            &item.value
        } else {
            &item.label
        }
    }

    fn primary_column_bounds(&self) -> (usize, usize) {
        let raw_min = self
            .layout
            .min_primary_column_width
            .or(self.layout.max_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let raw_max = self
            .layout
            .max_primary_column_width
            .or(self.layout.min_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let min = raw_min.min(raw_max).max(1);
        let max = raw_min.max(raw_max).max(1);
        (min, max)
    }

    fn primary_column_width(&self) -> usize {
        let (min, max) = self.primary_column_bounds();
        let widest = self.filtered.iter().fold(0usize, |acc, item| {
            acc.max(visible_width(Self::display_value(item)) + PRIMARY_COLUMN_GAP)
        });
        widest.clamp(min, max)
    }

    fn truncate_primary(
        &self,
        item: &SelectItem,
        is_selected: bool,
        max_width: usize,
        column_width: usize,
    ) -> String {
        let display = Self::display_value(item);
        let truncated = if let Some(custom) = &self.layout.truncate_primary {
            custom(SelectListTruncatePrimaryContext {
                text: display,
                max_width,
                column_width,
                item,
                is_selected,
            })
        } else {
            truncate_to_width(display, max_width, "", false)
        };
        truncate_to_width(&truncated, max_width, "", false)
    }

    fn normalize_single_line(text: &str) -> String {
        text.split(['\n', '\r'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn render_item(
        &self,
        item: &SelectItem,
        is_selected: bool,
        width: usize,
        description: Option<&str>,
        primary_column_width: usize,
    ) -> String {
        let prefix = if is_selected { "→ " } else { "  " };
        let prefix_width = visible_width(prefix);

        if let Some(desc) = description
            && width > 40
        {
            let effective = primary_column_width
                .min(width.saturating_sub(prefix_width).saturating_sub(4))
                .max(1);
            let max_primary = effective.saturating_sub(PRIMARY_COLUMN_GAP).max(1);
            let truncated_value = self.truncate_primary(item, is_selected, max_primary, effective);
            let truncated_value_width = visible_width(&truncated_value);
            let spacing_len = effective.saturating_sub(truncated_value_width).max(1);
            let spacing = " ".repeat(spacing_len);
            let description_start = prefix_width + truncated_value_width + spacing_len;
            let remaining = width.saturating_sub(description_start).saturating_sub(2);
            if remaining > MIN_DESCRIPTION_WIDTH {
                let truncated_desc = truncate_to_width(desc, remaining, "", false);
                if is_selected {
                    let full = format!("{prefix}{truncated_value}{spacing}{truncated_desc}");
                    return (self.theme.selected_text)(&full);
                }
                let desc_text = (self.theme.description)(&format!("{spacing}{truncated_desc}"));
                return format!("{prefix}{truncated_value}{desc_text}");
            }
        }

        let max_width = width.saturating_sub(prefix_width).saturating_sub(2);
        let truncated_value = self.truncate_primary(item, is_selected, max_width, max_width);
        if is_selected {
            (self.theme.selected_text)(&format!("{prefix}{truncated_value}"))
        } else {
            format!("{prefix}{truncated_value}")
        }
    }

    fn render_lines(&self, width: u16) -> Vec<String> {
        let width = usize::from(width);
        if self.filtered.is_empty() {
            return vec![(self.theme.no_match)("  No matching commands")];
        }

        let primary_column_width = self.primary_column_width();
        let len = self.filtered.len();
        let max_visible = self.max_visible.min(len);
        let half = max_visible / 2;
        let start = self
            .selected_index
            .saturating_sub(half)
            .min(len.saturating_sub(max_visible));
        let end = (start + max_visible).min(len);

        let mut lines = Vec::new();
        for i in start..end {
            let item = &self.filtered[i];
            let desc = item.description.as_deref().map(Self::normalize_single_line);
            lines.push(self.render_item(
                item,
                i == self.selected_index,
                width,
                desc.as_deref(),
                primary_column_width,
            ));
        }

        if start > 0 || end < len {
            let scroll = format!("  ({}/{})", self.selected_index + 1, len);
            let truncated = truncate_to_width(&scroll, width.saturating_sub(2), "", false);
            lines.push((self.theme.scroll_info)(&truncated));
        }
        lines
    }

    fn notify_selection_change(&mut self) {
        if let Some(item) = self.filtered.get(self.selected_index).cloned()
            && let Some(cb) = self.on_selection_change.as_mut()
        {
            cb(&item);
        }
    }
}

impl Component for SelectList {
    fn measure(&mut self, width: u16) -> u16 {
        let lines = self.render_lines(width);
        self.cache = Some((width, lines.clone()));
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let lines = match &self.cache {
            Some((w, lines)) if *w == area.width => lines.clone(),
            _ => self.render_lines(area.width),
        };
        paint_lines(area, buf, &lines);
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        let UiEvent::Key(key) = event else {
            return EventResult::Ignored;
        };
        let kb = get_keybindings();
        if kb.matches(key, "tui.select.up") {
            if self.filtered.is_empty() {
                return EventResult::Consumed;
            }
            self.selected_index = if self.selected_index == 0 {
                self.filtered.len() - 1
            } else {
                self.selected_index - 1
            };
            self.cache = None;
            self.notify_selection_change();
            return EventResult::Render;
        }
        if kb.matches(key, "tui.select.down") {
            if self.filtered.is_empty() {
                return EventResult::Consumed;
            }
            self.selected_index = if self.selected_index + 1 >= self.filtered.len() {
                0
            } else {
                self.selected_index + 1
            };
            self.cache = None;
            self.notify_selection_change();
            return EventResult::Render;
        }
        if kb.matches(key, "tui.select.confirm") {
            if let Some(item) = self.filtered.get(self.selected_index).cloned()
                && let Some(cb) = self.on_select.as_mut()
            {
                cb(&item);
            }
            return EventResult::Consumed;
        }
        if kb.matches(key, "tui.select.cancel") {
            if let Some(cb) = self.on_cancel.as_mut() {
                cb();
            }
            return EventResult::Consumed;
        }
        EventResult::Ignored
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::util::{render_snapshot, strip_ansi};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn items() -> Vec<SelectItem> {
        (0..10)
            .map(|i| {
                SelectItem::new(format!("cmd{i}"), format!("Command {i}"))
                    .with_description(format!("desc {i}"))
            })
            .collect()
    }

    #[test]
    fn empty_filter_message() {
        let mut list = SelectList::new(items(), 5, SelectListTheme::default());
        list.set_filter("zzz");
        let snap = render_snapshot(&mut list, 60);
        assert!(strip_ansi(&snap[0]).contains("No matching"));
    }

    #[test]
    fn scroll_footer_when_overflow() {
        let mut list = SelectList::new(items(), 3, SelectListTheme::default());
        list.set_selected_index(5);
        let snap = render_snapshot(&mut list, 60);
        let joined = snap
            .iter()
            .map(|s| strip_ansi(s))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("(6/10)") || joined.contains("/10)"));
    }

    #[test]
    fn keyboard_wrap() {
        let mut list = SelectList::new(items(), 5, SelectListTheme::default());
        let up = UiEvent::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(list.handle_event(&up), EventResult::Render);
        assert_eq!(list.selected_index, 9);
    }

    #[test]
    fn two_col_at_wide_width() {
        let mut list = SelectList::new(items(), 5, SelectListTheme::default());
        for width in [24_u16, 60, 80, 120] {
            let snap = render_snapshot(&mut list, width);
            assert!(!snap.is_empty());
        }
    }

    #[test]
    fn prefix_filter() {
        let mut list = SelectList::new(items(), 5, SelectListTheme::default());
        list.set_filter("cmd1");
        assert!(list.filtered.iter().all(|i| i.value.starts_with("cmd1")));
    }
}
