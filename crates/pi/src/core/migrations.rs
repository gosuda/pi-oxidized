//! Deterministic, best-effort startup migrations for legacy pi data.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use pi_ai::auth::{FileLockBackend, StoreError};
use serde_json::{Map, Value};

use super::config::{CONFIG_DIR_NAME, get_agent_dir, get_bin_dir_with};
use super::sessions::encode_cwd_for_session_dir;

/// Migration guide shown alongside deprecated extension-layout warnings.
pub const MIGRATION_GUIDE_URL: &str = "https://github.com/earendil-works/pi-mono/blob/main/packages/coding-agent/CHANGELOG.md#extensions-migration";
/// Extension documentation shown alongside deprecated extension-layout warnings.
pub const EXTENSIONS_DOC_URL: &str =
    "https://github.com/earendil-works/pi-mono/blob/main/packages/coding-agent/docs/extensions.md";

/// Result of running every startup migration.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MigrationResult {
    /// Provider identifiers whose credentials were durably written to `auth.json`.
    pub migrated_auth_providers: Vec<String>,
    /// Warnings for deprecated extension directories that still need user action.
    pub deprecation_warnings: Vec<String>,
}

/// Legacy-to-namespaced keybinding names from the compatibility target.
pub const KEYBINDING_NAME_MIGRATIONS: &[(&str, &str)] = &[
    ("cursorUp", "tui.editor.cursorUp"),
    ("cursorDown", "tui.editor.cursorDown"),
    ("cursorLeft", "tui.editor.cursorLeft"),
    ("cursorRight", "tui.editor.cursorRight"),
    ("cursorWordLeft", "tui.editor.cursorWordLeft"),
    ("cursorWordRight", "tui.editor.cursorWordRight"),
    ("cursorLineStart", "tui.editor.cursorLineStart"),
    ("cursorLineEnd", "tui.editor.cursorLineEnd"),
    ("jumpForward", "tui.editor.jumpForward"),
    ("jumpBackward", "tui.editor.jumpBackward"),
    ("pageUp", "tui.editor.pageUp"),
    ("pageDown", "tui.editor.pageDown"),
    ("deleteCharBackward", "tui.editor.deleteCharBackward"),
    ("deleteCharForward", "tui.editor.deleteCharForward"),
    ("deleteWordBackward", "tui.editor.deleteWordBackward"),
    ("deleteWordForward", "tui.editor.deleteWordForward"),
    ("deleteToLineStart", "tui.editor.deleteToLineStart"),
    ("deleteToLineEnd", "tui.editor.deleteToLineEnd"),
    ("yank", "tui.editor.yank"),
    ("yankPop", "tui.editor.yankPop"),
    ("undo", "tui.editor.undo"),
    ("newLine", "tui.input.newLine"),
    ("submit", "tui.input.submit"),
    ("tab", "tui.input.tab"),
    ("copy", "tui.input.copy"),
    ("selectUp", "tui.select.up"),
    ("selectDown", "tui.select.down"),
    ("selectPageUp", "tui.select.pageUp"),
    ("selectPageDown", "tui.select.pageDown"),
    ("selectConfirm", "tui.select.confirm"),
    ("selectCancel", "tui.select.cancel"),
    ("interrupt", "app.interrupt"),
    ("clear", "app.clear"),
    ("exit", "app.exit"),
    ("suspend", "app.suspend"),
    ("cycleThinkingLevel", "app.thinking.cycle"),
    ("cycleModelForward", "app.model.cycleForward"),
    ("cycleModelBackward", "app.model.cycleBackward"),
    ("selectModel", "app.model.select"),
    ("expandTools", "app.tools.expand"),
    ("toggleThinking", "app.thinking.toggle"),
    ("toggleSessionNamedFilter", "app.session.toggleNamedFilter"),
    ("externalEditor", "app.editor.external"),
    ("followUp", "app.message.followUp"),
    ("dequeue", "app.message.dequeue"),
    ("pasteImage", "app.clipboard.pasteImage"),
    ("newSession", "app.session.new"),
    ("tree", "app.session.tree"),
    ("fork", "app.session.fork"),
    ("resume", "app.session.resume"),
    ("treeFoldOrUp", "app.tree.foldOrUp"),
    ("treeUnfoldOrDown", "app.tree.unfoldOrDown"),
    ("treeEditLabel", "app.tree.editLabel"),
    ("treeToggleLabelTimestamp", "app.tree.toggleLabelTimestamp"),
    ("toggleSessionPath", "app.session.togglePath"),
    ("toggleSessionSort", "app.session.toggleSort"),
    ("renameSession", "app.session.rename"),
    ("deleteSession", "app.session.delete"),
    ("deleteSessionNoninvasive", "app.session.deleteNoninvasive"),
];

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn entry_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn is_real_directory(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

fn secure_agent_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_: &Path) -> io::Result<()> {
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    if private {
        secure_agent_dir(parent)?;
    } else {
        fs::create_dir_all(parent)?;
    }

    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let temporary = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let backup = parent.join(format!(
        ".{file_name}.{}.{}.backup",
        std::process::id(),
        sequence
    ));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        #[cfg(unix)]
        if !private && let Ok(metadata) = fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.sync_all()?;
        let had_target = entry_exists(path);
        if had_target {
            fs::rename(path, &backup)?;
            if let Err(error) = sync_parent(path) {
                let _ = fs::rename(&backup, path);
                return Err(error);
            }
        }
        if let Err(error) = fs::rename(&temporary, path) {
            if had_target {
                let _ = fs::rename(&backup, path);
            }
            return Err(error);
        }
        sync_parent(path)?;
        if had_target {
            fs::remove_file(&backup)?;
            sync_parent(path)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn json_object(path: &Path) -> Option<Map<String, Value>> {
    if !is_regular_file(path) {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .as_object()
        .cloned()
}

/// Migrate credentials in the process-selected agent directory.
#[must_use]
pub fn migrate_auth_to_auth_json() -> Vec<String> {
    migrate_auth_to_auth_json_in(&get_agent_dir())
}

/// Migrate legacy `oauth.json` and `settings.json.apiKeys` under `agent_dir`.
///
/// `auth.json` is committed and synced before either source is renamed or
/// rewritten. Source symlinks are ignored, and an existing `auth.json` (even a
/// symlink) makes this operation an idempotent no-op.
#[must_use]
pub fn migrate_auth_to_auth_json_in(agent_dir: &Path) -> Vec<String> {
    let auth_path = agent_dir.join("auth.json");
    let oauth_path = agent_dir.join("oauth.json");
    let settings_path = agent_dir.join("settings.json");
    let Ok(backend) = FileLockBackend::new(&auth_path) else {
        return Vec::new();
    };

    let migration = backend.with_lock_sync_unseeded(|_| {
        // Recheck under the same sibling lock used by every normal auth read
        // and write. `entry_exists` intentionally treats broken symlinks as
        // existing destinations, preserving the no-follow migration policy.
        if entry_exists(&auth_path) {
            return Ok((None, None));
        }

        let oauth = json_object(&oauth_path);
        let settings = json_object(&settings_path);
        let mut credentials = BTreeMap::<String, Value>::new();
        let mut providers = Vec::new();

        if let Some(entries) = oauth.as_ref() {
            for (provider, value) in entries {
                if let Some(raw) = value.as_object() {
                    let mut credential = raw.clone();
                    credential.insert("type".to_owned(), Value::String("oauth".to_owned()));
                    credentials.insert(provider.clone(), Value::Object(credential));
                    providers.push(provider.clone());
                }
            }
        }

        let mut settings_without_keys = settings.clone();
        let settings_had_keys = settings
            .as_ref()
            .is_some_and(|document| document.contains_key("apiKeys"));
        if let Some(document) = settings.as_ref()
            && let Some(api_keys) = document.get("apiKeys").and_then(Value::as_object)
        {
            for (provider, value) in api_keys {
                if credentials.contains_key(provider) {
                    continue;
                }
                if let Some(key) = value.as_str() {
                    credentials.insert(
                        provider.clone(),
                        serde_json::json!({ "type": "api_key", "key": key }),
                    );
                    providers.push(provider.clone());
                }
            }
            if let Some(updated) = settings_without_keys.as_mut() {
                updated.remove("apiKeys");
            }
        }

        if credentials.is_empty() {
            return Ok((None, None));
        }
        let serialized = serde_json::to_string_pretty(&credentials).map_err(|error| {
            StoreError::message(format!(
                "Failed to serialize migrated auth storage: {error}"
            ))
        })?;
        let outcome = (
            providers,
            settings_without_keys,
            settings_had_keys,
            oauth.is_some(),
        );
        Ok((Some(outcome), Some(serialized)))
    });

    let Ok(Some((providers, settings_without_keys, settings_had_keys, oauth_present))) = migration
    else {
        return Vec::new();
    };

    // Legacy sources remain untouched unless the auth commit succeeded.
    if let Some(updated) = settings_without_keys
        && settings_had_keys
        && let Ok(bytes) = serde_json::to_vec_pretty(&Value::Object(updated))
    {
        let _ = atomic_write(&settings_path, &bytes, false);
    }

    if oauth_present {
        let migrated_path = agent_dir.join("oauth.json.migrated");
        if !entry_exists(&migrated_path) && fs::rename(&oauth_path, &migrated_path).is_ok() {
            let _ = sync_parent(&migrated_path);
        }
    }

    providers
}

/// Relocate v0.30 session files from the process-selected agent root.
pub fn migrate_sessions_from_agent_root() {
    migrate_sessions_from_agent_root_in(&get_agent_dir());
}

/// Relocate regular top-level session files under `agent_dir/sessions/<encoded-cwd>`.
pub fn migrate_sessions_from_agent_root_in(agent_dir: &Path) {
    let Ok(entries) = fs::read_dir(agent_dir) else {
        return;
    };
    let mut files = entries.filter_map(Result::ok).collect::<Vec<_>>();
    files.sort_by_key(fs::DirEntry::file_name);
    for entry in files {
        let source = entry.path();
        if source.extension().and_then(|ext| ext.to_str()) != Some("jsonl")
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            continue;
        }
        let Ok(file) = File::open(&source) else {
            continue;
        };
        let mut first_line = String::new();
        if BufReader::new(file).read_line(&mut first_line).is_err() {
            continue;
        }
        let Ok(header) = serde_json::from_str::<Value>(&first_line) else {
            continue;
        };
        if header.get("type").and_then(Value::as_str) != Some("session") {
            continue;
        }
        let Some(cwd) = header.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        let Some(file_name) = source.file_name() else {
            continue;
        };
        let target_dir = agent_dir
            .join("sessions")
            .join(encode_cwd_for_session_dir(cwd));
        let target = target_dir.join(file_name);
        if entry_exists(&target) || fs::create_dir_all(&target_dir).is_err() {
            continue;
        }
        if fs::rename(&source, &target).is_ok() {
            let _ = sync_parent(&target);
            let _ = sync_parent(&source);
        }
    }
}

/// Move managed search binaries from `tools/` to `bin/`.
pub fn migrate_tools_to_bin() {
    let agent_dir = get_agent_dir();
    migrate_tools_to_bin_in(&agent_dir, &get_bin_dir_with(&agent_dir));
}

/// Move managed binaries without following a `tools/` symlink.
pub fn migrate_tools_to_bin_in(agent_dir: &Path, bin_dir: &Path) {
    let tools_dir = agent_dir.join("tools");
    if !is_real_directory(&tools_dir) {
        return;
    }
    for name in ["fd", "rg", "fd.exe", "rg.exe"] {
        let source = tools_dir.join(name);
        if !is_regular_file(&source) {
            continue;
        }
        let target = bin_dir.join(name);
        if entry_exists(&target) {
            if is_regular_file(&target)
                && File::open(&target).and_then(|file| file.sync_all()).is_ok()
                && fs::remove_file(&source).is_ok()
            {
                let _ = sync_parent(&source);
            }
            continue;
        }
        if fs::create_dir_all(bin_dir).is_ok() && fs::rename(&source, &target).is_ok() {
            let _ = sync_parent(&target);
            let _ = sync_parent(&source);
        }
    }
}

/// Migrate keybinding names using the compatibility map.
pub fn migrate_keybindings_config_file() {
    migrate_keybindings_config_file_in(&get_agent_dir(), KEYBINDING_NAME_MIGRATIONS);
}

/// Migrate keybindings with an injected legacy-name map.
pub fn migrate_keybindings_config_file_in(agent_dir: &Path, migrations: &[(&str, &str)]) {
    let path = agent_dir.join("keybindings.json");
    let Some(mut document) = json_object(&path) else {
        return;
    };
    let original = document.clone();
    for &(legacy, current) in migrations {
        let Some(value) = original.get(legacy).cloned() else {
            continue;
        };
        document.remove(legacy);
        if !original.contains_key(current) {
            document.insert(current.to_owned(), value);
        }
    }
    if document == original {
        return;
    }
    if let Ok(mut bytes) = serde_json::to_vec_pretty(&Value::Object(document)) {
        bytes.push(b'\n');
        let _ = atomic_write(&path, &bytes, false);
    }
}

fn migrate_commands_to_prompts(base_dir: &Path) {
    let commands = base_dir.join("commands");
    let prompts = base_dir.join("prompts");
    if entry_exists(&commands) && !entry_exists(&prompts) && fs::rename(&commands, &prompts).is_ok()
    {
        let _ = sync_parent(&prompts);
    }
}

fn deprecated_extension_warnings(base_dir: &Path, label: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    if entry_exists(&base_dir.join("hooks")) {
        warnings.push(format!(
            "{label} hooks/ directory found. Hooks have been renamed to extensions."
        ));
    }
    let tools = base_dir.join("tools");
    let has_custom_tools = match fs::symlink_metadata(&tools) {
        Ok(metadata) if metadata.file_type().is_symlink() => true,
        Ok(metadata) if metadata.file_type().is_dir() => fs::read_dir(&tools)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .any(|name| {
                let lower = name.to_ascii_lowercase();
                !name.starts_with('.')
                    && !matches!(lower.as_str(), "fd" | "rg" | "fd.exe" | "rg.exe")
            }),
        _ => false,
    };
    if has_custom_tools {
        warnings.push(format!(
            "{label} tools/ directory contains custom tools. Custom tools have been merged into extensions."
        ));
    }
    warnings
}

/// Rename legacy command directories and return deprecated extension warnings.
#[must_use]
pub fn migrate_extension_system(cwd: &Path) -> Vec<String> {
    migrate_extension_system_in(&get_agent_dir(), &cwd.join(CONFIG_DIR_NAME))
}

/// Explicit-path extension migration seam used by startup tests and embedders.
#[must_use]
pub fn migrate_extension_system_in(agent_dir: &Path, project_dir: &Path) -> Vec<String> {
    migrate_commands_to_prompts(agent_dir);
    migrate_commands_to_prompts(project_dir);
    let mut warnings = deprecated_extension_warnings(agent_dir, "Global");
    warnings.extend(deprecated_extension_warnings(project_dir, "Project"));
    warnings
}

/// Run startup migrations in their compatibility-defined deterministic order.
#[must_use]
pub fn run_migrations(cwd: &Path) -> MigrationResult {
    let agent_dir = get_agent_dir();
    let bin_dir = get_bin_dir_with(&agent_dir);
    run_migrations_in(
        &agent_dir,
        &bin_dir,
        &cwd.join(CONFIG_DIR_NAME),
        KEYBINDING_NAME_MIGRATIONS,
    )
}

/// Explicit-path orchestration seam.
#[must_use]
pub fn run_migrations_in(
    agent_dir: &Path,
    bin_dir: &Path,
    project_dir: &Path,
    keybinding_migrations: &[(&str, &str)],
) -> MigrationResult {
    let migrated_auth_providers = migrate_auth_to_auth_json_in(agent_dir);
    migrate_sessions_from_agent_root_in(agent_dir);
    migrate_tools_to_bin_in(agent_dir, bin_dir);
    migrate_keybindings_config_file_in(agent_dir, keybinding_migrations);
    let deprecation_warnings = migrate_extension_system_in(agent_dir, project_dir);
    MigrationResult {
        migrated_auth_providers,
        deprecation_warnings,
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;

    type TestResult = Result<(), Box<dyn Error>>;

    fn write_json(path: &Path, value: &Value) -> TestResult {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_vec_pretty(value)?)?;
        Ok(())
    }

    fn read_json(path: &Path) -> Result<Value, Box<dyn Error>> {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }

    #[test]
    fn auth_migrates_both_sources_preserves_unknown_settings_and_is_idempotent() -> TestResult {
        let temp = TempDir::new()?;
        let agent = temp.path();
        write_json(
            &agent.join("oauth.json"),
            &serde_json::json!({"anthropic":{"refresh":"r","access":"a","expires":4}}),
        )?;
        write_json(
            &agent.join("settings.json"),
            &serde_json::json!({"apiKeys":{"openai":"sk-test","anthropic":"ignored"},"future":{"x":1}}),
        )?;

        let providers = migrate_auth_to_auth_json_in(agent);
        assert_eq!(providers, ["anthropic", "openai"]);
        let auth = read_json(&agent.join("auth.json"))?;
        assert_eq!(auth["anthropic"]["type"], "oauth");
        assert_eq!(auth["openai"]["type"], "api_key");
        assert_eq!(auth["openai"]["key"], "sk-test");
        let settings = read_json(&agent.join("settings.json"))?;
        assert!(settings.get("apiKeys").is_none());
        assert_eq!(settings["future"]["x"], 1);
        assert!(!agent.join("oauth.json").exists());
        assert!(agent.join("oauth.json.migrated").exists());
        assert!(migrate_auth_to_auth_json_in(agent).is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn auth_file_and_directory_are_private() -> TestResult {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new()?;
        let agent = temp.path().join("agent");
        fs::create_dir(&agent)?;
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755))?;
        write_json(
            &agent.join("settings.json"),
            &serde_json::json!({"apiKeys":{"p":"k"}}),
        )?;
        fs::set_permissions(
            agent.join("settings.json"),
            fs::Permissions::from_mode(0o640),
        )?;
        let _ = migrate_auth_to_auth_json_in(&agent);
        assert_eq!(fs::metadata(&agent)?.permissions().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(agent.join("settings.json"))?
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            fs::metadata(agent.join("auth.json"))?.permissions().mode() & 0o777,
            0o600
        );
        Ok(())
    }

    #[test]
    fn malformed_source_does_not_block_other_source() -> TestResult {
        let temp = TempDir::new()?;
        fs::write(temp.path().join("oauth.json"), b"not json")?;
        write_json(
            &temp.path().join("settings.json"),
            &serde_json::json!({"apiKeys":{"p":"k"},"unknown":true}),
        )?;
        assert_eq!(migrate_auth_to_auth_json_in(temp.path()), ["p"]);
        assert!(temp.path().join("oauth.json").exists());
        assert_eq!(
            read_json(&temp.path().join("settings.json"))?["unknown"],
            true
        );
        Ok(())
    }

    #[test]
    fn oauth_migrated_collision_keeps_source_after_auth_is_persisted() -> TestResult {
        let temp = TempDir::new()?;
        write_json(
            &temp.path().join("oauth.json"),
            &serde_json::json!({"p":{"access":"a"}}),
        )?;
        fs::write(temp.path().join("oauth.json.migrated"), b"older")?;
        assert_eq!(migrate_auth_to_auth_json_in(temp.path()), ["p"]);
        assert!(temp.path().join("auth.json").exists());
        assert!(temp.path().join("oauth.json").exists());
        assert_eq!(fs::read(temp.path().join("oauth.json.migrated"))?, b"older");
        Ok(())
    }

    #[test]
    fn sessions_move_by_encoded_cwd_and_skip_malformed_and_collision() -> TestResult {
        let temp = TempDir::new()?;
        let agent = temp.path();
        fs::write(
            agent.join("good.jsonl"),
            b"{\"type\":\"session\",\"cwd\":\"/a:b/c\"}\n{}\n",
        )?;
        fs::write(agent.join("bad.jsonl"), b"not json\n")?;
        fs::write(
            agent.join("collision.jsonl"),
            b"{\"type\":\"session\",\"cwd\":\"/x\"}\n",
        )?;
        let collision_dir = agent.join("sessions/--x--");
        fs::create_dir_all(&collision_dir)?;
        fs::write(collision_dir.join("collision.jsonl"), b"existing")?;
        migrate_sessions_from_agent_root_in(agent);
        assert!(agent.join("sessions/--a-b-c--/good.jsonl").exists());
        assert!(!agent.join("good.jsonl").exists());
        assert!(agent.join("bad.jsonl").exists());
        assert!(agent.join("collision.jsonl").exists());
        migrate_sessions_from_agent_root_in(agent);
        assert!(agent.join("bad.jsonl").exists());
        Ok(())
    }

    #[test]
    fn managed_tools_move_and_collision_removes_only_old_regular_file() -> TestResult {
        let temp = TempDir::new()?;
        let agent = temp.path().join("agent");
        let bin = temp.path().join("managed-bin");
        fs::create_dir_all(agent.join("tools"))?;
        fs::create_dir_all(&bin)?;
        fs::write(agent.join("tools/fd"), b"fd")?;
        fs::write(agent.join("tools/rg"), b"old")?;
        fs::write(bin.join("rg"), b"new")?;
        fs::write(agent.join("tools/custom"), b"keep")?;
        migrate_tools_to_bin_in(&agent, &bin);
        assert_eq!(fs::read(bin.join("fd"))?, b"fd");
        assert_eq!(fs::read(bin.join("rg"))?, b"new");
        assert!(!agent.join("tools/rg").exists());
        assert!(agent.join("tools/custom").exists());
        Ok(())
    }

    #[test]
    fn keybindings_migrate_drop_shadowed_legacy_and_ignore_malformed() -> TestResult {
        let temp = TempDir::new()?;
        write_json(
            &temp.path().join("keybindings.json"),
            &serde_json::json!({"old":"a","new":"b","other":{"future":1}}),
        )?;
        migrate_keybindings_config_file_in(temp.path(), &[("old", "new")]);
        let migrated = read_json(&temp.path().join("keybindings.json"))?;
        assert!(migrated.get("old").is_none());
        assert_eq!(migrated["new"], "b");
        assert_eq!(migrated["other"]["future"], 1);
        fs::write(temp.path().join("keybindings.json"), b"[")?;
        migrate_keybindings_config_file_in(temp.path(), &[("old", "new")]);
        assert_eq!(fs::read(temp.path().join("keybindings.json"))?, b"[");
        Ok(())
    }

    #[test]
    fn extension_migration_renames_and_warns_in_stable_order() -> TestResult {
        let temp = TempDir::new()?;
        let global = temp.path().join("global");
        let project = temp.path().join("project");
        fs::create_dir_all(global.join("commands"))?;
        fs::create_dir_all(global.join("hooks"))?;
        fs::create_dir_all(project.join("tools"))?;
        fs::write(project.join("tools/custom.ts"), b"")?;
        let warnings = migrate_extension_system_in(&global, &project);
        assert!(global.join("prompts").exists());
        assert!(!global.join("commands").exists());
        assert_eq!(
            warnings,
            [
                "Global hooks/ directory found. Hooks have been renamed to extensions.",
                "Project tools/ directory contains custom tools. Custom tools have been merged into extensions.",
            ]
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_renamed_not_followed_and_sensitive_sources_are_ignored() -> TestResult {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new()?;
        let outside = temp.path().join("outside");
        let global = temp.path().join("global");
        fs::create_dir_all(&outside)?;
        fs::create_dir_all(&global)?;
        fs::write(outside.join("secret"), b"secret")?;
        symlink(&outside, global.join("commands"))?;
        symlink(outside.join("secret"), global.join("oauth.json"))?;
        symlink(&outside, global.join("tools"))?;
        migrate_tools_to_bin_in(&global, &global.join("bin"));
        let warnings = migrate_extension_system_in(&global, &temp.path().join("project"));
        assert!(
            fs::symlink_metadata(global.join("prompts"))?
                .file_type()
                .is_symlink()
        );
        assert!(!entry_exists(&global.join("commands")));
        assert_eq!(fs::read(outside.join("secret"))?, b"secret");
        assert!(!global.join("bin").exists());
        assert_eq!(
            warnings,
            [
                "Global tools/ directory contains custom tools. Custom tools have been merged into extensions."
            ]
        );
        assert!(migrate_auth_to_auth_json_in(&global).is_empty());
        Ok(())
    }

    #[test]
    fn auth_migration_uses_auth_sibling_lock() -> TestResult {
        let temp = TempDir::new()?;
        let agent = temp.path();
        write_json(
            &agent.join("settings.json"),
            &serde_json::json!({"apiKeys": {"openai": "sk-test"}}),
        )?;

        let backend = FileLockBackend::new(agent.join("auth.json"))?;
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            backend.with_lock_sync_unseeded(|_| {
                entered_tx
                    .send(())
                    .map_err(|error| StoreError::message(error.to_string()))?;
                std::thread::sleep(Duration::from_millis(400));
                Ok(((), None))
            })
        });
        entered_rx.recv()?;

        assert!(migrate_auth_to_auth_json_in(agent).is_empty());
        assert!(
            !agent.join("auth.json").exists(),
            "migration must not write outside the held sibling lock"
        );
        holder.join().map_err(|_| "lock holder panicked")??;

        assert_eq!(migrate_auth_to_auth_json_in(agent), ["openai"]);
        assert_eq!(
            read_json(&agent.join("auth.json"))?["openai"]["key"],
            "sk-test"
        );
        Ok(())
    }

    #[test]
    fn all_migrations_run_in_order_and_are_idempotent() -> TestResult {
        let temp = TempDir::new()?;
        let agent = temp.path().join("agent");
        let bin = agent.join("bin");
        let project = temp.path().join("project/.pi");
        fs::create_dir_all(agent.join("tools"))?;
        fs::create_dir_all(agent.join("commands"))?;
        fs::create_dir_all(project.join("hooks"))?;
        fs::write(agent.join("tools/fd"), b"binary")?;
        fs::write(
            agent.join("session.jsonl"),
            b"{\"type\":\"session\",\"cwd\":\"/work\"}\n",
        )?;
        write_json(
            &agent.join("settings.json"),
            &serde_json::json!({"apiKeys":{"p":"k"},"keep":7}),
        )?;
        write_json(
            &agent.join("keybindings.json"),
            &serde_json::json!({"old":"ctrl+x"}),
        )?;

        let first = run_migrations_in(&agent, &bin, &project, &[("old", "new")]);
        assert_eq!(first.migrated_auth_providers, ["p"]);
        assert_eq!(
            first.deprecation_warnings,
            ["Project hooks/ directory found. Hooks have been renamed to extensions."]
        );
        assert!(agent.join("sessions/--work--/session.jsonl").exists());
        assert!(bin.join("fd").exists());
        assert!(agent.join("prompts").exists());
        assert_eq!(read_json(&agent.join("keybindings.json"))?["new"], "ctrl+x");
        assert_eq!(read_json(&agent.join("settings.json"))?["keep"], 7);

        let second = run_migrations_in(&agent, &bin, &project, &[("old", "new")]);
        assert!(second.migrated_auth_providers.is_empty());
        assert_eq!(second.deprecation_warnings, first.deprecation_warnings);
        Ok(())
    }
}
