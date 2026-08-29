//! Keybinding definitions, defaults, and conflict detection.
//!
//! Ports `.references/pi/packages/tui/src/keybindings.ts` including the exact
//! 31 `TUI_KEYBINDINGS` defaults (lines 54–134) and `KeybindingsManager`
//! user-claim conflict detection.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{LazyLock, Mutex};

use crossterm::event::KeyEvent;

use crate::keys::{KeyId, key_matches};

/// Named keybinding action id (`"tui.editor.cursorUp"`, …).
pub type KeybindingId = String;

/// One action's default keys and optional description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingDefinition {
    /// Default key identifier(s).
    pub default_keys: Vec<KeyId>,
    /// Human-readable description.
    pub description: Option<&'static str>,
}

/// Map of action id → definition.
pub type KeybindingDefinitions = BTreeMap<&'static str, KeybindingDefinition>;

/// User override map: action id → keys (or empty to unbind).
///
/// A missing entry means “use defaults”. An explicit empty list unbinds.
pub type KeybindingsConfig = BTreeMap<String, Vec<KeyId>>;

/// Two or more user bindings claim the same physical key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
    /// The contested key.
    pub key: KeyId,
    /// Action ids that claim it (stable sorted order).
    pub keybindings: Vec<String>,
}

#[derive(Clone, Copy)]
struct DefaultBinding {
    id: &'static str,
    keys: &'static [&'static str],
    description: &'static str,
}

const DEFAULT_BINDINGS: [DefaultBinding; TUI_KEYBINDING_COUNT] = [
    DefaultBinding {
        id: "tui.editor.cursorUp",
        keys: &["up"],
        description: "Move cursor up",
    },
    DefaultBinding {
        id: "tui.editor.cursorDown",
        keys: &["down"],
        description: "Move cursor down",
    },
    DefaultBinding {
        id: "tui.editor.cursorLeft",
        keys: &["left", "ctrl+b"],
        description: "Move cursor left",
    },
    DefaultBinding {
        id: "tui.editor.cursorRight",
        keys: &["right", "ctrl+f"],
        description: "Move cursor right",
    },
    DefaultBinding {
        id: "tui.editor.cursorWordLeft",
        keys: &["alt+left", "ctrl+left", "alt+b"],
        description: "Move cursor word left",
    },
    DefaultBinding {
        id: "tui.editor.cursorWordRight",
        keys: &["alt+right", "ctrl+right", "alt+f"],
        description: "Move cursor word right",
    },
    DefaultBinding {
        id: "tui.editor.cursorLineStart",
        keys: &["home", "ctrl+a"],
        description: "Move to line start",
    },
    DefaultBinding {
        id: "tui.editor.cursorLineEnd",
        keys: &["end", "ctrl+e"],
        description: "Move to line end",
    },
    DefaultBinding {
        id: "tui.editor.jumpForward",
        keys: &["ctrl+]"],
        description: "Jump forward to character",
    },
    DefaultBinding {
        id: "tui.editor.jumpBackward",
        keys: &["ctrl+alt+]"],
        description: "Jump backward to character",
    },
    DefaultBinding {
        id: "tui.editor.pageUp",
        keys: &["pageUp"],
        description: "Page up",
    },
    DefaultBinding {
        id: "tui.editor.pageDown",
        keys: &["pageDown"],
        description: "Page down",
    },
    DefaultBinding {
        id: "tui.editor.deleteCharBackward",
        keys: &["backspace"],
        description: "Delete character backward",
    },
    DefaultBinding {
        id: "tui.editor.deleteCharForward",
        keys: &["delete", "ctrl+d"],
        description: "Delete character forward",
    },
    DefaultBinding {
        id: "tui.editor.deleteWordBackward",
        keys: &["ctrl+w", "alt+backspace"],
        description: "Delete word backward",
    },
    DefaultBinding {
        id: "tui.editor.deleteWordForward",
        keys: &["alt+d", "alt+delete"],
        description: "Delete word forward",
    },
    DefaultBinding {
        id: "tui.editor.deleteToLineStart",
        keys: &["ctrl+u"],
        description: "Delete to line start",
    },
    DefaultBinding {
        id: "tui.editor.deleteToLineEnd",
        keys: &["ctrl+k"],
        description: "Delete to line end",
    },
    DefaultBinding {
        id: "tui.editor.yank",
        keys: &["ctrl+y"],
        description: "Yank",
    },
    DefaultBinding {
        id: "tui.editor.yankPop",
        keys: &["alt+y"],
        description: "Yank pop",
    },
    DefaultBinding {
        id: "tui.editor.undo",
        keys: &["ctrl+-"],
        description: "Undo",
    },
    DefaultBinding {
        id: "tui.input.newLine",
        keys: &["shift+enter", "ctrl+j"],
        description: "Insert newline",
    },
    DefaultBinding {
        id: "tui.input.submit",
        keys: &["enter"],
        description: "Submit input",
    },
    DefaultBinding {
        id: "tui.input.tab",
        keys: &["tab"],
        description: "Tab / autocomplete",
    },
    DefaultBinding {
        id: "tui.input.copy",
        keys: &["ctrl+c"],
        description: "Copy selection",
    },
    DefaultBinding {
        id: "tui.select.up",
        keys: &["up"],
        description: "Move selection up",
    },
    DefaultBinding {
        id: "tui.select.down",
        keys: &["down"],
        description: "Move selection down",
    },
    DefaultBinding {
        id: "tui.select.pageUp",
        keys: &["pageUp"],
        description: "Selection page up",
    },
    DefaultBinding {
        id: "tui.select.pageDown",
        keys: &["pageDown"],
        description: "Selection page down",
    },
    DefaultBinding {
        id: "tui.select.confirm",
        keys: &["enter"],
        description: "Confirm selection",
    },
    DefaultBinding {
        id: "tui.select.cancel",
        keys: &["escape", "ctrl+c"],
        description: "Cancel selection",
    },
];

/// Exact 31 default TUI keybindings from `keybindings.ts:54-134`.
#[must_use]
pub fn tui_keybindings() -> KeybindingDefinitions {
    DEFAULT_BINDINGS
        .iter()
        .map(|binding| {
            let definition = KeybindingDefinition {
                default_keys: binding.keys.iter().copied().map(KeyId::from).collect(),
                description: Some(binding.description),
            };
            (binding.id, definition)
        })
        .collect()
}

/// Number of entries in [`tui_keybindings`] / `TUI_KEYBINDINGS`.
pub const TUI_KEYBINDING_COUNT: usize = 31;

/// Stable ordered list of the 31 default action ids.
pub const TUI_KEYBINDING_IDS: [&str; TUI_KEYBINDING_COUNT] = [
    "tui.editor.cursorUp",
    "tui.editor.cursorDown",
    "tui.editor.cursorLeft",
    "tui.editor.cursorRight",
    "tui.editor.cursorWordLeft",
    "tui.editor.cursorWordRight",
    "tui.editor.cursorLineStart",
    "tui.editor.cursorLineEnd",
    "tui.editor.jumpForward",
    "tui.editor.jumpBackward",
    "tui.editor.pageUp",
    "tui.editor.pageDown",
    "tui.editor.deleteCharBackward",
    "tui.editor.deleteCharForward",
    "tui.editor.deleteWordBackward",
    "tui.editor.deleteWordForward",
    "tui.editor.deleteToLineStart",
    "tui.editor.deleteToLineEnd",
    "tui.editor.yank",
    "tui.editor.yankPop",
    "tui.editor.undo",
    "tui.input.newLine",
    "tui.input.submit",
    "tui.input.tab",
    "tui.input.copy",
    "tui.select.up",
    "tui.select.down",
    "tui.select.pageUp",
    "tui.select.pageDown",
    "tui.select.confirm",
    "tui.select.cancel",
];

fn normalize_keys(keys: impl IntoIterator<Item = KeyId>) -> Vec<KeyId> {
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for key in keys {
        if seen.insert(key.as_str().to_owned()) {
            result.push(key);
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Display formatting (ports `keybinding-hints.ts`)
// ---------------------------------------------------------------------------

/// Format one `+`-separated part of a key chord for display (TS
/// `formatKeyPart`): on macOS the `alt` modifier renders as `option`.
fn format_key_part(part: &str, capitalize: bool) -> String {
    let display = if cfg!(target_os = "macos") && part.eq_ignore_ascii_case("alt") {
        "option"
    } else {
        part
    };
    if !capitalize {
        return display.to_owned();
    }
    let mut chars = display.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Format a raw key string for display (TS `formatKeyText`).
///
/// `/` separates key alternatives and `+` separates modifier chord parts;
/// each part renders independently, optionally capitalized
/// (`ctrl+o` → `Ctrl+O`, `escape/ctrl+c` → `Escape/Ctrl+C`).
#[must_use]
pub fn format_key_text(key: &str, capitalize: bool) -> String {
    key.split('/')
        .map(|alternative| {
            alternative
                .split('+')
                .map(|part| format_key_part(part, capitalize))
                .collect::<Vec<_>>()
                .join("+")
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Render resolved keys through [`format_key_text`] (TS `formatKeys`); an
/// empty key list renders as an empty string.
fn format_keys(keys: &[KeyId], capitalize: bool) -> String {
    if keys.is_empty() {
        return String::new();
    }
    let joined = keys.iter().map(KeyId::as_str).collect::<Vec<_>>().join("/");
    format_key_text(&joined, capitalize)
}

/// Resolves keybinding ids to key lists and reports user-binding conflicts.
///
/// Conflicts are computed only among **user** bindings that share a `KeyId`.
/// Default bindings that overlap (for example `enter` on submit and select
/// confirm) are intentional and not reported.
#[derive(Debug, Clone)]
pub struct KeybindingsManager {
    definitions: KeybindingDefinitions,
    user_bindings: KeybindingsConfig,
    keys_by_id: HashMap<String, Vec<KeyId>>,
    conflicts: Vec<KeybindingConflict>,
}

impl KeybindingsManager {
    /// Create a manager from definitions and optional user overrides.
    #[must_use]
    pub fn new(definitions: KeybindingDefinitions, user_bindings: KeybindingsConfig) -> Self {
        let mut manager = Self {
            definitions,
            user_bindings,
            keys_by_id: HashMap::new(),
            conflicts: Vec::new(),
        };
        manager.rebuild();
        manager
    }

    /// Create a manager with the shipped TUI defaults and no user overrides.
    #[must_use]
    pub fn with_tui_defaults() -> Self {
        Self::new(tui_keybindings(), KeybindingsConfig::new())
    }

    fn rebuild(&mut self) {
        self.keys_by_id.clear();
        self.conflicts.clear();

        // User-claim conflicts only.
        let mut user_claims: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (keybinding, keys) in &self.user_bindings {
            if !self.definitions.contains_key(keybinding.as_str()) {
                continue;
            }
            for key in normalize_keys(keys.iter().cloned()) {
                user_claims
                    .entry(key.as_str().to_owned())
                    .or_default()
                    .insert(keybinding.clone());
            }
        }
        for (key, claimants) in user_claims {
            if claimants.len() > 1 {
                self.conflicts.push(KeybindingConflict {
                    key: KeyId::from_raw(key),
                    keybindings: claimants.into_iter().collect(),
                });
            }
        }

        for (id, definition) in &self.definitions {
            let keys = match self.user_bindings.get(*id) {
                None => normalize_keys(definition.default_keys.iter().cloned()),
                Some(user_keys) => normalize_keys(user_keys.iter().cloned()),
            };
            self.keys_by_id.insert((*id).to_owned(), keys);
        }
    }

    /// True when `event` matches any key bound to `keybinding`.
    #[must_use]
    pub fn matches(&self, event: &KeyEvent, keybinding: &str) -> bool {
        self.keys_by_id
            .get(keybinding)
            .into_iter()
            .flatten()
            .any(|key| key_matches(event, key))
    }

    /// Resolved keys for an action (defaults or user override).
    #[must_use]
    pub fn get_keys(&self, keybinding: &str) -> Vec<KeyId> {
        self.keys_by_id.get(keybinding).cloned().unwrap_or_default()
    }

    /// Uncapitalized display text for an action's keys (TS `keyText`).
    ///
    /// Key alternatives join with `/` (`escape/ctrl+c`); an unknown or
    /// unbound id renders as an empty string.
    #[must_use]
    pub fn key_text(&self, keybinding: &str) -> String {
        format_keys(&self.get_keys(keybinding), false)
    }

    /// Capitalized display text for an action's keys (TS `keyDisplayText`).
    ///
    /// Same as [`key_text`](Self::key_text) with every chord part capitalized
    /// (`ctrl+o` → `Ctrl+O`).
    #[must_use]
    pub fn key_display_text(&self, keybinding: &str) -> String {
        format_keys(&self.get_keys(keybinding), true)
    }

    /// Definition for an action, if known.
    #[must_use]
    pub fn get_definition(&self, keybinding: &str) -> Option<&KeybindingDefinition> {
        self.definitions.get(keybinding)
    }

    /// Current user-binding conflicts.
    #[must_use]
    pub fn get_conflicts(&self) -> &[KeybindingConflict] {
        &self.conflicts
    }

    /// Replace user overrides and rebuild.
    pub fn set_user_bindings(&mut self, user_bindings: KeybindingsConfig) {
        self.user_bindings = user_bindings;
        self.rebuild();
    }

    /// Clone of current user overrides.
    #[must_use]
    pub fn get_user_bindings(&self) -> KeybindingsConfig {
        self.user_bindings.clone()
    }

    /// Fully resolved map of action → keys (single key or list).
    #[must_use]
    pub fn get_resolved_bindings(&self) -> KeybindingsConfig {
        let mut resolved = KeybindingsConfig::new();
        for id in self.definitions.keys() {
            let keys = self.get_keys(id);
            resolved.insert((*id).to_owned(), keys);
        }
        resolved
    }

    /// All known definition ids in sorted order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.definitions.keys().copied()
    }
}

static GLOBAL_KEYBINDINGS: LazyLock<Mutex<KeybindingsManager>> =
    LazyLock::new(|| Mutex::new(KeybindingsManager::with_tui_defaults()));

/// Install a process-global keybindings manager (TS `setKeybindings`).
pub fn set_keybindings(manager: KeybindingsManager) {
    match GLOBAL_KEYBINDINGS.lock() {
        Ok(mut guard) => *guard = manager,
        Err(poisoned) => {
            *poisoned.into_inner() = manager;
        }
    }
}

/// Snapshot of the process-global keybindings manager (TS `getKeybindings`).
///
/// Returns a clone so callers do not hold the global lock across event
/// handling. Mutations should go through [`set_keybindings`] or
/// [`update_global_keybindings`].
#[must_use]
pub fn get_keybindings() -> KeybindingsManager {
    match GLOBAL_KEYBINDINGS.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Mutate the process-global manager under the lock.
pub fn update_global_keybindings(f: impl FnOnce(&mut KeybindingsManager)) {
    match GLOBAL_KEYBINDINGS.lock() {
        Ok(mut guard) => f(&mut guard),
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

/// Uncapitalized display text for the process-global binding of `keybinding`
/// (TS `keyText`).
#[must_use]
pub fn key_text(keybinding: &str) -> String {
    get_keybindings().key_text(keybinding)
}

/// Capitalized display text for the process-global binding of `keybinding`
/// (TS `keyDisplayText`).
#[must_use]
pub fn key_display_text(keybinding: &str) -> String {
    get_keybindings().key_display_text(keybinding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{Key, key_press};
    use crossterm::event::{KeyCode, KeyModifiers};
    const EXPECTED_DEFAULTS: [(&str, &[&str]); 31] = [
        ("tui.editor.cursorUp", &["up"]),
        ("tui.editor.cursorDown", &["down"]),
        ("tui.editor.cursorLeft", &["left", "ctrl+b"]),
        ("tui.editor.cursorRight", &["right", "ctrl+f"]),
        (
            "tui.editor.cursorWordLeft",
            &["alt+left", "ctrl+left", "alt+b"],
        ),
        (
            "tui.editor.cursorWordRight",
            &["alt+right", "ctrl+right", "alt+f"],
        ),
        ("tui.editor.cursorLineStart", &["home", "ctrl+a"]),
        ("tui.editor.cursorLineEnd", &["end", "ctrl+e"]),
        ("tui.editor.jumpForward", &["ctrl+]"]),
        ("tui.editor.jumpBackward", &["ctrl+alt+]"]),
        ("tui.editor.pageUp", &["pageUp"]),
        ("tui.editor.pageDown", &["pageDown"]),
        ("tui.editor.deleteCharBackward", &["backspace"]),
        ("tui.editor.deleteCharForward", &["delete", "ctrl+d"]),
        (
            "tui.editor.deleteWordBackward",
            &["ctrl+w", "alt+backspace"],
        ),
        ("tui.editor.deleteWordForward", &["alt+d", "alt+delete"]),
        ("tui.editor.deleteToLineStart", &["ctrl+u"]),
        ("tui.editor.deleteToLineEnd", &["ctrl+k"]),
        ("tui.editor.yank", &["ctrl+y"]),
        ("tui.editor.yankPop", &["alt+y"]),
        ("tui.editor.undo", &["ctrl+-"]),
        ("tui.input.newLine", &["shift+enter", "ctrl+j"]),
        ("tui.input.submit", &["enter"]),
        ("tui.input.tab", &["tab"]),
        ("tui.input.copy", &["ctrl+c"]),
        ("tui.select.up", &["up"]),
        ("tui.select.down", &["down"]),
        ("tui.select.pageUp", &["pageUp"]),
        ("tui.select.pageDown", &["pageDown"]),
        ("tui.select.confirm", &["enter"]),
        ("tui.select.cancel", &["escape", "ctrl+c"]),
    ];

    #[test]
    fn exact_31_ids_and_defaults() {
        let definitions = tui_keybindings();
        let manager = KeybindingsManager::with_tui_defaults();
        assert_eq!(definitions.len(), TUI_KEYBINDING_COUNT);
        assert_eq!(TUI_KEYBINDING_IDS, EXPECTED_DEFAULTS.map(|(id, _)| id));
        for (binding_id, expected_keys) in EXPECTED_DEFAULTS {
            let resolved = manager.get_keys(binding_id);
            let actual: Vec<&str> = resolved.iter().map(KeyId::as_str).collect();
            assert_eq!(actual, expected_keys, "defaults for {binding_id}");
        }
    }

    #[test]
    fn matches_ctrl_j_as_newline_alias() {
        let manager = KeybindingsManager::with_tui_defaults();
        let ctrl_j = key_press(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert!(manager.matches(&ctrl_j, "tui.input.newLine"));
        let shift_enter = key_press(KeyCode::Enter, KeyModifiers::SHIFT);
        assert!(manager.matches(&shift_enter, "tui.input.newLine"));
        let enter = key_press(KeyCode::Enter, KeyModifiers::empty());
        assert!(manager.matches(&enter, "tui.input.submit"));
        assert!(!manager.matches(&enter, "tui.input.newLine"));
    }

    #[test]
    fn rebind_does_not_evict_other_defaults() {
        let mut user = KeybindingsConfig::new();
        user.insert(
            "tui.input.submit".to_owned(),
            vec![KeyId::from_raw("enter"), KeyId::from_raw("ctrl+enter")],
        );
        let manager = KeybindingsManager::new(tui_keybindings(), user);
        assert_eq!(
            manager
                .get_keys("tui.input.submit")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["enter", "ctrl+enter"]
        );
        assert_eq!(
            manager
                .get_keys("tui.select.confirm")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["enter"]
        );
    }

    #[test]
    fn reuse_key_on_select_does_not_evict_editor() {
        let mut user = KeybindingsConfig::new();
        user.insert(
            "tui.select.up".to_owned(),
            vec![KeyId::from_raw("up"), Key::ctrl("p")],
        );
        let manager = KeybindingsManager::new(tui_keybindings(), user);
        assert_eq!(
            manager
                .get_keys("tui.select.up")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["up", "ctrl+p"]
        );
        assert_eq!(
            manager
                .get_keys("tui.editor.cursorUp")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["up"]
        );
        // Defaults overlapping is fine — no conflict.
        assert!(manager.get_conflicts().is_empty());
    }

    #[test]
    fn reports_direct_user_binding_conflicts() {
        let mut user = KeybindingsConfig::new();
        user.insert(
            "tui.input.submit".to_owned(),
            vec![KeyId::from_raw("ctrl+x")],
        );
        user.insert(
            "tui.select.confirm".to_owned(),
            vec![KeyId::from_raw("ctrl+x")],
        );
        let manager = KeybindingsManager::new(tui_keybindings(), user);
        let conflicts = manager.get_conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].key.as_str(), "ctrl+x");
        assert_eq!(
            conflicts[0].keybindings,
            vec![
                "tui.input.submit".to_owned(),
                "tui.select.confirm".to_owned()
            ]
        );
        // Defaults for unrelated actions still present.
        assert_eq!(
            manager
                .get_keys("tui.editor.cursorLeft")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["left", "ctrl+b"]
        );
    }

    #[test]
    fn empty_user_list_unbinds() {
        let mut user = KeybindingsConfig::new();
        user.insert("tui.input.copy".to_owned(), vec![]);
        let manager = KeybindingsManager::new(tui_keybindings(), user);
        assert!(manager.get_keys("tui.input.copy").is_empty());
        let ctrl_c = key_press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!manager.matches(&ctrl_c, "tui.input.copy"));
        // select.cancel still has ctrl+c by default.
        assert!(manager.matches(&ctrl_c, "tui.select.cancel"));
    }

    #[test]
    fn format_key_text_capitalizes_each_chord_part() {
        assert_eq!(format_key_text("ctrl+o", true), "Ctrl+O");
        assert_eq!(format_key_text("shift+tab", true), "Shift+Tab");
        assert_eq!(format_key_text("escape", true), "Escape");
        assert_eq!(format_key_text("escape/ctrl+c", true), "Escape/Ctrl+C");
        assert_eq!(format_key_text("escape/ctrl+c", false), "escape/ctrl+c");
        assert_eq!(format_key_text("", true), "");
        let alt_enter = format_key_text("alt+enter", true);
        if cfg!(target_os = "macos") {
            assert_eq!(alt_enter, "Option+Enter");
        } else {
            assert_eq!(alt_enter, "Alt+Enter");
        }
    }

    #[test]
    fn manager_key_text_joins_resolved_alternatives() {
        let manager = KeybindingsManager::with_tui_defaults();
        assert_eq!(manager.key_text("tui.select.cancel"), "escape/ctrl+c");
        assert_eq!(
            manager.key_display_text("tui.select.cancel"),
            "Escape/Ctrl+C"
        );
        // Unknown ids resolve to no keys → blank display text.
        assert_eq!(manager.key_text("tui.input.missing"), "");
        assert_eq!(manager.key_display_text("tui.input.missing"), "");
    }

    #[test]
    fn key_display_text_follows_rebinds() {
        let mut user = KeybindingsConfig::new();
        user.insert("tui.input.copy".to_owned(), vec![KeyId::from_raw("ctrl+m")]);
        let manager = KeybindingsManager::new(tui_keybindings(), user);
        assert_eq!(manager.key_text("tui.input.copy"), "ctrl+m");
        assert_eq!(manager.key_display_text("tui.input.copy"), "Ctrl+M");
    }
}
