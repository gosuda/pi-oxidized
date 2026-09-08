//! Selector view-models (model / thinking / session / tree / settings / config / auth / scoped).
//!
//! Ports the `*SelectorComponent` family from
//! `.references/pi-2.0/packages/coding-agent/src/modes/interactive/components/`.
//! Each builder produces a pi-tui `SelectList` or `SettingsList` configured
//! against the thread-local current theme (set by [`super::view::compose`] via
//! [`super::theme::with_theme`]). Selectors *replace* the editor inline (not
//! overlays) in the reference; here they are plain components the composer
//! splices into the editor slot.

use std::collections::BTreeMap;

use pi_ai::ModelThinkingLevel;
use pi_tui::component::{Component, EventResult, UiEvent};
use pi_tui::components::{
    Input, SelectItem, SelectList, SettingItem, SettingsList, SettingsListOptions, Text,
};
use pi_tui::keybindings::get_keybindings;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crossterm::event::KeyEvent;

use super::state::{
    AuthSelectorEntry, ConfigSelectorEntry, ModelSelectorEntry, SelectorKind, SessionPickerEntry,
    SettingsRow, TreeEntry,
};
use super::theme;

/// Maximum visible rows for any selector (ports reference default).
pub const SELECTOR_MAX_VISIBLE: usize = 12;

/// Persistent exit hint appended to every select-list selector.
pub const SELECTOR_EXIT_HINT: &str = "  esc to cancel";

/// Tree selector filter modes for `app.tree.filter.*`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TreeFilterMode {
    /// Default view (hide bookkeeping entries).
    #[default]
    Default,
    /// Hide tool-result messages.
    NoTools,
    /// User messages only.
    UserOnly,
    /// Entries that carry an explicit label.
    LabeledOnly,
}

impl TreeFilterMode {
    /// Apply a named `app.tree.filter.*` binding to the current mode.
    #[must_use]
    pub fn apply_binding(self, binding_id: &str) -> Option<Self> {
        match binding_id {
            "app.tree.filter.default" => Some(Self::Default),
            "app.tree.filter.noTools" => Some(if self == Self::NoTools {
                Self::Default
            } else {
                Self::NoTools
            }),
            "app.tree.filter.userOnly" => Some(if self == Self::UserOnly {
                Self::Default
            } else {
                Self::UserOnly
            }),
            "app.tree.filter.labeledOnly" => Some(if self == Self::LabeledOnly {
                Self::Default
            } else {
                Self::LabeledOnly
            }),
            _ => None,
        }
    }
}

/// Compare session file paths with canonicalization when possible.
///
/// Falls back to exact string equality when either path cannot be
/// canonicalized (missing file, permission error, non-UTF8). Symlinked
/// active-session paths must still be blocked.
#[must_use]
pub fn same_session_path(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    match (
        std::fs::canonicalize(left).ok(),
        std::fs::canonicalize(right).ok(),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Sole owner of inline session-delete confirmation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SessionDeleteConfirm {
    /// No delete confirmation is armed.
    #[default]
    Idle,
    /// Waiting for Enter to delete `path`, or Esc to cancel confirmation.
    Armed {
        /// Session path pending deletion.
        path: String,
    },
}

impl SessionDeleteConfirm {
    /// Clear any armed confirmation.
    pub fn clear(&mut self) {
        *self = Self::Idle;
    }

    /// Whether a confirmation is armed.
    #[must_use]
    pub const fn is_armed(&self) -> bool {
        matches!(self, Self::Armed { .. })
    }
}

/// Noun-specific empty / no-match copy for a selector flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectorEmptyCopy {
    /// Shown when the selector has zero items.
    pub empty: &'static str,
    /// Shown when filtering yields zero matches.
    pub no_match: &'static str,
}

/// Extension select/confirm lists have no [`SelectorKind`].
pub const EXTENSION_EMPTY_COPY: SelectorEmptyCopy = SelectorEmptyCopy {
    empty: "  No matching options",
    no_match: "  No matching options",
};

/// Canonical per-flow empty-state copy. Exhaustive over [`SelectorKind`].
#[must_use]
pub fn selector_empty_copy(kind: SelectorKind) -> SelectorEmptyCopy {
    let both = |text| SelectorEmptyCopy {
        empty: text,
        no_match: text,
    };
    match kind {
        SelectorKind::Model | SelectorKind::ScopedModels => both("  No matching models"),
        SelectorKind::Thinking => both("  No matching thinking levels"),
        SelectorKind::Theme => both("  No matching themes"),
        SelectorKind::Session => both("  No sessions found"),
        SelectorKind::Tree => both("  No entries found"),
        SelectorKind::Fork => both("  No user messages found"),
        SelectorKind::Auth => SelectorEmptyCopy {
            empty: "  No providers available",
            no_match: "  No matching providers",
        },
        SelectorKind::AuthType => both("  No login methods available"),
        SelectorKind::Logout => SelectorEmptyCopy {
            empty: "  No providers logged in. Use /login first.",
            no_match: "  No matching providers",
        },
        SelectorKind::Settings | SelectorKind::Trust => SelectorEmptyCopy {
            empty: "  No settings available",
            no_match: "  No matching settings",
        },
        SelectorKind::Config => both("  No resources found"),
        SelectorKind::ImportConfirm | SelectorKind::ImportCwdConfirm => {
            both("  No matching options")
        }
    }
}

pub(super) fn apply_select_list_copy(list: SelectList, copy: SelectorEmptyCopy) -> SelectList {
    list.with_empty_text(copy.empty)
        .with_no_match_text(copy.no_match)
        .with_hint(SELECTOR_EXIT_HINT)
}

pub(super) fn apply_settings_list_copy(
    list: SettingsList,
    copy: SelectorEmptyCopy,
) -> SettingsList {
    list.with_empty_text(copy.empty)
        .with_no_match_text(copy.no_match)
}

// ---------------------------------------------------------------------------
// Select-list selectors (model / thinking / session / tree / auth / scoped)
// ---------------------------------------------------------------------------

/// Build the model selector. Reads the thread-local current theme.
#[must_use]
pub fn build_model_selector(entries: &[ModelSelectorEntry], selected: usize) -> Box<dyn Component> {
    let items = entries.iter().map(model_item).collect::<Vec<_>>();
    let mut list = apply_select_list_copy(
        SelectList::new(items, SELECTOR_MAX_VISIBLE, theme::select_list_theme()),
        selector_empty_copy(SelectorKind::Model),
    );
    list.set_selected_index(selected);
    Box::new(list)
}

/// Build the session picker.
#[must_use]
pub fn build_session_picker(entries: &[SessionPickerEntry], selected: usize) -> Box<dyn Component> {
    let items = entries.iter().map(session_item).collect::<Vec<_>>();
    let mut list = apply_select_list_copy(
        SelectList::new(items, SELECTOR_MAX_VISIBLE, theme::select_list_theme()),
        selector_empty_copy(SelectorKind::Session),
    );
    list.set_selected_index(selected);
    Box::new(list)
}

/// Build the auth/login selector.
#[must_use]
pub fn build_auth_selector(entries: &[AuthSelectorEntry], selected: usize) -> Box<dyn Component> {
    let items = entries.iter().map(auth_item).collect::<Vec<_>>();
    let mut list = apply_select_list_copy(
        SelectList::new(items, SELECTOR_MAX_VISIBLE, theme::select_list_theme()),
        selector_empty_copy(SelectorKind::Auth),
    );
    list.set_selected_index(selected);
    Box::new(list)
}

/// Build the scoped-models selector with `[x]`/`[ ]` enable marks.
#[must_use]
pub fn build_scoped_models_selector(
    entries: &[ModelSelectorEntry],
    enabled: &BTreeMap<String, bool>,
    selected: usize,
) -> Box<dyn Component> {
    let items = entries
        .iter()
        .map(|e| {
            let on = enabled.get(&e.value).copied().unwrap_or(false);
            let mark = if on { "[x]" } else { "[ ]" };
            SelectItem::new(e.value.clone(), format!("{mark} {}", e.label))
                .with_description(e.description.clone().unwrap_or_default())
        })
        .collect::<Vec<_>>();
    let mut list = apply_select_list_copy(
        SelectList::new(items, SELECTOR_MAX_VISIBLE, theme::select_list_theme()),
        selector_empty_copy(SelectorKind::ScopedModels),
    );
    list.set_selected_index(selected);
    Box::new(list)
}

/// Build the tree (branch) selector with depth indentation.
#[must_use]
pub fn build_tree_selector(entries: &[TreeEntry], selected: usize) -> Box<dyn Component> {
    let items = entries
        .iter()
        .map(|e| {
            let indent = "  ".repeat(e.depth);
            SelectItem::new(e.value.clone(), format!("{indent}{}", e.label))
        })
        .collect::<Vec<_>>();
    let mut list = apply_select_list_copy(
        SelectList::new(items, SELECTOR_MAX_VISIBLE, theme::select_list_theme()),
        selector_empty_copy(SelectorKind::Tree),
    );
    list.set_selected_index(selected);
    Box::new(list)
}

fn model_item(e: &ModelSelectorEntry) -> SelectItem {
    SelectItem::new(e.value.clone(), e.label.clone())
        .with_description(e.description.clone().unwrap_or_default())
}

fn session_item(e: &SessionPickerEntry) -> SelectItem {
    SelectItem::new(e.value.clone(), e.label.clone())
        .with_description(e.description.clone().unwrap_or_default())
}

fn auth_item(e: &AuthSelectorEntry) -> SelectItem {
    SelectItem::new(e.value.clone(), e.label.clone())
        .with_description(e.description.clone().unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Session selector with inline delete confirmation
// ---------------------------------------------------------------------------

type SessionItemCallback = Box<dyn FnMut(&SelectItem) + Send>;
type SessionCancelCallback = Box<dyn FnMut() + Send>;
type SessionDeleteCallback = Box<dyn FnMut(String) + Send>;
type SessionErrorCallback = Box<dyn FnMut(String) + Send>;
type SessionConfirmCallback = Box<dyn FnMut(Option<String>) + Send>;

/// Session picker that owns inline delete-confirmation state.
pub struct SessionSelector {
    list: SelectList,
    confirm: SessionDeleteConfirm,
    current_session_path: Option<String>,
    /// Called when the user confirms a session row (not delete).
    pub on_select: Option<SessionItemCallback>,
    /// Called when the selector is cancelled while unconfirmed.
    pub on_cancel: Option<SessionCancelCallback>,
    /// Called after Enter confirms an armed delete.
    pub on_delete: Option<SessionDeleteCallback>,
    /// Called when delete is blocked (active session).
    pub on_error: Option<SessionErrorCallback>,
    /// Called whenever confirmation arms or clears (`Some(path)` / `None`).
    pub on_confirm_change: Option<SessionConfirmCallback>,
}

impl SessionSelector {
    /// Build a session selector around a configured [`SelectList`].
    #[must_use]
    pub fn new(list: SelectList, current_session_path: Option<String>) -> Self {
        Self {
            list,
            confirm: SessionDeleteConfirm::Idle,
            current_session_path,
            on_select: None,
            on_cancel: None,
            on_delete: None,
            on_error: None,
            on_confirm_change: None,
        }
    }

    /// Current inline confirmation state (tests / diagnostics).
    #[must_use]
    pub const fn confirm_state(&self) -> &SessionDeleteConfirm {
        &self.confirm
    }

    fn set_confirm(&mut self, next: SessionDeleteConfirm) {
        self.confirm = next;
        if let Some(cb) = self.on_confirm_change.as_mut() {
            match &self.confirm {
                SessionDeleteConfirm::Idle => cb(None),
                SessionDeleteConfirm::Armed { path } => cb(Some(path.clone())),
            }
        }
    }

    fn start_delete_for_selected(&mut self) {
        let Some(item) = self.list.selected_item().cloned() else {
            return;
        };
        if self
            .current_session_path
            .as_deref()
            .is_some_and(|current| same_session_path(current, &item.value))
        {
            if let Some(cb) = self.on_error.as_mut() {
                cb("Cannot delete the currently active session".to_owned());
            }
            return;
        }
        self.set_confirm(SessionDeleteConfirm::Armed { path: item.value });
    }

    fn handle_key(&mut self, key: &KeyEvent) -> EventResult {
        let kb = get_keybindings();
        if let SessionDeleteConfirm::Armed { path } = &self.confirm {
            if kb.matches(key, "tui.select.confirm") {
                let path = path.clone();
                self.set_confirm(SessionDeleteConfirm::Idle);
                if let Some(cb) = self.on_delete.as_mut() {
                    cb(path);
                }
                return EventResult::Consumed;
            }
            if kb.matches(key, "tui.select.cancel") {
                self.set_confirm(SessionDeleteConfirm::Idle);
                return EventResult::Render;
            }
            return EventResult::Consumed;
        }

        // Ctrl+D always arms delete confirmation.
        if kb.matches(key, "app.session.delete") {
            self.start_delete_for_selected();
            return EventResult::Render;
        }
        // Ctrl+Backspace arms delete confirmation (alias for Ctrl+D).
        if kb.matches(key, "app.session.deleteNoninvasive") {
            self.start_delete_for_selected();
            return EventResult::Render;
        }

        if kb.matches(key, "tui.select.confirm") {
            if let Some(item) = self.list.selected_item().cloned()
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

        // Navigation and other list keys: clear any stale confirm first.
        self.list.handle_event(&UiEvent::Key(*key))
    }
}

impl Component for SessionSelector {
    fn measure(&mut self, width: u16) -> u16 {
        self.list.measure(width)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.list.render(area, buf);
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        match event {
            UiEvent::Key(key) => self.handle_key(key),
            other => self.list.handle_event(other),
        }
    }

    fn invalidate(&mut self) {
        // Layout-only: never clear SessionDeleteConfirm here. Paint/commit
        // invalidation would silently disarm an armed delete.
        self.list.invalidate();
    }
}

/// Build the live session selector used by interactive mode.
#[must_use]
pub fn build_session_selector_component(
    entries: &[SessionPickerEntry],
    selected: usize,
    current_session_path: Option<String>,
) -> SessionSelector {
    let items = entries.iter().map(session_item).collect::<Vec<_>>();
    let mut list = apply_select_list_copy(
        SelectList::new(items, SELECTOR_MAX_VISIBLE, theme::select_list_theme())
            .with_hint(SELECTOR_EXIT_HINT),
        selector_empty_copy(SelectorKind::Session),
    );
    list.set_selected_index(selected);
    SessionSelector::new(list, current_session_path)
}

// Save-chord select-list wrappers (model and thinking selectors)
// ---------------------------------------------------------------------------

type SaveDefaultCallback = Box<dyn FnMut(String) + Send>;

/// Select-list wrapper that intercepts one app-level save chord before the
/// inner list sees it (ports the reference selector `handleInput` save branch,
/// `app.models.save` on the model selector). Navigation, filter,
/// confirm, and cancel keys fall through to the wrapped [`SelectList`]
/// unchanged, so a user rebind of the save id — or a physical-key collision
/// with another `app.*` id — resolves through the shared keybindings manager,
/// never a hardcoded key.
pub struct SaveableSelectList {
    list: SelectList,
    save_binding: &'static str,
    /// Called with the selected row's value when the save chord fires.
    pub on_save_as_default: Option<SaveDefaultCallback>,
}

impl SaveableSelectList {
    /// Wrap `list` so `save_binding` triggers [`Self::on_save_as_default`].
    #[must_use]
    pub fn new(list: SelectList, save_binding: &'static str) -> Self {
        Self {
            list,
            save_binding,
            on_save_as_default: None,
        }
    }

    /// Replace rows and retain the selected value when it remains available.
    pub fn replace_items(
        &mut self,
        items: Vec<SelectItem>,
        selected_value: Option<&str>,
    ) {
        let selected_index = selected_value
            .and_then(|value| items.iter().position(|item| item.value == value))
            .unwrap_or(0);
        self.list.set_items(items);
        self.list.set_selected_index(selected_index);
    }

    /// Return the currently selected row.
    #[must_use]
    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.list.selected_item()
    }

    fn handle_key(&mut self, key: &KeyEvent) -> EventResult {
        if self.on_save_as_default.is_some()
            && get_keybindings().matches(key, self.save_binding)
        {
            // Consumed even with no selected row, mirroring the reference.
            if let Some(item) = self.list.selected_item().cloned()
                && let Some(cb) = self.on_save_as_default.as_mut()
            {
                cb(item.value);
            }
            return EventResult::Consumed;
        }
        self.list.handle_event(&UiEvent::Key(*key))
    }
}

impl Component for SaveableSelectList {
    fn measure(&mut self, width: u16) -> u16 {
        self.list.measure(width)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.list.render(area, buf);
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        match event {
            UiEvent::Key(key) => self.handle_key(key),
            other => self.list.handle_event(other),
        }
    }

    fn invalidate(&mut self) {
        self.list.invalidate();
    }
}

/// Thinking-level selector with search, session-only selection, and a
/// configurable save-as-default chord.
///
/// The selected row callback changes only the active session. The optional
/// save callback is invoked by `app.thinking.save` and is the only callback
/// that asks the runtime to update the global default.
pub struct ThinkingSelectorComponent {
    title: Text,
    cycle_hint: Text,
    search_input: Input,
    select_list: SaveableSelectList,
    footer: Text,
    all_items: Vec<SelectItem>,
}

impl ThinkingSelectorComponent {
    /// Build a selector around the available levels and session callbacks.
    #[must_use]
    pub fn new(
        current_level: ModelThinkingLevel,
        available_levels: Vec<ModelThinkingLevel>,
        on_select: Box<dyn FnMut(ModelThinkingLevel) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
        on_select_as_default: Option<Box<dyn FnMut(ModelThinkingLevel) + Send>>,
        default_level: Option<ModelThinkingLevel>,
    ) -> Self {
        let all_items = available_levels
            .into_iter()
            .map(|level| {
                let marker = if level == current_level { "✓ " } else { "  " };
                let description = thinking_level_description(level);
                let description = if default_level == Some(level) {
                    format!("{description} · default")
                } else {
                    description.to_owned()
                };
                SelectItem::new(
                    crate::core::agent_session::model::level_str(level),
                    format!("{marker}{}", crate::core::agent_session::model::level_str(level)),
                )
                .with_description(description)
            })
            .collect::<Vec<_>>();
        let selected_index = all_items
            .iter()
            .position(|item| {
                item.value == crate::core::agent_session::model::level_str(current_level)
            })
            .unwrap_or(0);

        let copy = selector_empty_copy(SelectorKind::Thinking);
        let mut select_list = SelectList::new(
            all_items.clone(),
            SELECTOR_MAX_VISIBLE,
            theme::select_list_theme(),
        )
        .with_empty_text(copy.empty)
        .with_no_match_text(copy.no_match);
        select_list.set_selected_index(selected_index);
        select_list.on_select = Some(Box::new(move |item| {
            if let Some(level) = parse_thinking_level(item.value.as_str()) {
                on_select(level);
            }
        }));
        select_list.on_cancel = Some(on_cancel);

        let mut select_list = SaveableSelectList::new(select_list, "app.thinking.save");
        if let Some(mut callback) = on_select_as_default {
            select_list.on_save_as_default = Some(Box::new(move |value| {
                if let Some(level) = parse_thinking_level(value.as_str()) {
                    callback(level);
                }
            }));
        }

        let mut search_input = Input::new();
        search_input.set_focused(true);
        Self {
            title: Text::with_padding("Thinking Level", 0, 0),
            cycle_hint: Text::with_padding(
                format!(
                    "{} cycles thinking levels in-session",
                    pi_tui::keybindings::key_display_text("app.thinking.cycle")
                ),
                0,
                0,
            ),
            search_input,
            select_list,
            footer: Text::with_padding(selector_save_hint("app.thinking.save"), 0, 0),
            all_items,
        }
    }

    fn apply_filter(&mut self) {
        let query = self.search_input.value().to_owned();
        let selected_value = self
            .select_list
            .selected_item()
            .map(|item| item.value.clone());
        let filtered = pi_tui::fuzzy::fuzzy_filter(&self.all_items, &query, |item| {
            item.value.as_str()
        });
        self.select_list
            .replace_items(filtered, selected_value.as_deref());
    }
}

impl Component for ThinkingSelectorComponent {
    fn measure(&mut self, width: u16) -> u16 {
        self.title
            .measure(width)
            .saturating_add(self.cycle_hint.measure(width))
            .saturating_add(self.search_input.measure(width))
            .saturating_add(self.select_list.measure(width))
            .saturating_add(self.footer.measure(width))
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let mut y = area.y;
        let bottom = area.bottom();
        let mut render_child = |child: &mut dyn Component| {
            if y >= bottom {
                return;
            }
            let height = child.measure(area.width).min(bottom - y);
            if height == 0 {
                return;
            }
            child.render(Rect::new(area.x, y, area.width, height), buf);
            y = y.saturating_add(height);
        };
        render_child(&mut self.title);
        render_child(&mut self.cycle_hint);
        render_child(&mut self.search_input);
        render_child(&mut self.select_list);
        render_child(&mut self.footer);
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        let list_result = self.select_list.handle_event(event);
        if !matches!(list_result, EventResult::Ignored) {
            return list_result;
        }

        let input_result = self.search_input.handle_event(event);
        if !matches!(input_result, EventResult::Ignored) {
            self.apply_filter();
        }
        input_result
    }

    fn invalidate(&mut self) {
        self.title.invalidate();
        self.cycle_hint.invalidate();
        self.search_input.invalidate();
        self.select_list.invalidate();
        self.footer.invalidate();
    }
}

fn thinking_level_description(level: ModelThinkingLevel) -> &'static str {
    match level {
        ModelThinkingLevel::Off => "No reasoning",
        ModelThinkingLevel::Minimal => "Very brief reasoning (~1k tokens)",
        ModelThinkingLevel::Low => "Light reasoning (~2k tokens)",
        ModelThinkingLevel::Medium => "Moderate reasoning (~8k tokens)",
        ModelThinkingLevel::High => "Deep reasoning (~16k tokens)",
        ModelThinkingLevel::Xhigh => "Extra-high reasoning (~32k tokens)",
        ModelThinkingLevel::Max => "Maximum reasoning",
    }
}

fn parse_thinking_level(value: &str) -> Option<ModelThinkingLevel> {
    match value {
        "off" => Some(ModelThinkingLevel::Off),
        "minimal" => Some(ModelThinkingLevel::Minimal),
        "low" => Some(ModelThinkingLevel::Low),
        "medium" => Some(ModelThinkingLevel::Medium),
        "high" => Some(ModelThinkingLevel::High),
        "xhigh" => Some(ModelThinkingLevel::Xhigh),
        "max" => Some(ModelThinkingLevel::Max),
        _ => None,
    }
}

/// Footer hint naming the configurable save chord (reference selector footer:
/// "⏎ to select · `<key>` to set as default · esc to cancel").
#[must_use]
pub fn selector_save_hint(save_binding: &str) -> String {
    format!(
        "  ⏎ to select · {} to set as default · esc to cancel",
        pi_tui::keybindings::key_display_text(save_binding)
    )
}

// ---------------------------------------------------------------------------
// Settings-list selectors (settings / config)
// ---------------------------------------------------------------------------

/// Build the settings selector (cycleable settings rows).
#[must_use]
pub fn build_settings_selector(rows: &[SettingsRow], selected: usize) -> Box<dyn Component> {
    let _ = selected;
    let items = rows.iter().map(setting_item).collect::<Vec<_>>();
    let list = apply_settings_list_copy(
        SettingsList::new(
            items,
            SELECTOR_MAX_VISIBLE,
            theme::settings_list_theme(),
            |_id, _value| {},
            || {},
            &SettingsListOptions::default(),
        ),
        selector_empty_copy(SelectorKind::Settings),
    );
    Box::new(list)
}

/// Build the config selector (resources list with config empty copy).
#[must_use]
pub fn build_config_selector(
    entries: &[ConfigSelectorEntry],
    selected: usize,
) -> Box<dyn Component> {
    let _ = selected;
    let items = entries.iter().map(setting_item).collect::<Vec<_>>();
    let list = apply_settings_list_copy(
        SettingsList::new(
            items,
            SELECTOR_MAX_VISIBLE,
            theme::settings_list_theme(),
            |_id, _value| {},
            || {},
            &SettingsListOptions::default(),
        ),
        selector_empty_copy(SelectorKind::Config),
    );
    Box::new(list)
}

fn setting_item(row: &SettingsRow) -> SettingItem {
    SettingItem {
        id: row.id.clone(),
        label: row.label.clone(),
        description: row.description.clone(),
        current_value: row.current_value.clone(),
        values: row.values.clone(),
        submenu: None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::SelectorKind;
    use super::super::view::{render_component, snapshot_buffer_plain};
    use super::*;

    const ALL_KINDS: &[SelectorKind] = &[
        SelectorKind::Model,
        SelectorKind::Thinking,
        SelectorKind::Session,
        SelectorKind::Tree,
        SelectorKind::Fork,
        SelectorKind::Trust,
        SelectorKind::Theme,
        SelectorKind::AuthType,
        SelectorKind::Auth,
        SelectorKind::Logout,
        SelectorKind::Settings,
        SelectorKind::Config,
        SelectorKind::ScopedModels,
        SelectorKind::ImportConfirm,
        SelectorKind::ImportCwdConfirm,
    ];

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum HelperBoundary {
        Select,
        Settings,
    }

    fn expected_mapping(kind: SelectorKind) -> (SelectorEmptyCopy, HelperBoundary) {
        match kind {
            SelectorKind::Model | SelectorKind::ScopedModels => (
                SelectorEmptyCopy {
                    empty: "  No matching models",
                    no_match: "  No matching models",
                },
                HelperBoundary::Select,
            ),
            SelectorKind::Thinking => (
                SelectorEmptyCopy {
                    empty: "  No matching thinking levels",
                    no_match: "  No matching thinking levels",
                },
                HelperBoundary::Select,
            ),
            SelectorKind::Session => (
                SelectorEmptyCopy {
                    empty: "  No sessions found",
                    no_match: "  No sessions found",
                },
                HelperBoundary::Select,
            ),
            SelectorKind::Tree => (
                SelectorEmptyCopy {
                    empty: "  No entries found",
                    no_match: "  No entries found",
                },
                HelperBoundary::Select,
            ),
            SelectorKind::Theme => (
                SelectorEmptyCopy {
                    empty: "  No matching themes",
                    no_match: "  No matching themes",
                },
                HelperBoundary::Select,
            ),
            SelectorKind::Fork => (
                SelectorEmptyCopy {
                    empty: "  No user messages found",
                    no_match: "  No user messages found",
                },
                HelperBoundary::Select,
            ),
            SelectorKind::Auth => (
                SelectorEmptyCopy {
                    empty: "  No providers available",
                    no_match: "  No matching providers",
                },
                HelperBoundary::Select,
            ),
            SelectorKind::AuthType => (
                SelectorEmptyCopy {
                    empty: "  No login methods available",
                    no_match: "  No login methods available",
                },
                HelperBoundary::Select,
            ),
            SelectorKind::Logout => (
                SelectorEmptyCopy {
                    empty: "  No providers logged in. Use /login first.",
                    no_match: "  No matching providers",
                },
                HelperBoundary::Select,
            ),
            SelectorKind::Settings | SelectorKind::Trust => (
                SelectorEmptyCopy {
                    empty: "  No settings available",
                    no_match: "  No matching settings",
                },
                HelperBoundary::Settings,
            ),
            SelectorKind::Config => (
                SelectorEmptyCopy {
                    empty: "  No resources found",
                    no_match: "  No resources found",
                },
                HelperBoundary::Settings,
            ),
            SelectorKind::ImportConfirm | SelectorKind::ImportCwdConfirm => (
                SelectorEmptyCopy {
                    empty: "  No matching options",
                    no_match: "  No matching options",
                },
                HelperBoundary::Select,
            ),
        }
    }

    fn plain_empty(comp: &mut dyn Component) -> String {
        let buf = render_component(comp, 80);
        snapshot_buffer_plain(&buf, 80, buf.area().height).join("\n")
    }

    fn selector_kind_mapping_is_exhaustive_at_helper_boundary() {
        assert_eq!(
            15,
            ALL_KINDS.len(),
            "update ALL_KINDS when SelectorKind grows"
        );
        for &kind in ALL_KINDS {
            let (expected, boundary) = expected_mapping(kind);
            let copy = selector_empty_copy(kind);
            assert_eq!(copy, expected, "{kind:?} selector_empty_copy drift");
            assert!(
                !copy.empty.contains("No matching commands")
                    && !copy.no_match.contains("No matching commands"),
                "{kind:?} must not use generic fallback"
            );
            let plain = match boundary {
                HelperBoundary::Select => {
                    let mut comp: Box<dyn Component> = match kind {
                        SelectorKind::Model => build_model_selector(&[], 0),
                        SelectorKind::Session => build_session_picker(&[], 0),
                        SelectorKind::Auth => build_auth_selector(&[], 0),
                        SelectorKind::ScopedModels => {
                            build_scoped_models_selector(&[], &BTreeMap::new(), 0)
                        }
                        SelectorKind::Tree => build_tree_selector(&[], 0),
                        _ => Box::new(apply_select_list_copy(
                            SelectList::new(
                                vec![],
                                SELECTOR_MAX_VISIBLE,
                                theme::select_list_theme(),
                            ),
                            copy,
                        )),
                    };
                    plain_empty(comp.as_mut())
                }
                HelperBoundary::Settings => {
                    let mut comp: Box<dyn Component> = match kind {
                        SelectorKind::Settings => build_settings_selector(&[], 0),
                        SelectorKind::Config => build_config_selector(&[], 0),
                        _ => Box::new(apply_settings_list_copy(
                            SettingsList::new(
                                vec![],
                                SELECTOR_MAX_VISIBLE,
                                theme::settings_list_theme(),
                                |_id, _value| {},
                                || {},
                                &SettingsListOptions::default(),
                            ),
                            copy,
                        )),
                    };
                    plain_empty(comp.as_mut())
                }
            };
            assert!(
                plain.contains(expected.empty.trim()),
                "{kind:?} empty copy missing:\n{plain}"
            );
            assert!(
                !plain.contains("No matching commands"),
                "{kind:?} generic fallback leaked:\n{plain}"
            );
            match boundary {
                HelperBoundary::Select => assert!(
                    plain.contains(SELECTOR_EXIT_HINT.trim()),
                    "{kind:?} select exit hint missing:\n{plain}"
                ),
                HelperBoundary::Settings => assert!(
                    plain.contains("Esc to cancel"),
                    "{kind:?} settings Esc hint missing:\n{plain}"
                ),
            }
        }
        let mut extension = apply_select_list_copy(
            SelectList::new(vec![], SELECTOR_MAX_VISIBLE, theme::select_list_theme()),
            EXTENSION_EMPTY_COPY,
        );
        let plain = plain_empty(&mut extension);
        assert!(plain.contains(EXTENSION_EMPTY_COPY.empty.trim()));
        assert!(plain.contains(SELECTOR_EXIT_HINT.trim()));
        assert!(!plain.contains("No matching commands"));
    }

    #[test]
    fn tree_filter_bindings_cover_four_modes() {
        let mode = TreeFilterMode::Default;
        assert_eq!(
            mode.apply_binding("app.tree.filter.default"),
            Some(TreeFilterMode::Default)
        );
        assert_eq!(
            mode.apply_binding("app.tree.filter.noTools"),
            Some(TreeFilterMode::NoTools)
        );
        assert_eq!(
            TreeFilterMode::NoTools.apply_binding("app.tree.filter.noTools"),
            Some(TreeFilterMode::Default)
        );
        assert_eq!(
            mode.apply_binding("app.tree.filter.userOnly"),
            Some(TreeFilterMode::UserOnly)
        );
        assert_eq!(
            mode.apply_binding("app.tree.filter.labeledOnly"),
            Some(TreeFilterMode::LabeledOnly)
        );
        assert_eq!(mode.apply_binding("app.exit"), None);
    }

    fn with_session_delete_bindings<R>(f: impl FnOnce() -> R) -> R {
        crate::core::keybindings::with_global_app_keybindings(f)
    }

    #[test]
    fn session_selector_arms_confirms_and_escapes_delete() {
        with_session_delete_bindings(|| {
            let entries = vec![
                SessionPickerEntry {
                    value: "/tmp/active.jsonl".to_owned(),
                    label: "active".to_owned(),
                    description: None,
                },
                SessionPickerEntry {
                    value: "/tmp/other.jsonl".to_owned(),
                    label: "other".to_owned(),
                    description: None,
                },
            ];
            let mut selector =
                build_session_selector_component(&entries, 1, Some("/tmp/active.jsonl".to_owned()));
            let deleted = std::sync::Arc::new(std::sync::Mutex::new(None));
            let deleted2 = std::sync::Arc::clone(&deleted);
            selector.on_delete = Some(Box::new(move |path| {
                *deleted2
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
            }));
            let ctrl_d = UiEvent::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('d'),
                crossterm::event::KeyModifiers::CONTROL,
            ));
            assert_eq!(selector.handle_event(&ctrl_d), EventResult::Render);
            assert!(selector.confirm_state().is_armed());
            let esc = UiEvent::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ));
            assert_eq!(selector.handle_event(&esc), EventResult::Render);
            assert!(!selector.confirm_state().is_armed());
            assert_eq!(selector.handle_event(&ctrl_d), EventResult::Render);
            let enter = UiEvent::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ));
            assert_eq!(selector.handle_event(&enter), EventResult::Consumed);
            assert!(!selector.confirm_state().is_armed());
            assert_eq!(
                deleted
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                    .as_deref(),
                Some("/tmp/other.jsonl")
            );
        });
    }

    #[test]
    fn session_selector_blocks_active_session_delete() {
        with_session_delete_bindings(|| {
            let entries = vec![SessionPickerEntry {
                value: "/tmp/active.jsonl".to_owned(),
                label: "active".to_owned(),
                description: None,
            }];
            let mut selector =
                build_session_selector_component(&entries, 0, Some("/tmp/active.jsonl".to_owned()));
            let errors = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let errors2 = std::sync::Arc::clone(&errors);
            selector.on_error = Some(Box::new(move |msg| {
                errors2
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(msg);
            }));
            let ctrl_d = UiEvent::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('d'),
                crossterm::event::KeyModifiers::CONTROL,
            ));
            assert_eq!(selector.handle_event(&ctrl_d), EventResult::Render);
            assert!(!selector.confirm_state().is_armed());
            assert_eq!(
                errors
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_slice(),
                ["Cannot delete the currently active session"]
            );
        });
    }

    #[test]
    fn session_selector_ctrl_backspace_arms_delete() {
        with_session_delete_bindings(|| {
            let entries = vec![SessionPickerEntry {
                value: "/tmp/other.jsonl".to_owned(),
                label: "other".to_owned(),
                description: None,
            }];
            let mut selector = build_session_selector_component(&entries, 0, None);
            let backspace = UiEvent::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Backspace,
                crossterm::event::KeyModifiers::CONTROL,
            ));
            assert_eq!(selector.handle_event(&backspace), EventResult::Render);
            assert!(
                matches!(selector.confirm_state(), SessionDeleteConfirm::Armed { path } if path == "/tmp/other.jsonl")
            );
            let esc = UiEvent::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ));
            assert_eq!(selector.handle_event(&esc), EventResult::Render);
            assert!(!selector.confirm_state().is_armed());
        });
    }

    #[test]
    fn session_selector_second_esc_closes_after_confirm_cancel() {
        with_session_delete_bindings(|| {
            let entries = vec![SessionPickerEntry {
                value: "/tmp/other.jsonl".to_owned(),
                label: "other".to_owned(),
                description: None,
            }];
            let mut selector = build_session_selector_component(&entries, 0, None);
            let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let cancelled2 = std::sync::Arc::clone(&cancelled);
            selector.on_cancel = Some(Box::new(move || {
                cancelled2.store(true, std::sync::atomic::Ordering::SeqCst);
            }));
            let ctrl_d = UiEvent::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('d'),
                crossterm::event::KeyModifiers::CONTROL,
            ));
            let esc = UiEvent::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ));
            assert_eq!(selector.handle_event(&ctrl_d), EventResult::Render);
            assert_eq!(selector.handle_event(&esc), EventResult::Render);
            assert!(!selector.confirm_state().is_armed());
            assert!(!cancelled.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(selector.handle_event(&esc), EventResult::Consumed);
            assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));
        });
    }

    #[test]
    #[expect(
        clippy::expect_used,
        reason = "test setup: tmp dir, file write, and symlink are irrecoverable preconditions"
    )]
    fn same_session_path_matches_symlink_and_falls_back() {
        let tmp = tempfile::tempdir().expect("tmp");
        let real = tmp.path().join("real.jsonl");
        std::fs::write(&real, b"{}").expect("write");
        let link = tmp.path().join("link.jsonl");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let real_s = real.to_string_lossy();
        let link_s = link.to_string_lossy();
        assert!(same_session_path(&real_s, &real_s));
        assert!(same_session_path(&real_s, &link_s));
        assert!(!same_session_path(
            &real_s,
            "/tmp/definitely-missing-g7.jsonl"
        ));
        assert!(!same_session_path("a", "b"));
    }
}
