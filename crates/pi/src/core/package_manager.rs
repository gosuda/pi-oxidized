//! Extension package management: source parsing, install paths, trust gating,
//! npm/git/local install/remove/update, and atomic `packages[]` settings edits.
//!
//! Ports the install/remove/update/list side of
//! `.references/pi-2.0/packages/coding-agent/src/core/package-manager.ts`. The
//! resolve-side resource collection lives in
//! [`crate::core::resources::discovery`]; [`PackageManager::resolve`]
//! delegates to [`PackagePathResolver`] so the two surfaces never disagree on
//! managed install paths. Source parsing reuses [`parse_package_source`] for
//! the same reason.
//!
//! # Design
//!
//! - [`Runner`] is a narrow, synchronous command-execution seam. [`SystemRunner`]
//!   spawns real subprocesses with a detached process group, a per-call timeout,
//!   and process-tree kill + reap on timeout. Tests inject a [`Runner`]
//!   implementation to assert exact argv and to simulate offline/timeout
//!   failures without touching the network.
//! - All network package operations apply [`NETWORK_TIMEOUT_MS`] (10 s) and are
//!   skipped entirely when `PI_OFFLINE` is enabled.
//! - Settings edits read the per-scope `packages[]`, compute the next array
//!   (idempotent normalize / dedupe, project-wins), and persist through
//!   [`SettingsManager`], whose locked overlay preserves every unknown field.

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use semver::{Version, VersionReq};
use serde_json::Value;
use thiserror::Error;

use crate::core::config::{CONFIG_DIR_NAME, PathInputOptions, resolve_path, resolve_path_with};
use crate::core::platform::process_tree::kill_process_tree;
use crate::core::resources::discovery::{
    PackagePathResolver, PackageResolveError, ParsedSource, ResolvedPaths, parse_package_source,
    temporary_dir_hash,
};
use crate::core::settings::{
    PackageSource, PackageSourceFilter, SettingsManager, SettingsManagerError,
};

/// Network package operation timeout in milliseconds (`NETWORK_TIMEOUT_MS`).
pub const NETWORK_TIMEOUT_MS: u64 = 10_000;

/// Canonical content written to a managed install root `.gitignore`.
const GITIGNORE_CONTENT: &str = "*\n!.gitignore\n";
/// Managed npm project `package.json` body.
const NPM_PROJECT_PACKAGE_JSON: &str = "{\n  \"name\": \"pi-extensions\",\n  \"private\": true\n}";

/// TypeScript `parseSource`: delegate to the shared resolver parser so install
/// paths and identities match [`PackagePathResolver`] exactly.
///
/// This is a free function (not an associated function on [`PackageManager`])
/// so callers do not have to fix the `Runner` type parameter.
#[must_use]
pub fn parse_source(source: &str) -> ParsedSource {
    parse_package_source(source)
}

/// Errors raised by [`PackageManager`].
#[derive(Debug, Error)]
pub enum PackageManagerError {
    /// Project-scoped storage was requested while the project is untrusted.
    #[error("Project is not trusted; refusing to access project package storage")]
    ProjectNotTrusted,
    /// A local install source path does not exist on disk.
    #[error("Path does not exist: {0}")]
    PathNotFound(String),
    /// A computed managed path escaped its install root.
    #[error("Refusing to use path outside package install root: {0}")]
    PathEscape(String),
    /// Git install root was missing for a non-temporary scope.
    #[error("Missing git install root")]
    MissingGitInstallRoot,
    /// Configured `npmCommand` had an empty first entry.
    #[error("Invalid npmCommand: first array entry must be a non-empty command")]
    InvalidNpmCommand,
    /// Install source kind is not supported.
    #[error("Unsupported install source: {0}")]
    UnsupportedInstallSource(String),
    /// Remove source kind is not supported.
    #[error("Unsupported remove source: {0}")]
    UnsupportedRemoveSource(String),
    /// No configured package matched an `update` filter.
    #[error("No matching package found for {0}")]
    NoMatchingPackage(String),
    /// No configured package matched an `update` filter, with a suggestion.
    #[error("No matching package found for {0}. Did you mean {1}?")]
    NoMatchingPackageWithSuggestion(String, String),
    /// A subprocess failed, timed out, or could not be spawned.
    #[error("{0}")]
    Runner(String),
    /// The underlying settings manager rejected a project write.
    #[error(transparent)]
    Settings(SettingsManagerError),
    /// A resolve-side path error propagated from [`PackagePathResolver`].
    #[error(transparent)]
    Resolve(#[from] PackageResolveError),
}

impl From<SettingsManagerError> for PackageManagerError {
    fn from(error: SettingsManagerError) -> Self {
        match error {
            SettingsManagerError::ProjectNotTrusted => Self::ProjectNotTrusted,
            error @ SettingsManagerError::InvalidSetting { .. } => Self::Settings(error),
        }
    }
}

/// Callback invoked for each package operation progress event.
pub type ProgressCallback = Box<dyn Fn(&ProgressEvent) + Send + Sync>;

/// Installed package scope: global agent directory or project `.pi`.
///
/// The temporary/CLI scope is internal to resolve-side discovery and never
/// appears in the install/remove/update API.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Scope {
    /// Global agent directory (`~/.pi/agent` or `PI_CODING_AGENT_DIR`).
    User,
    /// Project-local (`{cwd}/.pi`), trust-gated.
    Project,
}

impl Scope {
    /// Wire discriminant.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// Caller decision when a configured package is missing on disk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingSourceAction {
    /// Install the missing package.
    Install,
    /// Leave it missing and skip.
    Skip,
    /// Fail resolution with an error.
    Error,
}

/// One progress notification emitted during a package operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgressEvent {
    /// Event phase.
    pub kind: ProgressKind,
    /// Operation category.
    pub action: ProgressAction,
    /// Source string the operation targets.
    pub source: String,
    /// Human-readable detail (start message or error text).
    pub message: Option<String>,
}

/// Progress event phase (`type` in TypeScript).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressKind {
    /// Operation started.
    Start,
    /// Operation completed.
    Complete,
    /// Operation failed.
    Error,
}

/// Operation category (`action` in TypeScript).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressAction {
    /// `install`.
    Install,
    /// `remove`.
    Remove,
    /// `update`.
    Update,
    /// `pull` (temporary git refresh).
    Pull,
}

/// One configured package row returned by [`PackageManager::list_configured_packages`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfiguredPackage {
    /// Package source string (`npm:…`, git URL, or local path).
    pub source: String,
    /// Scope the package is configured in.
    pub scope: Scope,
    /// Whether the entry used the object/filter form.
    pub filtered: bool,
    /// Absolute install path when present on disk, else `None`.
    pub installed_path: Option<PathBuf>,
}

/// Command execution request handed to a [`Runner`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    /// Executable name or path.
    pub command: String,
    /// Argument vector (already includes any configured `npmCommand` prefix).
    pub args: Vec<String>,
    /// Working directory; `None` inherits the process cwd.
    pub cwd: Option<PathBuf>,
    /// Per-call timeout in milliseconds; `None` waits forever.
    pub timeout_ms: Option<u64>,
    /// Extra environment overrides merged on top of the process environment.
    pub env: Vec<(String, String)>,
}

impl RunRequest {
    /// Build a request with no timeout and no extra env.
    #[must_use]
    pub fn new(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            command: command.into(),
            args,
            cwd: None,
            timeout_ms: None,
            env: Vec::new(),
        }
    }

    /// Set the working directory.
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Set the per-call timeout.
    #[must_use]
    pub fn timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    /// Add one environment override.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// The TypeScript command label used in error messages
    /// (`{command} {args joined by space}`).
    #[must_use]
    pub fn label(&self) -> String {
        let mut label = self.command.clone();
        for arg in &self.args {
            label.push(' ');
            label.push_str(arg);
        }
        label
    }
}

/// Errors raised by a [`Runner`].
#[derive(Debug, Error)]
pub enum RunError {
    /// Process exited nonzero, failed to spawn, or was signaled.
    #[error("{0}")]
    Failed(String),
    /// Process did not exit before the requested timeout.
    #[error("{0}")]
    TimedOut(String),
}

/// Narrow, synchronous command-execution seam.
///
/// [`SystemRunner`] is the production implementation. Tests inject a fake to
/// assert exact argv and to simulate offline / timeout behavior without real
/// subprocesses. Both methods receive the in-flight [`RunRequest`] and apply
/// the same timeout / kill / reap semantics.
pub trait Runner: Send + Sync {
    /// Run with inherited stdout/stderr; error on nonzero exit.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Failed`] on a nonzero exit or spawn failure, and
    /// [`RunError::TimedOut`] when `req.timeout_ms` elapses first.
    fn run(&self, req: &RunRequest) -> Result<(), RunError>;

    /// Run capturing trimmed stdout; error on nonzero exit.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Failed`] (message includes `stderr || stdout`) on a
    /// nonzero exit or spawn failure, and [`RunError::TimedOut`] on timeout.
    fn capture(&self, req: &RunRequest) -> Result<String, RunError>;
}

/// Production [`Runner`] over real subprocesses.
///
/// Spawns each command in its own process group (Unix) or with
/// `CREATE_NO_WINDOW` (Windows), applies the requested timeout, and on timeout
/// kills the whole process tree then reaps it. stdout/stderr are inherited for
/// [`Runner::run`] and piped for [`Runner::capture`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, req: &RunRequest) -> Result<(), RunError> {
        let mut cmd = build_command(req);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::inherit());
        cmd.stderr(Stdio::inherit());
        configure_platform(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| RunError::Failed(format!("{}: {e}", req.label())))?;
        wait_with_timeout(&mut child, req)
    }

    fn capture(&self, req: &RunRequest) -> Result<String, RunError> {
        let mut cmd = build_command(req);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        configure_platform(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| RunError::Failed(format!("{}: {e}", req.label())))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_thread = std::thread::spawn(move || read_pipe(stdout));
        let stderr_thread = std::thread::spawn(move || read_pipe(stderr));

        match wait_with_timeout(&mut child, req) {
            Ok(()) => {
                let stdout_out = stdout_thread.join().unwrap_or_default();
                Ok(stdout_out.trim().to_owned())
            }
            Err(RunError::Failed(_)) => {
                let stdout_out = stdout_thread.join().unwrap_or_default();
                let stderr_out = stderr_thread.join().unwrap_or_default();
                let detail = if stderr_out.is_empty() {
                    stdout_out
                } else {
                    stderr_out
                };
                Err(RunError::Failed(format!(
                    "{} failed with: {detail}",
                    req.label()
                )))
            }
            Err(other) => Err(other),
        }
    }
}

/// Build a [`Command`] from a request, inheriting the process environment and
/// applying the requested overrides.
fn build_command(req: &RunRequest) -> Command {
    let mut cmd = Command::new(&req.command);
    cmd.args(&req.args);
    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd);
    }
    for (key, value) in &req.env {
        cmd.env(key, value);
    }
    cmd
}

/// Apply platform-specific spawn flags (process group on Unix,
/// `CREATE_NO_WINDOW` on Windows).
fn configure_platform(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

/// Wait for `child` with the request timeout, killing the tree on timeout.
fn wait_with_timeout(child: &mut std::process::Child, req: &RunRequest) -> Result<(), RunError> {
    use wait_timeout::ChildExt as _;
    let Some(timeout_ms) = req.timeout_ms else {
        let status = child
            .wait()
            .map_err(|e| RunError::Failed(format!("{}: {e}", req.label())))?;
        return exit_status_result(&req.label(), status);
    };
    let duration = Duration::from_millis(timeout_ms);
    match child.wait_timeout(duration) {
        Ok(Some(status)) => exit_status_result(&req.label(), status),
        Ok(None) => {
            kill_process_tree(child.id());
            let _ = child.wait();
            Err(RunError::TimedOut(format!(
                "{} timed out after {timeout_ms}ms",
                req.label()
            )))
        }
        Err(error) => Err(RunError::Failed(format!("{}: {error}", req.label()))),
    }
}

/// Map a finished exit status into a [`RunError`] when nonzero.
fn exit_status_result(label: &str, status: std::process::ExitStatus) -> Result<(), RunError> {
    if status.success() {
        Ok(())
    } else {
        let exit_status = match status.code() {
            Some(code) => format!("code {code}"),
            None => "signal".to_owned(),
        };
        Err(RunError::Failed(format!(
            "{label} failed with {exit_status}"
        )))
    }
}

/// Read an optional pipe to a string (best-effort).
fn read_pipe<R: Read>(mut pipe: Option<R>) -> String {
    let Some(inner) = pipe.as_mut() else {
        return String::new();
    };
    let mut buf = String::new();
    let _ = inner.read_to_string(&mut buf);
    buf
}

// Kill routing lives in [`crate::core::platform::process_tree::kill_process_tree`].

/// Options for constructing a [`PackageManager`].
#[derive(Clone, Debug)]
pub struct PackageManagerOptions {
    /// Project working directory.
    pub cwd: PathBuf,
    /// Agent config directory (`~/.pi/agent` or `PI_CODING_AGENT_DIR`).
    pub agent_dir: PathBuf,
    /// Optional home directory seam (defaults to the process home).
    pub home_dir: Option<PathBuf>,
}

impl PackageManagerOptions {
    /// Create options from cwd and agent dir.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, agent_dir: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            agent_dir: agent_dir.into(),
            home_dir: None,
        }
    }

    /// Override the home directory seam.
    #[must_use]
    pub fn home_dir(mut self, home_dir: impl Into<PathBuf>) -> Self {
        self.home_dir = Some(home_dir.into());
        self
    }
}

/// Extension package manager: install, remove, update, list, resolve.
///
/// Generic over a [`Runner`] so tests can inject command execution. Production
/// code uses [`PackageManager`] (which defaults to [`SystemRunner`]).
pub struct PackageManager<R: Runner = SystemRunner> {
    cwd: PathBuf,
    agent_dir: PathBuf,
    home_dir: Option<PathBuf>,
    runner: R,
    progress: Option<ProgressCallback>,
    /// `Some(force)` overrides `PI_OFFLINE`; `None` reads the env each call.
    offline: Option<bool>,
    /// Cached global npm root (`npm root -g` / `bun pm bin -g`), keyed by npmCommand argv.
    global_npm_root: Mutex<Option<(String, String)>>,
}

impl PackageManager<SystemRunner> {
    /// Create a manager backed by the real subprocess runner.
    #[must_use]
    pub fn new(options: PackageManagerOptions) -> Self {
        Self::with_runner(options, SystemRunner)
    }
}

impl<R: Runner> PackageManager<R> {
    /// Create a manager with an injected runner.
    #[must_use]
    pub fn with_runner(options: PackageManagerOptions, runner: R) -> Self {
        let cwd = resolve_path_with(
            &options.cwd.to_string_lossy(),
            Path::new("."),
            path_options(options.home_dir.as_deref()).trim(true),
        );
        let agent_dir = resolve_path_with(
            &options.agent_dir.to_string_lossy(),
            Path::new("."),
            path_options(options.home_dir.as_deref()).trim(true),
        );
        Self {
            cwd,
            agent_dir,
            home_dir: options.home_dir,
            runner,
            progress: None,
            offline: None,
            global_npm_root: Mutex::new(None),
        }
    }

    /// Force offline mode (`Some(true)`) or online (`Some(false)`), overriding
    /// `PI_OFFLINE`. `None` (the default) reads the env on each call.
    #[must_use]
    pub fn with_offline(mut self, offline: bool) -> Self {
        self.offline = Some(offline);
        self
    }

    /// Install a progress callback (replaces any previous one).
    pub fn set_progress_callback(&mut self, callback: Option<ProgressCallback>) {
        self.progress = callback;
    }

    /// Whether package network operations are skipped.
    fn is_offline(&self) -> bool {
        self.offline.unwrap_or_else(pi_offline_enabled)
    }

    /// Absolute install path for `source` in `scope`, or `None` when absent.
    ///
    /// # Errors
    ///
    /// Returns [`PackageManagerError::ProjectNotTrusted`] for project scope
    /// while untrusted, or [`PackageManagerError::Resolve`] on path escape.
    pub fn get_installed_path(
        &self,
        settings: &SettingsManager,
        source: &str,
        scope: Scope,
    ) -> Result<Option<PathBuf>, PackageManagerError> {
        let parsed = parse_source(source);
        match parsed {
            ParsedSource::Npm { name, .. } => {
                let path = self.npm_install_path(&name, scope, settings)?;
                Ok(path.exists().then_some(path))
            }
            ParsedSource::Git { host, path, .. } => {
                let path = self.git_install_path(&host, &path, scope)?;
                Ok(path.exists().then_some(path))
            }
            ParsedSource::Local { path } => {
                let base = self.base_dir_for_scope(scope);
                let resolved = self.resolve_path_from_base(&path, &base);
                Ok(resolved.exists().then_some(resolved))
            }
        }
    }

    /// Install `source` into `scope` without recording it in settings.
    ///
    /// Local sources only verify the path exists; npm and git run the package
    /// manager / git clone under [`NETWORK_TIMEOUT_MS`].
    ///
    /// # Errors
    ///
    /// Returns [`PackageManagerError::ProjectNotTrusted`] for untrusted project
    /// scope, [`PackageManagerError::PathNotFound`] for a missing local path,
    /// or [`PackageManagerError::Runner`] on subprocess failure.
    pub fn install(
        &self,
        settings: &SettingsManager,
        source: &str,
        scope: Scope,
    ) -> Result<(), PackageManagerError> {
        let parsed = parse_source(source);
        Self::assert_project_trusted(scope, settings)?;
        let settings_ref = settings;
        self.with_progress(
            ProgressAction::Install,
            source,
            format!("Installing {source}..."),
            || match &parsed {
                ParsedSource::Npm { spec, .. } => {
                    self.install_npm(settings_ref, spec, scope, false)
                }
                ParsedSource::Git { .. } => self.install_git(settings_ref, &parsed, scope),
                ParsedSource::Local { path } => {
                    let resolved = self.resolve_path(path);
                    if resolved.exists() {
                        Ok(())
                    } else {
                        Err(PackageManagerError::PathNotFound(path_string(&resolved)))
                    }
                }
            },
        )
    }

    /// Install then persist the source into the scope's `packages[]`.
    ///
    /// # Errors
    ///
    /// See [`Self::install`] and [`Self::add_source_to_settings`].
    pub fn install_and_persist(
        &self,
        settings: &mut SettingsManager,
        source: &str,
        scope: Scope,
    ) -> Result<(), PackageManagerError> {
        self.install(settings, source, scope)?;
        self.add_source_to_settings(settings, source, scope)?;
        Ok(())
    }

    /// Remove `source` from `scope` (disk only; settings untouched).
    ///
    /// Local sources are a no-op; npm uninstalls and git removes the clone.
    ///
    /// # Errors
    ///
    /// Returns [`PackageManagerError::ProjectNotTrusted`] for untrusted project
    /// scope or [`PackageManagerError::Runner`] on subprocess failure.
    pub fn remove(
        &self,
        settings: &SettingsManager,
        source: &str,
        scope: Scope,
    ) -> Result<(), PackageManagerError> {
        let parsed = parse_source(source);
        Self::assert_project_trusted(scope, settings)?;
        let settings_ref = settings;
        self.with_progress(
            ProgressAction::Remove,
            source,
            format!("Removing {source}..."),
            || match &parsed {
                ParsedSource::Npm { .. } => self.uninstall_npm(settings_ref, &parsed, scope),
                ParsedSource::Git { .. } => self.remove_git(&parsed, scope),
                ParsedSource::Local { .. } => Ok(()),
            },
        )
    }

    /// Remove from disk then drop the source from `packages[]`.
    ///
    /// Returns whether the settings array actually changed.
    ///
    /// # Errors
    ///
    /// See [`Self::remove`] and [`Self::remove_source_from_settings`].
    pub fn remove_and_persist(
        &self,
        settings: &mut SettingsManager,
        source: &str,
        scope: Scope,
    ) -> Result<bool, PackageManagerError> {
        self.remove(settings, source, scope)?;
        self.remove_source_from_settings(settings, source, scope)
    }

    /// Update one package (`Some`) or every configured package (`None`).
    ///
    /// Pinned npm versions are skipped (they are fixed). Pinned git refs are
    /// reconciled against the configured ref. Offline mode or an empty match
    /// set is a no-op. A filter that matches nothing is an error.
    ///
    /// # Errors
    ///
    /// Returns [`PackageManagerError::NoMatchingPackage`] (with a suggestion
    /// when one exists) for an unmatched filter, or [`PackageManagerError`]
    /// variants from the underlying install/clone operations.
    pub fn update_extensions(
        &self,
        settings: &SettingsManager,
        source: Option<&str>,
    ) -> Result<(), PackageManagerError> {
        if self.is_offline() {
            return Ok(());
        }
        let global = settings.get_global_settings();
        let project = settings.get_project_settings();
        let identity = source.map(|s| self.package_identity(s, None));
        let mut matched = false;
        let mut targets: Vec<(String, Scope)> = Vec::new();
        for pkg in global.packages.clone().unwrap_or_default() {
            let src = package_source_string(&pkg);
            if identity
                .as_ref()
                .is_some_and(|id| self.package_identity(&src, Some(Scope::User)) != *id)
            {
                continue;
            }
            matched = true;
            targets.push((src, Scope::User));
        }
        for pkg in project.packages.clone().unwrap_or_default() {
            let src = package_source_string(&pkg);
            if identity
                .as_ref()
                .is_some_and(|id| self.package_identity(&src, Some(Scope::Project)) != *id)
            {
                continue;
            }
            matched = true;
            targets.push((src, Scope::Project));
        }
        if source.is_some() && !matched {
            let configured: Vec<PackageSource> = global
                .packages
                .clone()
                .unwrap_or_default()
                .into_iter()
                .chain(project.packages.clone().unwrap_or_default())
                .collect();
            return Err(no_matching_package_error(source.unwrap_or(""), &configured));
        }
        self.update_configured_targets(settings, &targets)?;
        Ok(())
    }

    /// List every configured package across both scopes with its install path.
    ///
    /// # Errors
    ///
    /// Returns [`PackageManagerError`] only when an install-path lookup hits a
    /// trust or path-escape failure.
    pub fn list_configured_packages(
        &self,
        settings: &SettingsManager,
    ) -> Result<Vec<ConfiguredPackage>, PackageManagerError> {
        let global = settings.get_global_settings();
        let project = settings.get_project_settings();
        let mut out = Vec::new();
        for pkg in global.packages.clone().unwrap_or_default() {
            let source = package_source_string(&pkg);
            let filtered = matches!(pkg, PackageSource::Filtered(_));
            let installed_path = self.get_installed_path(settings, &source, Scope::User)?;
            out.push(ConfiguredPackage {
                source,
                scope: Scope::User,
                filtered,
                installed_path,
            });
        }
        for pkg in project.packages.clone().unwrap_or_default() {
            let source = package_source_string(&pkg);
            let filtered = matches!(pkg, PackageSource::Filtered(_));
            let installed_path = self.get_installed_path(settings, &source, Scope::Project)?;
            out.push(ConfiguredPackage {
                source,
                scope: Scope::Project,
                filtered,
                installed_path,
            });
        }
        Ok(out)
    }

    /// Resolve all configured packages and local resources to concrete paths.
    ///
    /// Delegates to [`PackagePathResolver::resolve`] so this surface and the
    /// resolve-side discovery never disagree on managed install locations.
    /// Missing installs are skipped (no network install here).
    ///
    /// # Errors
    ///
    /// Propagates [`PackageResolveError`] as [`PackageManagerError::Resolve`].
    pub fn resolve(
        &self,
        settings: &SettingsManager,
    ) -> Result<ResolvedPaths, PackageManagerError> {
        let resolver = PackagePathResolver::new(&self.cwd, &self.agent_dir, settings);
        Ok(resolver.resolve()?)
    }

    /// Add (or normalize) `source` in `scope`'s `packages[]`.
    ///
    /// Returns whether the settings array changed. Adding the same source is
    /// idempotent (returns `false`); a re-normalized local path returns `true`.
    ///
    /// # Errors
    ///
    /// Returns [`PackageManagerError::ProjectNotTrusted`] (via the settings
    /// setter) for untrusted project scope.
    pub fn add_source_to_settings(
        &self,
        settings: &mut SettingsManager,
        source: &str,
        scope: Scope,
    ) -> Result<bool, PackageManagerError> {
        let current = Self::scope_packages(settings, scope);
        let normalized = self.normalize_source_for_settings(source, scope);
        if let Some(index) = self.find_match(&current, source, scope) {
            let existing = &current[index];
            if package_source_string(existing) == normalized {
                return Ok(false);
            }
            let mut next = current.clone();
            next[index] = replace_source(existing, normalized);
            Self::set_scope_packages(settings, scope, &next)?;
            return Ok(true);
        }
        let mut next = current;
        next.push(PackageSource::Source(normalized));
        Self::set_scope_packages(settings, scope, &next)?;
        Ok(true)
    }

    /// Remove `source` from `scope`'s `packages[]`.
    ///
    /// Returns whether the array changed.
    ///
    /// # Errors
    ///
    /// Returns [`PackageManagerError::ProjectNotTrusted`] for untrusted project
    /// scope.
    pub fn remove_source_from_settings(
        &self,
        settings: &mut SettingsManager,
        source: &str,
        scope: Scope,
    ) -> Result<bool, PackageManagerError> {
        let current = Self::scope_packages(settings, scope);
        let mut next = Vec::new();
        for pkg in &current {
            if !self.sources_match(pkg, source, scope) {
                next.push(pkg.clone());
            }
        }
        if next.len() == current.len() {
            return Ok(false);
        }
        Self::set_scope_packages(settings, scope, &next)?;
        Ok(true)
    }

    // -- internal: progress ---------------------------------------------------

    fn with_progress<F>(
        &self,
        action: ProgressAction,
        source: &str,
        message: String,
        op: F,
    ) -> Result<(), PackageManagerError>
    where
        F: FnOnce() -> Result<(), PackageManagerError>,
    {
        self.emit(&ProgressEvent {
            kind: ProgressKind::Start,
            action,
            source: source.to_owned(),
            message: Some(message),
        });
        match op() {
            Ok(()) => {
                self.emit(&ProgressEvent {
                    kind: ProgressKind::Complete,
                    action,
                    source: source.to_owned(),
                    message: None,
                });
                Ok(())
            }
            Err(error) => {
                self.emit(&ProgressEvent {
                    kind: ProgressKind::Error,
                    action,
                    source: source.to_owned(),
                    message: Some(error.to_string()),
                });
                Err(error)
            }
        }
    }

    fn emit(&self, event: &ProgressEvent) {
        if let Some(callback) = &self.progress {
            callback(event);
        }
    }

    // -- internal: trust / settings ----------------------------------------

    fn assert_project_trusted(
        scope: Scope,
        settings: &SettingsManager,
    ) -> Result<(), PackageManagerError> {
        if scope == Scope::Project && !settings.is_project_trusted() {
            return Err(PackageManagerError::ProjectNotTrusted);
        }
        Ok(())
    }

    fn scope_packages(settings: &SettingsManager, scope: Scope) -> Vec<PackageSource> {
        match scope {
            Scope::Project => settings
                .get_project_settings()
                .packages
                .clone()
                .unwrap_or_default(),
            Scope::User => settings
                .get_global_settings()
                .packages
                .clone()
                .unwrap_or_default(),
        }
    }

    fn set_scope_packages(
        settings: &mut SettingsManager,
        scope: Scope,
        packages: &[PackageSource],
    ) -> Result<(), PackageManagerError> {
        match scope {
            Scope::Project => settings.set_project_packages(packages)?,
            Scope::User => settings.set_packages(packages),
        }
        Ok(())
    }

    fn find_match(&self, packages: &[PackageSource], input: &str, scope: Scope) -> Option<usize> {
        let right = self.source_match_key_for_input(input);
        for (index, pkg) in packages.iter().enumerate() {
            let left = self.source_match_key_for_settings(&package_source_string(pkg), scope);
            if left == right {
                return Some(index);
            }
        }
        None
    }

    fn sources_match(&self, existing: &PackageSource, input: &str, scope: Scope) -> bool {
        self.find_match(std::slice::from_ref(existing), input, scope)
            .is_some()
    }

    /// `getPackageIdentity`: identity ignores version/ref. Local identity is
    /// scope-base-relative when a scope is given.
    fn package_identity(&self, source: &str, scope: Option<Scope>) -> String {
        match parse_source(source) {
            ParsedSource::Npm { name, .. } => format!("npm:{name}"),
            ParsedSource::Git { host, path, .. } => format!("git:{host}/{path}"),
            ParsedSource::Local { path } => match scope {
                Some(scope) => {
                    let base = self.base_dir_for_scope(scope);
                    format!(
                        "local:{}",
                        path_string(&self.resolve_path_from_base(&path, &base))
                    )
                }
                None => format!("local:{}", path_string(&self.resolve_path(&path))),
            },
        }
    }

    fn source_match_key_for_input(&self, source: &str) -> String {
        match parse_source(source) {
            ParsedSource::Npm { name, .. } => format!("npm:{name}"),
            ParsedSource::Git { host, path, .. } => format!("git:{host}/{path}"),
            ParsedSource::Local { path } => {
                format!("local:{}", path_string(&self.resolve_path(&path)))
            }
        }
    }

    fn source_match_key_for_settings(&self, source: &str, scope: Scope) -> String {
        match parse_source(source) {
            ParsedSource::Npm { name, .. } => format!("npm:{name}"),
            ParsedSource::Git { host, path, .. } => format!("git:{host}/{path}"),
            ParsedSource::Local { path } => {
                let base = self.base_dir_for_scope(scope);
                format!(
                    "local:{}",
                    path_string(&self.resolve_path_from_base(&path, &base))
                )
            }
        }
    }

    fn normalize_source_for_settings(&self, source: &str, scope: Scope) -> String {
        match parse_source(source) {
            ParsedSource::Local { path } => {
                let base = self.base_dir_for_scope(scope);
                let resolved = self.resolve_path(&path);
                let rel = relative_path(&base, &resolved);
                rel.to_string_lossy().into_owned()
            }
            _ => source.to_owned(),
        }
    }

    // -- internal: npm ------------------------------------------------------

    fn npm_command(
        settings: &SettingsManager,
    ) -> Result<(String, Vec<String>), PackageManagerError> {
        let configured = settings.get_npm_command().unwrap_or_default();
        if configured.is_empty() {
            return Ok(("npm".to_owned(), Vec::new()));
        }
        let mut iter = configured.into_iter();
        let command = iter.next().ok_or(PackageManagerError::InvalidNpmCommand)?;
        if command.is_empty() {
            return Err(PackageManagerError::InvalidNpmCommand);
        }
        Ok((command, iter.collect()))
    }

    fn package_manager_name(command: &str, args: &[String]) -> String {
        let mut parts = vec![command.to_owned()];
        parts.extend(args.iter().cloned());
        let pm = match parts.iter().rposition(|p| p == "--") {
            Some(idx) => parts.get(idx + 1).cloned().unwrap_or_default(),
            None => command.to_owned(),
        };
        basename_no_exe(&pm)
    }

    /// `getNpmInstallArgs` dialect (no configured-arg prefix).
    fn npm_install_dialect(manager: &str, specs: &[String], install_root: &Path) -> Vec<String> {
        match manager {
            "bun" => {
                let mut out = vec!["install".to_owned()];
                out.extend(specs.iter().cloned());
                out.push("--cwd".to_owned());
                out.push(path_string(install_root));
                out.push("--omit=peer".to_owned());
                out
            }
            "pnpm" => {
                let mut out = vec!["install".to_owned()];
                out.extend(specs.iter().cloned());
                out.push("--prefix".to_owned());
                out.push(path_string(install_root));
                out.push("--config.auto-install-peers=false".to_owned());
                out.push("--config.strict-peer-dependencies=false".to_owned());
                out.push("--config.strict-dep-builds=false".to_owned());
                out
            }
            _ => {
                let mut out = vec!["install".to_owned()];
                out.extend(specs.iter().cloned());
                out.push("--prefix".to_owned());
                out.push(path_string(install_root));
                out.push("--legacy-peer-deps".to_owned());
                out
            }
        }
    }

    /// `uninstallNpm` dialect (no configured-arg prefix).
    fn npm_uninstall_dialect(manager: &str, name: &str, install_root: &Path) -> Vec<String> {
        match manager {
            "bun" => vec![
                "uninstall".to_owned(),
                name.to_owned(),
                "--cwd".to_owned(),
                path_string(install_root),
            ],
            "pnpm" => vec![
                "uninstall".to_owned(),
                name.to_owned(),
                "--prefix".to_owned(),
                path_string(install_root),
            ],
            _ => vec![
                "uninstall".to_owned(),
                name.to_owned(),
                "--prefix".to_owned(),
                path_string(install_root),
                "--legacy-peer-deps".to_owned(),
            ],
        }
    }

    /// `getGitDependencyInstallArgs` dialect.
    fn git_dependency_dialect(settings: &SettingsManager) -> Vec<String> {
        if settings.get_npm_command().is_some_and(|c| !c.is_empty()) {
            vec!["install".to_owned()]
        } else {
            vec!["install".to_owned(), "--omit=dev".to_owned()]
        }
    }

    /// Run `command prefix… dialect…` (TS `runNpmCommand`).
    fn run_npm(
        &self,
        settings: &SettingsManager,
        dialect: Vec<String>,
        cwd: Option<&Path>,
    ) -> Result<(), PackageManagerError> {
        let (command, prefix) = Self::npm_command(settings)?;
        let mut full = prefix;
        full.extend(dialect);
        let mut req = RunRequest::new(command, full).timeout_ms(NETWORK_TIMEOUT_MS);
        if let Some(cwd) = cwd {
            req = req.cwd(cwd);
        }
        self.runner.run(&req).map_err(|error| runner_error(&error))
    }

    fn install_npm(
        &self,
        settings: &SettingsManager,
        spec: &str,
        scope: Scope,
        temporary: bool,
    ) -> Result<(), PackageManagerError> {
        let install_root = self.npm_install_root(scope, temporary)?;
        Self::ensure_npm_project(&install_root)?;
        let (command, prefix) = Self::npm_command(settings)?;
        let manager = Self::package_manager_name(&command, &prefix);
        let spec_owned = spec.to_owned();
        let dialect =
            Self::npm_install_dialect(&manager, std::slice::from_ref(&spec_owned), &install_root);
        let mut full = prefix;
        full.extend(dialect);
        self.runner
            .run(&RunRequest::new(command, full).timeout_ms(NETWORK_TIMEOUT_MS))
            .map_err(|error| runner_error(&error))
    }

    fn uninstall_npm(
        &self,
        settings: &SettingsManager,
        parsed: &ParsedSource,
        scope: Scope,
    ) -> Result<(), PackageManagerError> {
        let ParsedSource::Npm { name, .. } = parsed else {
            return Ok(());
        };
        let install_root = self.npm_install_root(scope, false)?;
        if !install_root.exists() {
            return Ok(());
        }
        let (command, prefix) = Self::npm_command(settings)?;
        let manager = Self::package_manager_name(&command, &prefix);
        let dialect = Self::npm_uninstall_dialect(&manager, name, &install_root);
        let mut full = prefix;
        full.extend(dialect);
        self.runner
            .run(&RunRequest::new(command, full).timeout_ms(NETWORK_TIMEOUT_MS))
            .map_err(|error| runner_error(&error))
    }

    // -- internal: git ------------------------------------------------------

    fn install_git(
        &self,
        settings: &SettingsManager,
        parsed: &ParsedSource,
        scope: Scope,
    ) -> Result<(), PackageManagerError> {
        let ParsedSource::Git { repo, ref_name, .. } = parsed else {
            return Ok(());
        };
        let target = self.git_install_path_from_parsed(parsed, scope)?;
        if target.exists() {
            // Existing clone: reconcile to the configured ref/upstream. Deps are
            // reinstalled inside `ensure_git_ref` only when a reset happens.
            if let Some(reference) = ref_name {
                self.ensure_git_ref(
                    settings,
                    &target,
                    &["fetch".to_owned(), "origin".to_owned(), reference.clone()],
                    "FETCH_HEAD",
                )?;
            } else {
                let update_target = self.local_git_update_target(&target);
                self.ensure_git_ref(
                    settings,
                    &target,
                    &update_target.fetch_args,
                    &update_target.reference,
                )?;
            }
            return Ok(());
        }
        let root = self.git_install_root(scope);
        Self::ensure_git_ignore(&root)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| io_escape(&error))?;
        }
        self.runner
            .run(
                &RunRequest::new(
                    "git",
                    vec!["clone".to_owned(), repo.clone(), path_string(&target)],
                )
                .timeout_ms(NETWORK_TIMEOUT_MS),
            )
            .map_err(|error| runner_error(&error))?;
        // Fresh clone checks out the configured ref directly (TS `git checkout`).
        if let Some(reference) = ref_name {
            self.runner
                .run(
                    &RunRequest::new("git", vec!["checkout".to_owned(), reference.clone()])
                        .cwd(&target)
                        .timeout_ms(NETWORK_TIMEOUT_MS),
                )
                .map_err(|error| runner_error(&error))?;
        }
        if target.join("package.json").exists() {
            self.run_npm(
                settings,
                Self::git_dependency_dialect(settings),
                Some(&target),
            )?;
        }
        Ok(())
    }

    fn update_git(
        &self,
        settings: &SettingsManager,
        parsed: &ParsedSource,
        scope: Scope,
    ) -> Result<(), PackageManagerError> {
        let target = self.git_install_path_from_parsed(parsed, scope)?;
        if !target.exists() {
            return self.install_git(settings, parsed, scope);
        }
        let ParsedSource::Git { ref_name, .. } = parsed else {
            return Ok(());
        };
        // Deps reinstall happens inside `ensure_git_ref` when a reset occurs.
        if let Some(reference) = ref_name {
            self.ensure_git_ref(
                settings,
                &target,
                &["fetch".to_owned(), "origin".to_owned(), reference.clone()],
                "FETCH_HEAD",
            )?;
        } else {
            let update_target = self.local_git_update_target(&target);
            self.ensure_git_ref(
                settings,
                &target,
                &update_target.fetch_args,
                &update_target.reference,
            )?;
        }
        Ok(())
    }

    /// `ensureGitRef`: fetch the target ref, and when HEAD differs reset
    /// `--hard`, run `git clean -fdx`, and reinstall npm deps so a reconciled
    /// extension is pristine (no stale untracked files or `node_modules`).
    fn ensure_git_ref(
        &self,
        settings: &SettingsManager,
        target: &Path,
        fetch_args: &[String],
        reference: &str,
    ) -> Result<(), PackageManagerError> {
        self.runner
            .run(
                &RunRequest::new("git", fetch_args.to_vec())
                    .cwd(target)
                    .timeout_ms(NETWORK_TIMEOUT_MS),
            )
            .map_err(|error| runner_error(&error))?;
        let local_head = self
            .runner
            .capture(
                &RunRequest::new("git", vec!["rev-parse".to_owned(), "HEAD".to_owned()])
                    .cwd(target)
                    .timeout_ms(NETWORK_TIMEOUT_MS),
            )
            .map_err(|error| runner_error(&error))?
            .trim()
            .to_owned();
        let commit_ref = format!("{reference}^{{commit}}");
        let target_head = self
            .runner
            .capture(
                &RunRequest::new("git", vec!["rev-parse".to_owned(), commit_ref.clone()])
                    .cwd(target)
                    .timeout_ms(NETWORK_TIMEOUT_MS),
            )
            .map_err(|error| runner_error(&error))?
            .trim()
            .to_owned();
        if local_head == target_head {
            return Ok(());
        }
        self.runner
            .run(
                &RunRequest::new(
                    "git",
                    vec!["reset".to_owned(), "--hard".to_owned(), commit_ref],
                )
                .cwd(target)
                .timeout_ms(NETWORK_TIMEOUT_MS),
            )
            .map_err(|error| runner_error(&error))?;
        // Clean untracked files (extensions should be pristine after a reset).
        self.runner
            .run(
                &RunRequest::new("git", vec!["clean".to_owned(), "-fdx".to_owned()])
                    .cwd(target)
                    .timeout_ms(NETWORK_TIMEOUT_MS),
            )
            .map_err(|error| runner_error(&error))?;
        if target.join("package.json").exists() {
            self.run_npm(
                settings,
                Self::git_dependency_dialect(settings),
                Some(target),
            )?;
        }
        Ok(())
    }

    fn remove_git(&self, parsed: &ParsedSource, scope: Scope) -> Result<(), PackageManagerError> {
        let target = self.git_install_path_from_parsed(parsed, scope)?;
        if !target.exists() {
            return Ok(());
        }
        remove_all(&target)?;
        let root = self.git_install_root(scope);
        Self::prune_empty_git_parents(&target, &root);
        Ok(())
    }

    /// Detect the local upstream tracking target (`getLocalGitUpdateTarget`).
    fn local_git_update_target(&self, target: &Path) -> GitUpdateTarget {
        let upstream = self.runner.capture(
            &RunRequest::new(
                "git",
                vec![
                    "rev-parse".to_owned(),
                    "--abbrev-ref".to_owned(),
                    "@{upstream}".to_owned(),
                ],
            )
            .cwd(target)
            .timeout_ms(NETWORK_TIMEOUT_MS),
        );
        if let Ok(upstream) = upstream {
            let trimmed = upstream.trim();
            if let Some(branch) = trimmed.strip_prefix("origin/")
                && !branch.is_empty()
            {
                return GitUpdateTarget {
                    reference: "@{upstream}".to_owned(),
                    fetch_args: git_fetch_args(branch),
                };
            }
        }
        let _ = self.runner.run(
            &RunRequest::new(
                "git",
                vec![
                    "remote".to_owned(),
                    "set-head".to_owned(),
                    "origin".to_owned(),
                    "-a".to_owned(),
                ],
            )
            .cwd(target)
            .timeout_ms(NETWORK_TIMEOUT_MS),
        );
        let head_ref = self
            .runner
            .capture(
                &RunRequest::new(
                    "git",
                    vec![
                        "symbolic-ref".to_owned(),
                        "refs/remotes/origin/HEAD".to_owned(),
                    ],
                )
                .cwd(target)
                .timeout_ms(NETWORK_TIMEOUT_MS),
            )
            .unwrap_or_default()
            .trim()
            .trim_start_matches("refs/remotes/origin/")
            .to_owned();
        if head_ref.is_empty() {
            // TS final fallback: fetch HEAD directly into refs/remotes/origin/HEAD.
            return GitUpdateTarget {
                reference: "origin/HEAD".to_owned(),
                fetch_args: git_head_fallback_fetch_args(),
            };
        }
        GitUpdateTarget {
            reference: "origin/HEAD".to_owned(),
            fetch_args: git_fetch_args(&head_ref),
        }
    }

    // -- internal: update batching -----------------------------------------

    fn update_configured_targets(
        &self,
        settings: &SettingsManager,
        targets: &[(String, Scope)],
    ) -> Result<(), PackageManagerError> {
        if targets.is_empty() {
            return Ok(());
        }
        // Sequential update checks (correctness over TS's bounded concurrency;
        // the observable result — every matching package updated — is identical).
        for (source, scope) in targets {
            let parsed = parse_source(source);
            match &parsed {
                ParsedSource::Npm {
                    spec,
                    name,
                    version,
                    ..
                } => {
                    if is_exact_npm_version(version.as_deref()) {
                        continue;
                    }
                    if !self.should_update_npm(settings, name, spec, version.as_deref(), *scope)? {
                        continue;
                    }
                    // TS `updateNpmBatch`: unpinned packages install `name@latest`.
                    let update_spec = spec_if_version(spec, name, version.as_deref());
                    let scope = *scope;
                    self.with_progress(
                        ProgressAction::Update,
                        source,
                        format!("Updating {source}..."),
                        || self.install_npm(settings, &update_spec, scope, false),
                    )?;
                }
                ParsedSource::Git { .. } => {
                    let scope = *scope;
                    let parsed = parsed.clone();
                    self.with_progress(
                        ProgressAction::Update,
                        source,
                        format!("Updating {source}..."),
                        || self.update_git(settings, &parsed, scope),
                    )?;
                }
                ParsedSource::Local { .. } => {}
            }
        }
        Ok(())
    }

    fn should_update_npm(
        &self,
        settings: &SettingsManager,
        name: &str,
        spec: &str,
        version: Option<&str>,
        scope: Scope,
    ) -> Result<bool, PackageManagerError> {
        let installed_path = self.managed_npm_install_path(name, scope)?;
        let Some(installed_version) = read_installed_version(&installed_path) else {
            return Ok(true);
        };
        // TS `getLatestNpmVersion`: `source.version ? source.spec : source.name`.
        let view_spec = if version.is_some() {
            spec.to_owned()
        } else {
            name.to_owned()
        };
        let target = self.latest_npm_version(settings, view_spec, version)?;
        Ok(target.as_deref() != Some(installed_version.as_str()))
    }

    fn latest_npm_version(
        &self,
        settings: &SettingsManager,
        package_spec: String,
        range: Option<&str>,
    ) -> Result<Option<String>, PackageManagerError> {
        let (command, prefix) = Self::npm_command(settings)?;
        let mut args = prefix;
        args.push("view".to_owned());
        args.push(package_spec);
        args.push("version".to_owned());
        args.push("--json".to_owned());
        let req = RunRequest::new(command, args)
            .cwd(&self.cwd)
            .timeout_ms(NETWORK_TIMEOUT_MS);
        let Ok(raw) = self.runner.capture(&req) else {
            return Ok(None);
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        Ok(parse_npm_view_version(trimmed, range))
    }

    // -- internal: paths ----------------------------------------------------

    fn base_dir_for_scope(&self, scope: Scope) -> PathBuf {
        match scope {
            Scope::Project => self.cwd.join(CONFIG_DIR_NAME),
            Scope::User => self.agent_dir.clone(),
        }
    }

    fn npm_install_root(
        &self,
        scope: Scope,
        temporary: bool,
    ) -> Result<PathBuf, PackageManagerError> {
        if temporary {
            return self.temporary_dir("npm", None);
        }
        match scope {
            Scope::Project => Ok(self.cwd.join(CONFIG_DIR_NAME).join("npm")),
            Scope::User => Ok(self.agent_dir.join("npm")),
        }
    }

    fn git_install_root(&self, scope: Scope) -> PathBuf {
        match scope {
            Scope::Project => self.cwd.join(CONFIG_DIR_NAME).join("git"),
            Scope::User => self.agent_dir.join("git"),
        }
    }

    fn managed_npm_install_path(
        &self,
        name: &str,
        scope: Scope,
    ) -> Result<PathBuf, PackageManagerError> {
        Ok(self
            .npm_install_root(scope, false)?
            .join("node_modules")
            .join(name))
    }

    fn git_install_path(
        &self,
        host: &str,
        path: &str,
        scope: Scope,
    ) -> Result<PathBuf, PackageManagerError> {
        let root = self.git_install_root(scope);
        resolve_managed_path(&root, &[host, path])
    }

    fn git_install_path_from_parsed(
        &self,
        parsed: &ParsedSource,
        scope: Scope,
    ) -> Result<PathBuf, PackageManagerError> {
        let ParsedSource::Git { host, path, .. } = parsed else {
            return Err(PackageManagerError::UnsupportedInstallSource(
                "non-git".to_owned(),
            ));
        };
        self.git_install_path(host, path, scope)
    }

    fn npm_install_path(
        &self,
        name: &str,
        scope: Scope,
        settings: &SettingsManager,
    ) -> Result<PathBuf, PackageManagerError> {
        let managed = self.managed_npm_install_path(name, scope)?;
        if scope != Scope::User || managed.exists() {
            return Ok(managed);
        }
        if let Some(legacy) = self.legacy_global_npm_install_path(name, settings)?
            && legacy.exists()
        {
            return Ok(legacy);
        }
        Ok(managed)
    }

    fn legacy_global_npm_install_path(
        &self,
        name: &str,
        settings: &SettingsManager,
    ) -> Result<Option<PathBuf>, PackageManagerError> {
        if let Some(path) = self.pnpm_global_package_path(name, settings)? {
            return Ok(Some(path));
        }
        let root = self.global_npm_root(settings)?;
        if root.is_empty() {
            return Ok(None);
        }
        Ok(Some(Path::new(&root).join(name)))
    }

    fn pnpm_global_package_path(
        &self,
        name: &str,
        settings: &SettingsManager,
    ) -> Result<Option<PathBuf>, PackageManagerError> {
        let (command, args) = Self::npm_command(settings)?;
        if Self::package_manager_name(&command, &args) != "pnpm" {
            return Ok(None);
        }
        let mut full = args;
        full.extend([
            "list".to_owned(),
            "-g".to_owned(),
            "--depth".to_owned(),
            "0".to_owned(),
            "--json".to_owned(),
        ]);
        let Ok(output) = self
            .runner
            .capture(&RunRequest::new(command, full).timeout_ms(NETWORK_TIMEOUT_MS))
        else {
            return Ok(None);
        };
        Ok(parse_pnpm_global_path(&output, name))
    }

    fn global_npm_root(&self, settings: &SettingsManager) -> Result<String, PackageManagerError> {
        let (command, args) = Self::npm_command(settings)?;
        let key = command_key(&command, &args);
        if let Some(entry) = self
            .global_npm_root
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            && entry.0 == key
        {
            return Ok(entry.1);
        }
        let manager = Self::package_manager_name(&command, &args);
        let root = if manager == "bun" {
            let mut a = args.clone();
            a.extend(["pm".to_owned(), "bin".to_owned(), "-g".to_owned()]);
            let bin_dir = self
                .runner
                .capture(&RunRequest::new(command.clone(), a).timeout_ms(NETWORK_TIMEOUT_MS))
                .unwrap_or_default();
            let parent = Path::new(bin_dir.trim())
                .parent()
                .map_or_else(|| bin_dir.trim().to_owned(), path_string);
            format!(
                "{}/install/global/node_modules",
                parent.trim_end_matches('/')
            )
        } else {
            let mut a = args.clone();
            a.extend(["root".to_owned(), "-g".to_owned()]);
            self.runner
                .capture(&RunRequest::new(command.clone(), a).timeout_ms(NETWORK_TIMEOUT_MS))
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        if let Ok(mut guard) = self.global_npm_root.lock() {
            *guard = Some((key, root.clone()));
        }
        Ok(root)
    }

    fn temporary_dir(
        &self,
        prefix: &str,
        suffix: Option<&str>,
    ) -> Result<PathBuf, PackageManagerError> {
        let root = resolve_managed_path(&extension_temp_folder(&self.agent_dir), &[prefix])?;
        let suffix_str = suffix.unwrap_or("");
        let hash = temporary_dir_hash(prefix, suffix_str);
        if suffix_str.is_empty() {
            resolve_managed_path(&root, &[&hash])
        } else {
            resolve_managed_path(&root, &[&hash, suffix_str])
        }
    }

    fn ensure_npm_project(install_root: &Path) -> Result<(), PackageManagerError> {
        if !install_root.exists() {
            fs::create_dir_all(install_root).map_err(|error| io_escape(&error))?;
        }
        Self::ensure_git_ignore(install_root)?;
        let package_json = install_root.join("package.json");
        if !package_json.exists() {
            fs::write(&package_json, NPM_PROJECT_PACKAGE_JSON)
                .map_err(|error| io_escape(&error))?;
        }
        Ok(())
    }

    fn ensure_git_ignore(dir: &Path) -> Result<(), PackageManagerError> {
        if !dir.exists() {
            fs::create_dir_all(dir).map_err(|error| io_escape(&error))?;
        }
        let ignore_path = dir.join(".gitignore");
        if !ignore_path.exists() {
            fs::write(&ignore_path, GITIGNORE_CONTENT).map_err(|error| io_escape(&error))?;
        }
        Ok(())
    }

    fn prune_empty_git_parents(target: &Path, install_root: &Path) {
        let resolved_root = resolve_path(path_string(install_root));
        let mut current = match target.parent() {
            Some(parent) => parent.to_path_buf(),
            None => return,
        };
        while current.starts_with(&resolved_root) && current != resolved_root {
            if !current.exists() {
                current = match current.parent() {
                    Some(parent) => parent.to_path_buf(),
                    None => return,
                };
                continue;
            }
            let is_empty = fs::read_dir(&current).is_ok_and(|mut it| it.next().is_none());
            if !is_empty {
                break;
            }
            let _ = fs::remove_dir_all(&current);
            current = match current.parent() {
                Some(parent) => parent.to_path_buf(),
                None => return,
            };
        }
    }

    // -- internal: path resolution seams -----------------------------------

    fn resolve_path(&self, input: &str) -> PathBuf {
        resolve_path_with(
            input,
            &self.cwd,
            path_options(self.home_dir.as_deref()).trim(true),
        )
    }

    fn resolve_path_from_base(&self, input: &str, base: &Path) -> PathBuf {
        resolve_path_with(
            input,
            base,
            path_options(self.home_dir.as_deref()).trim(true),
        )
    }
}

// ===========================================================================
// Free helpers
// ===========================================================================

/// Extension temp folder (`{agentDir}/tmp/extensions`).
fn extension_temp_folder(agent_dir: &Path) -> PathBuf {
    agent_dir.join("tmp").join("extensions")
}

/// Resolve `root/parts…` refusing any component that escapes the root.
fn resolve_managed_path(root: &Path, parts: &[&str]) -> Result<PathBuf, PackageManagerError> {
    let resolved_root = resolve_path(path_string(root));
    let mut resolved = resolved_root.clone();
    for part in parts {
        for component in Path::new(part).components() {
            match component {
                Component::Normal(seg) => resolved.push(seg),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(PackageManagerError::PathEscape(path_string(&resolved)));
                }
            }
        }
    }
    if resolved != resolved_root && !resolved.starts_with(&resolved_root) {
        return Err(PackageManagerError::PathEscape(path_string(&resolved)));
    }
    Ok(resolved)
}

/// `relative(base, resolved)`, defaulting to `.` when equal (TS `rel || "."`).
fn relative_path(base: &Path, resolved: &Path) -> PathBuf {
    match resolved.strip_prefix(base) {
        Ok(rel) if rel.as_os_str().is_empty() => PathBuf::from("."),
        Ok(rel) => rel.to_path_buf(),
        Err(_) => resolved.to_path_buf(),
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn path_options(home_dir: Option<&Path>) -> PathInputOptions<'_> {
    PathInputOptions::new().home_dir(home_dir)
}

fn basename_no_exe(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .map_or_else(|| name.to_owned(), |s| s.to_string_lossy().into_owned());
    stem.trim_end_matches(".cmd")
        .trim_end_matches(".exe")
        .to_owned()
}

fn command_key(command: &str, args: &[String]) -> String {
    let mut key = command.to_owned();
    for arg in args {
        key.push('\0');
        key.push_str(arg);
    }
    key
}

fn git_fetch_args(branch: &str) -> Vec<String> {
    vec![
        "fetch".to_owned(),
        "--prune".to_owned(),
        "--no-tags".to_owned(),
        "origin".to_owned(),
        format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
    ]
}

/// `getLocalGitUpdateTarget` final fallback: fetch HEAD into the origin/HEAD ref.
fn git_head_fallback_fetch_args() -> Vec<String> {
    vec![
        "fetch".to_owned(),
        "--prune".to_owned(),
        "--no-tags".to_owned(),
        "origin".to_owned(),
        "+HEAD:refs/remotes/origin/HEAD".to_owned(),
    ]
}

/// `git@{upstream}` resolved fetch/reset target.
struct GitUpdateTarget {
    reference: String,
    fetch_args: Vec<String>,
}

fn is_exact_npm_version(version: Option<&str>) -> bool {
    version.is_some_and(|v| Version::parse(v).is_ok())
}

fn spec_if_version(spec: &str, name: &str, version: Option<&str>) -> String {
    if version.is_some() {
        spec.to_owned()
    } else {
        format!("{name}@latest")
    }
}

fn read_installed_version(install_path: &Path) -> Option<String> {
    let package_json = install_path.join("package.json");
    let content = fs::read_to_string(package_json).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    value
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn parse_npm_view_version(raw: &str, range: Option<&str>) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    if let Some(s) = value.as_str() {
        return Some(s.to_owned());
    }
    let arr = value.as_array()?;
    let mut versions: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned))
        .collect();
    if let Some(range) = range
        && let Ok(req) = VersionReq::parse(range)
    {
        versions.retain(|v| Version::parse(v).is_ok_and(|parsed| req.matches(&parsed)));
    }
    semantic_max(versions)
}

/// Select the highest semver from a list, stringifying the result (TS `rcompare`).
fn semantic_max(versions: Vec<String>) -> Option<String> {
    versions
        .into_iter()
        .filter_map(|v| Version::parse(&v).ok().map(|parsed| (v, parsed)))
        .max_by(|a, b| a.1.cmp(&b.1))
        .map(|(v, _)| v)
}

fn parse_pnpm_global_path(raw: &str, name: &str) -> Option<PathBuf> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let entries = value.as_array()?;
    for entry in entries {
        if let Some(deps) = entry.get("dependencies").and_then(Value::as_object)
            && let Some(path) = deps
                .get(name)
                .and_then(|d| d.get("path"))
                .and_then(Value::as_str)
        {
            return Some(PathBuf::from(path));
        }
    }
    None
}

fn package_source_string(pkg: &PackageSource) -> String {
    match pkg {
        PackageSource::Source(source) => source.clone(),
        PackageSource::Filtered(filter) => filter.source.clone(),
    }
}

fn replace_source(existing: &PackageSource, normalized: String) -> PackageSource {
    match existing {
        PackageSource::Source(_) => PackageSource::Source(normalized),
        PackageSource::Filtered(filter) => PackageSource::Filtered(PackageSourceFilter {
            source: normalized,
            autoload: filter.autoload,
            extensions: filter.extensions.clone(),
            skills: filter.skills.clone(),
            prompts: filter.prompts.clone(),
            themes: filter.themes.clone(),
            extra: filter.extra.clone(),
        }),
    }
}

fn no_matching_package_error(source: &str, configured: &[PackageSource]) -> PackageManagerError {
    match find_suggested_source(source, configured) {
        Some(suggestion) => {
            PackageManagerError::NoMatchingPackageWithSuggestion(source.to_owned(), suggestion)
        }
        None => PackageManagerError::NoMatchingPackage(source.to_owned()),
    }
}

fn find_suggested_source(source: &str, configured: &[PackageSource]) -> Option<String> {
    let trimmed = source.trim();
    for pkg in configured {
        let src = package_source_string(pkg);
        match parse_source(&src) {
            ParsedSource::Npm { name, spec, .. } => {
                if trimmed == name || trimmed == spec {
                    return Some(src);
                }
            }
            ParsedSource::Git {
                host,
                path,
                ref_name,
                ..
            } => {
                let shorthand = format!("{host}/{path}");
                let with_ref = ref_name.as_ref().map(|r| format!("{shorthand}@{r}"));
                if trimmed == shorthand || with_ref.is_some_and(|w| trimmed == w) {
                    return Some(src);
                }
            }
            ParsedSource::Local { .. } => {}
        }
    }
    None
}

fn runner_error(error: &RunError) -> PackageManagerError {
    PackageManagerError::Runner(error.to_string())
}

fn io_escape(error: &std::io::Error) -> PackageManagerError {
    PackageManagerError::Runner(error.to_string())
}

fn remove_all(path: &Path) -> Result<(), PackageManagerError> {
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|error| io_escape(&error))
    } else if path.exists() {
        fs::remove_file(path).map_err(|error| io_escape(&error))
    } else {
        Ok(())
    }
}

fn pi_offline_enabled() -> bool {
    match std::env::var("PI_OFFLINE") {
        Ok(value) => {
            value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests;
