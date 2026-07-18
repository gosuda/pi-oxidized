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

use pi_tui::component::Component;
use pi_tui::components::{SelectItem, SelectList, SettingItem, SettingsList, SettingsListOptions};

use super::state::{
    AuthSelectorEntry, ConfigSelectorEntry, ModelSelectorEntry, SessionPickerEntry, SettingsRow,
    TreeEntry,
};
use super::theme;

/// Maximum visible rows for any selector (ports reference default).
pub const SELECTOR_MAX_VISIBLE: usize = 12;

// ---------------------------------------------------------------------------
// Select-list selectors (model / session / tree / auth / scoped)
// ---------------------------------------------------------------------------

/// Build the model selector. Reads the thread-local current theme.
#[must_use]
pub fn build_model_selector(entries: &[ModelSelectorEntry], selected: usize) -> Box<dyn Component> {
    let items = entries.iter().map(model_item).collect::<Vec<_>>();
    let mut list = SelectList::new(items, SELECTOR_MAX_VISIBLE, theme::select_list_theme());
    list.set_selected_index(selected);
    Box::new(list)
}

/// Build the session picker.
#[must_use]
pub fn build_session_picker(entries: &[SessionPickerEntry], selected: usize) -> Box<dyn Component> {
    let items = entries.iter().map(session_item).collect::<Vec<_>>();
    let mut list = SelectList::new(items, SELECTOR_MAX_VISIBLE, theme::select_list_theme());
    list.set_selected_index(selected);
    Box::new(list)
}

/// Build the auth/login selector.
#[must_use]
pub fn build_auth_selector(entries: &[AuthSelectorEntry], selected: usize) -> Box<dyn Component> {
    let items = entries.iter().map(auth_item).collect::<Vec<_>>();
    let mut list = SelectList::new(items, SELECTOR_MAX_VISIBLE, theme::select_list_theme());
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
    let mut list = SelectList::new(items, SELECTOR_MAX_VISIBLE, theme::select_list_theme());
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
    let mut list = SelectList::new(items, SELECTOR_MAX_VISIBLE, theme::select_list_theme());
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
// Settings-list selectors (settings / config)
// ---------------------------------------------------------------------------

/// Build the settings selector (cycleable settings rows).
#[must_use]
pub fn build_settings_selector(rows: &[SettingsRow], selected: usize) -> Box<dyn Component> {
    let _ = selected;
    let items = rows.iter().map(setting_item).collect::<Vec<_>>();
    let options = SettingsListOptions::default();
    let list = SettingsList::new(
        items,
        SELECTOR_MAX_VISIBLE,
        theme::settings_list_theme(),
        |_id, _value| {},
        || {},
        &options,
    );
    Box::new(list)
}

/// Build the config selector (reuses the settings list).
#[must_use]
pub fn build_config_selector(
    entries: &[ConfigSelectorEntry],
    selected: usize,
) -> Box<dyn Component> {
    build_settings_selector(entries, selected)
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
