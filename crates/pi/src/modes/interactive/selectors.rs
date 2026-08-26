//! Selector view-models (model / session / tree / settings / config / auth / scoped).
//!
//! Ports the `*SelectorComponent` family from
//! `.references/pi/packages/coding-agent/src/modes/interactive/components/`.
//! Each builder produces a pi-tui `SelectList` or `SettingsList` configured
//! against the thread-local current theme (set by [`super::view::compose`] via
//! [`super::theme::with_theme`]). Selectors *replace* the editor inline (not
//! overlays) in the reference; here they are plain components the composer
//! splices into the editor slot.

use std::collections::BTreeMap;

use pi_tui::component::{Component, EventResult, UiEvent};
use pi_tui::components::{SelectItem, SelectList, SettingItem, SettingsList, SettingsListOptions};
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
        SelectorKind::Theme => both("  No matching themes"),
        SelectorKind::Session => both("  No sessions found"),
        SelectorKind::Tree => both("  No entries found"),
        SelectorKind::Fork => both("  No user messages found"),
        SelectorKind::Auth => SelectorEmptyCopy {
            empty: "  No providers available",
            no_match: "  No matching providers",
        },
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
// Select-list selectors (model / session / tree / auth / scoped)
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
    /// Live search/filter query. `app.session.deleteNoninvasive` arms delete
    /// only while this is empty; nonempty queries forward the chord unchanged.
    search_query: String,
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
            search_query: String::new(),
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

    /// Current search/filter query (tests / diagnostics).
    #[must_use]
    pub fn search_query(&self) -> &str {
        &self.search_query
    }

    /// Replace the live search query and refilter the list.
    pub fn set_search_query(&mut self, query: impl Into<String>) {
        self.search_query = query.into();
        self.list.set_filter(&self.search_query);
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
        // Ctrl+Backspace is a nonempty-search-safe alias: arm only when the
        // search query is empty; otherwise forward unchanged to SelectList.
        if kb.matches(key, "app.session.deleteNoninvasive") {
            if !self.search_query.is_empty() {
                return self.list.handle_event(&UiEvent::Key(*key));
            }
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
        SelectorKind::Session,
        SelectorKind::Tree,
        SelectorKind::Fork,
        SelectorKind::Trust,
        SelectorKind::Theme,
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

    #[test]
    fn selector_kind_mapping_is_exhaustive_at_helper_boundary() {
        assert_eq!(
            ALL_KINDS.len(),
            13,
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
    fn session_selector_ctrl_backspace_arms_only_when_search_empty() {
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
            assert!(selector.search_query().is_empty());
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
            selector.set_search_query("oth");
            assert!(!selector.search_query().is_empty());
            let forwarded = selector.handle_event(&backspace);
            assert!(
                matches!(forwarded, EventResult::Ignored | EventResult::Render),
                "got {forwarded:?}"
            );
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
