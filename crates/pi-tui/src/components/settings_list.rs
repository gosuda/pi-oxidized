//! Settings list with value cycling, submenu delegation, and fuzzy search.

use std::sync::{Arc, Mutex};

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;

use crate::component::{Component, EventResult, UiEvent};
use crate::fuzzy::fuzzy_filter;
use crate::keybindings::get_keybindings;
use crate::text::{truncate_to_width, visible_width, wrap_text_with_ansi};

use super::input::Input;
use super::util::paint_lines;

/// Completion payload from a settings submenu.
#[derive(Debug, Clone)]
enum SubmenuDone {
    /// User cancelled without a value.
    Cancelled,
    /// User selected a value.
    Selected(String),
}

/// Factory that builds a submenu component.
///
/// The `done` callback must be invoked with `Some(value)` on selection or
/// `None` on cancel; [`SettingsList`] closes the submenu when the holder is set.
pub type SubmenuFactory =
    Arc<dyn Fn(&str, Box<dyn FnMut(Option<String>) + Send>) -> Box<dyn Component> + Send + Sync>;

/// One settings row.
#[derive(Clone)]
pub struct SettingItem {
    /// Unique identifier.
    pub id: String,
    /// Display label.
    pub label: String,
    /// Optional description when selected.
    pub description: Option<String>,
    /// Current value text.
    pub current_value: String,
    /// Cycle values on confirm when set.
    pub values: Option<Vec<String>>,
    /// Optional submenu factory.
    pub submenu: Option<SubmenuFactory>,
}

impl std::fmt::Debug for SettingItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingItem")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("current_value", &self.current_value)
            .field("values", &self.values)
            .field("has_submenu", &self.submenu.is_some())
            .finish_non_exhaustive()
    }
}

/// Theme hooks for settings list.
#[derive(Clone)]
pub struct SettingsListTheme {
    /// Label style; second arg is selected.
    pub label: fn(&str, bool) -> String,
    /// Value style; second arg is selected.
    pub value: fn(&str, bool) -> String,
    /// Description style.
    pub description: fn(&str) -> String,
    /// Cursor glyph for selected row (e.g. `"> "`).
    pub cursor: String,
    /// Hint / footer style.
    pub hint: fn(&str) -> String,
}

impl Default for SettingsListTheme {
    fn default() -> Self {
        fn lab(s: &str, _selected: bool) -> String {
            s.to_owned()
        }
        fn one(s: &str) -> String {
            s.to_owned()
        }
        Self {
            label: lab,
            value: lab,
            description: one,
            cursor: "> ".to_owned(),
            hint: one,
        }
    }
}

/// Options for settings list.
#[derive(Debug, Clone, Default)]
pub struct SettingsListOptions {
    /// Enable fuzzy search input.
    pub enable_search: bool,
}

type SettingsChangeCallback = Box<dyn FnMut(&str, &str) + Send>;
type SettingsCancelCallback = Box<dyn FnMut() + Send>;

/// Settings list with optional search and submenu focus delegation.
pub struct SettingsList {
    items: Vec<SettingItem>,
    filtered: Vec<SettingItem>,
    theme: SettingsListTheme,
    selected_index: usize,
    max_visible: usize,
    on_change: SettingsChangeCallback,
    on_cancel: SettingsCancelCallback,
    search_input: Option<Input>,
    search_enabled: bool,
    empty_text: Option<String>,
    no_match_text: Option<String>,
    submenu: Option<Box<dyn Component>>,
    submenu_item_index: Option<usize>,
    pending_submenu_value: Option<Arc<Mutex<Option<SubmenuDone>>>>,
    pending_submenu_id: Option<String>,
    cache: Option<(u16, Vec<String>)>,
}

impl SettingsList {
    /// Create a settings list.
    pub fn new(
        items: Vec<SettingItem>,
        max_visible: usize,
        theme: SettingsListTheme,
        on_change: impl FnMut(&str, &str) + Send + 'static,
        on_cancel: impl FnMut() + Send + 'static,
        options: &SettingsListOptions,
    ) -> Self {
        let search_enabled = options.enable_search;
        let filtered = items.clone();
        Self {
            items,
            filtered,
            theme,
            selected_index: 0,
            max_visible: max_visible.max(1),
            on_change: Box::new(on_change),
            on_cancel: Box::new(on_cancel),
            search_input: search_enabled.then(Input::new),
            search_enabled,
            empty_text: None,
            no_match_text: None,
            submenu: None,
            submenu_item_index: None,
            pending_submenu_value: None,
            pending_submenu_id: None,
            cache: None,
        }
    }

    /// Override the message shown when the list has no items.
    #[must_use]
    pub fn with_empty_text(mut self, text: impl Into<String>) -> Self {
        self.empty_text = Some(text.into());
        self
    }

    /// Override the message shown when filtering produces no matches.
    #[must_use]
    pub fn with_no_match_text(mut self, text: impl Into<String>) -> Self {
        self.no_match_text = Some(text.into());
        self
    }

    /// Update an item's current value by id.
    pub fn update_value(&mut self, id: &str, new_value: impl Into<String>) {
        let new_value = new_value.into();
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.current_value.clone_from(&new_value);
        }
        if let Some(item) = self.filtered.iter_mut().find(|i| i.id == id) {
            item.current_value = new_value;
        }
        self.cache = None;
    }

    fn display_items(&self) -> &[SettingItem] {
        if self.search_enabled {
            &self.filtered
        } else {
            &self.items
        }
    }

    fn apply_filter(&mut self, query: &str) {
        self.filtered = fuzzy_filter(&self.items, query, |item| item.label.as_str());
        self.selected_index = 0;
        self.cache = None;
    }

    fn close_submenu(&mut self) {
        self.submenu = None;
        self.pending_submenu_value = None;
        self.pending_submenu_id = None;
        if let Some(idx) = self.submenu_item_index.take() {
            let len = self.display_items().len();
            self.selected_index = if len == 0 { 0 } else { idx.min(len - 1) };
        }
        self.cache = None;
    }

    /// Poll submenu completion holder; apply value + close when set.
    fn poll_submenu_done(&mut self) -> bool {
        let Some(holder) = &self.pending_submenu_value else {
            return false;
        };
        let taken = holder.lock().ok().and_then(|mut g| g.take());
        let Some(done) = taken else {
            return false;
        };
        if let SubmenuDone::Selected(value) = done
            && let Some(id) = self.pending_submenu_id.clone()
        {
            self.update_value(&id, value.clone());
            (self.on_change)(&id, &value);
        }
        self.close_submenu();
        true
    }

    fn activate_item(&mut self) {
        let items: Vec<SettingItem> = self.display_items().to_vec();
        let Some(item) = items.get(self.selected_index).cloned() else {
            return;
        };

        if let Some(factory) = item.submenu.clone() {
            self.submenu_item_index = Some(self.selected_index);
            let holder = Arc::new(Mutex::new(None::<SubmenuDone>));
            let holder_cb = holder.clone();
            let done: Box<dyn FnMut(Option<String>) + Send> = Box::new(move |v| {
                if let Ok(mut g) = holder_cb.lock() {
                    *g = Some(match v {
                        Some(value) => SubmenuDone::Selected(value),
                        None => SubmenuDone::Cancelled,
                    });
                }
            });
            let component = factory(&item.current_value, done);
            self.pending_submenu_value = Some(holder);
            self.pending_submenu_id = Some(item.id.clone());
            self.submenu = Some(component);
            self.cache = None;
            return;
        }

        if let Some(values) = &item.values {
            if values.is_empty() {
                return;
            }
            let current_index = values
                .iter()
                .position(|v| v == &item.current_value)
                .unwrap_or(0);
            let next = (current_index + 1) % values.len();
            let new_value = values[next].clone();
            self.update_value(&item.id, new_value.clone());
            (self.on_change)(&item.id, &new_value);
            self.cache = None;
        }
    }

    fn add_hint_line(&self, lines: &mut Vec<String>, width: usize) {
        lines.push(String::new());
        let text = if self.search_enabled {
            "  Type to search · Enter/Space to change · Esc to cancel"
        } else {
            "  Enter/Space to change · Esc to cancel"
        };
        let styled = (self.theme.hint)(text);
        lines.push(truncate_to_width(&styled, width, "...", false));
    }

    fn render_main_list(&mut self, width: u16) -> Vec<String> {
        let width = usize::from(width);
        let mut lines = Vec::new();

        if self.search_enabled
            && let Some(input) = self.search_input.as_mut()
        {
            let w = u16::try_from(width).unwrap_or(u16::MAX);
            let h = input.measure(w);
            let area = Rect::new(0, 0, w, h.max(1));
            let mut buf = Buffer::empty(area);
            input.render(area, &mut buf);
            let mut row = String::new();
            for x in 0..w {
                if let Some(cell) = buf.cell((x, 0))
                    && cell.diff_option != CellDiffOption::Skip
                {
                    row.push_str(cell.symbol());
                }
            }
            lines.push(row);
            lines.push(String::new());
        }

        if self.items.is_empty() {
            lines.push(
                (self.theme.hint)(self.empty_text.as_deref().unwrap_or("  No settings available")),
            );
            self.add_hint_line(&mut lines, width);
            return lines;
        }

        let display_len = self.display_items().len();
        if display_len == 0 {
            lines.push(truncate_to_width(
                &(self.theme.hint)(
                    self.no_match_text
                        .as_deref()
                        .unwrap_or("  No matching settings"),
                ),
                width,
                "...",
                false,
            ));
            self.add_hint_line(&mut lines, width);
            return lines;
        }

        let max_visible = self.max_visible.min(display_len);
        let half = max_visible / 2;
        let start = self
            .selected_index
            .saturating_sub(half)
            .min(display_len.saturating_sub(max_visible));
        let end = (start + max_visible).min(display_len);

        let max_label_width = self
            .items
            .iter()
            .map(|i| visible_width(&i.label))
            .max()
            .unwrap_or(0)
            .min(30);

        let display: Vec<SettingItem> = self.display_items().to_vec();
        for (i, item) in display.iter().enumerate().take(end).skip(start) {
            let is_selected = i == self.selected_index;
            let prefix = if is_selected {
                self.theme.cursor.clone()
            } else {
                "  ".to_owned()
            };
            let prefix_width = visible_width(&prefix);
            let pad = max_label_width.saturating_sub(visible_width(&item.label));
            let label_padded = format!("{}{}", item.label, " ".repeat(pad));
            let label_text = (self.theme.label)(&label_padded, is_selected);
            let separator = "  ";
            let used = prefix_width + max_label_width + visible_width(separator);
            let value_max = width.saturating_sub(used).saturating_sub(2);
            let value_text = (self.theme.value)(
                &truncate_to_width(&item.current_value, value_max, "", false),
                is_selected,
            );
            let row = format!("{prefix}{label_text}{separator}{value_text}");
            lines.push(truncate_to_width(&row, width, "...", false));
        }

        if start > 0 || end < display_len {
            let scroll = format!("  ({}/{})", self.selected_index + 1, display_len);
            lines.push((self.theme.hint)(&truncate_to_width(
                &scroll,
                width.saturating_sub(2),
                "",
                false,
            )));
        }

        if let Some(selected) = display.get(self.selected_index)
            && let Some(desc) = &selected.description
        {
            lines.push(String::new());
            for line in wrap_text_with_ansi(desc, width.saturating_sub(4).max(1)) {
                lines.push((self.theme.description)(&format!("  {line}")));
            }
        }

        self.add_hint_line(&mut lines, width);
        lines
    }
}

impl Component for SettingsList {
    fn measure(&mut self, width: u16) -> u16 {
        if self.poll_submenu_done() {
            // closed; fall through to main list
        }
        if let Some(sub) = self.submenu.as_mut() {
            return sub.measure(width);
        }
        let lines = self.render_main_list(width);
        self.cache = Some((width, lines.clone()));
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let _ = self.poll_submenu_done();
        if let Some(sub) = self.submenu.as_mut() {
            sub.render(area, buf);
            return;
        }
        let lines = match &self.cache {
            Some((w, lines)) if *w == area.width => lines.clone(),
            _ => self.render_main_list(area.width),
        };
        paint_lines(area, buf, &lines);
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        if self.poll_submenu_done() {
            return EventResult::Render;
        }
        if let Some(sub) = self.submenu.as_mut() {
            let result = sub.handle_event(event);
            if self.poll_submenu_done() {
                return EventResult::Render;
            }
            return result;
        }

        match event {
            UiEvent::Key(key) => {
                let kb = get_keybindings();
                let display_len = self.display_items().len();
                if kb.matches(key, "tui.select.up") {
                    if display_len == 0 {
                        return EventResult::Consumed;
                    }
                    self.selected_index = if self.selected_index == 0 {
                        display_len - 1
                    } else {
                        self.selected_index - 1
                    };
                    self.cache = None;
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.select.down") {
                    if display_len == 0 {
                        return EventResult::Consumed;
                    }
                    self.selected_index = if self.selected_index + 1 >= display_len {
                        0
                    } else {
                        self.selected_index + 1
                    };
                    self.cache = None;
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.select.confirm")
                    || matches!(key.code, crossterm::event::KeyCode::Char(' '))
                {
                    self.activate_item();
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.select.cancel") {
                    (self.on_cancel)();
                    return EventResult::Consumed;
                }
                if self.search_enabled
                    && let Some(input) = self.search_input.as_mut()
                {
                    // Strip spaces from search input (TS: data.replace(/ /g, ""))
                    if let crossterm::event::KeyCode::Char(c) = key.code {
                        if c == ' ' {
                            return EventResult::Consumed;
                        }
                        let mut k = *key;
                        k.code = crossterm::event::KeyCode::Char(c);
                        let r = input.handle_event(&UiEvent::Key(k));
                        if r.is_handled() {
                            let q = input.value().to_owned();
                            self.apply_filter(&q);
                            return EventResult::Render;
                        }
                    } else {
                        let r = input.handle_event(event);
                        if r.is_handled() {
                            let q = input.value().to_owned();
                            self.apply_filter(&q);
                            return EventResult::Render;
                        }
                    }
                }
                EventResult::Ignored
            }
            UiEvent::Paste(text) if self.search_enabled => {
                if let Some(input) = self.search_input.as_mut() {
                    let sanitized: String = text.chars().filter(|c| *c != ' ').collect();
                    input.paste(&sanitized);
                    let q = input.value().to_owned();
                    self.apply_filter(&q);
                    return EventResult::Render;
                }
                EventResult::Ignored
            }
            _ => EventResult::Ignored,
        }
    }

    fn invalidate(&mut self) {
        self.cache = None;
        if let Some(sub) = self.submenu.as_mut() {
            sub.invalidate();
        }
        if let Some(input) = self.search_input.as_mut() {
            input.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::util::{render_snapshot, strip_ansi};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn sample_items() -> Vec<SettingItem> {
        vec![
            SettingItem {
                id: "theme".into(),
                label: "Theme".into(),
                description: Some("Color theme".into()),
                current_value: "dark".into(),
                values: Some(vec!["dark".into(), "light".into()]),
                submenu: None,
            },
            SettingItem {
                id: "model".into(),
                label: "Model".into(),
                description: None,
                current_value: "fast".into(),
                values: Some(vec!["fast".into(), "smart".into()]),
                submenu: None,
            },
        ]
    }

    #[test]
    fn empty_state() {
        let mut list = SettingsList::new(
            vec![],
            5,
            SettingsListTheme::default(),
            |_, _| {},
            || {},
            &SettingsListOptions::default(),
        );
        let snap = render_snapshot(&mut list, 60);
        assert!(snap.iter().any(|l| strip_ansi(l).contains("No settings")));
    }

    #[test]
    fn empty_search_disabled_still_shows_esc_hint() {
        let mut list = SettingsList::new(
            vec![],
            5,
            SettingsListTheme::default(),
            |_, _| {},
            || {},
            &SettingsListOptions {
                enable_search: false,
            },
        );
        let snap = render_snapshot(&mut list, 60);
        let joined = snap
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("Esc to cancel"),
            "search-disabled empty list must show exit hint: {joined}"
        );
    }

    #[test]
    fn empty_and_no_match_overrides_render() {
        let mut empty = SettingsList::new(
            vec![],
            5,
            SettingsListTheme::default(),
            |_, _| {},
            || {},
            &SettingsListOptions::default(),
        )
        .with_empty_text("  No resources found");
        let empty_snap = render_snapshot(&mut empty, 60);
        assert!(
            empty_snap
                .iter()
                .any(|l| strip_ansi(l).contains("No resources found"))
        );

        let mut filtered = SettingsList::new(
            sample_items(),
            5,
            SettingsListTheme::default(),
            |_, _| {},
            || {},
            &SettingsListOptions {
                enable_search: true,
            },
        )
        .with_no_match_text("  No resources found");
        filtered.apply_filter("zzz");
        let filtered_snap = render_snapshot(&mut filtered, 60);
        assert!(
            filtered_snap
                .iter()
                .any(|l| strip_ansi(l).contains("No resources found"))
        );
    }

    #[test]
    fn cycle_values_on_enter() {
        let mut list = SettingsList::new(
            sample_items(),
            5,
            SettingsListTheme::default(),
            |_, _| {},
            || {},
            &SettingsListOptions::default(),
        );
        list.activate_item();
        assert_eq!(list.items[0].current_value, "light");
    }

    #[test]
    fn keyboard_nav() {
        let mut list = SettingsList::new(
            sample_items(),
            5,
            SettingsListTheme::default(),
            |_, _| {},
            || {},
            &SettingsListOptions::default(),
        );
        let down = UiEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(list.handle_event(&down), EventResult::Render);
        assert_eq!(list.selected_index, 1);
    }

    #[test]
    fn fuzzy_search_filters() {
        let mut list = SettingsList::new(
            sample_items(),
            5,
            SettingsListTheme::default(),
            |_, _| {},
            || {},
            &SettingsListOptions {
                enable_search: true,
            },
        );
        list.apply_filter("mod");
        assert_eq!(list.filtered.len(), 1);
        assert_eq!(list.filtered[0].id, "model");
    }

    #[test]
    fn widths_matrix() {
        let mut list = SettingsList::new(
            sample_items(),
            5,
            SettingsListTheme::default(),
            |_, _| {},
            || {},
            &SettingsListOptions::default(),
        );
        for w in [24_u16, 60, 80, 120] {
            let _ = render_snapshot(&mut list, w);
        }
    }
}
