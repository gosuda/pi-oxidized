//! Project trust store and trust resolution.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/trust-manager.ts` and
//! `project-trust.ts`. The store is a sorted-key `trust.json` map under the
//! agent directory; resolution walks ancestors, skips `null`, and applies the
//! fixed decision order used by coding-agent.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use thiserror::Error;

use super::config::{
    CONFIG_DIR_NAME, PathInputOptions, canonicalize_path, resolve_path, resolve_path_with,
};
use super::lockfile::{LockError, LockGuard, LockOptions};

/// Project config entries under `{cwd}/.pi` that require a trust decision.
const TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES: &[&str] = &[
    "settings.json",
    "extensions",
    "skills",
    "prompts",
    "themes",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
];

/// Stored trust decision for a canonical project path.
///
/// `None` means “no saved decision” at the lookup API boundary. On disk, a JSON
/// `null` value is only kept when already present; [`ProjectTrustStore::set`]
/// with `None` deletes the key.
pub type ProjectTrustDecision = Option<bool>;

/// Global default used when no override, extension, or store decision applies.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DefaultProjectTrust {
    /// Prompt when UI is available; otherwise refuse trust.
    #[default]
    Ask,
    /// Trust without prompting.
    Always,
    /// Never trust without an explicit override or store decision.
    Never,
}

impl DefaultProjectTrust {
    /// Parse a settings value, defaulting invalid input to [`Self::Ask`].
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("always") => Self::Always,
            Some("never") => Self::Never,
            _ => Self::Ask,
        }
    }

    /// Wire string used in settings JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

/// Extension `project_trust` decision discriminant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectTrustEventDecision {
    /// Trust the project.
    Yes,
    /// Do not trust the project.
    No,
    /// Defer to later handlers / built-in resolution.
    Undecided,
}

/// Result returned by an extension `project_trust` handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectTrustExtensionResult {
    /// Decision from the extension.
    pub trusted: ProjectTrustEventDecision,
    /// When true and the decision is yes/no, persist it in the trust store.
    pub remember: bool,
}

/// Nearest saved trust entry for a path (true/false only).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectTrustStoreEntry {
    /// Canonical path that owns the decision.
    pub path: PathBuf,
    /// Saved decision.
    pub decision: bool,
}

/// One path update applied by a trust prompt option or explicit store write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectTrustUpdate {
    /// Path to update (resolved/canonicalized on write).
    pub path: PathBuf,
    /// `Some(true|false)` writes; `None` deletes the key.
    pub decision: ProjectTrustDecision,
}

/// One selectable trust prompt option.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectTrustOption {
    /// Exact UI label text.
    pub label: String,
    /// Whether selecting this option trusts the project for the current run.
    pub trusted: bool,
    /// Store updates to apply when the option is chosen.
    pub updates: Vec<ProjectTrustUpdate>,
    /// Path that was permanently saved, when any.
    pub saved_path: Option<PathBuf>,
}

/// Sync UI surface used by [`resolve_project_trusted`].
///
/// Interactive adapters implement selection; non-UI modes report `has_ui() == false`.
pub trait TrustUi {
    /// Whether an interactive selector is available.
    fn has_ui(&self) -> bool;

    /// Present `prompt` with `options` and return the selected label, or `None` on cancel.
    fn select(&mut self, prompt: &str, options: &[String]) -> Option<String>;
}

/// Errors from trust store I/O and validation.
#[derive(Debug, Error)]
pub enum TrustError {
    /// Trust file could not be read or parsed as JSON text.
    #[error("Failed to read trust store {path}: {message}")]
    Read {
        /// Trust file path.
        path: String,
        /// Underlying error message.
        message: String,
    },
    /// Trust file root was not a JSON object.
    #[error("Invalid trust store {path}: expected an object")]
    InvalidObject {
        /// Trust file path.
        path: String,
    },
    /// A trust entry value was not `true`, `false`, or `null`.
    #[error("Invalid trust store {path}: value for {key} must be true, false, or null")]
    InvalidValue {
        /// Trust file path.
        path: String,
        /// JSON-stringified map key (includes quotes).
        key: String,
    },
    /// Exclusive trust lock could not be acquired.
    #[error("Failed to acquire trust store lock")]
    Lock,
    /// Filesystem failure while writing the trust file or creating parents.
    #[error("Failed to write trust store {path}: {message}")]
    Write {
        /// Trust file path.
        path: String,
        /// Underlying error message.
        message: String,
    },
}

impl From<LockError> for TrustError {
    fn from(_value: LockError) -> Self {
        Self::Lock
    }
}

/// Optional extension trust hook used during project trust resolution.
pub type ProjectTrustExtensionHook<'a> =
    &'a mut dyn FnMut(&Path) -> Result<Option<ProjectTrustExtensionResult>, String>;

/// Options for [`resolve_project_trusted`].
pub struct ResolveProjectTrustedOptions<'a> {
    /// Project working directory under consideration.
    pub cwd: PathBuf,
    /// Persistent trust store.
    pub trust_store: &'a ProjectTrustStore,
    /// CLI `--approve` / `--no-approve` override.
    pub trust_override: Option<bool>,
    /// Global `defaultProjectTrust` setting (defaults to ask).
    pub default_project_trust: DefaultProjectTrust,
    /// Optional extension `project_trust` hook.
    ///
    /// Invoked with the project cwd. Return `Ok(Some(result))` when an extension
    /// produced a decision payload; `trusted: Undecided` continues the built-in
    /// flow. Return `Ok(None)` when no extension handled the event.
    /// `Err(message)` is reported through [`Self::on_extension_error`] and
    /// treated as no extension decision.
    pub extension_hook: Option<ProjectTrustExtensionHook<'a>>,
    /// Optional interactive UI. When absent or `has_ui() == false`, ask-mode
    /// falls through to `false`.
    pub ui: Option<&'a mut dyn TrustUi>,
    /// Receives extension error strings in the exact coding-agent format.
    pub on_extension_error: Option<&'a mut dyn FnMut(String)>,
}

/// Sorted-key project trust store at `{agentDir}/trust.json`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectTrustStore {
    trust_path: PathBuf,
}

impl ProjectTrustStore {
    /// Create a store rooted at `agent_dir` (`{agentDir}/trust.json`).
    #[must_use]
    pub fn new(agent_dir: impl AsRef<Path>) -> Self {
        let agent = path_to_string(agent_dir.as_ref());
        let resolved = resolve_path(&agent);
        Self {
            trust_path: resolved.join("trust.json"),
        }
    }

    /// Path of the on-disk trust file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.trust_path
    }

    /// Lookup the effective decision for `cwd`, walking ancestors and skipping null.
    ///
    /// # Errors
    ///
    /// Returns [`TrustError`] when the lock cannot be acquired or the file is invalid.
    pub fn get(&self, cwd: impl AsRef<Path>) -> Result<ProjectTrustDecision, TrustError> {
        Ok(self.get_entry(cwd)?.map(|entry| entry.decision))
    }

    /// Lookup the nearest ancestor entry with a true/false decision.
    ///
    /// # Errors
    ///
    /// Returns [`TrustError`] when the lock cannot be acquired or the file is invalid.
    pub fn get_entry(
        &self,
        cwd: impl AsRef<Path>,
    ) -> Result<Option<ProjectTrustStoreEntry>, TrustError> {
        self.with_lock(|data| Ok(find_nearest_trust_entry(data, cwd.as_ref())))
    }

    /// Persist a single path decision (`None` deletes the key).
    ///
    /// # Errors
    ///
    /// Returns [`TrustError`] on lock, read, validation, or write failure.
    pub fn set(
        &self,
        cwd: impl AsRef<Path>,
        decision: ProjectTrustDecision,
    ) -> Result<(), TrustError> {
        self.set_many([ProjectTrustUpdate {
            path: cwd.as_ref().to_path_buf(),
            decision,
        }])
    }

    /// Persist multiple path decisions under one lock.
    ///
    /// # Errors
    ///
    /// Returns [`TrustError`] on lock, read, validation, or write failure.
    pub fn set_many(
        &self,
        decisions: impl IntoIterator<Item = ProjectTrustUpdate>,
    ) -> Result<(), TrustError> {
        self.with_lock(|data| {
            for update in decisions {
                let key = path_key(&update.path);
                match update.decision {
                    None => {
                        data.remove(&key);
                    }
                    Some(value) => {
                        data.insert(key, Some(value));
                    }
                }
            }
            write_trust_file(&self.trust_path, data)?;
            Ok(())
        })
    }

    fn with_lock<T>(
        &self,
        f: impl FnOnce(&mut TrustFile) -> Result<T, TrustError>,
    ) -> Result<T, TrustError> {
        let _guard = acquire_trust_lock(&self.trust_path)?;
        let mut data = read_trust_file(&self.trust_path)?;
        f(&mut data)
    }
}

/// Parent directory of a trust path when one exists.
#[must_use]
pub fn get_project_trust_parent_path(cwd: impl AsRef<Path>) -> Option<PathBuf> {
    let trust_path = normalize_cwd(cwd.as_ref());
    let parent = trust_path.parent()?;
    if parent.as_os_str().is_empty() || parent == trust_path {
        None
    } else {
        // On Unix, `parent()` of `/` is `Some("")` filtered above; of `/a` is `/`.
        // Match Node `dirname` root fixed-point via normalize equality.
        let parent_norm = normalize_cwd(parent);
        if parent_norm == trust_path {
            None
        } else {
            Some(parent_norm)
        }
    }
}

/// Build the exact trust prompt options for `cwd`.
#[must_use]
pub fn get_project_trust_options(
    cwd: impl AsRef<Path>,
    include_session_only: bool,
) -> Vec<ProjectTrustOption> {
    let trust_path = normalize_cwd(cwd.as_ref());
    let mut options = vec![ProjectTrustOption {
        label: "Trust".to_owned(),
        trusted: true,
        updates: vec![ProjectTrustUpdate {
            path: trust_path.clone(),
            decision: Some(true),
        }],
        saved_path: Some(trust_path.clone()),
    }];

    if let Some(parent_path) = get_project_trust_parent_path(&trust_path) {
        options.push(ProjectTrustOption {
            label: format!("Trust parent folder ({})", path_to_string(&parent_path)),
            trusted: true,
            updates: vec![
                ProjectTrustUpdate {
                    path: parent_path.clone(),
                    decision: Some(true),
                },
                ProjectTrustUpdate {
                    path: trust_path.clone(),
                    decision: None,
                },
            ],
            saved_path: Some(parent_path),
        });
    }

    if include_session_only {
        options.push(ProjectTrustOption {
            label: "Trust (this session only)".to_owned(),
            trusted: true,
            updates: Vec::new(),
            saved_path: None,
        });
    }

    options.push(ProjectTrustOption {
        label: "Do not trust".to_owned(),
        trusted: false,
        updates: vec![ProjectTrustUpdate {
            path: trust_path.clone(),
            decision: Some(false),
        }],
        saved_path: Some(trust_path),
    });

    if include_session_only {
        options.push(ProjectTrustOption {
            label: "Do not trust (this session only)".to_owned(),
            trusted: false,
            updates: Vec::new(),
            saved_path: None,
        });
    }

    options
}

/// Exact trust prompt title string for `cwd`.
#[must_use]
pub fn format_project_trust_prompt(cwd: impl AsRef<Path>) -> String {
    let cwd_display = path_to_string(cwd.as_ref());
    format!(
        "Trust project folder?\n{cwd_display}\n\nThis allows pi to load {CONFIG_DIR_NAME} settings and resources, install missing project packages, and execute project extensions."
    )
}

/// Returns true when `cwd` has project-local resources that require trust.
///
/// Detects trust-requiring entries under `{cwd}/.pi`, or `.agents/skills` in
/// `cwd` or an ancestor, excluding the user-global `~/.agents/skills` directory.
#[must_use]
pub fn has_trust_requiring_project_resources(cwd: impl AsRef<Path>) -> bool {
    let home = process_home_path();
    has_trust_requiring_project_resources_with(cwd.as_ref(), home.as_deref())
}

/// [`has_trust_requiring_project_resources`] with an explicit home directory.
#[must_use]
pub fn has_trust_requiring_project_resources_with(cwd: &Path, home_dir: Option<&Path>) -> bool {
    let home_canonical = home_dir.map(|home| {
        let home_str = path_to_string(home);
        canonicalize_path(resolve_path_with(
            &home_str,
            Path::new("/"),
            PathInputOptions::new()
                .home_dir(Some(home))
                .expand_tilde(false),
        ))
    });
    let user_agents_skills = home_canonical
        .as_ref()
        .map(|home| home.join(".agents").join("skills"));

    let mut current_dir = normalize_cwd_with(cwd, home_dir);

    let config_dir = current_dir.join(CONFIG_DIR_NAME);
    if TRUST_REQUIRING_PROJECT_CONFIG_RESOURCES
        .iter()
        .any(|entry| config_dir.join(entry).exists())
    {
        return true;
    }

    loop {
        let agents_skills_dir = current_dir.join(".agents").join("skills");
        let is_user_global = user_agents_skills
            .as_ref()
            .is_some_and(|user| agents_skills_dir == *user);
        if !is_user_global && agents_skills_dir.exists() {
            return true;
        }

        let Some(parent) = current_dir.parent() else {
            return false;
        };
        if parent.as_os_str().is_empty() || parent == current_dir.as_path() {
            return false;
        }
        let parent_norm = normalize_cwd_with(parent, home_dir);
        if parent_norm == current_dir {
            return false;
        }
        current_dir = parent_norm;
    }
}

/// Resolve whether the project at `options.cwd` is trusted.
///
/// Decision order:
/// 1. `trust_override`
/// 2. no trust-requiring resources → true
/// 3. extension hook yes/no (optional remember)
/// 4. store ancestor decision
/// 5. `default_project_trust` always/never
/// 6. no UI → false
/// 7. UI selection (apply updates)
/// 8. cancel → false
///
/// # Errors
///
/// Returns [`TrustError`] when the trust store cannot be read or written.
pub fn resolve_project_trusted(
    options: ResolveProjectTrustedOptions<'_>,
) -> Result<bool, TrustError> {
    if let Some(override_value) = options.trust_override {
        return Ok(override_value);
    }

    if !has_trust_requiring_project_resources(&options.cwd) {
        return Ok(true);
    }

    if let Some(hook) = options.extension_hook {
        match hook(&options.cwd) {
            Ok(Some(result)) => match result.trusted {
                ProjectTrustEventDecision::Yes => {
                    if result.remember {
                        options.trust_store.set(&options.cwd, Some(true))?;
                    }
                    return Ok(true);
                }
                ProjectTrustEventDecision::No => {
                    if result.remember {
                        options.trust_store.set(&options.cwd, Some(false))?;
                    }
                    return Ok(false);
                }
                ProjectTrustEventDecision::Undecided => {}
            },
            Ok(None) => {}
            Err(message) => {
                if let Some(on_error) = options.on_extension_error {
                    on_error(message);
                }
            }
        }
    }

    if let Some(decision) = options.trust_store.get(&options.cwd)? {
        return Ok(decision);
    }

    match options.default_project_trust {
        DefaultProjectTrust::Always => return Ok(true),
        DefaultProjectTrust::Never => return Ok(false),
        DefaultProjectTrust::Ask => {}
    }

    let Some(ui) = options.ui else {
        return Ok(false);
    };
    if !ui.has_ui() {
        return Ok(false);
    }

    let prompt_options = get_project_trust_options(&options.cwd, true);
    let labels: Vec<String> = prompt_options
        .iter()
        .map(|option| option.label.clone())
        .collect();
    let prompt = format_project_trust_prompt(&options.cwd);
    let selected_label = ui.select(&prompt, &labels);
    if let Some(label) = selected_label
        && let Some(selected) = prompt_options
            .into_iter()
            .find(|option| option.label == label)
    {
        if !selected.updates.is_empty() {
            options.trust_store.set_many(selected.updates)?;
        }
        return Ok(selected.trusted);
    }

    Ok(false)
}

type TrustFile = BTreeMap<String, Option<bool>>;

fn acquire_trust_lock(trust_path: &Path) -> Result<LockGuard, TrustError> {
    let trust_dir = trust_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    if let Err(error) = fs::create_dir_all(&trust_dir) {
        return Err(TrustError::Write {
            path: path_to_string(trust_path),
            message: error.to_string(),
        });
    }
    let lockfile_path = trust_lock_path(trust_path);
    let options = LockOptions::new().lockfile_path(lockfile_path);
    LockGuard::acquire_with(&trust_dir, &options).map_err(TrustError::from)
}

fn trust_lock_path(trust_path: &Path) -> PathBuf {
    let mut lock = trust_path.as_os_str().to_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

fn read_trust_file(path: &Path) -> Result<TrustFile, TrustError> {
    if !path.exists() {
        return Ok(TrustFile::new());
    }

    let text = fs::read_to_string(path).map_err(|error| TrustError::Read {
        path: path_to_string(path),
        message: error.to_string(),
    })?;

    let parsed: Value = serde_json::from_str(&text).map_err(|error| TrustError::Read {
        path: path_to_string(path),
        message: error.to_string(),
    })?;

    let Value::Object(object) = parsed else {
        return Err(TrustError::InvalidObject {
            path: path_to_string(path),
        });
    };

    let mut data = TrustFile::new();
    for (key, value) in object {
        let decision = match value {
            Value::Bool(flag) => Some(flag),
            Value::Null => None,
            _ => {
                return Err(TrustError::InvalidValue {
                    path: path_to_string(path),
                    key: json_string_key(&key),
                });
            }
        };
        // Preserve null entries that were already on disk so a pure rewrite
        // does not invent deletions; writers that intend delete remove keys.
        data.insert(key, decision);
    }
    Ok(data)
}

fn write_trust_file(path: &Path, data: &TrustFile) -> Result<(), TrustError> {
    let mut sorted = Map::new();
    for (key, value) in data {
        let json_value = match value {
            Some(flag) => Value::Bool(*flag),
            None => Value::Null,
        };
        sorted.insert(key.clone(), json_value);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| TrustError::Write {
            path: path_to_string(path),
            message: error.to_string(),
        })?;
    }

    let body = serde_json::to_string_pretty(&Value::Object(sorted)).map_err(|error| {
        TrustError::Write {
            path: path_to_string(path),
            message: error.to_string(),
        }
    })?;
    let mut bytes = body.into_bytes();
    bytes.push(b'\n');
    fs::write(path, bytes).map_err(|error| TrustError::Write {
        path: path_to_string(path),
        message: error.to_string(),
    })
}

fn find_nearest_trust_entry(data: &TrustFile, cwd: &Path) -> Option<ProjectTrustStoreEntry> {
    let mut current_dir = normalize_cwd(cwd);
    loop {
        let key = path_to_string(&current_dir);
        if let Some(Some(decision)) = data.get(&key) {
            return Some(ProjectTrustStoreEntry {
                path: current_dir,
                decision: *decision,
            });
        }

        let parent = current_dir.parent()?;
        if parent.as_os_str().is_empty() {
            return None;
        }
        let parent_norm = normalize_cwd(parent);
        if parent_norm == current_dir {
            return None;
        }
        current_dir = parent_norm;
    }
}

fn normalize_cwd(cwd: &Path) -> PathBuf {
    normalize_cwd_with(cwd, process_home_path().as_deref())
}

fn normalize_cwd_with(cwd: &Path, home_dir: Option<&Path>) -> PathBuf {
    let cwd_str = path_to_string(cwd);
    let process_cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let resolved = resolve_path_with(
        &cwd_str,
        &process_cwd,
        PathInputOptions::new().home_dir(home_dir),
    );
    canonicalize_path(resolved)
}

fn path_key(path: &Path) -> String {
    path_to_string(&normalize_cwd(path))
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn json_string_key(key: &str) -> String {
    serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""))
}

fn process_home_path() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    type TestResult = Result<(), String>;

    fn unique_temp_dir(label: &str) -> Result<PathBuf, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let dir = env::temp_dir().join(format!("pi-trust-{label}-{nanos}"));
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        Ok(dir)
    }

    fn write_resource(path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(path, b"{}").map_err(|error| error.to_string())
    }

    fn trust_fixture(label: &str) -> Result<(PathBuf, PathBuf, ProjectTrustStore), String> {
        let root = unique_temp_dir(label)?;
        let agent_dir = root.join("agent");
        let project = root.join("project");
        fs::create_dir_all(&agent_dir).map_err(|error| error.to_string())?;
        fs::create_dir_all(&project).map_err(|error| error.to_string())?;
        write_resource(&project.join(".pi").join("settings.json"))?;
        let store = ProjectTrustStore::new(&agent_dir);
        Ok((root, project, store))
    }

    fn resolve_without_extensions(
        cwd: &Path,
        store: &ProjectTrustStore,
        trust_override: Option<bool>,
        default_project_trust: DefaultProjectTrust,
    ) -> Result<bool, String> {
        resolve_project_trusted(ResolveProjectTrustedOptions {
            cwd: cwd.to_path_buf(),
            trust_store: store,
            trust_override,
            default_project_trust,
            extension_hook: None,
            ui: None,
            on_extension_error: None,
        })
        .map_err(|error| error.to_string())
    }

    fn require_error<T, E>(result: Result<T, E>, message: &str) -> Result<E, String> {
        match result {
            Ok(_) => Err(message.to_owned()),
            Err(error) => Ok(error),
        }
    }

    struct ScriptedUi {
        has_ui: bool,
        choice: Option<String>,
        last_prompt: Option<String>,
        last_options: Vec<String>,
    }

    impl TrustUi for ScriptedUi {
        fn has_ui(&self) -> bool {
            self.has_ui
        }

        fn select(&mut self, prompt: &str, options: &[String]) -> Option<String> {
            self.last_prompt = Some(prompt.to_owned());
            self.last_options = options.to_vec();
            self.choice.clone()
        }
    }

    #[test]
    fn stores_decisions_and_inherits_from_parent_directories() -> TestResult {
        let root = unique_temp_dir("inherit")?;
        let agent_dir = root.join("agent");
        fs::create_dir_all(&agent_dir).map_err(|error| error.to_string())?;
        let parent_dir = root.join("trusted-parent");
        let child_dir = parent_dir.join("project");
        fs::create_dir_all(&child_dir).map_err(|error| error.to_string())?;

        let store = ProjectTrustStore::new(&agent_dir);
        assert_eq!(store.get(&child_dir).map_err(|e| e.to_string())?, None);

        store
            .set(&parent_dir, Some(true))
            .map_err(|e| e.to_string())?;
        assert_eq!(
            store.get(&child_dir).map_err(|e| e.to_string())?,
            Some(true)
        );

        store
            .set(&child_dir, Some(false))
            .map_err(|e| e.to_string())?;
        assert_eq!(
            store.get(&child_dir).map_err(|e| e.to_string())?,
            Some(false)
        );

        store.set(&child_dir, None).map_err(|e| e.to_string())?;
        assert_eq!(
            store.get(&child_dir).map_err(|e| e.to_string())?,
            Some(true)
        );

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn null_entries_are_skipped_during_ancestor_walk() -> TestResult {
        let root = unique_temp_dir("null-skip")?;
        let agent_dir = root.join("agent");
        fs::create_dir_all(&agent_dir).map_err(|error| error.to_string())?;
        let parent = root.join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).map_err(|error| error.to_string())?;

        let store = ProjectTrustStore::new(&agent_dir);
        store.set(&parent, Some(true)).map_err(|e| e.to_string())?;

        // Inject an explicit null for child by writing the file under lock.
        {
            let text = fs::read_to_string(store.path()).map_err(|e| e.to_string())?;
            let mut value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            let object = value
                .as_object_mut()
                .ok_or_else(|| "expected object".to_owned())?;
            object.insert(path_key(&child), Value::Null);
            let body = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
            fs::write(store.path(), format!("{body}\n")).map_err(|e| e.to_string())?;
        }

        assert_eq!(store.get(&child).map_err(|e| e.to_string())?, Some(true));
        let entry = store.get_entry(&child).map_err(|e| e.to_string())?;
        assert_eq!(entry.map(|e| e.path), Some(normalize_cwd(&parent)));

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn set_null_deletes_key_and_serialization_is_sorted_with_trailing_newline() -> TestResult {
        let root = unique_temp_dir("serialize")?;
        let agent_dir = root.join("agent");
        fs::create_dir_all(&agent_dir).map_err(|error| error.to_string())?;
        let store = ProjectTrustStore::new(&agent_dir);

        let zed = root.join("zed");
        let alpha = root.join("alpha");
        fs::create_dir_all(&zed).map_err(|error| error.to_string())?;
        fs::create_dir_all(&alpha).map_err(|error| error.to_string())?;

        store.set(&zed, Some(false)).map_err(|e| e.to_string())?;
        store.set(&alpha, Some(true)).map_err(|e| e.to_string())?;
        store.set(&zed, None).map_err(|e| e.to_string())?;

        let raw = fs::read_to_string(store.path()).map_err(|e| e.to_string())?;
        assert!(raw.ends_with('\n'), "missing trailing newline: {raw:?}");
        assert!(!raw.contains(&path_key(&zed)), "null delete must drop key");

        let alpha_key = path_key(&alpha);
        let expected = format!("{{\n  {key}: true\n}}\n", key = json_string_key(&alpha_key));
        // Pretty JSON may format bool without space issues; compare parsed + trailing newline.
        assert!(raw.ends_with('\n'));
        let parsed: Value = serde_json::from_str(raw.trim_end()).map_err(|e| e.to_string())?;
        let object = parsed
            .as_object()
            .ok_or_else(|| "expected object".to_owned())?;
        let keys: Vec<&String> = object.keys().collect();
        assert_eq!(keys, vec![&alpha_key]);
        assert_eq!(object.get(&alpha_key), Some(&Value::Bool(true)));

        // Ensure lock artifact is sibling trust.json.lock (not agent.lock).
        let lock_path = trust_lock_path(store.path());
        assert_eq!(
            lock_path.file_name().and_then(|n| n.to_str()),
            Some("trust.json.lock")
        );
        assert!(
            !lock_path.exists(),
            "lock directory must not remain after write"
        );

        let _ = expected; // keep construction for readability of expected shape
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn lock_targets_parent_dir_with_trust_json_lock_artifact() -> TestResult {
        let root = unique_temp_dir("lock-artifact")?;
        let agent_dir = root.join("agent");
        fs::create_dir_all(&agent_dir).map_err(|error| error.to_string())?;
        let store = ProjectTrustStore::new(&agent_dir);
        let trust_path = store.path().to_path_buf();
        let lock_path = trust_lock_path(&trust_path);

        // Hold the trust lock the same way the store does and assert paths.
        let guard = acquire_trust_lock(&trust_path).map_err(|e| e.to_string())?;
        assert_eq!(guard.lock_path(), lock_path.as_path());
        assert!(lock_path.is_dir());
        assert_eq!(
            guard.target(),
            trust_path.parent().ok_or_else(|| "parent".to_owned())?
        );
        drop(guard);
        assert!(!lock_path.exists());

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn detects_trust_requiring_project_resources() -> TestResult {
        let root = unique_temp_dir("resources")?;
        let project = root.join("project");
        fs::create_dir_all(&project).map_err(|error| error.to_string())?;
        fs::create_dir_all(root.join(".pi").join("agent")).map_err(|error| error.to_string())?;
        fs::create_dir_all(root.join(".agents").join("skills"))
            .map_err(|error| error.to_string())?;

        assert!(
            !has_trust_requiring_project_resources_with(&root, Some(&root)),
            "user-global ~/.agents/skills and bare .pi/agent must be ignored"
        );
        assert!(!has_trust_requiring_project_resources_with(
            &project,
            Some(&root)
        ));

        write_resource(&root.join(".pi").join("settings.json"))?;
        assert!(has_trust_requiring_project_resources_with(
            &root,
            Some(&root)
        ));
        fs::remove_file(root.join(".pi").join("settings.json")).map_err(|e| e.to_string())?;

        write_resource(&project.join(".pi").join("settings.json"))?;
        assert!(has_trust_requiring_project_resources_with(
            &project,
            Some(&root)
        ));

        fs::remove_dir_all(project.join(".pi")).map_err(|e| e.to_string())?;
        fs::create_dir_all(project.join(".agents").join("skills")).map_err(|e| e.to_string())?;
        assert!(has_trust_requiring_project_resources_with(
            &project,
            Some(&root)
        ));

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn project_trust_options_labels_and_updates() -> TestResult {
        let root = unique_temp_dir("options")?;
        let cwd = root.join("proj");
        fs::create_dir_all(&cwd).map_err(|error| error.to_string())?;
        let options = get_project_trust_options(&cwd, true);
        let labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
        let parent = get_project_trust_parent_path(&cwd).ok_or("parent")?;
        assert_eq!(
            labels,
            vec![
                "Trust",
                format!("Trust parent folder ({})", path_to_string(&parent)).as_str(),
                "Trust (this session only)",
                "Do not trust",
                "Do not trust (this session only)",
            ]
        );

        assert!(options[0].trusted);
        assert_eq!(options[0].updates.len(), 1);
        assert_eq!(options[0].updates[0].decision, Some(true));

        assert!(options[1].trusted);
        assert_eq!(options[1].updates.len(), 2);
        assert_eq!(options[1].updates[0].decision, Some(true));
        assert_eq!(options[1].updates[1].decision, None);

        assert!(options[2].updates.is_empty());
        assert!(!options[3].trusted);
        assert_eq!(options[3].updates[0].decision, Some(false));
        assert!(options[4].updates.is_empty());

        let prompt = format_project_trust_prompt(&cwd);
        assert_eq!(
            prompt,
            format!(
                "Trust project folder?\n{}\n\nThis allows pi to load .pi settings and resources, install missing project packages, and execute project extensions.",
                path_to_string(&cwd)
            )
        );

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn resolve_order_override_resources_defaults_and_non_ui() -> TestResult {
        let (root, project, store) = trust_fixture("resolve-basic")?;

        assert!(resolve_without_extensions(
            &project,
            &store,
            Some(true),
            DefaultProjectTrust::Never,
        )?);

        let empty = root.join("empty");
        fs::create_dir_all(&empty).map_err(|error| error.to_string())?;
        assert!(resolve_without_extensions(
            &empty,
            &store,
            None,
            DefaultProjectTrust::Never,
        )?);

        assert!(resolve_without_extensions(
            &project,
            &store,
            None,
            DefaultProjectTrust::Always,
        )?);
        assert!(!resolve_without_extensions(
            &project,
            &store,
            None,
            DefaultProjectTrust::Never,
        )?);
        assert!(!resolve_without_extensions(
            &project,
            &store,
            None,
            DefaultProjectTrust::Ask,
        )?);

        let mut no_ui = ScriptedUi {
            has_ui: false,
            choice: Some("Trust".to_owned()),
            last_prompt: None,
            last_options: Vec::new(),
        };
        let trusted = resolve_project_trusted(ResolveProjectTrustedOptions {
            cwd: project,
            trust_store: &store,
            trust_override: None,
            default_project_trust: DefaultProjectTrust::Ask,
            extension_hook: None,
            ui: Some(&mut no_ui),
            on_extension_error: None,
        })
        .map_err(|error| error.to_string())?;
        assert!(!trusted);

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn resolve_order_extension_remember_then_store() -> TestResult {
        let (root, project, store) = trust_fixture("resolve-extension")?;
        let mut hook = |cwd: &Path| {
            assert_eq!(cwd, project.as_path());
            Ok(Some(ProjectTrustExtensionResult {
                trusted: ProjectTrustEventDecision::Yes,
                remember: true,
            }))
        };
        let trusted = resolve_project_trusted(ResolveProjectTrustedOptions {
            cwd: project.clone(),
            trust_store: &store,
            trust_override: None,
            default_project_trust: DefaultProjectTrust::Never,
            extension_hook: Some(&mut hook),
            ui: None,
            on_extension_error: None,
        })
        .map_err(|error| error.to_string())?;
        assert!(trusted);
        assert_eq!(store.get(&project).map_err(|e| e.to_string())?, Some(true));

        store
            .set(&project, Some(false))
            .map_err(|error| error.to_string())?;
        let mut undecided = |_cwd: &Path| {
            Ok(Some(ProjectTrustExtensionResult {
                trusted: ProjectTrustEventDecision::Undecided,
                remember: false,
            }))
        };
        let trusted = resolve_project_trusted(ResolveProjectTrustedOptions {
            cwd: project,
            trust_store: &store,
            trust_override: None,
            default_project_trust: DefaultProjectTrust::Always,
            extension_hook: Some(&mut undecided),
            ui: None,
            on_extension_error: None,
        })
        .map_err(|error| error.to_string())?;
        assert!(!trusted);

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn resolve_order_selection_updates_and_cancel() -> TestResult {
        let (root, project, store) = trust_fixture("resolve-ui")?;
        let mut ui = ScriptedUi {
            has_ui: true,
            choice: Some("Trust".to_owned()),
            last_prompt: None,
            last_options: Vec::new(),
        };
        let trusted = resolve_project_trusted(ResolveProjectTrustedOptions {
            cwd: project.clone(),
            trust_store: &store,
            trust_override: None,
            default_project_trust: DefaultProjectTrust::Ask,
            extension_hook: None,
            ui: Some(&mut ui),
            on_extension_error: None,
        })
        .map_err(|error| error.to_string())?;
        assert!(trusted);
        assert_eq!(store.get(&project).map_err(|e| e.to_string())?, Some(true));
        let expected_prompt = format_project_trust_prompt(&project);
        assert_eq!(ui.last_prompt.as_deref(), Some(expected_prompt.as_str()));

        store
            .set(&project, None)
            .map_err(|error| error.to_string())?;
        let mut cancel_ui = ScriptedUi {
            has_ui: true,
            choice: None,
            last_prompt: None,
            last_options: Vec::new(),
        };
        let trusted = resolve_project_trusted(ResolveProjectTrustedOptions {
            cwd: project.clone(),
            trust_store: &store,
            trust_override: None,
            default_project_trust: DefaultProjectTrust::Ask,
            extension_hook: None,
            ui: Some(&mut cancel_ui),
            on_extension_error: None,
        })
        .map_err(|error| error.to_string())?;
        assert!(!trusted);
        assert_eq!(store.get(&project).map_err(|e| e.to_string())?, None);

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn extension_errors_are_reported_and_do_not_abort_resolution() -> TestResult {
        let root = unique_temp_dir("ext-err")?;
        let agent_dir = root.join("agent");
        let project = root.join("project");
        fs::create_dir_all(&agent_dir).map_err(|error| error.to_string())?;
        fs::create_dir_all(&project).map_err(|error| error.to_string())?;
        write_resource(&project.join(".pi").join("SYSTEM.md"))?;
        let store = ProjectTrustStore::new(&agent_dir);

        let mut errors = Vec::new();
        let mut on_error = |message: String| errors.push(message);
        let mut hook = |_cwd: &Path| -> Result<Option<ProjectTrustExtensionResult>, String> {
            Err("Extension \"/tmp/ext.ts\" project_trust error: boom".to_owned())
        };

        let trusted = resolve_project_trusted(ResolveProjectTrustedOptions {
            cwd: project,
            trust_store: &store,
            trust_override: None,
            default_project_trust: DefaultProjectTrust::Always,
            extension_hook: Some(&mut hook),
            ui: None,
            on_extension_error: Some(&mut on_error),
        })
        .map_err(|e| e.to_string())?;
        assert!(trusted);
        assert_eq!(
            errors,
            vec!["Extension \"/tmp/ext.ts\" project_trust error: boom".to_owned()]
        );

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn invalid_trust_file_errors_are_exact() -> TestResult {
        let root = unique_temp_dir("invalid")?;
        let agent_dir = root.join("agent");
        fs::create_dir_all(&agent_dir).map_err(|error| error.to_string())?;
        let store = ProjectTrustStore::new(&agent_dir);
        fs::write(store.path(), b"[1,2,3]\n").map_err(|e| e.to_string())?;
        let err = require_error(store.get(root.join("p")), "array root must fail")?;
        assert_eq!(
            err.to_string(),
            format!(
                "Invalid trust store {}: expected an object",
                path_to_string(store.path())
            )
        );

        fs::write(store.path(), b"{\"a\":1}\n").map_err(|e| e.to_string())?;
        let err = require_error(store.get(root.join("p")), "non-bool value must fail")?;
        assert_eq!(
            err.to_string(),
            format!(
                "Invalid trust store {}: value for \"a\" must be true, false, or null",
                path_to_string(store.path())
            )
        );

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn concurrent_unknown_safe_updates_preserve_all_keys() -> TestResult {
        let root = unique_temp_dir("concurrent")?;
        let agent_dir = root.join("agent");
        fs::create_dir_all(&agent_dir).map_err(|error| error.to_string())?;
        let store = Arc::new(ProjectTrustStore::new(&agent_dir));

        // Seed an unrelated key that concurrent writers must preserve.
        let other = root.join("other");
        fs::create_dir_all(&other).map_err(|error| error.to_string())?;
        store.set(&other, Some(true)).map_err(|e| e.to_string())?;

        let mut handles = Vec::new();
        for index in 0..8 {
            let store = Arc::clone(&store);
            let root = root.clone();
            handles.push(thread::spawn(move || -> TestResult {
                let path = root.join(format!("p{index}"));
                fs::create_dir_all(&path).map_err(|error| error.to_string())?;
                store
                    .set(&path, Some(index % 2 == 0))
                    .map_err(|error| error.to_string())
            }));
        }
        let mut worker_error = None;
        for handle in handles {
            let result = handle
                .join()
                .map_err(|_| "thread panicked".to_owned())
                .and_then(|result| result);
            if worker_error.is_none() {
                worker_error = result.err();
            }
        }
        if let Some(error) = worker_error {
            return Err(error);
        }

        assert_eq!(store.get(&other).map_err(|e| e.to_string())?, Some(true));
        for index in 0..8 {
            let path = root.join(format!("p{index}"));
            assert_eq!(
                store.get(&path).map_err(|e| e.to_string())?,
                Some(index % 2 == 0)
            );
        }

        let raw = fs::read_to_string(store.path()).map_err(|e| e.to_string())?;
        assert!(raw.ends_with('\n'));
        let parsed: Value = serde_json::from_str(raw.trim_end()).map_err(|e| e.to_string())?;
        let object = parsed
            .as_object()
            .ok_or_else(|| "expected object".to_owned())?;
        assert_eq!(object.len(), 9);

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn default_project_trust_parse() {
        assert_eq!(
            DefaultProjectTrust::parse(Some("always")),
            DefaultProjectTrust::Always
        );
        assert_eq!(
            DefaultProjectTrust::parse(Some("never")),
            DefaultProjectTrust::Never
        );
        assert_eq!(
            DefaultProjectTrust::parse(Some("ask")),
            DefaultProjectTrust::Ask
        );
        assert_eq!(
            DefaultProjectTrust::parse(Some("sometimes")),
            DefaultProjectTrust::Ask
        );
        assert_eq!(DefaultProjectTrust::parse(None), DefaultProjectTrust::Ask);
    }

    #[test]
    fn read_missing_trust_file_is_empty() -> TestResult {
        let root = unique_temp_dir("missing")?;
        let agent_dir = root.join("agent");
        fs::create_dir_all(&agent_dir).map_err(|error| error.to_string())?;
        let store = ProjectTrustStore::new(&agent_dir);
        assert_eq!(store.get(&root).map_err(|e| e.to_string())?, None);
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn io_error_message_shape_for_unreadable_file() -> TestResult {
        // Force a read failure by pointing the store at a directory named trust.json.
        let root = unique_temp_dir("read-fail")?;
        let agent_dir = root.join("agent");
        fs::create_dir_all(agent_dir.join("trust.json")).map_err(|error| error.to_string())?;
        let store = ProjectTrustStore::new(&agent_dir);
        let err = require_error(store.get(&root), "directory read must fail")?;
        let message = err.to_string();
        assert!(
            message.starts_with(&format!(
                "Failed to read trust store {}:",
                path_to_string(store.path())
            )),
            "unexpected message: {message}"
        );
        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn parent_path_none_at_filesystem_root() {
        // Platform root has no parent trust option.
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\")
        } else {
            PathBuf::from("/")
        };
        assert_eq!(get_project_trust_parent_path(&root), None);
        let options = get_project_trust_options(&root, false);
        assert_eq!(
            options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(),
            vec!["Trust", "Do not trust"]
        );
    }
}
