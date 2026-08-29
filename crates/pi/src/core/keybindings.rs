//! App-level keybinding defaults and `keybindings.json` loading.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/keybindings.ts`
//! `KEYBINDINGS` (app.* ids + defaults) on top of the pi-tui
//! [`KeybindingsManager`](pi_tui::keybindings::KeybindingsManager). Legacy
//! name renames live in [`super::migrations`]; this module only loads the
//! already-migrated on-disk config.

use std::path::{Path, PathBuf};

use pi_tui::keybindings::{
    KeybindingDefinition, KeybindingDefinitions, KeybindingsConfig, KeybindingsManager,
    tui_keybindings,
};
use pi_tui::keys::KeyId;
use serde_json::Value;

use super::config::get_agent_dir;

/// File name under the agent dir (`keybindings.json`).
pub const KEYBINDINGS_FILE_NAME: &str = "keybindings.json";

/// One static app binding row (mirrors the TS `KEYBINDINGS` app.* entries).
struct AppDefault {
    id: &'static str,
    keys: &'static [&'static str],
    description: &'static str,
}

/// Platform-aware default keys for app.* bindings.
///
/// Selector-local bindings (session rename/delete, tree filters, models.*) are
/// included so `getKeys` / rebinds work when those surfaces resolve through the
/// shared manager; the interactive editor mapper only dispatches the subset
/// registered on the default editor in upstream `interactive-mode.ts`.
// Line count is the data table itself; splitting it hides the single
// authoritative list of app.* defaults.
#[allow(clippy::too_many_lines)]
fn app_default_rows() -> Vec<AppDefault> {
    let suspend_keys: &[&str] = if cfg!(windows) { &[] } else { &["ctrl+z"] };
    let paste_image_keys: &[&str] = if cfg!(windows) {
        &["alt+v"]
    } else {
        &["ctrl+v"]
    };
    let tree_fold_keys: &[&str] = if cfg!(target_os = "macos") {
        &["alt+left", "ctrl+left"]
    } else {
        &["ctrl+left", "alt+left"]
    };
    let tree_unfold_keys: &[&str] = if cfg!(target_os = "macos") {
        &["alt+right", "ctrl+right"]
    } else {
        &["ctrl+right", "alt+right"]
    };

    vec![
        AppDefault {
            id: "app.interrupt",
            keys: &["escape"],
            description: "Cancel or abort",
        },
        AppDefault {
            id: "app.clear",
            keys: &["ctrl+c"],
            description: "Clear editor",
        },
        AppDefault {
            id: "app.exit",
            keys: &["ctrl+d"],
            description: "Exit when editor is empty",
        },
        AppDefault {
            id: "app.suspend",
            keys: suspend_keys,
            description: "Suspend to background",
        },
        AppDefault {
            id: "app.thinking.cycle",
            keys: &["shift+tab"],
            description: "Cycle thinking level",
        },
        AppDefault {
            id: "app.model.cycleForward",
            keys: &["ctrl+p"],
            description: "Cycle to next model",
        },
        AppDefault {
            id: "app.model.cycleBackward",
            keys: &["shift+ctrl+p"],
            description: "Cycle to previous model",
        },
        AppDefault {
            id: "app.model.select",
            keys: &["ctrl+l"],
            description: "Open model selector",
        },
        AppDefault {
            id: "app.tools.expand",
            keys: &["ctrl+o"],
            description: "Toggle tool output",
        },
        AppDefault {
            id: "app.thinking.toggle",
            keys: &["ctrl+t"],
            description: "Toggle thinking blocks",
        },
        AppDefault {
            id: "app.session.toggleNamedFilter",
            keys: &["ctrl+n"],
            description: "Toggle named session filter",
        },
        AppDefault {
            id: "app.editor.external",
            keys: &["ctrl+g"],
            description: "Open external editor",
        },
        AppDefault {
            id: "app.message.copy",
            keys: &["ctrl+x"],
            description: "Copy message to clipboard",
        },
        AppDefault {
            id: "app.message.followUp",
            keys: &["alt+enter"],
            description: "Queue follow-up message",
        },
        AppDefault {
            id: "app.message.dequeue",
            keys: &["alt+up"],
            description: "Restore queued messages",
        },
        AppDefault {
            id: "app.clipboard.pasteImage",
            keys: paste_image_keys,
            description: "Paste image from clipboard (text fallback)",
        },
        // Unbound by default — slash/UI only until the user rebinds.
        AppDefault {
            id: "app.session.new",
            keys: &[],
            description: "Start a new session",
        },
        AppDefault {
            id: "app.session.tree",
            keys: &[],
            description: "Open session tree",
        },
        AppDefault {
            id: "app.session.fork",
            keys: &[],
            description: "Fork current session",
        },
        AppDefault {
            id: "app.session.resume",
            keys: &[],
            description: "Resume a session",
        },
        AppDefault {
            id: "app.tree.foldOrUp",
            keys: tree_fold_keys,
            description: "Fold tree branch or move up",
        },
        AppDefault {
            id: "app.tree.unfoldOrDown",
            keys: tree_unfold_keys,
            description: "Unfold tree branch or move down",
        },
        AppDefault {
            id: "app.tree.editLabel",
            keys: &["shift+l"],
            description: "Edit tree label",
        },
        AppDefault {
            id: "app.tree.toggleLabelTimestamp",
            keys: &["shift+t"],
            description: "Toggle tree label timestamps",
        },
        AppDefault {
            id: "app.session.togglePath",
            keys: &["ctrl+p"],
            description: "Toggle session path display",
        },
        AppDefault {
            id: "app.session.toggleSort",
            keys: &["ctrl+s"],
            description: "Toggle session sort mode",
        },
        AppDefault {
            id: "app.session.rename",
            keys: &["ctrl+r"],
            description: "Rename session",
        },
        AppDefault {
            id: "app.session.delete",
            keys: &["ctrl+d"],
            description: "Delete session",
        },
        AppDefault {
            id: "app.session.deleteNoninvasive",
            keys: &["ctrl+backspace"],
            description: "Delete session when query is empty",
        },
        AppDefault {
            id: "app.models.save",
            keys: &["ctrl+s"],
            description: "Save model selection",
        },
        AppDefault {
            id: "app.models.enableAll",
            keys: &["ctrl+a"],
            description: "Enable all models",
        },
        AppDefault {
            id: "app.models.clearAll",
            keys: &["ctrl+x"],
            description: "Clear all models",
        },
        AppDefault {
            id: "app.models.toggleProvider",
            keys: &["ctrl+p"],
            description: "Toggle all models for provider",
        },
        AppDefault {
            id: "app.models.reorderUp",
            keys: &["alt+up"],
            description: "Move model up in order",
        },
        AppDefault {
            id: "app.models.reorderDown",
            keys: &["alt+down"],
            description: "Move model down in order",
        },
        AppDefault {
            id: "app.tree.filter.default",
            keys: &["ctrl+d"],
            description: "Tree filter: default view",
        },
        AppDefault {
            id: "app.tree.filter.noTools",
            keys: &["ctrl+t"],
            description: "Tree filter: hide tool results",
        },
        AppDefault {
            id: "app.tree.filter.userOnly",
            keys: &["ctrl+u"],
            description: "Tree filter: user messages only",
        },
        AppDefault {
            id: "app.tree.filter.labeledOnly",
            keys: &["ctrl+l"],
            description: "Tree filter: labeled entries only",
        },
        AppDefault {
            id: "app.tree.filter.all",
            keys: &["ctrl+a"],
            description: "Tree filter: show all entries",
        },
        AppDefault {
            id: "app.tree.filter.cycleForward",
            keys: &["ctrl+o"],
            description: "Tree filter: cycle forward",
        },
        AppDefault {
            id: "app.tree.filter.cycleBackward",
            keys: &["shift+ctrl+o"],
            description: "Tree filter: cycle backward",
        },
    ]
}

/// Combined TUI + app keybinding definitions (TS `KEYBINDINGS`).
#[must_use]
pub fn app_keybindings() -> KeybindingDefinitions {
    let mut defs = tui_keybindings();
    for row in app_default_rows() {
        defs.insert(
            row.id,
            KeybindingDefinition {
                default_keys: row.keys.iter().copied().map(KeyId::from).collect(),
                description: Some(row.description),
            },
        );
    }
    defs
}

/// Manager with shipped defaults and no user overrides.
#[must_use]
pub fn app_keybindings_defaults() -> KeybindingsManager {
    KeybindingsManager::new(app_keybindings(), KeybindingsConfig::new())
}

/// Path to `{agent_dir}/keybindings.json`.
#[must_use]
pub fn keybindings_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(KEYBINDINGS_FILE_NAME)
}

/// Load user overrides from `keybindings.json` (missing/malformed → empty).
///
/// Accepts a string or string-array per action id, matching
/// `toKeybindingsConfig` in the TypeScript port. Unknown ids are kept so a
/// later definition extension can still honor them; the manager ignores ids
/// absent from its definitions when resolving matches.
#[must_use]
pub fn load_user_keybindings(path: &Path) -> KeybindingsConfig {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return KeybindingsConfig::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return KeybindingsConfig::new();
    };
    let Some(obj) = value.as_object() else {
        return KeybindingsConfig::new();
    };
    let mut config = KeybindingsConfig::new();
    for (id, binding) in obj {
        if let Some(keys) = binding_to_keys(binding) {
            config.insert(id.clone(), keys);
        }
    }
    config
}

fn binding_to_keys(binding: &Value) -> Option<Vec<KeyId>> {
    match binding {
        Value::String(s) => Some(vec![KeyId::from(s.as_str())]),
        Value::Array(items) => {
            let mut keys = Vec::with_capacity(items.len());
            for item in items {
                let Value::String(s) = item else {
                    return None;
                };
                keys.push(KeyId::from(s.as_str()));
            }
            Some(keys)
        }
        _ => None,
    }
}

/// Manager from an agent dir: defaults + `{agent}/keybindings.json` overlay.
#[must_use]
pub fn load_app_keybindings(agent_dir: &Path) -> KeybindingsManager {
    let user = load_user_keybindings(&keybindings_path(agent_dir));
    KeybindingsManager::new(app_keybindings(), user)
}

/// Manager for the process-selected agent directory.
#[must_use]
pub fn create_app_keybindings() -> KeybindingsManager {
    load_app_keybindings(&get_agent_dir())
}

/// Install app defaults (+ optional user file) as the process-global manager.
///
/// Call at interactive startup and on `/reload` so TUI components that read
/// `get_keybindings()` see the same table as [`super::super::modes::interactive::input::InputMapper`].
#[must_use]
pub fn install_app_keybindings(agent_dir: &Path) -> KeybindingsManager {
    let manager = load_app_keybindings(agent_dir);
    pi_tui::keybindings::set_keybindings(manager.clone());
    manager
}

/// Re-read `{agent}/keybindings.json` into the process-global manager.
#[must_use]
pub fn reload_app_keybindings(agent_dir: &Path) -> KeybindingsManager {
    install_app_keybindings(agent_dir)
}

/// Serialize process-global keybinding installs for interactive tests.
///
/// Always leaves [`app_keybindings_defaults`] installed afterward so sibling
/// tests keep `app.session.delete` / `app.exit` / tree-filter chords.
#[cfg(test)]
pub(crate) struct GlobalAppKeybindingsGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for GlobalAppKeybindingsGuard {
    fn drop(&mut self) {
        pi_tui::keybindings::set_keybindings(app_keybindings_defaults());
    }
}

#[cfg(test)]
pub(crate) fn lock_global_app_keybindings() -> GlobalAppKeybindingsGuard {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pi_tui::keybindings::set_keybindings(app_keybindings_defaults());
    GlobalAppKeybindingsGuard { _guard: guard }
}

#[cfg(test)]
pub(crate) fn with_global_app_keybindings<R>(f: impl FnOnce() -> R) -> R {
    let _guard = lock_global_app_keybindings();
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_tui::keys::KeyId;
    use tempfile::TempDir;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn app_defaults_include_named_filter_and_empty_session_new() {
        let mgr = app_keybindings_defaults();
        assert_eq!(
            mgr.get_keys("app.session.toggleNamedFilter")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["ctrl+n"]
        );
        assert!(mgr.get_keys("app.session.new").is_empty());
        assert!(mgr.get_keys("app.session.tree").is_empty());
        assert!(mgr.get_keys("app.session.fork").is_empty());
        assert!(mgr.get_keys("app.session.resume").is_empty());
        // No Rust-only global chords in the app table.
        assert!(mgr.get_definition("app.reload").is_none());
    }

    #[test]
    fn app_defaults_include_session_delete_and_tree_filter_chords() {
        let mgr = app_keybindings_defaults();
        assert_eq!(
            mgr.get_keys("app.session.delete")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["ctrl+d"]
        );
        assert_eq!(
            mgr.get_keys("app.session.deleteNoninvasive")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["ctrl+backspace"]
        );
        assert_eq!(
            mgr.get_keys("app.tree.filter.default")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["ctrl+d"]
        );
        assert_eq!(
            mgr.get_keys("app.tree.filter.noTools")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["ctrl+t"]
        );
        assert_eq!(
            mgr.get_keys("app.tree.filter.userOnly")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["ctrl+u"]
        );
        assert_eq!(
            mgr.get_keys("app.tree.filter.labeledOnly")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["ctrl+l"]
        );
    }

    #[test]
    fn user_file_rebinds_and_unbinds() -> TestResult {
        let temp = TempDir::new()?;
        std::fs::write(
            temp.path().join(KEYBINDINGS_FILE_NAME),
            r#"{
                "app.model.select": "ctrl+m",
                "app.tools.expand": []
            }"#,
        )?;
        let mgr = load_app_keybindings(temp.path());
        assert_eq!(
            mgr.get_keys("app.model.select")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["ctrl+m"]
        );
        assert!(mgr.get_keys("app.tools.expand").is_empty());
        // Unmentioned keeps default.
        assert_eq!(
            mgr.get_keys("app.message.copy")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["ctrl+x"]
        );
        Ok(())
    }

    #[test]
    fn missing_or_malformed_file_is_empty_overlay() -> TestResult {
        let temp = TempDir::new()?;
        let mgr = load_app_keybindings(temp.path());
        assert_eq!(
            mgr.get_keys("app.model.select")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["ctrl+l"]
        );
        std::fs::write(temp.path().join(KEYBINDINGS_FILE_NAME), b"[")?;
        let mgr = load_app_keybindings(temp.path());
        assert_eq!(
            mgr.get_keys("app.model.select")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["ctrl+l"]
        );
        Ok(())
    }
}
