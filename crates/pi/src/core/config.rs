//! Product path layout, package identity, and environment constants.
//!
//! Port of `coding-agent/src/config.ts` path/env surfaces plus the full
//! `utils/paths.ts` helpers those surfaces depend on (`PathInputOptions`,
//! normalize/resolve/canonicalize, cwd-relative formatting). Install and
//! self-update detection is intentionally omitted until a callsite needs it.

use std::borrow::Cow;
use std::env;
use std::path::{Component, Path, PathBuf};

use url::Url;

/// npm package name used by the TypeScript coding-agent package.
pub const PACKAGE_NAME: &str = "@earendil-works/pi-coding-agent";

/// Short application name used for env prefixes and file names.
pub const APP_NAME: &str = "pi";

/// Display title used when no custom `piConfig.name` is configured.
pub const APP_TITLE: &str = "π";
/// Rust package version exposed through the product config surface.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Project-local config directory name (for example `{cwd}/.pi`).
pub const CONFIG_DIR_NAME: &str = ".pi";

/// Whether this build is an official distribution.
///
/// The TypeScript reference checks the runtime `package.json` identity
/// (`@earendil-works/pi-coding-agent`, app name `pi`, config dir `.pi`). In
/// the native binary these are compile-time constants baked into the crate,
/// so this is resolved without filesystem I/O. First-run setup and any other
/// official-only surface gate on it.
#[must_use]
pub fn is_official_distribution() -> bool {
    PACKAGE_NAME == "@earendil-works/pi-coding-agent"
        && APP_NAME == "pi"
        && CONFIG_DIR_NAME == ".pi"
}

/// Environment variable that overrides the agent config directory.
pub const ENV_AGENT_DIR: &str = "PI_CODING_AGENT_DIR";

/// Environment variable that overrides the session root directory.
pub const ENV_SESSION_DIR: &str = "PI_CODING_AGENT_SESSION_DIR";

/// Environment variable that overrides the shipped package asset root.
pub const ENV_PACKAGE_DIR: &str = "PI_PACKAGE_DIR";

/// Environment variable that overrides the share viewer base URL.
pub const ENV_SHARE_VIEWER_URL: &str = "PI_SHARE_VIEWER_URL";

/// Default share viewer base URL, including the trailing slash.
pub const DEFAULT_SHARE_VIEWER_URL: &str = "https://pi.dev/session/";

/// Options controlling path normalization.
///
/// Mirrors TypeScript `PathInputOptions`. [`PathInputOptions::new`] and
/// [`Default`] both default `expand_tilde` to `true`.
#[derive(Clone, Copy, Debug)]
pub struct PathInputOptions<'a> {
    trim_flag: u8,
    expand_tilde_flag: u8,
    /// Home directory used for `~` expansion. When `None` and expansion is
    /// enabled, the process home directory is used.
    pub home_dir: Option<&'a Path>,
    strip_at_prefix_flag: u8,
    normalize_unicode_spaces_flag: u8,
}

impl Default for PathInputOptions<'_> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> PathInputOptions<'a> {
    /// Defaults matching TypeScript `normalizePath` option defaults.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            trim_flag: 0,
            expand_tilde_flag: 1,
            home_dir: None,
            strip_at_prefix_flag: 0,
            normalize_unicode_spaces_flag: 0,
        }
    }

    /// Set whether leading/trailing whitespace is trimmed.
    #[must_use]
    pub const fn trim(mut self, trim: bool) -> Self {
        self.trim_flag = if trim { 1 } else { 0 };
        self
    }

    /// Return whether leading/trailing whitespace is trimmed.
    #[must_use]
    pub const fn trims_input(self) -> bool {
        self.trim_flag != 0
    }

    /// Set whether a leading `~` is expanded.
    #[must_use]
    pub const fn expand_tilde(mut self, expand_tilde: bool) -> Self {
        self.expand_tilde_flag = if expand_tilde { 1 } else { 0 };
        self
    }

    /// Return whether a leading `~` is expanded.
    #[must_use]
    pub const fn expands_tilde(self) -> bool {
        self.expand_tilde_flag != 0
    }

    /// Set the home directory used for `~` expansion.
    #[must_use]
    pub const fn home_dir(mut self, home_dir: Option<&'a Path>) -> Self {
        self.home_dir = home_dir;
        self
    }

    /// Set whether a leading `@` is stripped.
    #[must_use]
    pub const fn strip_at_prefix(mut self, strip_at_prefix: bool) -> Self {
        self.strip_at_prefix_flag = if strip_at_prefix { 1 } else { 0 };
        self
    }

    /// Return whether a leading `@` is stripped.
    #[must_use]
    pub const fn strips_at_prefix(self) -> bool {
        self.strip_at_prefix_flag != 0
    }

    /// Set whether Unicode space variants are normalized.
    #[must_use]
    pub const fn normalize_unicode_spaces(mut self, normalize_unicode_spaces: bool) -> Self {
        self.normalize_unicode_spaces_flag = if normalize_unicode_spaces { 1 } else { 0 };
        self
    }

    /// Return whether Unicode space variants are normalized.
    #[must_use]
    pub const fn normalizes_unicode_spaces(self) -> bool {
        self.normalize_unicode_spaces_flag != 0
    }
}

/// Expand a leading bare `~` using process home, matching `expandTildePath`.
#[must_use]
pub fn expand_tilde_path(path: impl AsRef<str>) -> PathBuf {
    expand_tilde_path_with(path.as_ref(), process_home_dir().as_deref())
}

/// Expand a leading bare `~` using an explicit home directory.
///
/// Only `~`, `~/…`, and (on Windows) `~\…` are expanded. `~user` is left
/// unchanged, matching the TypeScript helper.
#[must_use]
pub fn expand_tilde_path_with(path: &str, home_dir: Option<&Path>) -> PathBuf {
    normalize_path(path, PathInputOptions::new().home_dir(home_dir))
}

/// Normalize a path string according to [`PathInputOptions`].
///
/// Order matches TypeScript `normalizePath`:
/// 1. optional trim
/// 2. optional Unicode-space normalization
/// 3. optional leading-`@` strip
/// 4. optional tilde expansion (`~`, `~/`, Windows `~\` only — not `~user`)
/// 5. `file://` conversion via WHATWG `Url::to_file_path`
///
/// Relative paths stay relative. This does not join against a base directory;
/// use [`resolve_path`] / [`resolve_path_with`] for that.
#[must_use]
pub fn normalize_path(input: &str, options: PathInputOptions<'_>) -> PathBuf {
    let mut normalized = if options.trims_input() {
        input.trim().to_owned()
    } else {
        input.to_owned()
    };

    if options.normalizes_unicode_spaces() {
        normalized = replace_unicode_spaces(&normalized);
    }

    if options.strips_at_prefix() && normalized.starts_with('@') {
        normalized.remove(0);
    }

    if options.expands_tilde() {
        let home = options.home_dir.map_or_else(
            || process_home_dir().map(Cow::Owned),
            |home| Some(Cow::Borrowed(home)),
        );
        if let Some(home) = home.as_deref() {
            if normalized == "~" {
                return home.to_path_buf();
            }
            if let Some(rest) = normalized.strip_prefix("~/") {
                return home.join(rest);
            }
            if cfg!(windows)
                && let Some(rest) = normalized.strip_prefix("~\\")
            {
                return home.join(rest);
            }
        }
    }

    if normalized.starts_with("file://")
        && let Ok(url) = Url::parse(&normalized)
        && let Ok(path) = url.to_file_path()
    {
        return path;
    }

    PathBuf::from(normalized)
}

/// Returns true when `value` is not a remote package source or URL protocol.
///
/// Bare names, relative paths, and `file:` URLs are local. Matches
/// TypeScript `isLocalPath`.
#[must_use]
pub fn is_local_path(value: &str) -> bool {
    let trimmed = value.trim();
    !(trimmed.starts_with("npm:")
        || trimmed.starts_with("git:")
        || trimmed.starts_with("github:")
        || trimmed.starts_with("http:")
        || trimmed.starts_with("https:")
        || trimmed.starts_with("ssh:"))
}

/// Resolve `input` against the process working directory after normalization.
#[must_use]
pub fn resolve_path(input: impl AsRef<str>) -> PathBuf {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve_path_with(
        input.as_ref(),
        &cwd,
        PathInputOptions::new().home_dir(process_home_dir().as_deref()),
    )
}

/// Resolve `input` against `base_dir` after normalization.
///
/// Both `input` and `base_dir` are normalized (base uses default option flags
/// with the same home directory seam as `options`). Absolute inputs ignore
/// the base; relative inputs are joined and cleaned like Node `path.resolve`.
#[must_use]
pub fn resolve_path_with(input: &str, base_dir: &Path, options: PathInputOptions<'_>) -> PathBuf {
    let normalized = normalize_path(input, options);
    // TypeScript always normalizes the base with default options (tilde on).
    let base_options = PathInputOptions::new().home_dir(options.home_dir);
    let normalized_base = if let Some(base_str) = base_dir.to_str() {
        normalize_path(base_str, base_options)
    } else {
        base_dir.to_path_buf()
    };

    if normalized.is_absolute() {
        resolve_like_node(None, &normalized)
    } else {
        resolve_like_node(Some(&normalized_base), &normalized)
    }
}

/// Canonicalize `path` by following symlinks.
///
/// Falls back to the original path when the target does not exist or cannot
/// be resolved, matching TypeScript `canonicalizePath`.
#[must_use]
pub fn canonicalize_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    match path.canonicalize() {
        Ok(canonical) => canonical,
        Err(_) => path.to_path_buf(),
    }
}

/// Return a cwd-relative path when `file_path` is inside `cwd`.
///
/// Matches TypeScript `getCwdRelativePath`: returns `None` when the path is
/// outside `cwd`, `"."` when equal, otherwise a relative path using the
/// platform separator.
#[must_use]
pub fn get_cwd_relative_path(file_path: impl AsRef<str>, cwd: impl AsRef<str>) -> Option<PathBuf> {
    get_cwd_relative_path_with(
        file_path.as_ref(),
        cwd.as_ref(),
        process_home_dir().as_deref(),
    )
}

/// [`get_cwd_relative_path`] with an explicit home directory seam.
#[must_use]
pub fn get_cwd_relative_path_with(
    file_path: &str,
    cwd: &str,
    home_dir: Option<&Path>,
) -> Option<PathBuf> {
    let options = PathInputOptions::new().home_dir(home_dir);
    let process_cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // TypeScript: resolvePath(cwd) uses process.cwd() as the default base.
    let resolved_cwd = resolve_path_with(cwd, &process_cwd, options);
    let resolved_path = resolve_path_with(file_path, &resolved_cwd, options);
    let relative_path = pathdiff_relative(&resolved_cwd, &resolved_path);

    let relative_str = relative_path.to_string_lossy();
    let is_inside_cwd = relative_str.is_empty()
        || (relative_str != ".."
            && !relative_str.starts_with(&format!("..{}", std::path::MAIN_SEPARATOR))
            && !relative_path.is_absolute());

    if !is_inside_cwd {
        return None;
    }
    if relative_str.is_empty() {
        Some(PathBuf::from("."))
    } else {
        Some(relative_path)
    }
}

/// Format `file_path` relative to `cwd` when possible, otherwise absolute.
///
/// Path separators in the result are always `/`, matching TypeScript
/// `formatPathRelativeToCwdOrAbsolute`.
#[must_use]
pub fn format_path_relative_to_cwd_or_absolute(
    file_path: impl AsRef<str>,
    cwd: impl AsRef<str>,
) -> String {
    format_path_relative_to_cwd_or_absolute_with(
        file_path.as_ref(),
        cwd.as_ref(),
        process_home_dir().as_deref(),
    )
}

/// [`format_path_relative_to_cwd_or_absolute`] with an explicit home seam.
#[must_use]
pub fn format_path_relative_to_cwd_or_absolute_with(
    file_path: &str,
    cwd: &str,
    home_dir: Option<&Path>,
) -> String {
    let options = PathInputOptions::new().home_dir(home_dir);
    let absolute_path = resolve_path_with(file_path, Path::new(cwd), options);
    let absolute_str = absolute_path.to_string_lossy();
    let display = get_cwd_relative_path_with(absolute_str.as_ref(), cwd, home_dir).map_or_else(
        || absolute_str.into_owned(),
        |path| path.to_string_lossy().into_owned(),
    );
    display.replace('\\', "/")
}

/// Resolve the shipped package asset root from process environment and
/// `current_exe`.
#[must_use]
pub fn get_package_dir() -> PathBuf {
    get_package_dir_with(
        env::var_os(ENV_PACKAGE_DIR).map(PathBuf::from).as_deref(),
        env::current_exe().ok().as_deref(),
        process_home_dir().as_deref(),
    )
}

/// Resolve the shipped package asset root with explicit seams.
///
/// Precedence:
/// 1. `package_dir_override` (from `PI_PACKAGE_DIR`), tilde-expanded
/// 2. parent directory of `executable`
/// 3. empty path (caller should treat as unresolved)
#[must_use]
pub fn get_package_dir_with(
    package_dir_override: Option<&Path>,
    executable: Option<&Path>,
    home_dir: Option<&Path>,
) -> PathBuf {
    if let Some(override_dir) = package_dir_override {
        if let Some(text) = override_dir.to_str() {
            return expand_tilde_path_with(text, home_dir);
        }
        return override_dir.to_path_buf();
    }

    if let Some(exe) = executable {
        if let Some(parent) = exe.parent()
            && !parent.as_os_str().is_empty()
        {
            return parent.to_path_buf();
        }
        return PathBuf::from(".");
    }

    PathBuf::new()
}

/// Path to shipped themes for the native binary layout (`{package}/theme`).
#[must_use]
pub fn get_themes_dir() -> PathBuf {
    get_themes_dir_with(&get_package_dir())
}

/// Path to shipped themes under an explicit package directory.
#[must_use]
pub fn get_themes_dir_with(package_dir: &Path) -> PathBuf {
    package_dir.join("theme")
}

/// Path to the HTML export template directory (`{package}/export-html`).
#[must_use]
pub fn get_export_template_dir() -> PathBuf {
    get_export_template_dir_with(&get_package_dir())
}

/// Path to the HTML export template directory under an explicit package root.
#[must_use]
pub fn get_export_template_dir_with(package_dir: &Path) -> PathBuf {
    package_dir.join("export-html")
}

/// Path to `package.json` beside the package root.
#[must_use]
pub fn get_package_json_path() -> PathBuf {
    get_package_json_path_with(&get_package_dir())
}

/// Path to `package.json` under an explicit package root.
#[must_use]
pub fn get_package_json_path_with(package_dir: &Path) -> PathBuf {
    package_dir.join("package.json")
}

/// Absolute-resolved path to shipped `README.md`.
#[must_use]
pub fn get_readme_path() -> PathBuf {
    get_readme_path_with(&get_package_dir())
}

/// Absolute-resolved path to `README.md` under an explicit package root.
#[must_use]
pub fn get_readme_path_with(package_dir: &Path) -> PathBuf {
    resolve_existing_join(package_dir, "README.md")
}

/// Absolute-resolved path to the shipped `docs` directory.
#[must_use]
pub fn get_docs_path() -> PathBuf {
    get_docs_path_with(&get_package_dir())
}

/// Absolute-resolved path to `docs` under an explicit package root.
#[must_use]
pub fn get_docs_path_with(package_dir: &Path) -> PathBuf {
    resolve_existing_join(package_dir, "docs")
}

/// Absolute-resolved path to the shipped `examples` directory.
#[must_use]
pub fn get_examples_path() -> PathBuf {
    get_examples_path_with(&get_package_dir())
}

/// Absolute-resolved path to `examples` under an explicit package root.
#[must_use]
pub fn get_examples_path_with(package_dir: &Path) -> PathBuf {
    resolve_existing_join(package_dir, "examples")
}

/// Absolute-resolved path to shipped `CHANGELOG.md`.
#[must_use]
pub fn get_changelog_path() -> PathBuf {
    get_changelog_path_with(&get_package_dir())
}

/// Absolute-resolved path to `CHANGELOG.md` under an explicit package root.
#[must_use]
pub fn get_changelog_path_with(package_dir: &Path) -> PathBuf {
    resolve_existing_join(package_dir, "CHANGELOG.md")
}

/// Path to built-in interactive assets (`{package}/assets`).
#[must_use]
pub fn get_interactive_assets_dir() -> PathBuf {
    get_interactive_assets_dir_with(&get_package_dir())
}

/// Path to built-in interactive assets under an explicit package root.
#[must_use]
pub fn get_interactive_assets_dir_with(package_dir: &Path) -> PathBuf {
    package_dir.join("assets")
}

/// Path to a single bundled interactive asset.
#[must_use]
pub fn get_bundled_interactive_asset_path(name: impl AsRef<Path>) -> PathBuf {
    get_bundled_interactive_asset_path_with(&get_package_dir(), name.as_ref())
}

/// Path to a single bundled interactive asset under an explicit package root.
#[must_use]
pub fn get_bundled_interactive_asset_path_with(package_dir: &Path, name: &Path) -> PathBuf {
    get_interactive_assets_dir_with(package_dir).join(name)
}

/// Agent config directory from process environment and home directory.
#[must_use]
pub fn get_agent_dir() -> PathBuf {
    get_agent_dir_with(
        env::var_os(ENV_AGENT_DIR)
            .and_then(|value| value.into_string().ok())
            .as_deref(),
        process_home_dir().as_deref(),
    )
}

/// Agent config directory with explicit env/home seams.
///
/// When `env_agent_dir` is set it is tilde-expanded; otherwise the default is
/// `{home}/{CONFIG_DIR_NAME}/agent` (for example `~/.pi/agent`).
#[must_use]
pub fn get_agent_dir_with(env_agent_dir: Option<&str>, home_dir: Option<&Path>) -> PathBuf {
    if let Some(env_dir) = env_agent_dir {
        return expand_tilde_path_with(env_dir, home_dir);
    }
    match home_dir {
        Some(home) => home.join(CONFIG_DIR_NAME).join("agent"),
        None => PathBuf::from(CONFIG_DIR_NAME).join("agent"),
    }
}

/// User custom themes directory (`{agent}/themes`).
#[must_use]
pub fn get_custom_themes_dir() -> PathBuf {
    get_custom_themes_dir_with(&get_agent_dir())
}

/// User custom themes directory under an explicit agent directory.
#[must_use]
pub fn get_custom_themes_dir_with(agent_dir: &Path) -> PathBuf {
    agent_dir.join("themes")
}

/// Path to `models.json`.
#[must_use]
pub fn get_models_path() -> PathBuf {
    get_models_path_with(&get_agent_dir())
}

/// Path to `models.json` under an explicit agent directory.
#[must_use]
pub fn get_models_path_with(agent_dir: &Path) -> PathBuf {
    agent_dir.join("models.json")
}

/// Path to `auth.json`.
#[must_use]
pub fn get_auth_path() -> PathBuf {
    get_auth_path_with(&get_agent_dir())
}

/// Path to `auth.json` under an explicit agent directory.
#[must_use]
pub fn get_auth_path_with(agent_dir: &Path) -> PathBuf {
    agent_dir.join("auth.json")
}

/// Path to `settings.json`.
#[must_use]
pub fn get_settings_path() -> PathBuf {
    get_settings_path_with(&get_agent_dir())
}

/// Path to `settings.json` under an explicit agent directory.
#[must_use]
pub fn get_settings_path_with(agent_dir: &Path) -> PathBuf {
    agent_dir.join("settings.json")
}

/// Path to the tools directory.
#[must_use]
pub fn get_tools_dir() -> PathBuf {
    get_tools_dir_with(&get_agent_dir())
}

/// Path to the tools directory under an explicit agent directory.
#[must_use]
pub fn get_tools_dir_with(agent_dir: &Path) -> PathBuf {
    agent_dir.join("tools")
}

/// Path to the managed binaries directory.
#[must_use]
pub fn get_bin_dir() -> PathBuf {
    get_bin_dir_with(&get_agent_dir())
}

/// Path to the managed binaries directory under an explicit agent directory.
#[must_use]
pub fn get_bin_dir_with(agent_dir: &Path) -> PathBuf {
    agent_dir.join("bin")
}

/// Path to the prompt templates directory.
#[must_use]
pub fn get_prompts_dir() -> PathBuf {
    get_prompts_dir_with(&get_agent_dir())
}

/// Path to the prompt templates directory under an explicit agent directory.
#[must_use]
pub fn get_prompts_dir_with(agent_dir: &Path) -> PathBuf {
    agent_dir.join("prompts")
}

/// Path to the sessions directory.
#[must_use]
pub fn get_sessions_dir() -> PathBuf {
    get_sessions_dir_with(&get_agent_dir())
}

/// Path to the sessions directory under an explicit agent directory.
#[must_use]
pub fn get_sessions_dir_with(agent_dir: &Path) -> PathBuf {
    agent_dir.join("sessions")
}

/// Path to the debug log file (`{agent}/{APP_NAME}-debug.log`).
#[must_use]
pub fn get_debug_log_path() -> PathBuf {
    get_debug_log_path_with(&get_agent_dir())
}

/// Path to the debug log file under an explicit agent directory.
#[must_use]
pub fn get_debug_log_path_with(agent_dir: &Path) -> PathBuf {
    agent_dir.join(format!("{APP_NAME}-debug.log"))
}

/// Share viewer URL for `gist_id` using process environment.
#[must_use]
pub fn get_share_viewer_url(gist_id: impl AsRef<str>) -> String {
    get_share_viewer_url_with(
        gist_id.as_ref(),
        env::var(ENV_SHARE_VIEWER_URL).ok().as_deref(),
    )
}

/// Share viewer URL for `gist_id` with an explicit base override.
///
/// Wire shape is `{base}#{gistId}` with no extra slash insertion. Missing
/// override falls back to [`DEFAULT_SHARE_VIEWER_URL`].
#[must_use]
pub fn get_share_viewer_url_with(gist_id: &str, base_override: Option<&str>) -> String {
    let base = base_override.unwrap_or(DEFAULT_SHARE_VIEWER_URL);
    format!("{base}#{gist_id}")
}

fn process_home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

fn resolve_existing_join(package_dir: &Path, child: &str) -> PathBuf {
    let joined = package_dir.join(child);
    resolve_like_node(None, &joined)
}

fn replace_unicode_spaces(input: &str) -> String {
    input
        .chars()
        .map(|ch| match ch {
            '\u{00A0}' | '\u{2000}' | '\u{2001}' | '\u{2002}' | '\u{2003}' | '\u{2004}'
            | '\u{2005}' | '\u{2006}' | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}'
            | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// Clean `.` / `..` like Node `path.resolve` output (absolute-aware).
fn normalize_dot_segments(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut absolute = false;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                out.push(prefix.as_os_str());
            }
            Component::RootDir => {
                out.push(component.as_os_str());
                absolute = true;
            }
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::ParentDir) | None if !absolute => {
                    out.push("..");
                }
                _ => {}
            },
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        if absolute {
            PathBuf::from(std::path::MAIN_SEPARATOR.to_string())
        } else {
            PathBuf::from(".")
        }
    } else {
        out
    }
}

/// Approximate Node `path.resolve` for one optional base and one path.
fn resolve_like_node(base: Option<&Path>, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(base) = base {
        if base.is_absolute() {
            base.join(path)
        } else if let Ok(cwd) = env::current_dir() {
            cwd.join(base).join(path)
        } else {
            base.join(path)
        }
    } else if let Ok(cwd) = env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    };
    normalize_dot_segments(&joined)
}

/// Approximate Node `path.relative(from, to)`.
fn pathdiff_relative(from: &Path, to: &Path) -> PathBuf {
    let from_components: Vec<Component<'_>> = from.components().collect();
    let to_components: Vec<Component<'_>> = to.components().collect();

    let mut common = 0usize;
    while common < from_components.len()
        && common < to_components.len()
        && from_components[common] == to_components[common]
    {
        common += 1;
    }

    let mut out = PathBuf::new();
    for component in from_components.iter().skip(common) {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir | Component::Normal(_) => out.push(".."),
        }
    }
    for component in to_components.iter().skip(common) {
        out.push(component.as_os_str());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    type TestResult = Result<(), String>;

    fn unique_temp_dir(label: &str) -> Result<PathBuf, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!(
            "pi-config-{label}-{}-{nanos}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        Ok(dir)
    }

    #[test]
    fn constants_match_reference_defaults() {
        assert_eq!(PACKAGE_NAME, "@earendil-works/pi-coding-agent");
        assert_eq!(APP_NAME, "pi");
        assert_eq!(APP_TITLE, "π");
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        assert_eq!(CONFIG_DIR_NAME, ".pi");
        assert_eq!(ENV_AGENT_DIR, "PI_CODING_AGENT_DIR");
        assert_eq!(ENV_SESSION_DIR, "PI_CODING_AGENT_SESSION_DIR");
        assert_eq!(ENV_PACKAGE_DIR, "PI_PACKAGE_DIR");
        assert_eq!(ENV_SHARE_VIEWER_URL, "PI_SHARE_VIEWER_URL");
        assert_eq!(DEFAULT_SHARE_VIEWER_URL, "https://pi.dev/session/");
    }

    #[test]
    fn agent_dir_defaults_under_home_config() -> TestResult {
        let home = unique_temp_dir("home")?;
        let agent = get_agent_dir_with(None, Some(&home));
        assert_eq!(agent, home.join(".pi").join("agent"));
        let _ = fs::remove_dir_all(home);
        Ok(())
    }

    #[test]
    fn agent_dir_env_override_is_tilde_expanded() -> TestResult {
        let home = unique_temp_dir("home-override")?;
        let agent = get_agent_dir_with(Some("~/custom-agent"), Some(&home));
        assert_eq!(agent, home.join("custom-agent"));

        let explicit = unique_temp_dir("explicit-agent")?;
        let explicit_str = explicit.to_string_lossy().into_owned();
        let absolute = get_agent_dir_with(Some(&explicit_str), Some(&home));
        assert_eq!(absolute, explicit);

        let _ = fs::remove_dir_all(home);
        let _ = fs::remove_dir_all(explicit);
        Ok(())
    }

    #[test]
    fn expand_tilde_only_handles_bare_home() -> TestResult {
        let home = unique_temp_dir("tilde")?;
        assert_eq!(expand_tilde_path_with("~", Some(&home)), home);
        assert_eq!(
            expand_tilde_path_with("~/agent", Some(&home)),
            home.join("agent")
        );
        assert_eq!(
            expand_tilde_path_with("~user/agent", Some(&home)),
            PathBuf::from("~user/agent")
        );

        let abs = unique_temp_dir("abs-path")?;
        let abs_str = abs.to_string_lossy().into_owned();
        assert_eq!(expand_tilde_path_with(&abs_str, Some(&home)), abs);

        let _ = fs::remove_dir_all(home);
        let _ = fs::remove_dir_all(abs);
        Ok(())
    }

    #[test]
    fn package_dir_override_and_executable_fallback() -> TestResult {
        let home = unique_temp_dir("pkg-home")?;
        let override_dir = unique_temp_dir("pkg-override")?;
        let exe_parent = unique_temp_dir("exe-parent")?;
        let exe = exe_parent.join("pi");

        let from_override = get_package_dir_with(Some(&override_dir), Some(&exe), Some(&home));
        assert_eq!(from_override, override_dir);

        let tilde_override =
            get_package_dir_with(Some(Path::new("~/packaged")), Some(&exe), Some(&home));
        assert_eq!(tilde_override, home.join("packaged"));

        let from_exe = get_package_dir_with(None, Some(&exe), Some(&home));
        assert_eq!(from_exe, exe_parent);

        let missing = get_package_dir_with(None, None, Some(&home));
        assert_eq!(missing, PathBuf::new());

        let _ = fs::remove_dir_all(home);
        let _ = fs::remove_dir_all(override_dir);
        let _ = fs::remove_dir_all(exe_parent);
        Ok(())
    }

    #[test]
    fn package_child_layout_matches_binary_shipped_paths() -> TestResult {
        let package = unique_temp_dir("package-layout")?;
        assert_eq!(get_themes_dir_with(&package), package.join("theme"));
        assert_eq!(
            get_export_template_dir_with(&package),
            package.join("export-html")
        );
        assert_eq!(
            get_package_json_path_with(&package),
            package.join("package.json")
        );
        assert_eq!(
            get_interactive_assets_dir_with(&package),
            package.join("assets")
        );
        assert_eq!(
            get_bundled_interactive_asset_path_with(&package, Path::new("icon.png")),
            package.join("assets").join("icon.png")
        );

        let readme = get_readme_path_with(&package);
        assert!(readme.ends_with("README.md"));
        assert!(readme.is_absolute());
        assert_eq!(
            readme.file_name().and_then(|name| name.to_str()),
            Some("README.md")
        );
        assert!(get_docs_path_with(&package).ends_with("docs"));
        assert!(get_examples_path_with(&package).ends_with("examples"));
        assert!(get_changelog_path_with(&package).ends_with("CHANGELOG.md"));

        let _ = fs::remove_dir_all(package);
        Ok(())
    }

    #[test]
    fn agent_child_layout_matches_reference() -> TestResult {
        let agent = unique_temp_dir("agent-layout")?;
        assert_eq!(get_custom_themes_dir_with(&agent), agent.join("themes"));
        assert_eq!(get_models_path_with(&agent), agent.join("models.json"));
        assert_eq!(get_auth_path_with(&agent), agent.join("auth.json"));
        assert_eq!(get_settings_path_with(&agent), agent.join("settings.json"));
        assert_eq!(get_tools_dir_with(&agent), agent.join("tools"));
        assert_eq!(get_bin_dir_with(&agent), agent.join("bin"));
        assert_eq!(get_prompts_dir_with(&agent), agent.join("prompts"));
        assert_eq!(get_sessions_dir_with(&agent), agent.join("sessions"));
        assert_eq!(get_debug_log_path_with(&agent), agent.join("pi-debug.log"));
        let _ = fs::remove_dir_all(agent);
        Ok(())
    }

    #[test]
    fn share_viewer_url_uses_fragment_and_override() {
        assert_eq!(
            get_share_viewer_url_with("abc123", None),
            "https://pi.dev/session/#abc123"
        );
        assert_eq!(
            get_share_viewer_url_with("gist-id", Some("https://example.test/view/")),
            "https://example.test/view/#gist-id"
        );
        // No slash is injected between base and fragment.
        assert_eq!(
            get_share_viewer_url_with("x", Some("https://example.test/view")),
            "https://example.test/view#x"
        );
    }

    #[test]
    fn canonicalize_falls_back_for_missing_path() -> TestResult {
        let root = unique_temp_dir("canon")?;
        let missing = root.join("does-not-exist").join("nested");
        let canonical = canonicalize_path(&missing);
        assert_eq!(canonical, missing);

        let existing = root.join("present.txt");
        fs::write(&existing, b"ok").map_err(|error| error.to_string())?;
        let canonical_existing = canonicalize_path(&existing);
        assert!(canonical_existing.is_absolute());
        assert_eq!(
            canonical_existing
                .file_name()
                .and_then(|name| name.to_str()),
            Some("present.txt")
        );

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn resolve_path_joins_relative_against_base() -> TestResult {
        let base = unique_temp_dir("resolve-base")?;
        let home = unique_temp_dir("resolve-home")?;
        let abs_root = unique_temp_dir("resolve-abs")?;
        let abs_file = abs_root.join("abs.txt");
        let abs_str = abs_file.to_string_lossy().into_owned();

        let resolved = resolve_path_with(
            "child/file.txt",
            &base,
            PathInputOptions::new().home_dir(Some(&home)),
        );
        assert_eq!(resolved, base.join("child").join("file.txt"));

        let absolute = resolve_path_with(
            &abs_str,
            &base,
            PathInputOptions::new().home_dir(Some(&home)),
        );
        assert_eq!(absolute, abs_file);

        let tilde = resolve_path_with(
            "~/rel.txt",
            &base,
            PathInputOptions::new().home_dir(Some(&home)),
        );
        assert_eq!(tilde, home.join("rel.txt"));

        let _ = fs::remove_dir_all(base);
        let _ = fs::remove_dir_all(home);
        let _ = fs::remove_dir_all(abs_root);
        Ok(())
    }

    #[test]
    fn normalize_path_options_and_file_url() -> TestResult {
        let home = unique_temp_dir("normalize-home")?;
        let strip_target = unique_temp_dir("strip-target")?.join("file.txt");
        let strip_str = strip_target.to_string_lossy().into_owned();

        let trimmed = normalize_path(
            "  ~/agent  ",
            PathInputOptions::new().trim(true).home_dir(Some(&home)),
        );
        assert_eq!(trimmed, home.join("agent"));

        let at_prefixed = format!("@{strip_str}");
        let stripped = normalize_path(
            &at_prefixed,
            PathInputOptions::new()
                .strip_at_prefix(true)
                .expand_tilde(false),
        );
        assert_eq!(stripped, strip_target);

        let unicode = normalize_path(
            "a\u{00A0}b\u{2003}c\u{3000}d",
            PathInputOptions::new()
                .normalize_unicode_spaces(true)
                .expand_tilde(false),
        );
        assert_eq!(unicode, PathBuf::from("a b c d"));

        // Relative paths stay relative.
        let relative = normalize_path("rel/path", PathInputOptions::new().expand_tilde(false));
        assert_eq!(relative, PathBuf::from("rel/path"));
        assert!(!relative.is_absolute());

        // ~user is not expanded.
        assert_eq!(
            normalize_path("~user/x", PathInputOptions::new().home_dir(Some(&home))),
            PathBuf::from("~user/x")
        );

        let file_target = unique_temp_dir("file-url")?.join("example file.txt");
        let file_url = Url::from_file_path(&file_target)
            .map_err(|()| "failed to build file URL".to_owned())?
            .to_string();
        let decoded = normalize_path(&file_url, PathInputOptions::new().expand_tilde(false));
        assert_eq!(decoded, file_target);

        let _ = fs::remove_dir_all(home);
        if let Some(parent) = strip_target.parent() {
            let _ = fs::remove_dir_all(parent);
        }
        if let Some(parent) = file_target.parent() {
            let _ = fs::remove_dir_all(parent);
        }
        Ok(())
    }

    #[test]
    fn cwd_relative_and_format_helpers() -> TestResult {
        let root = unique_temp_dir("cwd-rel")?;
        let outside_root = unique_temp_dir("cwd-outside")?;
        let nested = root.join("src").join("main.rs");
        fs::create_dir_all(nested.parent().ok_or("parent")?).map_err(|error| error.to_string())?;
        fs::write(&nested, b"fn main() {}").map_err(|error| error.to_string())?;

        let root_str = root.to_string_lossy().into_owned();
        let nested_str = nested.to_string_lossy().into_owned();
        let outside = outside_root.join("other");
        let outside_str = outside.to_string_lossy().into_owned();

        let relative = get_cwd_relative_path_with(&nested_str, &root_str, None)
            .ok_or("expected inside cwd")?;
        assert_eq!(relative, PathBuf::from("src").join("main.rs"));

        let equal =
            get_cwd_relative_path_with(&root_str, &root_str, None).ok_or("expected equal cwd")?;
        assert_eq!(equal, PathBuf::from("."));

        assert!(get_cwd_relative_path_with(&outside_str, &root_str, None).is_none());

        let formatted = format_path_relative_to_cwd_or_absolute_with(&nested_str, &root_str, None);
        assert_eq!(formatted, "src/main.rs");

        let absolute_formatted =
            format_path_relative_to_cwd_or_absolute_with(&outside_str, &root_str, None);
        assert_eq!(absolute_formatted, outside_str.replace('\\', "/"));

        assert!(!is_local_path("npm:foo"));
        assert!(!is_local_path("https://example.com"));
        assert!(is_local_path("file:///tmp/x"));
        assert!(is_local_path("./local"));

        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside_root);
        Ok(())
    }
}
