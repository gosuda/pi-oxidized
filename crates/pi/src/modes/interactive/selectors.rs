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
    AuthSelectorEntry, ConfigSelectorEntry, ModelSelectorEntry, SelectorKind, SessionPickerEntry,
    SettingsRow, TreeEntry,
};
use super::theme;

/// Maximum visible rows for any selector (ports reference default).
pub const SELECTOR_MAX_VISIBLE: usize = 12;

/// Persistent exit hint appended to every select-list selector.
pub const SELECTOR_EXIT_HINT: &str = "  esc to cancel";

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
        SelectorKind::Auth,
        SelectorKind::Settings,
        SelectorKind::Config,
        SelectorKind::ScopedModels,
        SelectorKind::Theme,
        SelectorKind::ImportConfirm,
        SelectorKind::ImportCwdConfirm,
        SelectorKind::Logout,
    ];

    #[derive(Clone, Copy)]
    enum HelperBoundary {
        /// Runtime/builders route through [`apply_select_list_copy`].
        Select,
        /// Runtime/builders route through [`apply_settings_list_copy`].
        Settings,
    }

    /// Exact expected copy + which helper boundary runtime/builders use.
    fn expected_mapping(kind: SelectorKind) -> (SelectorEmptyCopy, HelperBoundary) {
        match kind {
            SelectorKind::Model | SelectorKind::ScopedModels => (
                SelectorEmptyCopy {
                    empty: "  No matching models",
                    no_match: "  No matching models",
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

    /// Exhaustive guard: every [`SelectorKind`] maps, and the helper boundary
    /// used by runtime/builders renders that copy (builders where they exist).
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
                    // Prefer dedicated builders when they exist (same helper path).
                    let mut comp: Box<dyn Component> = match kind {
                        SelectorKind::Model => build_model_selector(&[], 0),
                        SelectorKind::Session => build_session_picker(&[], 0),
                        SelectorKind::Auth => build_auth_selector(&[], 0),
                        SelectorKind::ScopedModels => {
                            build_scoped_models_selector(&[], &BTreeMap::new(), 0)
                        }
                        SelectorKind::Tree => build_tree_selector(&[], 0),
                        // Runtime Theme / Fork / Import* / Logout: apply_select_list_copy.
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
                        // Runtime Trust: apply_settings_list_copy.
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
                "{kind:?} empty copy missing in helper/builder render:\n{plain}"
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

        // Extension lists have no SelectorKind; runtime uses EXTENSION_EMPTY_COPY.
        let mut extension = apply_select_list_copy(
            SelectList::new(vec![], SELECTOR_MAX_VISIBLE, theme::select_list_theme()),
            EXTENSION_EMPTY_COPY,
        );
        let plain = plain_empty(&mut extension);
        assert!(
            plain.contains(EXTENSION_EMPTY_COPY.empty.trim()),
            "extension empty copy missing:\n{plain}"
        );
        assert!(
            plain.contains(SELECTOR_EXIT_HINT.trim()),
            "extension exit hint missing:\n{plain}"
        );
        assert!(
            !plain.contains("No matching commands"),
            "extension leaked generic fallback:\n{plain}"
        );
    }
}
