//! Global and project settings: wire types, migrations, trust-gated merge, and
//! atomic persistence.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/settings-manager.ts`.
//!
//! Settings documents are carried as raw [`Map<String, Value>`] inside
//! [`SettingsManager`] so unknown keys, wrong-typed values, and explicit
//! `null` survive load → merge → save exactly like TypeScript. Typed
//! [`Settings`] / nested wire structs are a tolerant view; a known key with
//! the wrong JSON type reads as `None` in the view while remaining on disk.
//!
//! Persistence re-reads under the S0 [`LockGuard`], overlays only modified
//! top-level (and nested) keys, pretty-prints with two-space indent, and does
//! not force a trailing newline. A scope with a parse error refuses to save.
//! Project data is fully gated when untrusted; the refusal error is exact.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use pi_agent::QueueMode;
use pi_ai::{ModelThinkingLevel, Transport};
use serde_json::{Map, Value};
use thiserror::Error;
use uuid::Uuid;

use super::config::{CONFIG_DIR_NAME, expand_tilde_path, get_agent_dir, resolve_path};
use super::lockfile::LockGuard;
use super::trust::DefaultProjectTrust;

/// Default tokens reserved for prompt + response during compaction.
pub const DEFAULT_COMPACTION_RESERVE_TOKENS: u64 = 16384;
/// Default recent-message tokens kept after compaction.
pub const DEFAULT_COMPACTION_KEEP_RECENT_TOKENS: u64 = 20000;
/// Default tokens reserved for branch-summary generation.
pub const DEFAULT_BRANCH_SUMMARY_RESERVE_TOKENS: u64 = 16384;
/// Default automatic-retry attempt count.
pub const DEFAULT_RETRY_MAX_RETRIES: u64 = 3;
/// Default automatic-retry base backoff delay in milliseconds.
pub const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 2000;
/// Default maximum server-requested retry delay in milliseconds.
pub const DEFAULT_PROVIDER_MAX_RETRY_DELAY_MS: u64 = 60000;
/// Default HTTP header/body idle timeout (`http-dispatcher.ts`).
pub const DEFAULT_HTTP_IDLE_TIMEOUT_MS: u64 = 300_000;
/// Default preferred inline image width in terminal cells.
pub const DEFAULT_IMAGE_WIDTH_CELLS: u64 = 60;
/// Default maximum visible autocomplete items.
pub const DEFAULT_AUTOCOMPLETE_MAX_VISIBLE: u64 = 5;
/// Default markdown code block indent.
pub const DEFAULT_CODE_BLOCK_INDENT: &str = "  ";

const U64_MAX_F64: f64 = 18_446_744_073_709_551_616.0;
/// `i64::MIN` as an exact `f64` (exactly `-2^63`).
const I64_MIN_F64: f64 = -9_223_372_036_854_775_808.0;

const KNOWN_SETTINGS_KEYS: &[&str] = &[
    "lastChangelogVersion",
    "defaultProvider",
    "defaultModel",
    "defaultThinkingLevel",
    "transport",
    "steeringMode",
    "followUpMode",
    "theme",
    "themeMode",
    "compaction",
    "branchSummary",
    "retry",
    "hideThinkingBlock",
    "showCacheMissNotices",
    "externalEditor",
    "shellPath",
    "quietStartup",
    "defaultProjectTrust",
    "shellCommandPrefix",
    "npmCommand",
    "collapseChangelog",
    "enableInstallTelemetry",
    "enableAnalytics",
    "trackingId",
    "packages",
    "extensions",
    "skills",
    "prompts",
    "themes",
    "enableSkillCommands",
    "terminal",
    "images",
    "enabledModels",
    "doubleEscapeAction",
    "treeFilterMode",
    "thinkingBudgets",
    "editorPaddingX",
    "outputPad",
    "autocompleteMaxVisible",
    "showHardwareCursor",
    "markdown",
    "warnings",
    "sessionDir",
    "httpProxy",
    "httpIdleTimeoutMs",
    "websocketConnectTimeoutMs",
];

const PACKAGE_SOURCE_FILTER_KEYS: &[&str] = &[
    "source",
    "autoload",
    "extensions",
    "skills",
    "prompts",
    "themes",
];

/// Settings document scope: the global agent file or the project file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsScope {
    /// `{agentDir}/settings.json`.
    Global,
    /// `{cwd}/.pi/settings.json`.
    Project,
}

/// One error recorded by [`SettingsManager`], tagged with its scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingsError {
    /// Scope that produced the error.
    pub scope: SettingsScope,
    /// Underlying error message.
    pub message: String,
}

/// Errors thrown synchronously by [`SettingsManager`] methods.
#[derive(Debug, Error)]
pub enum SettingsManagerError {
    /// Project write attempted while the project is not trusted.
    #[error("Project is not trusted; refusing to write project settings")]
    ProjectNotTrusted,
    /// Invalid value passed to, or stored in, a numeric setting.
    #[error("Invalid {setting} setting: {value}")]
    InvalidSetting {
        /// Wire name of the setting.
        setting: &'static str,
        /// JavaScript `String(value)` rendering of the offending value.
        value: String,
    },
}

/// Options for [`SettingsManager::create`] / [`from_storage`] / [`in_memory`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SettingsManagerCreateOptions {
    /// Whether project settings are trusted and loaded. Defaults to `true`.
    pub project_trusted: bool,
}

impl Default for SettingsManagerCreateOptions {
    fn default() -> Self {
        Self {
            project_trusted: true,
        }
    }
}

impl SettingsManagerCreateOptions {
    /// Default options (project trusted).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set whether the project is trusted.
    #[must_use]
    pub const fn project_trusted(mut self, project_trusted: bool) -> Self {
        self.project_trusted = project_trusted;
        self
    }
}

/// Action for double-escape with an empty editor (`doubleEscapeAction`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DoubleEscapeAction {
    /// Fork the session.
    Fork,
    /// Open the session tree (default).
    #[default]
    Tree,
    /// Do nothing.
    None,
}

impl DoubleEscapeAction {
    /// Wire string used in settings JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fork => "fork",
            Self::Tree => "tree",
            Self::None => "none",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "fork" => Some(Self::Fork),
            "tree" => Some(Self::Tree),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// Default filter when opening `/tree` (`treeFilterMode`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TreeFilterMode {
    /// No filtering (default).
    #[default]
    Default,
    /// Hide tool messages.
    NoTools,
    /// Show only user messages.
    UserOnly,
    /// Show only labeled entries.
    LabeledOnly,
    /// Show everything.
    All,
}

impl TreeFilterMode {
    /// Wire string used in settings JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::NoTools => "no-tools",
            Self::UserOnly => "user-only",
            Self::LabeledOnly => "labeled-only",
            Self::All => "all",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "no-tools" => Some(Self::NoTools),
            "user-only" => Some(Self::UserOnly),
            "labeled-only" => Some(Self::LabeledOnly),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// Theme polarity mode (`themeMode`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ThemeMode {
    /// Match the terminal background (default).
    #[default]
    Auto,
    /// Always use the light member of the current theme family.
    Light,
    /// Always use the dark member of the current theme family.
    Dark,
}

impl ThemeMode {
    /// Wire string used in settings JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }
}

/// Infer the effective `themeMode` from the stored `theme` value.
///
/// Used when `themeMode` is unset or invalid, and for plain theme names
/// before `themeMode` becomes meaningful to the user.
#[must_use]
fn infer_theme_mode(theme: Option<&str>) -> ThemeMode {
    match theme {
        None => ThemeMode::Auto,
        Some(name) if name.contains('/') => ThemeMode::Auto,
        Some(name) if name == "light" || name.ends_with("-light") => ThemeMode::Light,
        Some(name) if name == "dark" || name.ends_with("-dark") => ThemeMode::Dark,
        Some(_) => ThemeMode::Dark,
    }
}

/// Horizontal chat-message output padding (`outputPad`, wire `0 | 1`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutputPad {
    /// No padding.
    Zero,
    /// One cell of padding (default).
    #[default]
    One,
}

impl OutputPad {
    /// Wire number used in settings JSON.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        match self {
            Self::Zero => 0,
            Self::One => 1,
        }
    }
}

/// Nested `compaction` settings object.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompactionSettings {
    /// Whether automatic compaction is enabled (default: true).
    pub enabled: Option<bool>,
    /// Tokens reserved for prompt + LLM response (default: 16384).
    pub reserve_tokens: Option<u64>,
    /// Recent-message tokens kept (default: 20000).
    pub keep_recent_tokens: Option<u64>,
    /// Unknown nested keys preserved from the raw document.
    pub extra: Map<String, Value>,
}

/// Nested `branchSummary` settings object.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BranchSummarySettings {
    /// Tokens reserved for prompt + LLM response (default: 16384).
    pub reserve_tokens: Option<u64>,
    /// Skip the "Summarize branch?" prompt, defaulting to no summary.
    pub skip_prompt: Option<bool>,
    /// Unknown nested keys preserved from the raw document.
    pub extra: Map<String, Value>,
}

/// Nested `retry.provider` settings object.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderRetrySettings {
    /// SDK/provider request timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// SDK/provider retry attempts.
    pub max_retries: Option<u64>,
    /// Max server-requested delay before failing (default: 60000).
    pub max_retry_delay_ms: Option<u64>,
    /// Unknown nested keys preserved from the raw document.
    pub extra: Map<String, Value>,
}

/// Nested `retry` settings object.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RetrySettings {
    /// Whether automatic retry is enabled (default: true).
    pub enabled: Option<bool>,
    /// Retry attempts (default: 3).
    pub max_retries: Option<u64>,
    /// Base delay for exponential backoff in milliseconds (default: 2000).
    pub base_delay_ms: Option<u64>,
    /// Provider-level retry overrides.
    pub provider: Option<ProviderRetrySettings>,
    /// Unknown nested keys preserved from the raw document.
    pub extra: Map<String, Value>,
}

/// Nested `terminal` settings object.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalSettings {
    /// Show inline images when the terminal supports them (default: true).
    pub show_images: Option<bool>,
    /// Preferred inline image width in terminal cells (default: 60).
    pub image_width_cells: Option<u64>,
    /// Clear empty rows when content shrinks (default: false).
    pub clear_on_shrink: Option<bool>,
    /// OSC 9;4 terminal progress indicators (default: false).
    pub show_terminal_progress: Option<bool>,
    /// Unknown nested keys preserved from the raw document.
    pub extra: Map<String, Value>,
}

/// Nested `images` settings object.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImageSettings {
    /// Resize images to 2000x2000 max for model compatibility (default: true).
    pub auto_resize: Option<bool>,
    /// Prevent all images from being sent to providers (default: false).
    pub block_images: Option<bool>,
    /// Unknown nested keys preserved from the raw document.
    pub extra: Map<String, Value>,
}

/// Nested `thinkingBudgets` settings object with custom token budgets.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThinkingBudgetsSettings {
    /// Budget for the minimal thinking level.
    pub minimal: Option<u64>,
    /// Budget for the low thinking level.
    pub low: Option<u64>,
    /// Budget for the medium thinking level.
    pub medium: Option<u64>,
    /// Budget for the high thinking level.
    pub high: Option<u64>,
    /// Unknown nested keys preserved from the raw document.
    pub extra: Map<String, Value>,
}

/// Nested `markdown` settings object.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MarkdownSettings {
    /// Code block indent (default: two spaces).
    pub code_block_indent: Option<String>,
    /// Unknown nested keys preserved from the raw document.
    pub extra: Map<String, Value>,
}

/// Nested `warnings` settings object.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WarningSettings {
    /// Warn about Anthropic extra usage (default: true).
    pub anthropic_extra_usage: Option<bool>,
    /// Unknown nested keys preserved from the raw document.
    pub extra: Map<String, Value>,
}

/// Package source for npm/git packages (`packages` array elements).
#[derive(Clone, Debug, PartialEq)]
pub enum PackageSource {
    /// Load all resources from the package (string form).
    Source(String),
    /// Object form with resource filtering.
    Filtered(PackageSourceFilter),
}

/// Object form of [`PackageSource`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PackageSourceFilter {
    /// Package source specifier (npm/git/local path).
    pub source: String,
    /// When false, only explicit resource patterns are applied.
    pub autoload: Option<bool>,
    /// Extension file/directory patterns to load.
    pub extensions: Option<Vec<String>>,
    /// Skill file/directory patterns to load.
    pub skills: Option<Vec<String>>,
    /// Prompt template path patterns to load.
    pub prompts: Option<Vec<String>>,
    /// Theme path patterns to load.
    pub themes: Option<Vec<String>>,
    /// Unknown nested keys preserved from the raw document.
    pub extra: Map<String, Value>,
}

impl PackageSource {
    /// Tolerant conversion from a raw JSON value.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(source) => Some(Self::Source(source.clone())),
            Value::Object(map) => {
                let source = map.get("source").and_then(Value::as_str)?.to_owned();
                Some(Self::Filtered(PackageSourceFilter {
                    source,
                    autoload: map.get("autoload").and_then(Value::as_bool),
                    extensions: string_array(map.get("extensions")),
                    skills: string_array(map.get("skills")),
                    prompts: string_array(map.get("prompts")),
                    themes: string_array(map.get("themes")),
                    extra: unknown_fields(map, PACKAGE_SOURCE_FILTER_KEYS),
                }))
            }
            _ => None,
        }
    }

    /// Serialize to the raw JSON wire form.
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Self::Source(source) => Value::String(source.clone()),
            Self::Filtered(filter) => {
                let mut map = filter.extra.clone();
                map.insert("source".to_owned(), Value::String(filter.source.clone()));
                insert_opt_bool(&mut map, "autoload", filter.autoload);
                insert_opt_strings(&mut map, "extensions", filter.extensions.as_deref());
                insert_opt_strings(&mut map, "skills", filter.skills.as_deref());
                insert_opt_strings(&mut map, "prompts", filter.prompts.as_deref());
                insert_opt_strings(&mut map, "themes", filter.themes.as_deref());
                Value::Object(map)
            }
        }
    }
}

/// Typed view over a settings document.
///
/// Every field mirrors the TypeScript `Settings` interface. Unknown keys are
/// preserved in [`Self::extra`]; a known key whose stored value has the wrong
/// JSON type reads as `None` here while remaining untouched on disk.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    /// Last changelog version acknowledged by the user.
    pub last_changelog_version: Option<String>,
    /// Default provider identifier.
    pub default_provider: Option<String>,
    /// Default model identifier.
    pub default_model: Option<String>,
    /// Default thinking level (wire includes `"off"`).
    pub default_thinking_level: Option<ModelThinkingLevel>,
    /// Streaming transport preference (default: auto).
    pub transport: Option<Transport>,
    /// Steering queue drain mode (default: one-at-a-time).
    pub steering_mode: Option<QueueMode>,
    /// Follow-up queue drain mode (default: one-at-a-time).
    pub follow_up_mode: Option<QueueMode>,
    /// Theme name or path.
    pub theme: Option<String>,
    /// Theme polarity mode (`auto`, `light`, `dark`).
    pub theme_mode: Option<ThemeMode>,
    /// Compaction settings.
    pub compaction: Option<CompactionSettings>,
    /// Branch-summary settings.
    pub branch_summary: Option<BranchSummarySettings>,
    /// Retry settings.
    pub retry: Option<RetrySettings>,
    /// Hide thinking blocks in the transcript (default: false).
    pub hide_thinking_block: Option<bool>,
    /// Show prompt-cache-miss transcript notices (default: false).
    pub show_cache_miss_notices: Option<bool>,
    /// Command for the Ctrl+G external editor; takes precedence over VISUAL/EDITOR.
    pub external_editor: Option<String>,
    /// Custom shell path; supports leading `~` expansion.
    pub shell_path: Option<String>,
    /// Suppress startup output (default: false).
    pub quiet_startup: Option<bool>,
    /// Default project trust decision; honored from the global file only.
    pub default_project_trust: Option<DefaultProjectTrust>,
    /// Prefix prepended to every bash command.
    pub shell_command_prefix: Option<String>,
    /// argv-style command used for npm lookup/install operations.
    pub npm_command: Option<Vec<String>>,
    /// Show condensed changelog after update (default: false).
    pub collapse_changelog: Option<bool>,
    /// Anonymous version/update ping after updates (default: true).
    pub enable_install_telemetry: Option<bool>,
    /// Opt-in analytics data sharing (default: false).
    pub enable_analytics: Option<bool>,
    /// Analytics tracking identifier, generated on first analytics opt-in.
    pub tracking_id: Option<String>,
    /// npm/git package sources.
    pub packages: Option<Vec<PackageSource>>,
    /// Local extension file paths or directories.
    pub extensions: Option<Vec<String>>,
    /// Local skill file paths or directories.
    pub skills: Option<Vec<String>>,
    /// Local prompt template paths or directories.
    pub prompts: Option<Vec<String>>,
    /// Local theme file paths or directories.
    pub themes: Option<Vec<String>>,
    /// Register skills as `/skill:name` commands (default: true).
    pub enable_skill_commands: Option<bool>,
    /// Terminal settings.
    pub terminal: Option<TerminalSettings>,
    /// Image settings.
    pub images: Option<ImageSettings>,
    /// Model patterns for cycling (same format as `--models`).
    pub enabled_models: Option<Vec<String>>,
    /// Double-escape action with empty editor (default: tree).
    pub double_escape_action: Option<DoubleEscapeAction>,
    /// Default `/tree` filter.
    pub tree_filter_mode: Option<TreeFilterMode>,
    /// Custom token budgets for thinking levels.
    pub thinking_budgets: Option<ThinkingBudgetsSettings>,
    /// Horizontal input-editor padding (default: 0).
    pub editor_padding_x: Option<u64>,
    /// Horizontal chat-output padding (default: 1).
    pub output_pad: Option<OutputPad>,
    /// Max visible autocomplete items (default: 5).
    pub autocomplete_max_visible: Option<u64>,
    /// Show the terminal cursor while still positioning it for IME.
    pub show_hardware_cursor: Option<bool>,
    /// Markdown rendering settings.
    pub markdown: Option<MarkdownSettings>,
    /// Warning toggles.
    pub warnings: Option<WarningSettings>,
    /// Custom session storage directory (same format as `--session-dir`).
    pub session_dir: Option<String>,
    /// Proxy URL applied as `HTTP_PROXY`/`HTTPS_PROXY` for managed HTTP clients.
    pub http_proxy: Option<String>,
    /// HTTP header/body idle timeout in milliseconds; 0 disables.
    pub http_idle_timeout_ms: Option<u64>,
    /// WebSocket connect/open handshake timeout in milliseconds; 0 disables.
    pub websocket_connect_timeout_ms: Option<u64>,
    /// Unknown top-level keys preserved from the raw document.
    pub extra: Map<String, Value>,
}

impl Settings {
    /// Build the tolerant typed view over a raw settings object.
    #[must_use]
    pub fn from_map(map: &Map<String, Value>) -> Self {
        Self {
            last_changelog_version: string_field(map, "lastChangelogVersion"),
            default_provider: string_field(map, "defaultProvider"),
            default_model: string_field(map, "defaultModel"),
            default_thinking_level: parse_thinking_level(map.get("defaultThinkingLevel")),
            transport: parse_transport(map.get("transport")),
            steering_mode: parse_queue_mode(map.get("steeringMode")),
            follow_up_mode: parse_queue_mode(map.get("followUpMode")),
            theme: string_field(map, "theme"),
            theme_mode: map
                .get("themeMode")
                .and_then(Value::as_str)
                .and_then(ThemeMode::parse),
            compaction: nested_field(map, "compaction", CompactionSettings::from_map),
            branch_summary: nested_field(map, "branchSummary", BranchSummarySettings::from_map),
            retry: nested_field(map, "retry", RetrySettings::from_map),
            hide_thinking_block: bool_field(map, "hideThinkingBlock"),
            show_cache_miss_notices: bool_field(map, "showCacheMissNotices"),
            external_editor: string_field(map, "externalEditor"),
            shell_path: string_field(map, "shellPath"),
            quiet_startup: bool_field(map, "quietStartup"),
            default_project_trust: parse_default_project_trust(map.get("defaultProjectTrust")),
            shell_command_prefix: string_field(map, "shellCommandPrefix"),
            npm_command: string_array(map.get("npmCommand")),
            collapse_changelog: bool_field(map, "collapseChangelog"),
            enable_install_telemetry: bool_field(map, "enableInstallTelemetry"),
            enable_analytics: bool_field(map, "enableAnalytics"),
            tracking_id: string_field(map, "trackingId"),
            packages: map
                .get("packages")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(PackageSource::from_value).collect()),
            extensions: string_array(map.get("extensions")),
            skills: string_array(map.get("skills")),
            prompts: string_array(map.get("prompts")),
            themes: string_array(map.get("themes")),
            enable_skill_commands: bool_field(map, "enableSkillCommands"),
            terminal: nested_field(map, "terminal", TerminalSettings::from_map),
            images: nested_field(map, "images", ImageSettings::from_map),
            enabled_models: string_array(map.get("enabledModels")),
            double_escape_action: map
                .get("doubleEscapeAction")
                .and_then(Value::as_str)
                .and_then(DoubleEscapeAction::parse),
            tree_filter_mode: map
                .get("treeFilterMode")
                .and_then(Value::as_str)
                .and_then(TreeFilterMode::parse),
            thinking_budgets: nested_field(
                map,
                "thinkingBudgets",
                ThinkingBudgetsSettings::from_map,
            ),
            editor_padding_x: number_to_u64(map.get("editorPaddingX")),
            output_pad: parse_output_pad(map.get("outputPad")),
            autocomplete_max_visible: number_to_u64(map.get("autocompleteMaxVisible")),
            show_hardware_cursor: bool_field(map, "showHardwareCursor"),
            markdown: nested_field(map, "markdown", MarkdownSettings::from_map),
            warnings: nested_field(map, "warnings", WarningSettings::from_map),
            session_dir: string_field(map, "sessionDir"),
            http_proxy: string_field(map, "httpProxy"),
            http_idle_timeout_ms: map.get("httpIdleTimeoutMs").and_then(parse_timeout_ms),
            websocket_connect_timeout_ms: map
                .get("websocketConnectTimeoutMs")
                .and_then(parse_timeout_ms),
            extra: unknown_fields(map, KNOWN_SETTINGS_KEYS),
        }
    }

    /// Serialize the typed view back to a raw settings object.
    ///
    /// `extra` keys are written first, then known keys, so a manually built
    /// `Settings` with a colliding `extra` key keeps the typed field's value.
    #[must_use]
    pub fn to_map(&self) -> Map<String, Value> {
        let mut map = self.extra.clone();
        self.insert_scalar_fields(&mut map);
        self.insert_nested_fields(&mut map);
        self.insert_resource_fields(&mut map);
        map
    }

    fn insert_scalar_fields(&self, map: &mut Map<String, Value>) {
        insert_opt_string(
            map,
            "lastChangelogVersion",
            self.last_changelog_version.as_deref(),
        );
        insert_opt_string(map, "defaultProvider", self.default_provider.as_deref());
        insert_opt_string(map, "defaultModel", self.default_model.as_deref());
        insert_opt_value(
            map,
            "defaultThinkingLevel",
            self.default_thinking_level.map(thinking_level_value),
        );
        insert_opt_value(map, "transport", self.transport.map(transport_value));
        insert_opt_value(
            map,
            "steeringMode",
            self.steering_mode.map(queue_mode_value),
        );
        insert_opt_value(
            map,
            "followUpMode",
            self.follow_up_mode.map(queue_mode_value),
        );
        insert_opt_string(map, "theme", self.theme.as_deref());
        insert_opt_value(
            map,
            "themeMode",
            self.theme_mode
                .map(|mode| Value::String(mode.as_str().to_owned())),
        );
        insert_opt_bool(map, "hideThinkingBlock", self.hide_thinking_block);
        insert_opt_bool(map, "showCacheMissNotices", self.show_cache_miss_notices);
        insert_opt_string(map, "externalEditor", self.external_editor.as_deref());
        insert_opt_string(map, "shellPath", self.shell_path.as_deref());
        insert_opt_bool(map, "quietStartup", self.quiet_startup);
        insert_opt_value(
            map,
            "defaultProjectTrust",
            self.default_project_trust
                .map(|trust| Value::String(trust.as_str().to_owned())),
        );
        insert_opt_string(
            map,
            "shellCommandPrefix",
            self.shell_command_prefix.as_deref(),
        );
        insert_opt_bool(map, "collapseChangelog", self.collapse_changelog);
        insert_opt_bool(map, "enableInstallTelemetry", self.enable_install_telemetry);
        insert_opt_bool(map, "enableAnalytics", self.enable_analytics);
        insert_opt_string(map, "trackingId", self.tracking_id.as_deref());
        insert_opt_bool(map, "enableSkillCommands", self.enable_skill_commands);
        insert_opt_value(
            map,
            "doubleEscapeAction",
            self.double_escape_action
                .map(|action| Value::String(action.as_str().to_owned())),
        );
        insert_opt_value(
            map,
            "treeFilterMode",
            self.tree_filter_mode
                .map(|mode| Value::String(mode.as_str().to_owned())),
        );
        insert_opt_u64(map, "editorPaddingX", self.editor_padding_x);
        insert_opt_value(
            map,
            "outputPad",
            self.output_pad.map(|pad| Value::from(pad.as_u64())),
        );
        insert_opt_u64(map, "autocompleteMaxVisible", self.autocomplete_max_visible);
        insert_opt_bool(map, "showHardwareCursor", self.show_hardware_cursor);
        insert_opt_string(map, "sessionDir", self.session_dir.as_deref());
        insert_opt_string(map, "httpProxy", self.http_proxy.as_deref());
        insert_opt_u64(map, "httpIdleTimeoutMs", self.http_idle_timeout_ms);
        insert_opt_u64(
            map,
            "websocketConnectTimeoutMs",
            self.websocket_connect_timeout_ms,
        );
    }

    fn insert_nested_fields(&self, map: &mut Map<String, Value>) {
        insert_opt_value(
            map,
            "compaction",
            self.compaction
                .as_ref()
                .map(|value| Value::Object(value.to_map())),
        );
        insert_opt_value(
            map,
            "branchSummary",
            self.branch_summary
                .as_ref()
                .map(|value| Value::Object(value.to_map())),
        );
        insert_opt_value(
            map,
            "retry",
            self.retry
                .as_ref()
                .map(|value| Value::Object(value.to_map())),
        );
        insert_opt_value(
            map,
            "terminal",
            self.terminal
                .as_ref()
                .map(|value| Value::Object(value.to_map())),
        );
        insert_opt_value(
            map,
            "images",
            self.images
                .as_ref()
                .map(|value| Value::Object(value.to_map())),
        );
        insert_opt_value(
            map,
            "thinkingBudgets",
            self.thinking_budgets
                .as_ref()
                .map(|value| Value::Object(value.to_map())),
        );
        insert_opt_value(
            map,
            "markdown",
            self.markdown
                .as_ref()
                .map(|value| Value::Object(value.to_map())),
        );
        insert_opt_value(
            map,
            "warnings",
            self.warnings
                .as_ref()
                .map(|value| Value::Object(value.to_map())),
        );
    }

    fn insert_resource_fields(&self, map: &mut Map<String, Value>) {
        insert_opt_strings(map, "npmCommand", self.npm_command.as_deref());
        insert_opt_value(
            map,
            "packages",
            self.packages.as_ref().map(|packages| {
                Value::Array(packages.iter().map(PackageSource::to_value).collect())
            }),
        );
        insert_opt_strings(map, "extensions", self.extensions.as_deref());
        insert_opt_strings(map, "skills", self.skills.as_deref());
        insert_opt_strings(map, "prompts", self.prompts.as_deref());
        insert_opt_strings(map, "themes", self.themes.as_deref());
        insert_opt_strings(map, "enabledModels", self.enabled_models.as_deref());
    }
}

impl CompactionSettings {
    fn from_map(map: &Map<String, Value>) -> Self {
        Self {
            enabled: bool_field(map, "enabled"),
            reserve_tokens: number_to_u64(map.get("reserveTokens")),
            keep_recent_tokens: number_to_u64(map.get("keepRecentTokens")),
            extra: unknown_fields(map, &["enabled", "reserveTokens", "keepRecentTokens"]),
        }
    }

    fn to_map(&self) -> Map<String, Value> {
        let mut map = self.extra.clone();
        insert_opt_bool(&mut map, "enabled", self.enabled);
        insert_opt_u64(&mut map, "reserveTokens", self.reserve_tokens);
        insert_opt_u64(&mut map, "keepRecentTokens", self.keep_recent_tokens);
        map
    }
}

impl BranchSummarySettings {
    fn from_map(map: &Map<String, Value>) -> Self {
        Self {
            reserve_tokens: number_to_u64(map.get("reserveTokens")),
            skip_prompt: bool_field(map, "skipPrompt"),
            extra: unknown_fields(map, &["reserveTokens", "skipPrompt"]),
        }
    }

    fn to_map(&self) -> Map<String, Value> {
        let mut map = self.extra.clone();
        insert_opt_u64(&mut map, "reserveTokens", self.reserve_tokens);
        insert_opt_bool(&mut map, "skipPrompt", self.skip_prompt);
        map
    }
}

impl ProviderRetrySettings {
    fn from_map(map: &Map<String, Value>) -> Self {
        Self {
            timeout_ms: number_to_u64(map.get("timeoutMs")),
            max_retries: number_to_u64(map.get("maxRetries")),
            max_retry_delay_ms: number_to_u64(map.get("maxRetryDelayMs")),
            extra: unknown_fields(map, &["timeoutMs", "maxRetries", "maxRetryDelayMs"]),
        }
    }

    fn to_map(&self) -> Map<String, Value> {
        let mut map = self.extra.clone();
        insert_opt_u64(&mut map, "timeoutMs", self.timeout_ms);
        insert_opt_u64(&mut map, "maxRetries", self.max_retries);
        insert_opt_u64(&mut map, "maxRetryDelayMs", self.max_retry_delay_ms);
        map
    }
}

impl RetrySettings {
    fn from_map(map: &Map<String, Value>) -> Self {
        Self {
            enabled: bool_field(map, "enabled"),
            max_retries: number_to_u64(map.get("maxRetries")),
            base_delay_ms: number_to_u64(map.get("baseDelayMs")),
            provider: nested_field(map, "provider", ProviderRetrySettings::from_map),
            extra: unknown_fields(map, &["enabled", "maxRetries", "baseDelayMs", "provider"]),
        }
    }

    fn to_map(&self) -> Map<String, Value> {
        let mut map = self.extra.clone();
        insert_opt_bool(&mut map, "enabled", self.enabled);
        insert_opt_u64(&mut map, "maxRetries", self.max_retries);
        insert_opt_u64(&mut map, "baseDelayMs", self.base_delay_ms);
        insert_opt_value(
            &mut map,
            "provider",
            self.provider
                .as_ref()
                .map(|value| Value::Object(value.to_map())),
        );
        map
    }
}

impl TerminalSettings {
    fn from_map(map: &Map<String, Value>) -> Self {
        Self {
            show_images: bool_field(map, "showImages"),
            image_width_cells: number_to_u64(map.get("imageWidthCells")),
            clear_on_shrink: bool_field(map, "clearOnShrink"),
            show_terminal_progress: bool_field(map, "showTerminalProgress"),
            extra: unknown_fields(
                map,
                &[
                    "showImages",
                    "imageWidthCells",
                    "clearOnShrink",
                    "showTerminalProgress",
                ],
            ),
        }
    }

    fn to_map(&self) -> Map<String, Value> {
        let mut map = self.extra.clone();
        insert_opt_bool(&mut map, "showImages", self.show_images);
        insert_opt_u64(&mut map, "imageWidthCells", self.image_width_cells);
        insert_opt_bool(&mut map, "clearOnShrink", self.clear_on_shrink);
        insert_opt_bool(
            &mut map,
            "showTerminalProgress",
            self.show_terminal_progress,
        );
        map
    }
}

impl ImageSettings {
    fn from_map(map: &Map<String, Value>) -> Self {
        Self {
            auto_resize: bool_field(map, "autoResize"),
            block_images: bool_field(map, "blockImages"),
            extra: unknown_fields(map, &["autoResize", "blockImages"]),
        }
    }

    fn to_map(&self) -> Map<String, Value> {
        let mut map = self.extra.clone();
        insert_opt_bool(&mut map, "autoResize", self.auto_resize);
        insert_opt_bool(&mut map, "blockImages", self.block_images);
        map
    }
}

impl ThinkingBudgetsSettings {
    fn from_map(map: &Map<String, Value>) -> Self {
        Self {
            minimal: number_to_u64(map.get("minimal")),
            low: number_to_u64(map.get("low")),
            medium: number_to_u64(map.get("medium")),
            high: number_to_u64(map.get("high")),
            extra: unknown_fields(map, &["minimal", "low", "medium", "high"]),
        }
    }

    fn to_map(&self) -> Map<String, Value> {
        let mut map = self.extra.clone();
        insert_opt_u64(&mut map, "minimal", self.minimal);
        insert_opt_u64(&mut map, "low", self.low);
        insert_opt_u64(&mut map, "medium", self.medium);
        insert_opt_u64(&mut map, "high", self.high);
        map
    }
}

impl MarkdownSettings {
    fn from_map(map: &Map<String, Value>) -> Self {
        Self {
            code_block_indent: string_field(map, "codeBlockIndent"),
            extra: unknown_fields(map, &["codeBlockIndent"]),
        }
    }

    fn to_map(&self) -> Map<String, Value> {
        let mut map = self.extra.clone();
        insert_opt_string(
            &mut map,
            "codeBlockIndent",
            self.code_block_indent.as_deref(),
        );
        map
    }
}

impl WarningSettings {
    fn from_map(map: &Map<String, Value>) -> Self {
        Self {
            anthropic_extra_usage: bool_field(map, "anthropicExtraUsage"),
            extra: unknown_fields(map, &["anthropicExtraUsage"]),
        }
    }

    fn to_map(&self) -> Map<String, Value> {
        let mut map = self.extra.clone();
        insert_opt_bool(&mut map, "anthropicExtraUsage", self.anthropic_extra_usage);
        map
    }
}

/// Fully-resolved compaction configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedCompactionSettings {
    /// Whether automatic compaction is enabled.
    pub enabled: bool,
    /// Tokens reserved for prompt + LLM response.
    pub reserve_tokens: u64,
    /// Recent-message tokens kept.
    pub keep_recent_tokens: u64,
}

/// Fully-resolved branch-summary configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedBranchSummarySettings {
    /// Tokens reserved for prompt + LLM response.
    pub reserve_tokens: u64,
    /// Whether the "Summarize branch?" prompt is skipped.
    pub skip_prompt: bool,
}

/// Fully-resolved retry configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedRetrySettings {
    /// Whether automatic retry is enabled.
    pub enabled: bool,
    /// Retry attempts.
    pub max_retries: u64,
    /// Base delay for exponential backoff in milliseconds.
    pub base_delay_ms: u64,
}

/// Fully-resolved provider retry configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedProviderRetrySettings {
    /// SDK/provider request timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// SDK/provider retry attempts.
    pub max_retries: Option<u64>,
    /// Max server-requested delay before failing.
    pub max_retry_delay_ms: u64,
}

/// Storage backend for settings documents.
///
/// Ports the TypeScript `SettingsStorage` interface: `with_lock` hands the
/// current raw document text (or `None` when the file does not exist) to `f`;
/// the callback returns the next text to persist, or `Ok(None)` to perform no
/// write. A callback `Err` aborts without writing.
pub trait SettingsStorage: Send + Sync {
    /// Run `f` under the scope's exclusive lock.
    ///
    /// # Errors
    ///
    /// Returns the lock, read, write, or callback error message.
    fn with_lock(
        &mut self,
        scope: SettingsScope,
        f: &mut dyn FnMut(Option<String>) -> Result<Option<String>, String>,
    ) -> Result<(), String>;
}

/// File-backed storage: `{agentDir}/settings.json` and `{cwd}/.pi/settings.json`.
///
/// Mirrors TypeScript `FileSettingsStorage`:
/// - the settings file itself is locked (`settings.json.lock` sibling)
/// - a missing file is read without acquiring the lock when no write happens
/// - the parent directory is created only when content is actually written
#[derive(Debug)]
pub struct FileSettingsStorage {
    global_settings_path: PathBuf,
    project_settings_path: PathBuf,
}

impl FileSettingsStorage {
    /// Create storage rooted at `cwd` (project) and `agent_dir` (global).
    #[must_use]
    pub fn new(cwd: impl AsRef<Path>, agent_dir: impl AsRef<Path>) -> Self {
        let resolved_cwd = resolve_path(path_to_string(cwd.as_ref()));
        let resolved_agent_dir = resolve_path(path_to_string(agent_dir.as_ref()));
        Self {
            global_settings_path: resolved_agent_dir.join("settings.json"),
            project_settings_path: resolved_cwd.join(CONFIG_DIR_NAME).join("settings.json"),
        }
    }

    /// Path of the global settings file.
    #[must_use]
    pub fn global_settings_path(&self) -> &Path {
        &self.global_settings_path
    }

    /// Path of the project settings file.
    #[must_use]
    pub fn project_settings_path(&self) -> &Path {
        &self.project_settings_path
    }

    fn path_for(&self, scope: SettingsScope) -> &Path {
        match scope {
            SettingsScope::Global => &self.global_settings_path,
            SettingsScope::Project => &self.project_settings_path,
        }
    }
}

impl SettingsStorage for FileSettingsStorage {
    fn with_lock(
        &mut self,
        scope: SettingsScope,
        f: &mut dyn FnMut(Option<String>) -> Result<Option<String>, String>,
    ) -> Result<(), String> {
        let path = self.path_for(scope).to_path_buf();
        let dir = path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

        let file_exists = path.exists();
        let mut guard = if file_exists {
            Some(LockGuard::acquire(&path).map_err(|error| error.to_string())?)
        } else {
            None
        };
        let current =
            if file_exists {
                Some(fs::read_to_string(&path).map_err(|error| {
                    format!("Failed to read settings {}: {error}", path.display())
                })?)
            } else {
                None
            };
        let next = f(current)?;
        if let Some(next) = next {
            if !dir.exists() {
                fs::create_dir_all(&dir).map_err(|error| {
                    format!(
                        "Failed to create settings directory {}: {error}",
                        dir.display()
                    )
                })?;
            }
            if guard.is_none() {
                guard = Some(LockGuard::acquire(&path).map_err(|error| error.to_string())?);
            }
            fs::write(&path, next)
                .map_err(|error| format!("Failed to write settings {}: {error}", path.display()))?;
        }
        drop(guard);
        Ok(())
    }
}

/// In-memory storage backend (no file I/O).
#[derive(Debug, Default)]
pub struct InMemorySettingsStorage {
    global: Option<String>,
    project: Option<String>,
}

impl InMemorySettingsStorage {
    /// Create empty in-memory storage.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SettingsStorage for InMemorySettingsStorage {
    fn with_lock(
        &mut self,
        scope: SettingsScope,
        f: &mut dyn FnMut(Option<String>) -> Result<Option<String>, String>,
    ) -> Result<(), String> {
        let slot = match scope {
            SettingsScope::Global => &mut self.global,
            SettingsScope::Project => &mut self.project,
        };
        let next = f(slot.clone())?;
        if let Some(next) = next {
            *slot = Some(next);
        }
        Ok(())
    }
}

/// Settings manager: loads, migrates, merges, and persists global and
/// trust-gated project settings.
///
/// Internally documents are raw JSON objects; merges and migrations operate at
/// the JSON level exactly like the TypeScript implementation. Writes are
/// performed synchronously inside setters; storage errors are recorded and
/// surfaced through [`Self::drain_errors`]. The only methods that return
/// errors are project writes while untrusted and the timeout getter/setter
/// validations.
pub struct SettingsManager {
    storage: Box<dyn SettingsStorage>,
    global_settings: Map<String, Value>,
    project_settings: Map<String, Value>,
    settings: Map<String, Value>,
    project_trusted: bool,
    modified_fields: BTreeSet<String>,
    modified_nested_fields: BTreeMap<String, BTreeSet<String>>,
    modified_project_fields: BTreeSet<String>,
    modified_project_nested_fields: BTreeMap<String, BTreeSet<String>>,
    global_settings_load_error: Option<String>,
    project_settings_load_error: Option<String>,
    errors: Vec<SettingsError>,
    /// Serializes in-process writes so [`Self::flush`] can wait for them.
    write_mutex: Mutex<()>,
}

impl SettingsManager {
    fn new(
        storage: Box<dyn SettingsStorage>,
        global_settings: Map<String, Value>,
        project_settings: Map<String, Value>,
        global_load_error: Option<String>,
        project_load_error: Option<String>,
        initial_errors: Vec<SettingsError>,
        project_trusted: bool,
    ) -> Self {
        let settings = deep_merge_settings(&global_settings, &project_settings);
        Self {
            storage,
            global_settings,
            project_settings,
            settings,
            project_trusted,
            modified_fields: BTreeSet::new(),
            modified_nested_fields: BTreeMap::new(),
            modified_project_fields: BTreeSet::new(),
            modified_project_nested_fields: BTreeMap::new(),
            global_settings_load_error: global_load_error,
            project_settings_load_error: project_load_error,
            errors: initial_errors,
            write_mutex: Mutex::new(()),
        }
    }

    /// Create a manager backed by settings files under `agent_dir`
    /// (default: [`get_agent_dir`]) and `{cwd}/.pi`.
    #[must_use]
    pub fn create(
        cwd: impl AsRef<Path>,
        agent_dir: Option<impl AsRef<Path>>,
        options: SettingsManagerCreateOptions,
    ) -> Self {
        let agent_dir = agent_dir.map_or_else(get_agent_dir, |dir| dir.as_ref().to_path_buf());
        Self::from_storage(Box::new(FileSettingsStorage::new(cwd, agent_dir)), options)
    }

    /// Create a manager from an arbitrary storage backend.
    #[must_use]
    pub fn from_storage(
        mut storage: Box<dyn SettingsStorage>,
        options: SettingsManagerCreateOptions,
    ) -> Self {
        let project_trusted = options.project_trusted;
        let (global_settings, global_error) =
            Self::try_load_from_storage(storage.as_mut(), SettingsScope::Global, true);
        let (project_settings, project_error) =
            Self::try_load_from_storage(storage.as_mut(), SettingsScope::Project, project_trusted);
        let mut initial_errors = Vec::new();
        if let Some(message) = &global_error {
            initial_errors.push(SettingsError {
                scope: SettingsScope::Global,
                message: message.clone(),
            });
        }
        if let Some(message) = &project_error {
            initial_errors.push(SettingsError {
                scope: SettingsScope::Project,
                message: message.clone(),
            });
        }
        Self::new(
            storage,
            global_settings,
            project_settings,
            global_error,
            project_error,
            initial_errors,
            project_trusted,
        )
    }

    /// Create an in-memory manager (no file I/O) seeded with `settings`.
    #[must_use]
    pub fn in_memory(settings: &Settings, options: SettingsManagerCreateOptions) -> Self {
        let mut storage = InMemorySettingsStorage::new();
        let mut initial = settings.to_map();
        migrate_settings(&mut initial);
        let seed =
            serde_json::to_string_pretty(&Value::Object(initial)).unwrap_or_else(|_| "{}".into());
        let _ = storage.with_lock(SettingsScope::Global, &mut |_| Ok(Some(seed.clone())));
        Self::from_storage(Box::new(storage), options)
    }

    fn load_from_storage(
        storage: &mut dyn SettingsStorage,
        scope: SettingsScope,
        project_trusted: bool,
    ) -> Result<Map<String, Value>, String> {
        if scope == SettingsScope::Project && !project_trusted {
            return Ok(Map::new());
        }
        let mut content: Option<String> = None;
        storage.with_lock(scope, &mut |current| {
            content = current;
            Ok(None)
        })?;
        // `if (!content) return {}` — missing *or empty* file loads as empty.
        let Some(text) = content.filter(|text| !text.is_empty()) else {
            return Ok(Map::new());
        };
        parse_settings_text(&text)
    }

    fn try_load_from_storage(
        storage: &mut dyn SettingsStorage,
        scope: SettingsScope,
        project_trusted: bool,
    ) -> (Map<String, Value>, Option<String>) {
        match Self::load_from_storage(storage, scope, project_trusted) {
            Ok(settings) => (settings, None),
            Err(message) => (Map::new(), Some(message)),
        }
    }

    /// Typed view of the global settings document.
    #[must_use]
    pub fn get_global_settings(&self) -> Settings {
        Settings::from_map(&self.global_settings)
    }

    /// Typed view of the project settings document (empty while untrusted).
    #[must_use]
    pub fn get_project_settings(&self) -> Settings {
        Settings::from_map(&self.project_settings)
    }

    /// Whether the project is currently trusted.
    #[must_use]
    pub const fn is_project_trusted(&self) -> bool {
        self.project_trusted
    }

    /// Change project trust. `false` discards project data; `true` reloads it.
    pub fn set_project_trusted(&mut self, trusted: bool) {
        if self.project_trusted == trusted {
            return;
        }
        self.project_trusted = trusted;
        self.modified_project_fields.clear();
        self.modified_project_nested_fields.clear();
        if !trusted {
            self.project_settings = Map::new();
            self.project_settings_load_error = None;
            self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
            return;
        }
        let (project_settings, project_error) =
            Self::try_load_from_storage(self.storage.as_mut(), SettingsScope::Project, true);
        self.project_settings = project_settings;
        self.project_settings_load_error.clone_from(&project_error);
        if let Some(message) = project_error {
            self.record_error(SettingsScope::Project, message);
        }
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
    }

    /// Reload both scopes from storage, discarding pending modifications.
    pub fn reload(&mut self) {
        let (global_settings, global_error) =
            Self::try_load_from_storage(self.storage.as_mut(), SettingsScope::Global, true);
        match global_error {
            None => {
                self.global_settings = global_settings;
                self.global_settings_load_error = None;
            }
            Some(message) => {
                self.global_settings_load_error = Some(message.clone());
                self.record_error(SettingsScope::Global, message);
            }
        }
        self.modified_fields.clear();
        self.modified_nested_fields.clear();
        self.modified_project_fields.clear();
        self.modified_project_nested_fields.clear();
        let (project_settings, project_error) = Self::try_load_from_storage(
            self.storage.as_mut(),
            SettingsScope::Project,
            self.project_trusted,
        );
        match project_error {
            None => {
                self.project_settings = project_settings;
                self.project_settings_load_error = None;
            }
            Some(message) => {
                self.project_settings_load_error = Some(message.clone());
                self.record_error(SettingsScope::Project, message);
            }
        }
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
    }

    /// Apply additional overrides on top of the merged settings.
    ///
    /// Overrides are not persisted and are discarded by the next save (TS parity).
    pub fn apply_overrides(&mut self, overrides: &Map<String, Value>) {
        self.settings = deep_merge_settings(&self.settings, overrides);
    }

    /// Wait for any in-process write to finish.
    ///
    /// Setters acquire the same mutex around persistence, so this blocks until
    /// concurrent writers (if any) complete, matching TypeScript `flush` after
    /// the write queue drains.
    pub fn flush(&self) {
        let _guard = match self.write_mutex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
    }

    /// Take all recorded errors, leaving the error list empty.
    #[must_use]
    pub fn drain_errors(&mut self) -> Vec<SettingsError> {
        std::mem::take(&mut self.errors)
    }

    fn record_error(&mut self, scope: SettingsScope, message: String) {
        self.errors.push(SettingsError { scope, message });
    }

    fn clear_modified_scope(&mut self, scope: SettingsScope) {
        match scope {
            SettingsScope::Global => {
                self.modified_fields.clear();
                self.modified_nested_fields.clear();
            }
            SettingsScope::Project => {
                self.modified_project_fields.clear();
                self.modified_project_nested_fields.clear();
            }
        }
    }

    fn assert_project_trusted_for_write(&self) -> Result<(), SettingsManagerError> {
        if self.project_trusted {
            Ok(())
        } else {
            Err(SettingsManagerError::ProjectNotTrusted)
        }
    }

    fn persist_scoped_settings(
        &mut self,
        scope: SettingsScope,
        snapshot_settings: &Map<String, Value>,
        modified_fields: &BTreeSet<String>,
        modified_nested_fields: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<(), String> {
        if scope == SettingsScope::Project && !self.project_trusted {
            return Err(SettingsManagerError::ProjectNotTrusted.to_string());
        }
        let _write_guard = match self.write_mutex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.storage.with_lock(scope, &mut |current| {
            let current_file_settings = match current {
                Some(text) if !text.is_empty() => parse_settings_text(&text)?,
                _ => Map::new(),
            };
            let mut merged_settings = current_file_settings.clone();
            for field in modified_fields {
                let value = snapshot_settings.get(field);
                match (modified_nested_fields.get(field), value) {
                    (Some(nested_modified), Some(Value::Object(in_memory_nested))) => {
                        let mut merged_nested = match current_file_settings.get(field) {
                            Some(Value::Object(base_nested)) => base_nested.clone(),
                            _ => Map::new(),
                        };
                        for nested_key in nested_modified {
                            match in_memory_nested.get(nested_key) {
                                Some(nested_value) => {
                                    merged_nested.insert(nested_key.clone(), nested_value.clone());
                                }
                                None => {
                                    merged_nested.remove(nested_key);
                                }
                            }
                        }
                        merged_settings.insert(field.clone(), Value::Object(merged_nested));
                    }
                    _ => match value {
                        Some(value) => {
                            merged_settings.insert(field.clone(), value.clone());
                        }
                        None => {
                            merged_settings.remove(field);
                        }
                    },
                }
            }
            serde_json::to_string_pretty(&Value::Object(merged_settings))
                .map(Some)
                .map_err(|error| error.to_string())
        })
    }

    fn save(&mut self) {
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
        if self.global_settings_load_error.is_some() {
            return;
        }
        let snapshot = self.global_settings.clone();
        let modified_fields = self.modified_fields.clone();
        let modified_nested_fields = self.modified_nested_fields.clone();
        match self.persist_scoped_settings(
            SettingsScope::Global,
            &snapshot,
            &modified_fields,
            &modified_nested_fields,
        ) {
            Ok(()) => self.clear_modified_scope(SettingsScope::Global),
            Err(message) => self.record_error(SettingsScope::Global, message),
        }
    }

    fn save_project_settings(
        &mut self,
        settings: Map<String, Value>,
    ) -> Result<(), SettingsManagerError> {
        self.assert_project_trusted_for_write()?;
        self.project_settings = settings;
        self.settings = deep_merge_settings(&self.global_settings, &self.project_settings);
        if self.project_settings_load_error.is_some() {
            return Ok(());
        }
        let snapshot = self.project_settings.clone();
        let modified_fields = self.modified_project_fields.clone();
        let modified_nested_fields = self.modified_project_nested_fields.clone();
        match self.persist_scoped_settings(
            SettingsScope::Project,
            &snapshot,
            &modified_fields,
            &modified_nested_fields,
        ) {
            Ok(()) => {
                self.clear_modified_scope(SettingsScope::Project);
                Ok(())
            }
            Err(message) => {
                self.record_error(SettingsScope::Project, message);
                Ok(())
            }
        }
    }

    fn update_project_settings(
        &mut self,
        field: &'static str,
        update: impl FnOnce(&mut Map<String, Value>),
    ) -> Result<(), SettingsManagerError> {
        self.assert_project_trusted_for_write()?;
        let mut project_settings = self.project_settings.clone();
        update(&mut project_settings);
        self.modified_project_fields.insert(field.to_owned());
        self.save_project_settings(project_settings)
    }

    fn set_global_field(&mut self, field: &'static str, value: Value) {
        self.global_settings.insert(field.to_owned(), value);
        self.modified_fields.insert(field.to_owned());
        self.save();
    }

    fn set_global_optional_field(&mut self, field: &'static str, value: Option<Value>) {
        match value {
            Some(value) => {
                self.global_settings.insert(field.to_owned(), value);
            }
            None => {
                self.global_settings.remove(field);
            }
        }
        self.modified_fields.insert(field.to_owned());
        self.save();
    }

    fn set_global_nested_field(
        &mut self,
        field: &'static str,
        nested_key: &'static str,
        value: Value,
    ) {
        let mut nested = match self.global_settings.get_mut(field) {
            Some(Value::Object(object)) => std::mem::take(object),
            _ => Map::new(),
        };
        nested.insert(nested_key.to_owned(), value);
        self.global_settings
            .insert(field.to_owned(), Value::Object(nested));
        self.modified_fields.insert(field.to_owned());
        self.modified_nested_fields
            .entry(field.to_owned())
            .or_default()
            .insert(nested_key.to_owned());
        self.save();
    }

    fn merged_bool(&self, key: &str) -> Option<bool> {
        self.settings.get(key).and_then(Value::as_bool)
    }

    fn merged_nested_bool(&self, key: &str, nested_key: &str) -> Option<bool> {
        self.settings
            .get(key)
            .and_then(Value::as_object)?
            .get(nested_key)?
            .as_bool()
    }

    fn merged_nested_u64(&self, key: &str, nested_key: &str) -> Option<u64> {
        number_to_u64(
            self.settings
                .get(key)
                .and_then(Value::as_object)?
                .get(nested_key),
        )
    }

    fn merged_string_array(&self, key: &str) -> Vec<String> {
        string_array(self.settings.get(key)).unwrap_or_default()
    }

    // -- Changelog / session -------------------------------------------------

    /// `lastChangelogVersion` from merged settings.
    #[must_use]
    pub fn get_last_changelog_version(&self) -> Option<String> {
        string_field(&self.settings, "lastChangelogVersion")
    }

    /// Set `lastChangelogVersion` (global).
    pub fn set_last_changelog_version(&mut self, version: &str) {
        self.set_global_field("lastChangelogVersion", Value::String(version.to_owned()));
    }

    /// `sessionDir` with `~` expansion; empty string returned verbatim.
    #[must_use]
    pub fn get_session_dir(&self) -> Option<String> {
        normalize_optional_path(self.settings.get("sessionDir"))
    }

    // -- Model ----------------------------------------------------------------

    /// `defaultProvider` from merged settings.
    #[must_use]
    pub fn get_default_provider(&self) -> Option<String> {
        string_field(&self.settings, "defaultProvider")
    }

    /// `defaultModel` from merged settings.
    #[must_use]
    pub fn get_default_model(&self) -> Option<String> {
        string_field(&self.settings, "defaultModel")
    }

    /// Set `defaultProvider` (global).
    pub fn set_default_provider(&mut self, provider: &str) {
        self.set_global_field("defaultProvider", Value::String(provider.to_owned()));
    }

    /// Set `defaultModel` (global).
    pub fn set_default_model(&mut self, model_id: &str) {
        self.set_global_field("defaultModel", Value::String(model_id.to_owned()));
    }

    /// Set both `defaultProvider` and `defaultModel` with one save (global).
    pub fn set_default_model_and_provider(&mut self, provider: &str, model_id: &str) {
        self.global_settings.insert(
            "defaultProvider".to_owned(),
            Value::String(provider.to_owned()),
        );
        self.global_settings.insert(
            "defaultModel".to_owned(),
            Value::String(model_id.to_owned()),
        );
        self.modified_fields.insert("defaultProvider".to_owned());
        self.modified_fields.insert("defaultModel".to_owned());
        self.save();
    }

    /// `defaultThinkingLevel` from merged settings.
    #[must_use]
    pub fn get_default_thinking_level(&self) -> Option<ModelThinkingLevel> {
        parse_thinking_level(self.settings.get("defaultThinkingLevel"))
    }

    /// Set `defaultThinkingLevel` (global).
    pub fn set_default_thinking_level(&mut self, level: ModelThinkingLevel) {
        self.set_global_field("defaultThinkingLevel", thinking_level_value(level));
    }

    /// `enabledModels` from merged settings.
    #[must_use]
    pub fn get_enabled_models(&self) -> Option<Vec<String>> {
        string_array(self.settings.get("enabledModels"))
    }

    /// Set `enabledModels`; `None` removes the key (global).
    pub fn set_enabled_models(&mut self, patterns: Option<Vec<String>>) {
        self.set_global_optional_field(
            "enabledModels",
            patterns
                .map(|patterns| Value::Array(patterns.into_iter().map(Value::String).collect())),
        );
    }

    /// `thinkingBudgets` from merged settings.
    #[must_use]
    pub fn get_thinking_budgets(&self) -> Option<ThinkingBudgetsSettings> {
        nested_field(
            &self.settings,
            "thinkingBudgets",
            ThinkingBudgetsSettings::from_map,
        )
    }

    // -- Modes ----------------------------------------------------------------

    /// Steering queue drain mode (default: one-at-a-time).
    #[must_use]
    pub fn get_steering_mode(&self) -> QueueMode {
        parse_queue_mode(self.settings.get("steeringMode")).unwrap_or_default()
    }

    /// Set `steeringMode` (global).
    pub fn set_steering_mode(&mut self, mode: QueueMode) {
        self.set_global_field("steeringMode", queue_mode_value(mode));
    }

    /// Follow-up queue drain mode (default: one-at-a-time).
    #[must_use]
    pub fn get_follow_up_mode(&self) -> QueueMode {
        parse_queue_mode(self.settings.get("followUpMode")).unwrap_or_default()
    }

    /// Set `followUpMode` (global).
    pub fn set_follow_up_mode(&mut self, mode: QueueMode) {
        self.set_global_field("followUpMode", queue_mode_value(mode));
    }

    /// Streaming transport preference (default: auto).
    #[must_use]
    pub fn get_transport(&self) -> Transport {
        parse_transport(self.settings.get("transport")).unwrap_or(Transport::Auto)
    }

    /// Set `transport` (global).
    pub fn set_transport(&mut self, transport: Transport) {
        self.set_global_field("transport", transport_value(transport));
    }

    // -- Theme ----------------------------------------------------------------

    /// Raw `theme` setting when it is a string.
    #[must_use]
    pub fn get_theme_setting(&self) -> Option<String> {
        string_field(&self.settings, "theme")
    }

    /// Theme name or pair; `None` when unset.
    ///
    /// Slash-pair strings such as `"a/b"` are returned verbatim so callers
    /// can resolve them later.
    #[must_use]
    pub fn get_theme(&self) -> Option<String> {
        self.get_theme_setting()
    }

    /// Set `theme` (global).
    pub fn set_theme(&mut self, theme: &str) {
        self.set_global_field("theme", Value::String(theme.to_owned()));
    }

    /// Theme polarity mode (default inferred from `theme`).
    #[must_use]
    pub fn get_theme_mode(&self) -> ThemeMode {
        self.global_settings
            .get("themeMode")
            .and_then(Value::as_str)
            .and_then(ThemeMode::parse)
            .unwrap_or_else(|| infer_theme_mode(self.get_theme().as_deref()))
    }

    /// Set `themeMode` (global).
    pub fn set_theme_mode(&mut self, mode: ThemeMode) {
        self.set_global_field("themeMode", Value::String(mode.as_str().to_owned()));
    }

    /// `themes` resource paths from merged settings.
    #[must_use]
    pub fn get_theme_paths(&self) -> Vec<String> {
        self.merged_string_array("themes")
    }

    /// Set `themes` resource paths (global).
    pub fn set_theme_paths(&mut self, paths: Vec<String>) {
        self.set_global_field(
            "themes",
            Value::Array(paths.into_iter().map(Value::String).collect()),
        );
    }

    /// Set `themes` resource paths in the project file.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsManagerError::ProjectNotTrusted`] when untrusted.
    pub fn set_project_theme_paths(
        &mut self,
        paths: Vec<String>,
    ) -> Result<(), SettingsManagerError> {
        self.update_project_settings("themes", |settings| {
            settings.insert(
                "themes".to_owned(),
                Value::Array(paths.into_iter().map(Value::String).collect()),
            );
        })
    }

    // -- Compaction / branch summary ------------------------------------------

    /// `compaction.enabled` (default: true).
    #[must_use]
    pub fn get_compaction_enabled(&self) -> bool {
        self.merged_nested_bool("compaction", "enabled")
            .unwrap_or(true)
    }

    /// Set `compaction.enabled` (global, nested merge).
    pub fn set_compaction_enabled(&mut self, enabled: bool) {
        self.set_global_nested_field("compaction", "enabled", Value::Bool(enabled));
    }

    /// `compaction.reserveTokens` (default: 16384).
    #[must_use]
    pub fn get_compaction_reserve_tokens(&self) -> u64 {
        self.merged_nested_u64("compaction", "reserveTokens")
            .unwrap_or(DEFAULT_COMPACTION_RESERVE_TOKENS)
    }

    /// `compaction.keepRecentTokens` (default: 20000).
    #[must_use]
    pub fn get_compaction_keep_recent_tokens(&self) -> u64 {
        self.merged_nested_u64("compaction", "keepRecentTokens")
            .unwrap_or(DEFAULT_COMPACTION_KEEP_RECENT_TOKENS)
    }

    /// Fully-resolved compaction settings.
    #[must_use]
    pub fn get_compaction_settings(&self) -> ResolvedCompactionSettings {
        ResolvedCompactionSettings {
            enabled: self.get_compaction_enabled(),
            reserve_tokens: self.get_compaction_reserve_tokens(),
            keep_recent_tokens: self.get_compaction_keep_recent_tokens(),
        }
    }

    /// Fully-resolved branch-summary settings.
    #[must_use]
    pub fn get_branch_summary_settings(&self) -> ResolvedBranchSummarySettings {
        ResolvedBranchSummarySettings {
            reserve_tokens: self
                .merged_nested_u64("branchSummary", "reserveTokens")
                .unwrap_or(DEFAULT_BRANCH_SUMMARY_RESERVE_TOKENS),
            skip_prompt: self.get_branch_summary_skip_prompt(),
        }
    }

    /// `branchSummary.skipPrompt` (default: false).
    #[must_use]
    pub fn get_branch_summary_skip_prompt(&self) -> bool {
        self.merged_nested_bool("branchSummary", "skipPrompt")
            .unwrap_or(false)
    }

    // -- Retry / HTTP ---------------------------------------------------------

    /// `retry.enabled` (default: true).
    #[must_use]
    pub fn get_retry_enabled(&self) -> bool {
        self.merged_nested_bool("retry", "enabled").unwrap_or(true)
    }

    /// Set `retry.enabled` (global, nested merge).
    pub fn set_retry_enabled(&mut self, enabled: bool) {
        self.set_global_nested_field("retry", "enabled", Value::Bool(enabled));
    }

    /// Fully-resolved automatic retry settings.
    #[must_use]
    pub fn get_retry_settings(&self) -> ResolvedRetrySettings {
        ResolvedRetrySettings {
            enabled: self.get_retry_enabled(),
            max_retries: self
                .merged_nested_u64("retry", "maxRetries")
                .unwrap_or(DEFAULT_RETRY_MAX_RETRIES),
            base_delay_ms: self
                .merged_nested_u64("retry", "baseDelayMs")
                .unwrap_or(DEFAULT_RETRY_BASE_DELAY_MS),
        }
    }

    /// Fully-resolved provider retry settings (`maxRetryDelayMs` default 60000).
    #[must_use]
    pub fn get_provider_retry_settings(&self) -> ResolvedProviderRetrySettings {
        let provider = self
            .settings
            .get("retry")
            .and_then(Value::as_object)
            .and_then(|retry| retry.get("provider").and_then(Value::as_object));
        ResolvedProviderRetrySettings {
            timeout_ms: provider.and_then(|p| number_to_u64(p.get("timeoutMs"))),
            max_retries: provider.and_then(|p| number_to_u64(p.get("maxRetries"))),
            max_retry_delay_ms: provider
                .and_then(|p| number_to_u64(p.get("maxRetryDelayMs")))
                .unwrap_or(DEFAULT_PROVIDER_MAX_RETRY_DELAY_MS),
        }
    }

    /// `httpIdleTimeoutMs` (default: 300000).
    ///
    /// # Errors
    ///
    /// Returns [`SettingsManagerError::InvalidSetting`] when a stored value is
    /// present but not a usable timeout.
    pub fn get_http_idle_timeout_ms(&self) -> Result<u64, SettingsManagerError> {
        Ok(
            parse_timeout_setting(self.settings.get("httpIdleTimeoutMs"), "httpIdleTimeoutMs")?
                .unwrap_or(DEFAULT_HTTP_IDLE_TIMEOUT_MS),
        )
    }

    /// Set `httpIdleTimeoutMs` (global); the value is floored.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsManagerError::InvalidSetting`] when non-finite or negative.
    pub fn set_http_idle_timeout_ms(
        &mut self,
        timeout_ms: f64,
    ) -> Result<(), SettingsManagerError> {
        if !timeout_ms.is_finite() || timeout_ms < 0.0 {
            return Err(SettingsManagerError::InvalidSetting {
                setting: "httpIdleTimeoutMs",
                value: js_number_to_string(timeout_ms),
            });
        }
        self.set_global_field("httpIdleTimeoutMs", json_floored_number(timeout_ms));
        Ok(())
    }

    /// `websocketConnectTimeoutMs`; `None` when unset.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsManagerError::InvalidSetting`] when a stored value is
    /// present but not a usable timeout.
    pub fn get_web_socket_connect_timeout_ms(&self) -> Result<Option<u64>, SettingsManagerError> {
        parse_timeout_setting(
            self.settings.get("websocketConnectTimeoutMs"),
            "websocketConnectTimeoutMs",
        )
    }

    // -- UI flags -------------------------------------------------------------

    /// `hideThinkingBlock` (default: false).
    #[must_use]
    pub fn get_hide_thinking_block(&self) -> bool {
        self.merged_bool("hideThinkingBlock").unwrap_or(false)
    }

    /// `showCacheMissNotices` (default: false).
    #[must_use]
    pub fn get_show_cache_miss_notices(&self) -> bool {
        self.merged_bool("showCacheMissNotices").unwrap_or(false)
    }

    /// External editor command: configured → VISUAL → EDITOR → notepad/nano.
    #[must_use]
    pub fn get_external_editor_command(&self) -> String {
        if let Some(configured) = string_field(&self.settings, "externalEditor")
            && !configured.trim().is_empty()
        {
            return configured;
        }
        if let Some(editor) = env::var("VISUAL").ok().filter(|value| !value.is_empty()) {
            return editor;
        }
        if let Some(editor) = env::var("EDITOR").ok().filter(|value| !value.is_empty()) {
            return editor;
        }
        if cfg!(windows) {
            "notepad".to_owned()
        } else {
            "nano".to_owned()
        }
    }

    /// Set `hideThinkingBlock` (global).
    pub fn set_hide_thinking_block(&mut self, hide: bool) {
        self.set_global_field("hideThinkingBlock", Value::Bool(hide));
    }

    /// Set `showCacheMissNotices` (global).
    pub fn set_show_cache_miss_notices(&mut self, show: bool) {
        self.set_global_field("showCacheMissNotices", Value::Bool(show));
    }

    /// `quietStartup` (default: false).
    #[must_use]
    pub fn get_quiet_startup(&self) -> bool {
        self.merged_bool("quietStartup").unwrap_or(false)
    }

    /// Set `quietStartup` (global).
    pub fn set_quiet_startup(&mut self, quiet: bool) {
        self.set_global_field("quietStartup", Value::Bool(quiet));
    }

    /// `doubleEscapeAction` (default: tree).
    #[must_use]
    pub fn get_double_escape_action(&self) -> DoubleEscapeAction {
        self.settings
            .get("doubleEscapeAction")
            .and_then(Value::as_str)
            .and_then(DoubleEscapeAction::parse)
            .unwrap_or_default()
    }

    /// Set `doubleEscapeAction` (global).
    pub fn set_double_escape_action(&mut self, action: DoubleEscapeAction) {
        self.set_global_field(
            "doubleEscapeAction",
            Value::String(action.as_str().to_owned()),
        );
    }

    /// `treeFilterMode`; an unrecognized stored value yields `default`.
    #[must_use]
    pub fn get_tree_filter_mode(&self) -> TreeFilterMode {
        self.settings
            .get("treeFilterMode")
            .and_then(Value::as_str)
            .and_then(TreeFilterMode::parse)
            .unwrap_or_default()
    }

    /// Set `treeFilterMode` (global).
    pub fn set_tree_filter_mode(&mut self, mode: TreeFilterMode) {
        self.set_global_field("treeFilterMode", Value::String(mode.as_str().to_owned()));
    }

    /// `showHardwareCursor`; falls back to `PI_HARDWARE_CURSOR === "1"`.
    #[must_use]
    pub fn get_show_hardware_cursor(&self) -> bool {
        self.merged_bool("showHardwareCursor")
            .unwrap_or_else(|| env_flag("PI_HARDWARE_CURSOR"))
    }

    /// Set `showHardwareCursor` (global).
    pub fn set_show_hardware_cursor(&mut self, enabled: bool) {
        self.set_global_field("showHardwareCursor", Value::Bool(enabled));
    }

    /// `editorPaddingX` (default: 0; the setter clamps to 0..=3).
    #[must_use]
    pub fn get_editor_padding_x(&self) -> u64 {
        number_to_u64(self.settings.get("editorPaddingX")).unwrap_or(0)
    }

    /// Set `editorPaddingX` (global), clamped to `0..=3` after flooring.
    pub fn set_editor_padding_x(&mut self, padding: f64) {
        self.set_global_field(
            "editorPaddingX",
            js_clamped_floored_number(padding, 0.0, 3.0),
        );
    }

    /// `outputPad`; only an exact numeric `0` yields [`OutputPad::Zero`].
    #[must_use]
    pub fn get_output_pad(&self) -> OutputPad {
        let is_zero = self
            .settings
            .get("outputPad")
            .and_then(Value::as_f64)
            .is_some_and(|value| js_number_eq(value, 0.0));
        if is_zero {
            OutputPad::Zero
        } else {
            OutputPad::One
        }
    }

    /// Set `outputPad` (global).
    pub fn set_output_pad(&mut self, padding: OutputPad) {
        self.set_global_field("outputPad", Value::from(padding.as_u64()));
    }

    /// `autocompleteMaxVisible` (default: 5; the setter clamps to 3..=20).
    #[must_use]
    pub fn get_autocomplete_max_visible(&self) -> u64 {
        number_to_u64(self.settings.get("autocompleteMaxVisible"))
            .unwrap_or(DEFAULT_AUTOCOMPLETE_MAX_VISIBLE)
    }

    /// Set `autocompleteMaxVisible` (global), clamped to `3..=20` after flooring.
    pub fn set_autocomplete_max_visible(&mut self, max_visible: f64) {
        self.set_global_field(
            "autocompleteMaxVisible",
            js_clamped_floored_number(max_visible, 3.0, 20.0),
        );
    }

    // -- Shell / npm ----------------------------------------------------------

    /// `shellPath` with `~` expansion; empty string returned verbatim.
    #[must_use]
    pub fn get_shell_path(&self) -> Option<String> {
        normalize_optional_path(self.settings.get("shellPath"))
    }

    /// Set `shellPath`; `None` removes the key (global).
    pub fn set_shell_path(&mut self, path: Option<String>) {
        self.set_global_optional_field("shellPath", path.map(Value::String));
    }

    /// `shellCommandPrefix` from merged settings.
    #[must_use]
    pub fn get_shell_command_prefix(&self) -> Option<String> {
        string_field(&self.settings, "shellCommandPrefix")
    }

    /// Set `shellCommandPrefix`; `None` removes the key (global).
    pub fn set_shell_command_prefix(&mut self, prefix: Option<String>) {
        self.set_global_optional_field("shellCommandPrefix", prefix.map(Value::String));
    }

    /// `npmCommand` argv from merged settings.
    #[must_use]
    pub fn get_npm_command(&self) -> Option<Vec<String>> {
        string_array(self.settings.get("npmCommand"))
    }

    /// Set `npmCommand`; `None` removes the key (global).
    pub fn set_npm_command(&mut self, command: Option<Vec<String>>) {
        self.set_global_optional_field(
            "npmCommand",
            command.map(|command| Value::Array(command.into_iter().map(Value::String).collect())),
        );
    }

    // -- Trust / telemetry ----------------------------------------------------

    /// `defaultProjectTrust` from the **global** settings only.
    #[must_use]
    pub fn get_default_project_trust(&self) -> DefaultProjectTrust {
        DefaultProjectTrust::parse(
            self.global_settings
                .get("defaultProjectTrust")
                .and_then(Value::as_str),
        )
    }

    /// Set `defaultProjectTrust` (global).
    pub fn set_default_project_trust(&mut self, default_project_trust: DefaultProjectTrust) {
        self.set_global_field(
            "defaultProjectTrust",
            Value::String(default_project_trust.as_str().to_owned()),
        );
    }

    /// `collapseChangelog` (default: false).
    #[must_use]
    pub fn get_collapse_changelog(&self) -> bool {
        self.merged_bool("collapseChangelog").unwrap_or(false)
    }

    /// Set `collapseChangelog` (global).
    pub fn set_collapse_changelog(&mut self, collapse: bool) {
        self.set_global_field("collapseChangelog", Value::Bool(collapse));
    }

    /// `enableInstallTelemetry` (default: true).
    #[must_use]
    pub fn get_enable_install_telemetry(&self) -> bool {
        self.merged_bool("enableInstallTelemetry").unwrap_or(true)
    }

    /// Set `enableInstallTelemetry` (global).
    pub fn set_enable_install_telemetry(&mut self, enabled: bool) {
        self.set_global_field("enableInstallTelemetry", Value::Bool(enabled));
    }

    /// `enableAnalytics` (default: false).
    #[must_use]
    pub fn get_enable_analytics(&self) -> bool {
        self.merged_bool("enableAnalytics").unwrap_or(false)
    }

    /// `trackingId` from merged settings.
    #[must_use]
    pub fn get_tracking_id(&self) -> Option<String> {
        string_field(&self.settings, "trackingId")
    }

    /// Set analytics opt-in; generates a `UUIDv4` tracking id on first opt-in.
    pub fn set_enable_analytics(&mut self, enabled: bool) {
        self.global_settings
            .insert("enableAnalytics".to_owned(), Value::Bool(enabled));
        self.modified_fields.insert("enableAnalytics".to_owned());
        let has_tracking_id = self
            .global_settings
            .get("trackingId")
            .is_some_and(js_truthy);
        if enabled && !has_tracking_id {
            self.global_settings.insert(
                "trackingId".to_owned(),
                Value::String(Uuid::new_v4().to_string()),
            );
            self.modified_fields.insert("trackingId".to_owned());
        }
        self.save();
    }

    // -- Resources ------------------------------------------------------------

    /// `packages` from merged settings.
    #[must_use]
    pub fn get_packages(&self) -> Vec<PackageSource> {
        self.settings
            .get("packages")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(PackageSource::from_value).collect())
            .unwrap_or_default()
    }

    /// Set `packages` (global).
    pub fn set_packages(&mut self, packages: &[PackageSource]) {
        self.set_global_field(
            "packages",
            Value::Array(packages.iter().map(PackageSource::to_value).collect()),
        );
    }

    /// Set `packages` in the project file.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsManagerError::ProjectNotTrusted`] when untrusted.
    pub fn set_project_packages(
        &mut self,
        packages: &[PackageSource],
    ) -> Result<(), SettingsManagerError> {
        self.update_project_settings("packages", |settings| {
            settings.insert(
                "packages".to_owned(),
                Value::Array(packages.iter().map(PackageSource::to_value).collect()),
            );
        })
    }

    /// `extensions` resource paths from merged settings.
    #[must_use]
    pub fn get_extension_paths(&self) -> Vec<String> {
        self.merged_string_array("extensions")
    }

    /// Set `extensions` resource paths (global).
    pub fn set_extension_paths(&mut self, paths: Vec<String>) {
        self.set_global_field(
            "extensions",
            Value::Array(paths.into_iter().map(Value::String).collect()),
        );
    }

    /// Set `extensions` resource paths in the project file.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsManagerError::ProjectNotTrusted`] when untrusted.
    pub fn set_project_extension_paths(
        &mut self,
        paths: Vec<String>,
    ) -> Result<(), SettingsManagerError> {
        self.update_project_settings("extensions", |settings| {
            settings.insert(
                "extensions".to_owned(),
                Value::Array(paths.into_iter().map(Value::String).collect()),
            );
        })
    }

    /// `skills` resource paths from merged settings.
    #[must_use]
    pub fn get_skill_paths(&self) -> Vec<String> {
        self.merged_string_array("skills")
    }

    /// Set `skills` resource paths (global).
    pub fn set_skill_paths(&mut self, paths: Vec<String>) {
        self.set_global_field(
            "skills",
            Value::Array(paths.into_iter().map(Value::String).collect()),
        );
    }

    /// Set `skills` resource paths in the project file.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsManagerError::ProjectNotTrusted`] when untrusted.
    pub fn set_project_skill_paths(
        &mut self,
        paths: Vec<String>,
    ) -> Result<(), SettingsManagerError> {
        self.update_project_settings("skills", |settings| {
            settings.insert(
                "skills".to_owned(),
                Value::Array(paths.into_iter().map(Value::String).collect()),
            );
        })
    }

    /// `prompts` resource paths from merged settings.
    #[must_use]
    pub fn get_prompt_template_paths(&self) -> Vec<String> {
        self.merged_string_array("prompts")
    }

    /// Set `prompts` resource paths (global).
    pub fn set_prompt_template_paths(&mut self, paths: Vec<String>) {
        self.set_global_field(
            "prompts",
            Value::Array(paths.into_iter().map(Value::String).collect()),
        );
    }

    /// Set `prompts` resource paths in the project file.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsManagerError::ProjectNotTrusted`] when untrusted.
    pub fn set_project_prompt_template_paths(
        &mut self,
        paths: Vec<String>,
    ) -> Result<(), SettingsManagerError> {
        self.update_project_settings("prompts", |settings| {
            settings.insert(
                "prompts".to_owned(),
                Value::Array(paths.into_iter().map(Value::String).collect()),
            );
        })
    }

    /// `enableSkillCommands` (default: true).
    #[must_use]
    pub fn get_enable_skill_commands(&self) -> bool {
        self.merged_bool("enableSkillCommands").unwrap_or(true)
    }

    /// Set `enableSkillCommands` (global).
    pub fn set_enable_skill_commands(&mut self, enabled: bool) {
        self.set_global_field("enableSkillCommands", Value::Bool(enabled));
    }

    // -- Terminal / images ----------------------------------------------------

    /// `terminal.showImages` (default: true).
    #[must_use]
    pub fn get_show_images(&self) -> bool {
        self.merged_nested_bool("terminal", "showImages")
            .unwrap_or(true)
    }

    /// Set `terminal.showImages` (global, nested merge).
    pub fn set_show_images(&mut self, show: bool) {
        self.set_global_nested_field("terminal", "showImages", Value::Bool(show));
    }

    /// `terminal.imageWidthCells` (default: 60; floored, min 1).
    #[must_use]
    pub fn get_image_width_cells(&self) -> u64 {
        let value = self
            .settings
            .get("terminal")
            .and_then(Value::as_object)
            .and_then(|terminal| terminal.get("imageWidthCells"));
        let Some(number) = value.and_then(Value::as_f64) else {
            return DEFAULT_IMAGE_WIDTH_CELLS;
        };
        if !number.is_finite() {
            return DEFAULT_IMAGE_WIDTH_CELLS;
        }
        floor_max_to_u64(number, 1)
    }

    /// Set `terminal.imageWidthCells` (global, nested merge); floored, min 1.
    pub fn set_image_width_cells(&mut self, width: f64) {
        self.set_global_nested_field(
            "terminal",
            "imageWidthCells",
            js_min_floored_number(width, 1.0),
        );
    }

    /// `terminal.clearOnShrink`; falls back to `PI_CLEAR_ON_SHRINK === "1"`.
    #[must_use]
    pub fn get_clear_on_shrink(&self) -> bool {
        self.merged_nested_bool("terminal", "clearOnShrink")
            .unwrap_or_else(|| env_flag("PI_CLEAR_ON_SHRINK"))
    }

    /// Set `terminal.clearOnShrink` (global, nested merge).
    pub fn set_clear_on_shrink(&mut self, enabled: bool) {
        self.set_global_nested_field("terminal", "clearOnShrink", Value::Bool(enabled));
    }

    /// `terminal.showTerminalProgress` (default: false).
    #[must_use]
    pub fn get_show_terminal_progress(&self) -> bool {
        self.merged_nested_bool("terminal", "showTerminalProgress")
            .unwrap_or(false)
    }

    /// Set `terminal.showTerminalProgress` (global, nested merge).
    pub fn set_show_terminal_progress(&mut self, enabled: bool) {
        self.set_global_nested_field("terminal", "showTerminalProgress", Value::Bool(enabled));
    }

    /// `images.autoResize` (default: true).
    #[must_use]
    pub fn get_image_auto_resize(&self) -> bool {
        self.merged_nested_bool("images", "autoResize")
            .unwrap_or(true)
    }

    /// Set `images.autoResize` (global, nested merge).
    pub fn set_image_auto_resize(&mut self, enabled: bool) {
        self.set_global_nested_field("images", "autoResize", Value::Bool(enabled));
    }

    /// `images.blockImages` (default: false).
    #[must_use]
    pub fn get_block_images(&self) -> bool {
        self.merged_nested_bool("images", "blockImages")
            .unwrap_or(false)
    }

    /// Set `images.blockImages` (global, nested merge).
    pub fn set_block_images(&mut self, blocked: bool) {
        self.set_global_nested_field("images", "blockImages", Value::Bool(blocked));
    }

    // -- Markdown / warnings --------------------------------------------------

    /// `markdown.codeBlockIndent` (default: two spaces).
    #[must_use]
    pub fn get_code_block_indent(&self) -> String {
        self.settings
            .get("markdown")
            .and_then(Value::as_object)
            .and_then(|markdown| markdown.get("codeBlockIndent"))
            .and_then(Value::as_str)
            .map_or_else(|| DEFAULT_CODE_BLOCK_INDENT.to_owned(), str::to_owned)
    }

    /// `warnings` object (empty when absent).
    #[must_use]
    pub fn get_warnings(&self) -> WarningSettings {
        nested_field(&self.settings, "warnings", WarningSettings::from_map).unwrap_or_default()
    }

    /// Set `warnings` (global, full overlay).
    pub fn set_warnings(&mut self, warnings: &WarningSettings) {
        self.set_global_field("warnings", Value::Object(warnings.to_map()));
    }
}

// ---------------------------------------------------------------------------
// Helpers: parse, merge, migrate, wire conversions
// ---------------------------------------------------------------------------

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn env_flag(name: &str) -> bool {
    env::var(name).is_ok_and(|value| value == "1")
}

fn string_field(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn bool_field(map: &Map<String, Value>, key: &str) -> Option<bool> {
    map.get(key).and_then(Value::as_bool)
}

fn nested_field<T>(
    map: &Map<String, Value>,
    key: &str,
    from: impl FnOnce(&Map<String, Value>) -> T,
) -> Option<T> {
    map.get(key).and_then(Value::as_object).map(from)
}

fn string_array(value: Option<&Value>) -> Option<Vec<String>> {
    value.and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect()
    })
}

fn unknown_fields(map: &Map<String, Value>, known: &[&str]) -> Map<String, Value> {
    map.iter()
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn insert_opt_value(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), value);
    }
}

fn insert_opt_string(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    insert_opt_value(map, key, value.map(|text| Value::String(text.to_owned())));
}

fn insert_opt_bool(map: &mut Map<String, Value>, key: &str, value: Option<bool>) {
    insert_opt_value(map, key, value.map(Value::Bool));
}

fn insert_opt_u64(map: &mut Map<String, Value>, key: &str, value: Option<u64>) {
    insert_opt_value(map, key, value.map(Value::from));
}

fn insert_opt_strings(map: &mut Map<String, Value>, key: &str, values: Option<&[String]>) {
    insert_opt_value(
        map,
        key,
        values.map(|values| Value::Array(values.iter().cloned().map(Value::String).collect())),
    );
}

fn number_to_u64(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if let Some(unsigned) = value.as_u64() {
        return Some(unsigned);
    }
    let number = value.as_f64()?;
    finite_floor_to_u64(number)
}

fn js_number_eq(lhs: f64, rhs: f64) -> bool {
    lhs.partial_cmp(&rhs) == Some(std::cmp::Ordering::Equal)
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| !js_number_eq(n, 0.0)),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn js_number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value.is_sign_positive() {
            "Infinity".to_owned()
        } else {
            "-Infinity".to_owned()
        };
    }
    if js_number_eq(value, 0.0) {
        return "0".to_owned();
    }
    format!("{value}")
}

fn js_value_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => {
            if let Some(signed) = number.as_i64() {
                signed.to_string()
            } else if let Some(unsigned) = number.as_u64() {
                unsigned.to_string()
            } else {
                number
                    .as_f64()
                    .map_or_else(|| "0".to_owned(), js_number_to_string)
            }
        }
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_value_to_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

/// Floor `value` and encode as a JSON number without float→int casts.
///
/// Non-finite inputs become JSON `null` (matching `JSON.stringify(NaN|±Infinity)`).
fn json_floored_number(value: f64) -> Value {
    let floored = value.floor();
    if !floored.is_finite() {
        return Value::Null;
    }
    if floored >= 0.0 {
        match finite_floor_to_u64(floored) {
            Some(unsigned) => Value::from(unsigned),
            None => Value::Null,
        }
    } else {
        match finite_floor_to_i64(floored) {
            Some(signed) => Value::from(signed),
            None => Value::Null,
        }
    }
}

fn js_clamped_floored_number(value: f64, min: f64, max: f64) -> Value {
    let floored = value.floor();
    if floored.is_nan() {
        return Value::Null;
    }
    // NaN already filtered; clamp is safe.
    let clamped = floored.clamp(min, max);
    json_floored_number(clamped)
}

fn js_min_floored_number(value: f64, min: f64) -> Value {
    let floored = value.floor();
    if floored.is_nan() {
        return Value::Null;
    }
    if !floored.is_finite() {
        // +Inf → stringify null; -Inf → min
        return if floored.is_sign_positive() {
            Value::Null
        } else {
            json_floored_number(min)
        };
    }
    json_floored_number(floored.max(min))
}

fn floor_max_to_u64(value: f64, min: u64) -> u64 {
    let Ok(min_f) = min.to_string().parse::<f64>() else {
        return min;
    };
    let floored = value.floor().max(min_f);
    match finite_floor_to_u64(floored) {
        Some(result) => result,
        None => min,
    }
}

fn parse_timeout_ms(value: &Value) -> Option<u64> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.eq_ignore_ascii_case("disabled") {
                return Some(0);
            }
            if trimmed.is_empty() {
                return None;
            }
            js_number_from_string(trimmed).and_then(finite_floor_to_u64)
        }
        Value::Number(number) => number.as_f64().and_then(finite_floor_to_u64),
        _ => None,
    }
}

fn parse_timeout_setting(
    value: Option<&Value>,
    setting: &'static str,
) -> Result<Option<u64>, SettingsManagerError> {
    match value {
        None => Ok(None),
        Some(value) => match parse_timeout_ms(value) {
            Some(ms) => Ok(Some(ms)),
            None => Err(SettingsManagerError::InvalidSetting {
                setting,
                value: js_value_to_string(value),
            }),
        },
    }
}

/// Floor a non-negative finite `f64` to `u64` via decimal text (no float cast).
fn finite_floor_to_u64(value: f64) -> Option<u64> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let floored = value.floor();
    if floored >= U64_MAX_F64 {
        return Some(u64::MAX);
    }
    // Integer-valued f64 in the safe range formats as a plain digit string.
    let text = format!("{floored:.0}");
    text.parse::<u64>().ok()
}

/// Floor a non-positive finite `f64` to `i64` via decimal text (no float cast).
fn finite_floor_to_i64(value: f64) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    let floored = value.floor();
    // i64::MIN is exactly -2^63, representable in f64.
    if floored < I64_MIN_F64 {
        return Some(i64::MIN);
    }
    if floored > 0.0 {
        // Positive values use the u64 path elsewhere; keep signed path defensive.
        let text = format!("{floored:.0}");
        return text.parse::<i64>().ok();
    }
    let text = format!("{floored:.0}");
    text.parse::<i64>().ok()
}

fn js_number_from_string(text: &str) -> Option<f64> {
    if text.is_empty() {
        return Some(0.0);
    }
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let magnitude = if rest == "Infinity" {
        Some(f64::INFINITY)
    } else if let Some(digits) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        parse_radix(digits, 16)
    } else if let Some(digits) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
        parse_radix(digits, 8)
    } else if let Some(digits) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
        parse_radix(digits, 2)
    } else if is_js_decimal(rest) {
        rest.parse::<f64>().ok()
    } else {
        None
    }?;
    Some(if negative { -magnitude } else { magnitude })
}

/// Parse a radix integer string into `f64` using only `From<u32>` (no casts).
fn parse_radix(digits: &str, radix: u32) -> Option<f64> {
    if digits.is_empty() {
        return None;
    }
    let mut value = 0.0_f64;
    let radix_f = f64::from(radix);
    for ch in digits.chars() {
        let digit = ch.to_digit(radix)?;
        value = value.mul_add(radix_f, f64::from(digit));
    }
    Some(value)
}

fn is_js_decimal(text: &str) -> bool {
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(index) => (&text[..index], Some(&text[index + 1..])),
        None => (text, None),
    };
    if let Some(exp) = exponent {
        let exp_digits = exp.strip_prefix(['+', '-']).unwrap_or(exp);
        if exp_digits.is_empty() || !exp_digits.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    let mut seen_digit = false;
    let mut seen_dot = false;
    for ch in mantissa.chars() {
        if ch == '.' {
            if seen_dot {
                return false;
            }
            seen_dot = true;
        } else if ch.is_ascii_digit() {
            seen_digit = true;
        } else {
            return false;
        }
    }
    seen_digit
}

fn parse_thinking_level(value: Option<&Value>) -> Option<ModelThinkingLevel> {
    match value?.as_str()? {
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

fn thinking_level_value(level: ModelThinkingLevel) -> Value {
    Value::String(
        match level {
            ModelThinkingLevel::Off => "off",
            ModelThinkingLevel::Minimal => "minimal",
            ModelThinkingLevel::Low => "low",
            ModelThinkingLevel::Medium => "medium",
            ModelThinkingLevel::High => "high",
            ModelThinkingLevel::Xhigh => "xhigh",
            ModelThinkingLevel::Max => "max",
        }
        .to_owned(),
    )
}

fn parse_transport(value: Option<&Value>) -> Option<Transport> {
    match value?.as_str()? {
        "sse" => Some(Transport::Sse),
        "websocket" => Some(Transport::Websocket),
        "websocket-cached" => Some(Transport::WebsocketCached),
        "auto" => Some(Transport::Auto),
        _ => None,
    }
}

fn transport_value(transport: Transport) -> Value {
    Value::String(
        match transport {
            Transport::Sse => "sse",
            Transport::Websocket => "websocket",
            Transport::WebsocketCached => "websocket-cached",
            Transport::Auto => "auto",
        }
        .to_owned(),
    )
}

fn parse_queue_mode(value: Option<&Value>) -> Option<QueueMode> {
    match value?.as_str()? {
        "all" => Some(QueueMode::All),
        "one-at-a-time" => Some(QueueMode::OneAtATime),
        _ => None,
    }
}

fn queue_mode_value(mode: QueueMode) -> Value {
    Value::String(
        match mode {
            QueueMode::All => "all",
            QueueMode::OneAtATime => "one-at-a-time",
        }
        .to_owned(),
    )
}

fn parse_default_project_trust(value: Option<&Value>) -> Option<DefaultProjectTrust> {
    match value?.as_str()? {
        "ask" => Some(DefaultProjectTrust::Ask),
        "always" => Some(DefaultProjectTrust::Always),
        "never" => Some(DefaultProjectTrust::Never),
        _ => None,
    }
}

fn parse_output_pad(value: Option<&Value>) -> Option<OutputPad> {
    let number = value?.as_f64()?;
    if js_number_eq(number, 0.0) {
        Some(OutputPad::Zero)
    } else if js_number_eq(number, 1.0) {
        Some(OutputPad::One)
    } else {
        None
    }
}

fn normalize_optional_path(value: Option<&Value>) -> Option<String> {
    let raw = value?.as_str()?;
    if raw.is_empty() {
        return Some(raw.to_owned());
    }
    Some(path_to_string(&expand_tilde_path(raw)))
}

fn parse_settings_text(text: &str) -> Result<Map<String, Value>, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    match value {
        Value::Object(mut map) => {
            migrate_settings(&mut map);
            Ok(map)
        }
        Value::Array(items) => {
            // TS spreads a top-level array at each use site; we spread at load.
            Ok(spread_array(&items))
        }
        other => Err(format!(
            "Cannot use 'in' operator to search for 'queueMode' in {}",
            js_value_to_string(&other)
        )),
    }
}

fn spread_array(items: &[Value]) -> Map<String, Value> {
    items
        .iter()
        .enumerate()
        .map(|(index, value)| (index.to_string(), value.clone()))
        .collect()
}

/// One-level object merge with array replacement (ports `deepMergeSettings`).
fn deep_merge_settings(
    base: &Map<String, Value>,
    overrides: &Map<String, Value>,
) -> Map<String, Value> {
    let mut result = base.clone();
    for (key, override_value) in overrides {
        let merged_value = match (base.get(key), override_value) {
            (Some(Value::Object(base_object)), Value::Object(override_object)) => {
                let mut merged = base_object.clone();
                for (nested_key, nested_value) in override_object {
                    merged.insert(nested_key.clone(), nested_value.clone());
                }
                Value::Object(merged)
            }
            _ => override_value.clone(),
        };
        result.insert(key.clone(), merged_value);
    }
    result
}

/// Migrate legacy settings keys in the exact TypeScript order.
fn migrate_settings(settings: &mut Map<String, Value>) {
    // 1. queueMode → steeringMode (only when steeringMode absent).
    if settings.contains_key("queueMode")
        && !settings.contains_key("steeringMode")
        && let Some(value) = settings.remove("queueMode")
    {
        settings.insert("steeringMode".to_owned(), value);
    }

    // 2. websockets boolean → transport (only when transport absent).
    if !settings.contains_key("transport")
        && let Some(Value::Bool(websockets)) = settings.get("websockets").cloned()
    {
        settings.remove("websockets");
        settings.insert(
            "transport".to_owned(),
            Value::String(if websockets {
                "websocket".to_owned()
            } else {
                "sse".to_owned()
            }),
        );
    }

    // 3. skills object → enableSkillCommands hoist + customDirectories array.
    if let Some(Value::Object(skills_object)) = settings.get("skills").cloned() {
        if let Some(enable) = skills_object.get("enableSkillCommands")
            && !settings.contains_key("enableSkillCommands")
        {
            settings.insert("enableSkillCommands".to_owned(), enable.clone());
        }
        match skills_object.get("customDirectories") {
            Some(Value::Array(dirs)) if !dirs.is_empty() => {
                settings.insert("skills".to_owned(), Value::Array(dirs.clone()));
            }
            _ => {
                settings.remove("skills");
            }
        }
    }

    // 4. retry.maxDelayMs → retry.provider.maxRetryDelayMs.
    if let Some(Value::Object(retry_object)) = settings.get("retry").cloned() {
        let mut retry_object = retry_object;
        let max_delay = retry_object
            .get("maxDelayMs")
            .filter(|value| value.is_number())
            .cloned();
        if let Some(max_delay_value) = max_delay {
            let provider_object_like: Option<Map<String, Value>> =
                match retry_object.get("provider") {
                    Some(Value::Object(map)) => Some(map.clone()),
                    Some(Value::Array(items)) => Some(spread_array(items)),
                    _ => None,
                };
            let current_max = provider_object_like
                .as_ref()
                .and_then(|provider| provider.get("maxRetryDelayMs"));
            let needs_migration = matches!(
                (provider_object_like.is_some(), current_max),
                (false, _) | (true, None | Some(Value::Null))
            );
            if needs_migration {
                let mut provider = provider_object_like.unwrap_or_default();
                provider.insert("maxRetryDelayMs".to_owned(), max_delay_value);
                retry_object.insert("provider".to_owned(), Value::Object(provider));
            }
        }
        retry_object.remove("maxDelayMs");
        settings.insert("retry".to_owned(), Value::Object(retry_object));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    type TestResult = Result<(), String>;

    fn unique_temp_dir(label: &str) -> Result<PathBuf, String> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let dir = env::temp_dir().join(format!("pi-settings-{label}-{nanos}"));
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        Ok(dir)
    }

    fn write_settings_file(path: &Path, contents: &str) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(path, contents).map_err(|error| error.to_string())
    }

    fn read_text(path: &Path) -> Result<String, String> {
        fs::read_to_string(path).map_err(|error| error.to_string())
    }

    fn parse_file(path: &Path) -> Result<Value, String> {
        let text = read_text(path)?;
        serde_json::from_str(&text).map_err(|error| error.to_string())
    }

    fn make_dirs(label: &str) -> Result<(PathBuf, PathBuf, PathBuf), String> {
        let root = unique_temp_dir(label)?;
        let agent = root.join("agent");
        let project = root.join("project");
        fs::create_dir_all(&agent).map_err(|error| error.to_string())?;
        fs::create_dir_all(&project).map_err(|error| error.to_string())?;
        Ok((root, agent, project))
    }

    fn create_manager(project: &Path, agent: &Path, trusted: bool) -> SettingsManager {
        SettingsManager::create(
            project,
            Some(agent),
            SettingsManagerCreateOptions::default().project_trusted(trusted),
        )
    }

    #[test]
    fn defaults_on_empty_settings() -> TestResult {
        let manager = SettingsManager::in_memory(
            &Settings::default(),
            SettingsManagerCreateOptions::default(),
        );
        assert_eq!(manager.get_transport(), Transport::Auto);
        assert_eq!(manager.get_steering_mode(), QueueMode::OneAtATime);
        assert_eq!(manager.get_follow_up_mode(), QueueMode::OneAtATime);
        assert!(manager.get_compaction_enabled());
        assert_eq!(manager.get_compaction_reserve_tokens(), 16384);
        assert_eq!(manager.get_compaction_keep_recent_tokens(), 20000);
        let branch = manager.get_branch_summary_settings();
        assert_eq!(branch.reserve_tokens, 16384);
        assert!(!branch.skip_prompt);
        let retry = manager.get_retry_settings();
        assert!(retry.enabled);
        assert_eq!(retry.max_retries, 3);
        assert_eq!(retry.base_delay_ms, 2000);
        assert_eq!(
            manager.get_provider_retry_settings().max_retry_delay_ms,
            60000
        );
        let idle = manager
            .get_http_idle_timeout_ms()
            .map_err(|error| error.to_string())?;
        assert_eq!(idle, 300_000);
        assert_eq!(manager.get_image_width_cells(), 60);
        assert_eq!(manager.get_editor_padding_x(), 0);
        assert_eq!(manager.get_output_pad(), OutputPad::One);
        assert_eq!(manager.get_autocomplete_max_visible(), 5);
        assert_eq!(manager.get_code_block_indent(), "  ");
        assert_eq!(manager.get_tree_filter_mode(), TreeFilterMode::Default);
        assert_eq!(manager.get_double_escape_action(), DoubleEscapeAction::Tree);
        assert!(manager.get_enable_install_telemetry());
        assert!(!manager.get_enable_analytics());
        assert!(manager.get_enable_skill_commands());
        assert!(manager.get_show_images());
        assert!(manager.get_image_auto_resize());
        assert!(!manager.get_block_images());
        assert!(!manager.get_show_terminal_progress());
        assert!(!manager.get_quiet_startup());
        assert!(!manager.get_hide_thinking_block());
        assert!(!manager.get_show_cache_miss_notices());
        assert!(!manager.get_collapse_changelog());
        assert_eq!(
            manager.get_default_project_trust(),
            DefaultProjectTrust::Ask
        );
        assert!(manager.get_theme().is_none());
        if env::var_os("PI_CLEAR_ON_SHRINK").is_none() {
            assert!(!manager.get_clear_on_shrink());
        }
        if env::var_os("PI_HARDWARE_CURSOR").is_none() {
            assert!(!manager.get_show_hardware_cursor());
        }
        if env::var_os("VISUAL").is_none() && env::var_os("EDITOR").is_none() {
            assert_eq!(
                manager.get_external_editor_command(),
                if cfg!(windows) { "notepad" } else { "nano" }
            );
        }
        Ok(())
    }

    #[test]
    fn clamps_and_floor_behaviors() -> TestResult {
        let mut manager = SettingsManager::in_memory(
            &Settings::default(),
            SettingsManagerCreateOptions::default(),
        );
        manager.set_editor_padding_x(9.7);
        assert_eq!(manager.get_editor_padding_x(), 3);
        manager.set_editor_padding_x(-2.3);
        assert_eq!(manager.get_editor_padding_x(), 0);
        manager.set_autocomplete_max_visible(100.0);
        assert_eq!(manager.get_autocomplete_max_visible(), 20);
        manager.set_autocomplete_max_visible(2.0);
        assert_eq!(manager.get_autocomplete_max_visible(), 3);
        manager.set_autocomplete_max_visible(7.9);
        assert_eq!(manager.get_autocomplete_max_visible(), 7);
        manager.set_image_width_cells(0.2);
        assert_eq!(manager.get_image_width_cells(), 1);
        manager.set_image_width_cells(80.9);
        assert_eq!(manager.get_image_width_cells(), 80);
        manager
            .set_http_idle_timeout_ms(1500.7)
            .map_err(|error| error.to_string())?;
        let idle = manager
            .get_http_idle_timeout_ms()
            .map_err(|error| error.to_string())?;
        assert_eq!(idle, 1500);
        let error = match manager.set_http_idle_timeout_ms(-1.0) {
            Ok(()) => return Err("expected negative rejection".into()),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "Invalid httpIdleTimeoutMs setting: -1");
        let error = match manager.set_http_idle_timeout_ms(f64::NAN) {
            Ok(()) => return Err("expected nan rejection".into()),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "Invalid httpIdleTimeoutMs setting: NaN");
        let error = match manager.set_http_idle_timeout_ms(f64::INFINITY) {
            Ok(()) => return Err("expected inf rejection".into()),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "Invalid httpIdleTimeoutMs setting: Infinity"
        );
        Ok(())
    }

    #[test]
    fn http_timeout_parsing_and_exact_errors() -> TestResult {
        let (_root, agent, project) = make_dirs("timeout-parse")?;
        write_settings_file(
            &agent.join("settings.json"),
            r#"{
  "httpIdleTimeoutMs": "disabled",
  "websocketConnectTimeoutMs": "5000"
}"#,
        )?;
        let manager = create_manager(&project, &agent, true);
        assert_eq!(
            manager
                .get_http_idle_timeout_ms()
                .map_err(|e| e.to_string())?,
            0
        );
        assert_eq!(
            manager
                .get_web_socket_connect_timeout_ms()
                .map_err(|e| e.to_string())?,
            Some(5000)
        );

        write_settings_file(
            &agent.join("settings.json"),
            r#"{ "websocketConnectTimeoutMs": "abc" }"#,
        )?;
        let manager = create_manager(&project, &agent, true);
        let Err(error) = manager.get_web_socket_connect_timeout_ms() else {
            return Err("expected invalid stored timeout".into());
        };
        assert_eq!(
            error.to_string(),
            "Invalid websocketConnectTimeoutMs setting: abc"
        );
        Ok(())
    }

    #[test]
    fn migrations_exact_order_and_conditions() -> TestResult {
        let (_root, agent, project) = make_dirs("migrations")?;
        write_settings_file(
            &agent.join("settings.json"),
            r#"{
  "queueMode": "all",
  "websockets": false,
  "skills": {
    "enableSkillCommands": false,
    "customDirectories": ["/x"]
  },
  "retry": {
    "maxDelayMs": 7000
  }
}"#,
        )?;
        let mut manager = create_manager(&project, &agent, true);
        assert_eq!(manager.get_steering_mode(), QueueMode::All);
        assert_eq!(manager.get_transport(), Transport::Sse);
        assert_eq!(manager.get_skill_paths(), vec!["/x".to_owned()]);
        assert!(!manager.get_enable_skill_commands());
        assert_eq!(
            manager.get_provider_retry_settings().max_retry_delay_ms,
            7000
        );
        manager.set_quiet_startup(true);
        let value = parse_file(&agent.join("settings.json"))?;
        assert_eq!(value["steeringMode"], "all");
        assert!(value.get("queueMode").is_none());
        assert_eq!(value["transport"], "sse");
        assert!(value.get("websockets").is_none());
        assert_eq!(
            value["skills"],
            Value::Array(vec![Value::String("/x".into())])
        );
        assert_eq!(value["enableSkillCommands"], false);
        assert_eq!(value["retry"]["provider"]["maxRetryDelayMs"], 7000);
        assert!(value["retry"].get("maxDelayMs").is_none());
        assert_eq!(value["quietStartup"], true);

        // Both present: queueMode kept; websockets kept.
        write_settings_file(
            &agent.join("settings.json"),
            r#"{
  "queueMode": "all",
  "steeringMode": "one-at-a-time",
  "websockets": true,
  "transport": "sse",
  "skills": { "customDirectories": [] },
  "enableSkillCommands": true,
  "retry": { "maxDelayMs": 5000, "provider": { "maxRetryDelayMs": 1000 } }
}"#,
        )?;
        let mut manager = create_manager(&project, &agent, true);
        assert_eq!(manager.get_steering_mode(), QueueMode::OneAtATime);
        assert_eq!(manager.get_transport(), Transport::Sse);
        assert!(manager.get_skill_paths().is_empty());
        assert!(manager.get_enable_skill_commands());
        assert_eq!(
            manager.get_provider_retry_settings().max_retry_delay_ms,
            1000
        );
        manager.set_theme("dark");
        let value = parse_file(&agent.join("settings.json"))?;
        assert_eq!(value["queueMode"], "all");
        assert_eq!(value["websockets"], true);
        assert!(value.get("skills").is_none());
        assert!(value["retry"].get("maxDelayMs").is_none());
        assert_eq!(value["retry"]["provider"]["maxRetryDelayMs"], 1000);

        // null maxRetryDelayMs migrates; empty customDirectories deletes skills.
        write_settings_file(
            &agent.join("settings.json"),
            r#"{
  "retry": { "maxDelayMs": 5000, "provider": { "maxRetryDelayMs": null } }
}"#,
        )?;
        let manager = create_manager(&project, &agent, true);
        assert_eq!(
            manager.get_provider_retry_settings().max_retry_delay_ms,
            5000
        );
        Ok(())
    }

    #[test]
    fn unknown_keys_roundtrip_top_and_nested() -> TestResult {
        let (_root, agent, project) = make_dirs("unknown")?;
        write_settings_file(
            &agent.join("settings.json"),
            r#"{
  "futureThing": { "a": 1 },
  "compaction": { "enabled": false, "futureNested": [1, 2] },
  "theme": "dark"
}"#,
        )?;
        let mut manager = create_manager(&project, &agent, true);
        let view = manager.get_global_settings();
        assert!(view.extra.contains_key("futureThing"));
        assert_eq!(
            view.compaction
                .as_ref()
                .and_then(|c| c.extra.get("futureNested")),
            Some(&Value::Array(vec![Value::from(1), Value::from(2)]))
        );
        manager.set_quiet_startup(true);
        let value = parse_file(&agent.join("settings.json"))?;
        assert_eq!(value["futureThing"]["a"], 1);
        assert_eq!(value["compaction"]["enabled"], false);
        assert_eq!(
            value["compaction"]["futureNested"],
            Value::Array(vec![Value::from(1), Value::from(2)])
        );
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["quietStartup"], true);
        Ok(())
    }

    #[test]
    fn arrays_replace_and_nested_objects_merge_one_level() -> TestResult {
        let (_root, agent, project) = make_dirs("arrays")?;
        write_settings_file(
            &agent.join("settings.json"),
            r#"{
  "extensions": ["g1", "g2"],
  "retry": { "enabled": false, "provider": { "timeoutMs": 5 } },
  "terminal": { "showImages": false, "imageWidthCells": 80 }
}"#,
        )?;
        write_settings_file(
            &project.join(".pi").join("settings.json"),
            r#"{
  "extensions": ["p1"],
  "retry": { "provider": { "maxRetries": 2 } },
  "terminal": { "showImages": true }
}"#,
        )?;
        let manager = create_manager(&project, &agent, true);
        assert_eq!(manager.get_extension_paths(), vec!["p1".to_owned()]);
        assert!(!manager.get_retry_enabled());
        let provider = manager.get_provider_retry_settings();
        assert_eq!(provider.max_retries, Some(2));
        assert_eq!(provider.timeout_ms, None);
        assert!(manager.get_show_images());
        assert_eq!(manager.get_image_width_cells(), 80);
        Ok(())
    }

    #[test]
    fn parse_error_refuses_save_without_clobber() -> TestResult {
        let (_root, agent, project) = make_dirs("parse-error")?;
        let bad = "{ not json";
        write_settings_file(&agent.join("settings.json"), bad)?;
        let mut manager = create_manager(&project, &agent, true);
        let errors = manager.drain_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].scope, SettingsScope::Global);
        manager.set_theme("dark");
        // In-memory view updates, but file is not clobbered.
        assert_eq!(manager.get_theme().as_deref(), Some("dark"));
        assert_eq!(read_text(&agent.join("settings.json"))?, bad);

        // Recovery: rewrite valid file and reload.
        write_settings_file(&agent.join("settings.json"), r#"{ "theme": "light" }"#)?;
        manager.reload();
        assert!(manager.drain_errors().is_empty());
        assert_eq!(manager.get_theme().as_deref(), Some("light"));
        manager.set_quiet_startup(true);
        let value = parse_file(&agent.join("settings.json"))?;
        assert_eq!(value["theme"], "light");
        assert_eq!(value["quietStartup"], true);
        Ok(())
    }

    #[test]
    fn concurrent_external_changes_are_merged() -> TestResult {
        let (_root, agent, project) = make_dirs("external")?;
        write_settings_file(
            &agent.join("settings.json"),
            r#"{
  "theme": "dark",
  "customUnknown": 1,
  "compaction": {
    "enabled": false,
    "reserveTokens": 999,
    "futureNested": "keep"
  }
}"#,
        )?;
        let mut manager = create_manager(&project, &agent, true);

        // External process rewrite after load.
        write_settings_file(
            &agent.join("settings.json"),
            r#"{
  "theme": "light",
  "customUnknown": 1,
  "externalNew": true,
  "compaction": {
    "enabled": false,
    "reserveTokens": 12345,
    "futureNested": "keep"
  }
}"#,
        )?;
        manager.set_default_model("gpt-5");
        manager.set_compaction_enabled(true);
        let value = parse_file(&agent.join("settings.json"))?;
        assert_eq!(value["theme"], "light");
        assert_eq!(value["externalNew"], true);
        assert_eq!(value["customUnknown"], 1);
        assert_eq!(value["defaultModel"], "gpt-5");
        assert_eq!(value["compaction"]["enabled"], true);
        assert_eq!(value["compaction"]["reserveTokens"], 12345);
        assert_eq!(value["compaction"]["futureNested"], "keep");
        // In-memory snapshot still has the pre-external reserveTokens.
        assert_eq!(manager.get_compaction_reserve_tokens(), 999);
        Ok(())
    }

    #[test]
    fn project_write_when_untrusted_fails_exact() -> TestResult {
        let (_root, agent, project) = make_dirs("trust")?;
        write_settings_file(
            &project.join(".pi").join("settings.json"),
            r#"{ "extensions": ["/secret"] }"#,
        )?;
        let mut manager = create_manager(&project, &agent, false);
        assert!(!manager.is_project_trusted());
        assert!(manager.get_extension_paths().is_empty());
        assert!(manager.drain_errors().is_empty());
        let error = match manager.set_project_skill_paths(vec!["s".into()]) {
            Ok(()) => return Err("expected trust error".into()),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "Project is not trusted; refusing to write project settings"
        );
        assert!(
            !project.join(".pi").join("settings.json").exists()
                || read_text(&project.join(".pi").join("settings.json"))?
                    == r#"{ "extensions": ["/secret"] }"#
        );

        manager.set_project_trusted(true);
        assert_eq!(manager.get_extension_paths(), vec!["/secret".to_owned()]);
        manager.set_project_trusted(false);
        assert!(manager.get_extension_paths().is_empty());
        Ok(())
    }

    #[test]
    fn project_resource_setters_write_project_file_only() -> TestResult {
        let (_root, agent, project) = make_dirs("project-set")?;
        write_settings_file(
            &project.join(".pi").join("settings.json"),
            r#"{ "futureKey": 7 }"#,
        )?;
        let mut manager = create_manager(&project, &agent, true);
        manager
            .set_project_extension_paths(vec!["./ext".into()])
            .map_err(|e| e.to_string())?;
        manager
            .set_project_skill_paths(vec!["./skill".into()])
            .map_err(|e| e.to_string())?;
        manager
            .set_project_prompt_template_paths(vec!["./prompt".into()])
            .map_err(|e| e.to_string())?;
        manager
            .set_project_theme_paths(vec!["./theme".into()])
            .map_err(|e| e.to_string())?;
        manager
            .set_project_packages(&[PackageSource::Source("npm:pkg".into())])
            .map_err(|e| e.to_string())?;

        assert!(!agent.join("settings.json").exists());
        let value = parse_file(&project.join(".pi").join("settings.json"))?;
        assert_eq!(value["futureKey"], 7);
        assert_eq!(
            value["extensions"],
            Value::Array(vec![Value::String("./ext".into())])
        );
        assert_eq!(
            value["skills"],
            Value::Array(vec![Value::String("./skill".into())])
        );
        assert_eq!(
            value["prompts"],
            Value::Array(vec![Value::String("./prompt".into())])
        );
        assert_eq!(
            value["themes"],
            Value::Array(vec![Value::String("./theme".into())])
        );
        assert_eq!(
            value["packages"],
            Value::Array(vec![Value::String("npm:pkg".into())])
        );
        assert_eq!(manager.get_extension_paths(), vec!["./ext".to_owned()]);
        assert_eq!(manager.get_skill_paths(), vec!["./skill".to_owned()]);
        assert_eq!(
            manager.get_prompt_template_paths(),
            vec!["./prompt".to_owned()]
        );
        assert_eq!(manager.get_theme_paths(), vec!["./theme".to_owned()]);
        assert_eq!(
            manager.get_packages(),
            vec![PackageSource::Source("npm:pkg".into())]
        );

        // Project load error: silent refuse.
        write_settings_file(&project.join(".pi").join("settings.json"), "{ bad")?;
        let mut manager = create_manager(&project, &agent, true);
        let errors = manager.drain_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].scope, SettingsScope::Project);
        manager
            .set_project_extension_paths(vec!["x".into()])
            .map_err(|e| e.to_string())?;
        assert_eq!(
            read_text(&project.join(".pi").join("settings.json"))?,
            "{ bad"
        );
        Ok(())
    }

    #[test]
    fn no_trailing_newline_and_pretty_format() -> TestResult {
        let (_root, agent, project) = make_dirs("newline")?;
        let mut manager = create_manager(&project, &agent, true);
        manager.set_theme("dark");
        manager.set_quiet_startup(true);
        let text = read_text(&agent.join("settings.json"))?;
        assert!(
            !text.ends_with('\n'),
            "must not force trailing newline: {text:?}"
        );
        let expected = serde_json::to_string_pretty(&serde_json::json!({
            "quietStartup": true,
            "theme": "dark"
        }))
        .map_err(|e| e.to_string())?;
        assert_eq!(text, expected);
        Ok(())
    }

    #[test]
    fn theme_slash_pair_preserved() {
        let mut manager = SettingsManager::in_memory(
            &Settings::default(),
            SettingsManagerCreateOptions::default(),
        );
        manager.set_theme("foo/bar");
        assert_eq!(manager.get_theme_setting().as_deref(), Some("foo/bar"));
        assert_eq!(manager.get_theme().as_deref(), Some("foo/bar"));
        manager.set_theme("plain");
        assert_eq!(manager.get_theme().as_deref(), Some("plain"));
    }

    #[test]
    fn theme_mode_roundtrip() {
        let mut manager = SettingsManager::in_memory(
            &Settings::default(),
            SettingsManagerCreateOptions::default(),
        );
        assert_eq!(manager.get_theme_mode(), ThemeMode::Auto);
        manager.set_theme_mode(ThemeMode::Light);
        assert_eq!(manager.get_theme_mode(), ThemeMode::Light);
        let value = manager.get_global_settings().to_map();
        assert_eq!(value["themeMode"], "light");

        manager.set_theme_mode(ThemeMode::Dark);
        assert_eq!(manager.get_theme_mode(), ThemeMode::Dark);
        manager.set_theme_mode(ThemeMode::Auto);
        assert_eq!(manager.get_theme_mode(), ThemeMode::Auto);
    }

    #[test]
    fn theme_mode_fallback_matrix() {
        let cases: &[(Option<&str>, Option<&str>, ThemeMode)] = &[
            (None, None, ThemeMode::Auto),
            (None, Some("purple"), ThemeMode::Auto),
            (Some("a/b"), None, ThemeMode::Auto),
            (Some("m3-light"), None, ThemeMode::Light),
            (Some("light"), None, ThemeMode::Light),
            (Some("m3-dark"), None, ThemeMode::Dark),
            (Some("dark"), None, ThemeMode::Dark),
            (Some("mytheme"), None, ThemeMode::Dark),
            (Some("mytheme"), Some("purple"), ThemeMode::Dark),
        ];

        for (theme, mode_str, expected) in cases {
            let mut map = Map::new();
            if let Some(t) = theme {
                map.insert("theme".into(), Value::String(t.to_string()));
            }
            if let Some(m) = mode_str {
                map.insert("themeMode".into(), Value::String(m.to_string()));
            }
            let manager = SettingsManager::in_memory(
                &Settings::from_map(&map),
                SettingsManagerCreateOptions::default(),
            );
            assert_eq!(
                manager.get_theme_mode(),
                *expected,
                "theme={theme:?}, mode={mode_str:?}"
            );
        }
    }

    #[test]
    fn analytics_tracking_id_lifecycle() -> TestResult {
        let mut manager = SettingsManager::in_memory(
            &Settings::default(),
            SettingsManagerCreateOptions::default(),
        );
        assert!(manager.get_tracking_id().is_none());
        manager.set_enable_analytics(true);
        let first = manager
            .get_tracking_id()
            .ok_or_else(|| "expected generated tracking id".to_owned())?;
        assert!(Uuid::parse_str(&first).is_ok());
        manager.set_enable_analytics(false);
        manager.set_enable_analytics(true);
        assert_eq!(manager.get_tracking_id().as_deref(), Some(first.as_str()));

        let seed = Settings {
            tracking_id: Some("fixed-id".into()),
            enable_analytics: Some(false),
            ..Settings::default()
        };
        let mut manager =
            SettingsManager::in_memory(&seed, SettingsManagerCreateOptions::default());
        manager.set_enable_analytics(true);
        assert_eq!(manager.get_tracking_id().as_deref(), Some("fixed-id"));
        Ok(())
    }

    #[test]
    fn default_project_trust_is_global_only() -> TestResult {
        let (_root, agent, project) = make_dirs("dpt")?;
        write_settings_file(
            &project.join(".pi").join("settings.json"),
            r#"{ "defaultProjectTrust": "always" }"#,
        )?;
        let manager = create_manager(&project, &agent, true);
        assert_eq!(
            manager.get_default_project_trust(),
            DefaultProjectTrust::Ask
        );

        write_settings_file(
            &agent.join("settings.json"),
            r#"{ "defaultProjectTrust": "always" }"#,
        )?;
        let manager = create_manager(&project, &agent, true);
        assert_eq!(
            manager.get_default_project_trust(),
            DefaultProjectTrust::Always
        );

        write_settings_file(
            &agent.join("settings.json"),
            r#"{ "defaultProjectTrust": "bogus" }"#,
        )?;
        let manager = create_manager(&project, &agent, true);
        assert_eq!(
            manager.get_default_project_trust(),
            DefaultProjectTrust::Ask
        );
        Ok(())
    }

    #[test]
    fn session_dir_normalization() -> TestResult {
        let (_root, agent, project) = make_dirs("session-dir")?;
        write_settings_file(
            &agent.join("settings.json"),
            r#"{ "sessionDir": "/abs/path" }"#,
        )?;
        let manager = create_manager(&project, &agent, true);
        assert_eq!(manager.get_session_dir().as_deref(), Some("/abs/path"));
        write_settings_file(&agent.join("settings.json"), r#"{ "sessionDir": "" }"#)?;
        let manager = create_manager(&project, &agent, true);
        assert_eq!(manager.get_session_dir().as_deref(), Some(""));
        Ok(())
    }

    #[test]
    fn apply_overrides_remerged_on_save() {
        let mut manager = SettingsManager::in_memory(
            &Settings::default(),
            SettingsManagerCreateOptions::default(),
        );
        let mut overrides = Map::new();
        overrides.insert("theme".into(), Value::String("override".into()));
        manager.apply_overrides(&overrides);
        assert_eq!(manager.get_theme().as_deref(), Some("override"));
        manager.set_quiet_startup(true);
        // save re-merges global+project, discarding overrides.
        assert!(manager.get_theme().is_none());
        assert!(manager.get_quiet_startup());
    }

    #[test]
    fn reload_picks_up_external_changes() -> TestResult {
        let (_root, agent, project) = make_dirs("reload")?;
        write_settings_file(&agent.join("settings.json"), r#"{ "theme": "dark" }"#)?;
        let mut manager = create_manager(&project, &agent, true);
        assert_eq!(manager.get_theme().as_deref(), Some("dark"));
        write_settings_file(&agent.join("settings.json"), r#"{ "theme": "light" }"#)?;
        assert_eq!(manager.get_theme().as_deref(), Some("dark"));
        manager.reload();
        assert_eq!(manager.get_theme().as_deref(), Some("light"));
        Ok(())
    }

    #[test]
    fn in_memory_storage_roundtrip() -> TestResult {
        let mut storage = InMemorySettingsStorage::new();
        storage.with_lock(SettingsScope::Global, &mut |_| {
            Ok(Some(r#"{"theme":"dark"}"#.to_owned()))
        })?;
        let mut manager = SettingsManager::from_storage(
            Box::new(storage),
            SettingsManagerCreateOptions::default(),
        );
        assert_eq!(manager.get_theme().as_deref(), Some("dark"));
        manager.set_theme("light");
        assert_eq!(
            manager.get_global_settings().theme.as_deref(),
            Some("light")
        );
        Ok(())
    }

    #[test]
    fn missing_file_load_creates_no_lock_artifact() -> TestResult {
        let (root, agent, project) = make_dirs("lock-artifact")?;
        let manager = create_manager(&project, &agent, true);
        assert!(!agent.join("settings.json.lock").exists());
        drop(manager);
        assert!(!agent.join("settings.json").exists());
        assert!(!agent.join("settings.json.lock").exists());

        // Write creates parent dirs for a nested agent path.
        let nested_agent = root.join("nested").join("agent");
        let mut manager = create_manager(&project, &nested_agent, true);
        manager.set_theme("dark");
        assert!(nested_agent.join("settings.json").exists());
        assert!(!nested_agent.join("settings.json.lock").exists());
        Ok(())
    }

    #[test]
    fn output_pad_strict_zero() -> TestResult {
        let (_root, agent, project) = make_dirs("output-pad")?;
        write_settings_file(&agent.join("settings.json"), r#"{ "outputPad": 0 }"#)?;
        let manager = create_manager(&project, &agent, true);
        assert_eq!(manager.get_output_pad(), OutputPad::Zero);
        write_settings_file(&agent.join("settings.json"), r#"{ "outputPad": 2 }"#)?;
        let manager = create_manager(&project, &agent, true);
        assert_eq!(manager.get_output_pad(), OutputPad::One);
        write_settings_file(&agent.join("settings.json"), r#"{ "outputPad": "0" }"#)?;
        let manager = create_manager(&project, &agent, true);
        assert_eq!(manager.get_output_pad(), OutputPad::One);
        Ok(())
    }

    #[test]
    fn package_sources_typed_roundtrip() {
        let mut manager = SettingsManager::in_memory(
            &Settings::default(),
            SettingsManagerCreateOptions::default(),
        );
        let packages = vec![
            PackageSource::Source("npm:a".into()),
            PackageSource::Filtered(PackageSourceFilter {
                source: "git:b".into(),
                autoload: Some(false),
                extensions: Some(vec!["e".into()]),
                skills: None,
                prompts: None,
                themes: None,
                extra: {
                    let mut extra = Map::new();
                    extra.insert("future".into(), Value::from(1));
                    extra
                },
            }),
        ];
        manager.set_packages(&packages);
        assert_eq!(manager.get_packages(), packages);
        let view = manager.get_global_settings();
        assert_eq!(view.packages, Some(packages));
    }

    #[test]
    fn set_default_model_and_provider_marks_both() -> TestResult {
        let (_root, agent, project) = make_dirs("both")?;
        let mut manager = create_manager(&project, &agent, true);
        manager.set_default_model_and_provider("openai", "gpt-5");
        let value = parse_file(&agent.join("settings.json"))?;
        assert_eq!(value["defaultProvider"], "openai");
        assert_eq!(value["defaultModel"], "gpt-5");
        Ok(())
    }

    #[test]
    fn typed_settings_from_map_to_map_preserves_extra() {
        let mut map = Map::new();
        map.insert("theme".into(), Value::String("dark".into()));
        map.insert("future".into(), Value::from(42));
        let settings = Settings::from_map(&map);
        assert_eq!(settings.theme.as_deref(), Some("dark"));
        assert_eq!(settings.extra.get("future"), Some(&Value::from(42)));
        let roundtrip = settings.to_map();
        assert_eq!(roundtrip.get("theme"), Some(&Value::String("dark".into())));
        assert_eq!(roundtrip.get("future"), Some(&Value::from(42)));
    }
}
