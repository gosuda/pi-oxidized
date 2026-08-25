//! Live interactive runtime: owns the [`Tui`] writer, the [`TerminalInput`]
//! reader, a [`SessionHost`], and the [`ViewState`] projection.
//!
//! This module is the **only** stdout owner for the interactive mode and the
//! only place that translates [`ViewAction`]s into session calls. Everything
//! outside this file is pure (stateless compose, pure input mapping, view
//! data). The runtime:
//!
//! 1. Spawns one event-pump task that converts the [`SessionHost`]'s callback
//!    subscription into a bounded [`mpsc`] of [`AgentSessionEvent`]s.
//! 2. Runs the main `tokio::select!` loop over: UI events (keys / paste /
//!    resize / focus), session events, partial-message watch ticks, the
//!    background coalescer deadline, and shutdown signals.
//! 3. Routes every UI event first to the live [`Editor`] component, then to
//!    [`InputMapper`] for app-level dispatch, then forwards the resulting
//!    [`ViewAction`] queue to `dispatch_action`.
//! 4. Projects each [`AgentSessionEvent`] into [`ViewState`] mutations and
//!    schedules a coalesced background paint (≤ 16 ms window). Input-driven
//!    paints bypass the coalescer and commit on the same loop turn.
//! 5. On `Resize`: coalesces to one [`Txn::Reanchor`] without clearing.
//!    On `settle`: emits [`Txn::Settle`] containing the scrollback block and
//!    the inline redraw in one stage-3 write.
//! 6. On `Suspend` / `Exit` / fatal I/O failure: restores terminal modes via
//!    the [`TerminalGuard`] (owned by the caller) and returns.
//!
//! The runtime is generic over the writer `W` (so tests can inject a
//! [`std::io::Cursor`]`<`[`Vec`]`<u8>>` or
//! [`TransactionRecorder`](pi_tui::terminal::TransactionRecorder)) and the
//! session host `S` (so tests inject a [`FakeSessionHost`]). Production wires
//! `W = io::Stdout` and `S = AgentSessionHost` (a future thin wrapper around
//! `Arc<AgentSession>`).
//!
//! # No stdout clone, no second stdin owner, no clears
//!
//! The runtime owns exactly one [`Tui<W>`], which owns the sole stdout handle.
//! [`TerminalInput`] owns the sole [`crossterm::event::EventStream`]. All
//! terminal mutations go through [`Tui::commit`], whose stage-3 audit rejects
//! any banned clear sequence (`CSI 2J` / `CSI 3J`).

use std::fmt::Debug;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;
use std::time::{Duration, Instant};

use futures::future::{BoxFuture, poll_fn};
use pi_ai::AssistantMessage;
use pi_tui::component::{Component, EventResult, UiEvent};
use pi_tui::components::editor::{Editor, EditorOptions};
use pi_tui::keys::{
    ParsedKeyId, encode_key_event, key_matches_parsed, parse_key_id, set_kitty_protocol_active,
};
use pi_tui::terminal::caps::TerminalCapabilities;
use pi_tui::terminal::input::TerminalInput;
use pi_tui::terminal::probe::{TerminalTheme, detect_terminal_theme, probe_terminal};
use pi_tui::terminal::writer::{ReanchorCause, SettledBlock, Tui, Txn};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::core::agent_session::events::AgentSessionEvent;
use crate::core::agent_session::extension_runner::ExtensionRunner;
use crate::core::agent_session::prompt::{PromptOptions, StreamingBehavior};
use crate::core::agent_session_runtime::{ForkOutcome, SwitchOutcome};
use crate::core::extension_host::ExtensionUiEvent;
use crate::core::extension_runtime_set::ExtensionRuntimeSet;
use crate::core::platform::external_editor::{EditOutcome, edit_text_in_external_editor};
use pi_ext::client::{HostUiRequest, HostUiResponse};
use pi_ext::protocol::{
    KeyEventKindWire, KeyModifiersWire, NotifyLevel, SlotPlacement, ThemeCatalogEntry,
    ThemeColorValue, ThemeSet, ThemeUpdate, ThemeWire, UiControl, UiEventRequest, UiEventWire,
    UiStateWire,
};
use pi_ext::sanitize::SanitizedSlot;

use crate::core::settings::ThemeMode;

use super::input::{DoubleEscapeAction, InputMapper, InputState};
use super::messages::{AssistantMessageView, MessageView};
#[cfg(test)]
use super::state;
use super::state::{
    BillingMode, DiagnosticSeverity, EditorBorder, FocusArea, Overlay, OverlayKind, PendingKind,
    PendingMessage, SessionStatus, StartupDiagnostic, StatusKind, ViewAction, ViewState,
    WidgetSlot,
};
use super::theme::ResolvedTheme;
use super::view::{ComposedSection, compose};

/// Maximum time the runtime will wait for one [`Tui::commit`] before declaring
/// a draw deadlock (cursor-query trap, runaway probe, etc.).
///
/// Mirrors the 5 s hard per-draw timeout of master-plan check 6. The check
/// itself is enforced by the PTY test harness (the synchronous `Tui::commit`
/// cannot be interrupted mid-call), but the runtime surfaces the constant so
/// callers can wire their own alarm.
pub const DRAW_TIMEOUT: Duration = Duration::from_secs(5);

/// Background coalescing window for streaming / tool / plugin updates.
pub const BACKGROUND_COALESCE_WINDOW: Duration = Duration::from_millis(16);

/// Spinner tick cadence; matches `DEFAULT_INTERVAL_MS` (`loader.rs`), the
/// interval the braille `Loader` frames were designed for.
const SPINNER_TICK: Duration = Duration::from_millis(80);

/// Bound on the runtime's incoming event channel. Matches the agent crate's
/// extension-queue capacity so a lagging consumer surfaces backpressure early.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Maximum UTF-8 bytes of sanitized terminal title payload (OSC 0).
pub(crate) const MAX_TERMINAL_TITLE_BYTES: usize = 256;

/// OSC 0 set-icon-name-and-window-title introducer (`ESC ] 0 ;`).
const OSC0_SET_TITLE_PREFIX: &[u8] = b"\x1b]0;";

/// BEL terminates an OSC sequence.
const OSC_BEL: u8 = 0x07;

/// Sanitize extension-supplied terminal title text for OSC 0 emission.
///
/// Drops every [`char::is_control`] scalar and stops before the sanitized
/// payload would exceed [`MAX_TERMINAL_TITLE_BYTES`] UTF-8 bytes, never
/// splitting a scalar.
#[must_use]
pub(crate) fn sanitize_terminal_title(title: &str) -> String {
    let mut out = String::new();
    let mut byte_len = 0usize;
    for ch in title.chars() {
        if ch.is_control() {
            continue;
        }
        let ch_len = ch.len_utf8();
        if byte_len + ch_len > MAX_TERMINAL_TITLE_BYTES {
            break;
        }
        out.push(ch);
        byte_len += ch_len;
    }
    out
}

/// Encode a safe OSC 0 set-title sequence for `title`.
///
/// Only the sanitized payload is written between the fixed introducer and
/// BEL terminator; hostile control/C1 bytes cannot break the sink.
#[must_use]
pub(crate) fn encode_osc0_set_title(title: &str) -> Vec<u8> {
    let sanitized = sanitize_terminal_title(title);
    let mut sequence = Vec::with_capacity(OSC0_SET_TITLE_PREFIX.len() + sanitized.len() + 1);
    sequence.extend_from_slice(OSC0_SET_TITLE_PREFIX);
    sequence.extend_from_slice(sanitized.as_bytes());
    sequence.push(OSC_BEL);
    sequence
}

// ---------------------------------------------------------------------------
// SessionHost trait
// ---------------------------------------------------------------------------

/// Mutually exclusive foreground activity reported by a session snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionActivity {
    /// No foreground session activity.
    #[default]
    Idle,
    /// The agent is currently streaming a response.
    Streaming,
    /// Context compaction is running.
    Compacting,
    /// A retry backoff is in progress.
    Retrying,
    /// Branch summarization is in progress.
    Summarizing,
}

/// Snapshot of session state used to project [`ViewState`].
///
/// Production builds a real snapshot from `AgentSession` accessors; tests
/// return whatever they like.
#[derive(Clone, Debug, Default)]
pub struct SessionSnapshot {
    /// Current mutually exclusive foreground activity.
    pub activity: SessionActivity,
    /// Whether bash execution is running.
    pub bash_running: bool,
    /// Active thinking level label (for footer + editor border).
    pub thinking_level_label: String,
    /// Active model id (footer).
    pub model_id: String,
    /// Whether the active model supports reasoning.
    pub reasoning: bool,
    /// Pending steering messages (mirror).
    pub steering: Vec<String>,
    /// Pending follow-up messages (mirror).
    pub follow_up: Vec<String>,
    /// Queue delivery mode for follow-up messages.
    pub follow_up_mode: super::state::QueueMode,
}

/// Session-derived footer values that require async access to persisted history.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionFooterSnapshot {
    /// Cumulative input tokens across the persisted session.
    pub total_input: u64,
    /// Cumulative output tokens across the persisted session.
    pub total_output: u64,
    /// Cumulative cache-read tokens.
    pub total_cache_read: u64,
    /// Cumulative cache-write tokens.
    pub total_cache_write: u64,
    /// Cumulative cost in USD.
    pub total_cost: f64,
    /// Context-window size in tokens.
    pub context_window: u64,
    /// Context usage percent when known.
    pub context_percent: Option<f64>,
    /// Active model provider.
    pub provider: Option<String>,
    /// Number of providers in the active model catalog.
    pub provider_count: usize,
    /// Active thinking level.
    pub thinking_level: pi_ai::ModelThinkingLevel,
    /// Whether bash execution is running.
    pub bash_running: bool,
    /// Whether billing is covered by an OAuth subscription.
    pub subscription: bool,
    /// Whether automatic compaction is enabled.
    pub auto_compact: bool,
}

impl Default for SessionFooterSnapshot {
    fn default() -> Self {
        Self {
            total_input: 0,
            total_output: 0,
            total_cache_read: 0,
            total_cache_write: 0,
            total_cost: 0.0,
            context_window: 0,
            context_percent: None,
            provider: None,
            provider_count: 0,
            thinking_level: pi_ai::ModelThinkingLevel::Off,
            bash_running: false,
            subscription: false,
            auto_compact: true,
        }
    }
}

impl SessionSnapshot {
    /// Whether the agent is currently streaming a response.
    #[must_use]
    pub fn is_streaming(&self) -> bool {
        self.activity == SessionActivity::Streaming
    }

    /// Whether context compaction is currently running.
    #[must_use]
    pub fn is_compacting(&self) -> bool {
        self.activity == SessionActivity::Compacting
    }
}

/// Outcome of a `/clone` request (ports the branches of upstream
/// `handleCloneCommand`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloneOutcome {
    /// The session was cloned into a new session.
    Cloned,
    /// No current leaf entry — there is nothing to clone yet.
    NothingToClone,
    /// A before-switch hook cancelled the clone.
    Cancelled,
}

/// Typed `/import` failure so the runtime can mirror upstream's per-error
/// handling (`MissingSessionCwdError` prompt, file-not-found notice, fatal).
#[derive(Clone, Debug)]
pub enum ImportError {
    /// The imported session's recorded cwd no longer exists; carries the
    /// fallback cwd the runtime would continue in (upstream `issue.fallbackCwd`).
    MissingCwd {
        /// Fallback (runtime) cwd offered for the retry.
        fallback_cwd: String,
    },
    /// The import source file was not found.
    FileNotFound(String),
    /// Any other import failure.
    Other(String),
}

impl ImportError {
    /// Human-readable message for the file-not-found / fatal notice.
    fn message(&self) -> String {
        match self {
            Self::MissingCwd { fallback_cwd } => {
                format!(
                    "Stored session working directory does not exist (fallback: {fallback_cwd})"
                )
            }
            Self::FileNotFound(message) | Self::Other(message) => message.clone(),
        }
    }
}

/// Scoped-model selector entries and their enabled-state map.
pub type ScopedModelEntries = (
    Vec<super::state::ModelSelectorEntry>,
    std::collections::BTreeMap<String, bool>,
);

/// Asynchronous session surface consumed by the runtime.
///
/// All async methods return `BoxFuture` so the trait stays object-safe; the
/// runtime is generic over `S: SessionHost` so production wires a thin
/// `AgentSessionHost` wrapper and tests wire [`FakeSessionHost`]. Methods that
/// can fail return `Result<_, String>`; the runtime records the error onto
/// the status indicator (never panics, never aborts the loop).
///
/// # Implementation invariants
///
/// - `subscribe` MUST invoke its callback for every public session event,
///   including ones emitted during async actions performed by this trait.
/// - `partial_rx` MAY return a receiver that never fires (no streaming); the
///   runtime treats `None` updates as no-ops.
/// - The runtime NEVER holds the host across `.await` points that touch the
///   same host mutably; each action is dispatched on a fresh `&self` borrow.
pub trait SessionHost: Send + Sync + 'static {
    /// Snapshot of synchronous state for view projection.
    fn snapshot(&self) -> SessionSnapshot;

    /// Snapshot persisted token/cost/context state for the footer.
    fn footer_snapshot(&self) -> BoxFuture<'_, SessionFooterSnapshot> {
        Box::pin(std::future::ready(SessionFooterSnapshot::default()))
    }

    /// Subscribe to public session events. The returned [`EventSubscription`]
    /// owns an mpsc receiver plus the unsubscribe token.
    fn subscribe(&self) -> EventSubscription;

    /// Receiver for the latest partial assistant message (`None` when idle).
    fn partial_rx(&self) -> watch::Receiver<Option<Arc<AssistantMessage>>>;

    // ----- Async actions (object-safe via BoxFuture) -----

    /// Submit a prompt.
    fn prompt(&self, text: &str, opts: PromptOptions) -> BoxFuture<'_, Result<(), String>>;

    /// Steer the in-flight stream (mid-turn injection).
    fn steer(&self, text: &str) -> BoxFuture<'_, Result<(), String>>;

    /// Queue a follow-up message for the next turn.
    fn follow_up(&self, text: &str) -> BoxFuture<'_, Result<(), String>>;

    /// Abort the active run, retry, compaction, bash, or branch summary.
    ///
    /// The returned future owns the concrete session selected at method-call
    /// time. Interactive prompt operations retain it so a later session
    /// replacement cannot redirect cleanup to the replacement session.
    fn abort(&self) -> BoxFuture<'static, Result<(), String>>;

    /// Manually compact the context with optional custom instructions.
    fn compact(&self, instructions: Option<&str>) -> BoxFuture<'_, Result<(), String>>;

    /// Cycle the thinking level forward.
    fn cycle_thinking_level(&self) -> BoxFuture<'_, Result<(), String>>;

    /// Cycle the active model in the given direction.
    fn cycle_model(&self, forward: bool) -> BoxFuture<'_, Result<(), String>>;

    /// Reload extensions / resources / keybindings.
    fn reload(&self) -> BoxFuture<'_, Result<Vec<String>, String>>;

    /// Returns the full transcript for the current session (used on rebind).
    fn messages(&self) -> Vec<pi_agent::AgentMessage>;

    /// Concrete extension host for interactive UI bridging, when enabled.
    fn host_extension_runner(&self) -> Option<Arc<ExtensionRuntimeSet>> {
        None
    }

    /// Initial persisted thinking-block visibility.
    fn hide_thinking_block(&self) -> bool {
        false
    }

    /// Persist thinking-block visibility.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when persisting the preference fails.
    fn set_hide_thinking_block(&self, _hide: bool) -> Result<(), String> {
        Ok(())
    }
    /// Current theme settings: raw `theme` string (may be a `light/dark`
    /// pair) and the `themeMode` polarity.
    fn theme_settings(&self) -> (Option<String>, ThemeMode) {
        (None, ThemeMode::Auto)
    }

    /// Persist the `theme` setting (raw string) and `themeMode`.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when persisting fails.
    fn persist_theme(&self, _theme: &str, _mode: ThemeMode) -> Result<(), String> {
        Ok(())
    }

    /// Apply one settings-row change from the settings/config selector.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when the id is unknown or persisting
    /// fails.
    fn apply_settings_change(&self, _id: &str, _value: &str) -> Result<(), String> {
        Ok(())
    }

    /// Persist a completed first-run wizard selection.
    ///
    /// # Errors
    ///
    /// Returns a human-readable message when persisting fails.
    fn persist_first_run(
        &self,
        _selection: &crate::core::platform::first_run::FirstRunSelection,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Configured external editor command.
    fn external_editor_command(&self) -> String {
        if cfg!(windows) {
            "notepad".to_owned()
        } else {
            "nano".to_owned()
        }
    }

    /// Fetch the model list for the model selector.
    fn get_model_entries(
        &self,
    ) -> BoxFuture<'_, Result<Vec<super::state::ModelSelectorEntry>, String>>;

    /// Fetch the recent sessions for the session picker.
    fn get_session_entries(
        &self,
    ) -> BoxFuture<'_, Result<Vec<super::state::SessionPickerEntry>, String>>;

    /// Fetch the session tree (entries with depth) for the tree selector.
    fn get_tree_entries(&self) -> BoxFuture<'_, Result<Vec<super::state::TreeEntry>, String>>;

    /// Fetch the user-message fork list (tree entries, only user messages).
    fn get_fork_entries(&self) -> BoxFuture<'_, Result<Vec<super::state::TreeEntry>, String>>;

    /// Fetch the trust-state settings rows.
    fn get_trust_entries(&self) -> BoxFuture<'_, Result<Vec<super::state::SettingsRow>, String>>;

    /// Fetch the auth selector entries (provider list).
    fn get_auth_entries(
        &self,
    ) -> BoxFuture<'_, Result<Vec<super::state::AuthSelectorEntry>, String>>;

    /// Fetch the scoped-models selector entries with current enabled map.
    fn get_scoped_models_entries(&self) -> BoxFuture<'_, Result<ScopedModelEntries, String>>;

    /// Fetch the settings selector rows.
    fn get_settings_entries(&self)
    -> BoxFuture<'_, Result<Vec<super::state::SettingsRow>, String>>;

    /// Fetch the config selector rows.
    fn get_config_entries(&self) -> BoxFuture<'_, Result<Vec<super::state::SettingsRow>, String>>;

    /// Execute a bash command (the runtime passes the typed command minus the
    /// `!` / `!!` prefix).
    fn execute_bash(
        &self,
        command: &str,
        exclude_from_context: bool,
    ) -> BoxFuture<'_, Result<(), String>>;

    /// Start a new session (replacement pipeline). `Ok(Cancelled)` when a
    /// `before_switch` extension hook cancels the replacement.
    fn new_session(&self) -> BoxFuture<'_, Result<SwitchOutcome, String>>;

    /// Open the fork selector's confirmation; runtime supplies the entry id.
    /// `Ok(Cancelled)` when a `before_fork` extension hook cancels the fork.
    fn fork(&self, entry_id: &str) -> BoxFuture<'_, Result<ForkOutcome, String>>;

    /// Clone the session at the current leaf. `Ok(NothingToClone)` when there is
    /// no leaf yet; `Ok(Cancelled)` when a before-switch hook cancels the fork.
    fn clone(&self) -> BoxFuture<'_, Result<CloneOutcome, String>>;

    /// Switch to a different session file (resume). `Ok(Cancelled)` when a
    /// `before_switch` extension hook cancels the switch.
    fn switch_session(&self, path: &str) -> BoxFuture<'_, Result<SwitchOutcome, String>>;

    /// Export the current session to HTML; runtime passes an optional path.
    fn export_html(&self, path: Option<&str>) -> BoxFuture<'_, Result<String, String>>;

    /// Export the current session to JSONL; runtime passes an optional path.
    fn export_jsonl(&self, path: Option<&str>) -> BoxFuture<'_, Result<String, String>>;

    /// Import and replace the current session from a JSONL file. `cwd_override`
    /// supplies the fallback cwd for the missing-cwd retry (upstream
    /// `importFromJsonl(path, selectedCwd)`).
    fn import_jsonl(
        &self,
        path: &str,
        cwd_override: Option<&str>,
    ) -> BoxFuture<'_, Result<bool, ImportError>>;

    /// Export to a temp HTML and upload it as a secret gist; returns
    /// `(viewer_url, gist_url)`.
    fn share(&self) -> BoxFuture<'_, Result<(String, String), String>>;

    /// Aggregate session statistics for `/session`.
    fn session_stats(&self) -> BoxFuture<'_, crate::core::agent_session::stats::SessionStats>;

    /// Set the session display name and return the normalized name the session
    /// manager actually stored (upstream reads it back via `getSessionName`).
    fn set_session_name(&self, name: &str) -> BoxFuture<'_, Result<Option<String>, String>>;

    /// List stored credentials offered by the `/logout` selector.
    fn logout_provider_options(
        &self,
    ) -> BoxFuture<'_, Result<Vec<super::state::LogoutOption>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Remove the stored credential for `provider_id` (upstream
    /// `modelRuntime.logout`).
    fn logout(&self, provider_id: &str) -> BoxFuture<'_, Result<(), String>>;

    /// Copy the last assistant text (returns the text so the runtime can
    /// resolve the platform clipboard).
    fn last_assistant_text(&self) -> BoxFuture<'_, Result<Option<String>, String>>;
}

/// Subscription returned by [`SessionHost::subscribe`].
///
/// Owns the receiver side of the event channel plus the unsubscribe token.
/// Dropping this drops both — listeners are cleaned up automatically.
pub struct EventSubscription {
    /// Receiver for events pumped from the host.
    pub rx: mpsc::UnboundedReceiver<AgentSessionEvent>,
    /// Unsubscribe handle; fires on drop.
    pub unsubscribe: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Debug for EventSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSubscription").finish_non_exhaustive()
    }
}

impl Drop for EventSubscription {
    fn drop(&mut self) {
        if let Some(unsub) = self.unsubscribe.take() {
            unsub();
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime options / exit / outcome
// ---------------------------------------------------------------------------
/// Why the runtime exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InteractiveExit {
    /// User exited cleanly (Ctrl+D / double Ctrl+C / `/quit`).
    Clean,
    /// Terminal I/O failed; the process should exit nonzero.
    IoFailure,
    /// A draw timed out (cursor-query deadlock guard).
    DrawDeadlock,
    /// The session ended (host signaled shutdown).
    SessionEnded,
    /// Process suspension requested (`Ctrl+Z`). The caller (`run_interactive_mode`)
    /// drives the actual SIGTSTP via the [`pi_tui::terminal::guard::TerminalGuard`]
    /// then loops `run()` to resume.
    Suspend,
    /// Temporarily restore the terminal and run the configured external editor.
    ExternalEditor,
}

/// Outcome of dispatching one [`ViewAction`].
pub struct InteractiveRuntimeOptions {
    /// Initial resolved theme (dark / light).
    pub theme: Arc<ResolvedTheme>,
    /// Terminal capabilities (sync output, image protocol, hyperlinks, …).
    pub caps: TerminalCapabilities,
    /// Detected terminal background polarity for automatic theme selection.
    pub terminal_theme: TerminalTheme,
    /// Initial terminal size.
    pub size: (u16, u16),
    /// Initial inline viewport height.
    pub viewport_height: u16,
    /// Quiet mode suppresses the logo header.
    pub quiet: bool,
    /// Double-Esc action ("none" / "tree" / "fork").
    pub double_escape: DoubleEscapeAction,
    /// Show hardware cursor (debug / accessibility).
    pub hardware_cursor: bool,
    pending_ui_events: Vec<UiEvent>,
}

/// Outcome of dispatching one [`ViewAction`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActionOutcome {
    /// No observable effect.
    None,
    /// The view changed and a repaint is needed.
    Repaint,
    /// The runtime should exit cleanly.
    Exit,
    /// The process should suspend after restoring terminal state.
    Suspend,
    /// Pause the runtime while the outer terminal owner runs an editor child.
    ExternalEditor,
}

/// Split a `/command args` string into `(name, args)`; `None` when `text` is not
/// a slash command. `args` has leading whitespace trimmed.
fn parse_slash_command(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix('/')?;
    match rest.split_once(char::is_whitespace) {
        Some((name, args)) => Some((name, args.trim_start())),
        None => Some((rest, "")),
    }
}

/// Extract the first path argument (`getPathCommandArgument` semantics): the
/// first whitespace-delimited token, honoring a single/double-quoted span.
/// `None` when `args` is empty or holds an unterminated quote.
fn parse_path_argument(args: &str) -> Option<String> {
    let args = args.trim_start();
    let first = args.chars().next()?;
    if first == '"' || first == '\'' {
        let rest = &args[1..];
        return rest.find(first).map(|idx| rest[..idx].to_owned());
    }
    let end = args.find(char::is_whitespace).unwrap_or(args.len());
    Some(args[..end].to_owned())
}

/// Render `/session` stats as a markdown block (ports `handleSessionCommand`;
/// per-model and cache-waste breakdown omitted — see divergence ledger).
fn format_session_info(
    stats: &crate::core::agent_session::stats::SessionStats,
    name: Option<&str>,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("**Session Info**\n\n");
    if let Some(name) = name {
        let _ = writeln!(out, "Name: {name}");
    }
    let file = stats.session_file.as_deref().unwrap_or("In-memory");
    let _ = writeln!(out, "File: {file}");
    let _ = writeln!(out, "ID: {}\n", stats.session_id);
    let _ = writeln!(out, "**Messages**");
    let _ = writeln!(out, "Total: {}", stats.total_messages);
    let _ = writeln!(out, "User: {}", stats.user_messages);
    let _ = writeln!(out, "Assistant: {}", stats.assistant_messages);
    let _ = writeln!(
        out,
        "Tools: {} calls, {} results\n",
        stats.tool_calls, stats.tool_results
    );
    let tokens = &stats.tokens;
    let prompt_tokens = tokens
        .input
        .saturating_add(tokens.cache_read)
        .saturating_add(tokens.cache_write);
    let _ = writeln!(out, "**Tokens**");
    let _ = writeln!(out, "Input: {prompt_tokens}");
    let _ = writeln!(out, "Output: {}", tokens.output);
    let _ = writeln!(out, "Total: {}", tokens.total);
    if stats.cost > 0.0 {
        let _ = write!(out, "\n**Cost**\nTotal: ${:.3}", stats.cost);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionReplacement {
    New,
    Fork,
    Clone,
}

impl Default for InteractiveRuntimeOptions {
    fn default() -> Self {
        Self {
            theme: super::theme::dark(),
            caps: TerminalCapabilities::default(),
            terminal_theme: TerminalTheme::Dark,
            size: (80, 24),
            viewport_height: 24,
            quiet: false,
            double_escape: DoubleEscapeAction::None,
            hardware_cursor: false,
            pending_ui_events: Vec::new(),
        }
    }
}

impl InteractiveRuntimeOptions {
    /// Build production startup options from environment capabilities.
    ///
    /// Detection performs blocking terminal I/O (see
    /// [`TerminalCapabilities::detect`]), so async callers must offload it
    /// with `tokio::task::spawn_blocking` rather than run it on a runtime
    /// worker.
    #[must_use]
    pub fn detect() -> Self {
        let caps = TerminalCapabilities::detect();
        let colorfgbg = std::env::var("COLORFGBG").ok();
        Self {
            caps: caps.clone(),
            terminal_theme: detect_terminal_theme(caps.dark_background, colorfgbg.as_deref()),
            ..Self::default()
        }
    }
}

/// Component wrapper that splices the live editor into the composed view.
struct InteractiveRoot {
    pre_editor: Vec<ComposedSection>,
    editor: Editor,
    post_editor: Vec<ComposedSection>,
    overlay: Option<Box<dyn Component>>,
    selector: Option<Box<dyn Component>>,
    dialog_title: Option<Box<dyn Component>>,
    focus: FocusArea,
}

impl InteractiveRoot {
    #[cfg(test)]
    fn build(view: &ViewState, editor: Editor, selector: Option<Box<dyn Component>>) -> Self {
        let composed = compose(view);
        let mut sections = composed.sections;
        let editor_idx = sections
            .iter()
            .position(|section| section.label == "editor")
            .unwrap_or(sections.len().saturating_sub(1));
        let pre_editor: Vec<_> = sections.drain(0..editor_idx).collect();
        if !sections.is_empty() {
            sections.remove(0);
        }
        Self {
            pre_editor,
            editor,
            post_editor: sections,
            overlay: composed.overlay,
            selector,
            dialog_title: None,
            focus: view.focus,
        }
    }

    fn build_with_chat(
        view: &mut ViewState,
        editor: Editor,
        selector: Option<Box<dyn Component>>,
        dialog_title: Option<Box<dyn Component>>,
        prefix: Box<dyn Component>,
        tail: Box<dyn Component>,
    ) -> Self {
        let messages = std::mem::take(&mut view.messages);
        let mut composed = compose(view);
        view.messages = messages;
        if let Some(index) = composed
            .sections
            .iter()
            .position(|section| section.label == "chat")
        {
            composed.sections[index] = ComposedSection {
                label: "chat-prefix",
                component: prefix,
            };
            composed.sections.insert(
                index + 1,
                ComposedSection {
                    label: "chat-tail",
                    component: tail,
                },
            );
        }
        let mut sections = composed.sections;
        let editor_idx = sections
            .iter()
            .position(|section| section.label == "editor")
            .unwrap_or(sections.len().saturating_sub(1));
        let pre_editor: Vec<_> = sections.drain(..editor_idx).collect();
        let overlay = composed.overlay;
        if !sections.is_empty() {
            sections.remove(0);
        }
        Self {
            pre_editor,
            editor,
            post_editor: sections,
            overlay,
            selector,
            dialog_title,
            focus: view.focus,
        }
    }

    fn take_section(&mut self, label: &'static str) -> Option<Box<dyn Component>> {
        let section = self
            .pre_editor
            .iter_mut()
            .find(|section| section.label == label)?;
        Some(std::mem::replace(
            &mut section.component,
            Box::new(pi_tui::components::Text::new(String::new())),
        ))
    }

    fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    /// Inner width the editor actually renders into.
    ///
    /// WHY: `render_editor_with_marker` shifts the editor right by 2 columns
    /// to make room for the prompt marker (whenever the area is at least 2
    /// wide), so the editor wraps its text at `width - 2`, not `width`. Both
    /// the measure and render paths must feed the editor this same value, or
    /// wrapped input can demand rows that were never allocated and clip the
    /// editor or the footer beneath it. The narrow-terminal fallback (a
    /// sub-2-column area renders unshifted) is mirrored exactly.
    fn editor_width(width: u16) -> u16 {
        if width >= 2 { width - 2 } else { width }
    }

    /// Render the live editor with its prompt marker visible.
    ///
    /// WHY: `build_with_chat` drops the composed editor section and renders the
    /// live `Editor` in its place, so the `❯ `/`$ ` marker built by the pure
    /// view never reaches interactive users. Painting the marker into the two
    /// columns to the left of the editor and shifting the editor's Rect right
    /// by 2 makes the marker visible and lands the editor's left edge at
    /// column 2 (D3 shared left edge). The editor computes its cursor position
    /// relative to the Rect it renders into, so the shift keeps cursor math
    /// correct.
    ///
    /// The marker is painted on the editor's first BODY row, not the top
    /// border. The pi-tui `Editor` is a bordered box: its `render` paints a
    /// single top border on its first row — `paint_top_border` consumes
    /// `area.y` and returns `area.y + 1`, so the first body row (the input
    /// text) is `area.y + 1` (verified in
    /// `crates/pi-tui/src/components/editor/mod.rs`; its
    /// `measure_includes_borders` test asserts a measured height >= 3). The
    /// glyph therefore lands beside the input text rather than beside the
    /// border. With fewer than 2 rows there is no body row to hold the marker,
    /// so the editor renders unshifted.
    fn render_editor_with_marker(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width >= 2 && area.height >= 2 {
            let (glyph, color) = {
                let text = self.editor.get_text();
                super::view::editor_prompt_marker(&text)
            };
            let colored = super::theme::current().fg(color, glyph);
            // First body row = one past the single top-border row.
            pi_tui::components::util::paint_line(area.x, area.y + 1, 2, buf, &colored);
            let shifted = Rect::new(area.x + 2, area.y, area.width - 2, area.height);
            self.editor.render(shifted, buf);
        } else {
            self.editor.render(area, buf);
        }
    }
}

fn visible_suffix(heights: &[u16], available: u16) -> (usize, u16) {
    let mut used = 0_u16;
    let mut start = heights.len();
    let mut skipped_rows = 0_u16;
    for (index, &height) in heights.iter().enumerate().rev() {
        if used == available {
            break;
        }
        start = index;
        let remaining = available - used;
        if height > remaining {
            skipped_rows = height - remaining;
            break;
        }
        used += height;
    }
    (start, skipped_rows)
}

fn render_bottom_clipped(
    component: &mut dyn Component,
    area: Rect,
    measured_height: u16,
    skipped_rows: u16,
    buf: &mut Buffer,
) {
    if area.is_empty() {
        return;
    }
    if skipped_rows == 0 {
        component.render(area, buf);
        return;
    }

    let source_area = Rect::new(0, 0, area.width, measured_height);
    let mut source = Buffer::empty(source_area);
    component.render(source_area, &mut source);
    for row in 0..area.height {
        for column in 0..area.width {
            let source_position = (column, skipped_rows + row);
            let target_position = (area.x + column, area.y + row);
            if let (Some(source_cell), Some(target_cell)) =
                (source.cell(source_position), buf.cell_mut(target_position))
            {
                *target_cell = source_cell.clone();
            }
        }
    }
}

impl Component for InteractiveRoot {
    fn measure(&mut self, width: u16) -> u16 {
        let pre_height = self.pre_editor.iter_mut().fold(0_u16, |height, section| {
            height.saturating_add(section.component.measure(width))
        });
        let title_height = self
            .dialog_title
            .as_mut()
            .map_or(0, |title| title.measure(width));
        let body_height = if self.focus == FocusArea::Selector {
            self.selector
                .as_mut()
                .map_or(0, |selector| selector.measure(width))
        } else {
            self.editor.measure(Self::editor_width(width))
        };
        let middle_height = title_height.saturating_add(body_height);
        self.post_editor.iter_mut().fold(
            pre_height.saturating_add(middle_height),
            |height, section| height.saturating_add(section.component.measure(width)),
        )
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let pre_heights = self
            .pre_editor
            .iter_mut()
            .map(|section| section.component.measure(area.width))
            .collect::<Vec<_>>();
        let title_height = self
            .dialog_title
            .as_mut()
            .map_or(0, |title| title.measure(area.width));
        let body_height = if self.focus == FocusArea::Selector {
            self.selector
                .as_mut()
                .map_or(0, |selector| selector.measure(area.width))
        } else {
            self.editor.measure(Self::editor_width(area.width))
        };
        let middle_height = title_height.saturating_add(body_height);
        let post_heights = self
            .post_editor
            .iter_mut()
            .map(|section| section.component.measure(area.width))
            .collect::<Vec<_>>();
        let middle_height = middle_height.min(area.height);
        let post_height = post_heights
            .iter()
            .copied()
            .fold(0_u16, u16::saturating_add)
            .min(area.height - middle_height);
        let pre_height = area.height - middle_height - post_height;
        let (pre_start, skipped_rows) = visible_suffix(&pre_heights, pre_height);
        let bottom = area.bottom();
        let mut y = area.y;

        for (offset, section) in self.pre_editor[pre_start..].iter_mut().enumerate() {
            let measured_height = pre_heights[pre_start + offset];
            let skipped_rows = if offset == 0 { skipped_rows } else { 0 };
            let height = measured_height
                .saturating_sub(skipped_rows)
                .min(bottom.saturating_sub(y));
            render_bottom_clipped(
                section.component.as_mut(),
                Rect::new(area.x, y, area.width, height),
                measured_height,
                skipped_rows,
                buf,
            );
            y = y.saturating_add(height);
        }

        let height = middle_height.min(bottom.saturating_sub(y));
        if height > 0 {
            let rendered_title_height = title_height.min(height);
            if rendered_title_height > 0
                && let Some(title) = self.dialog_title.as_mut()
            {
                title.render(Rect::new(area.x, y, area.width, rendered_title_height), buf);
            }
            let body_height = height.saturating_sub(rendered_title_height);
            if body_height > 0 {
                let body_area = Rect::new(
                    area.x,
                    y.saturating_add(rendered_title_height),
                    area.width,
                    body_height,
                );
                if self.focus == FocusArea::Selector {
                    if let Some(selector) = self.selector.as_mut() {
                        selector.render(body_area, buf);
                    }
                } else {
                    self.render_editor_with_marker(body_area, buf);
                }
            }
            y = y.saturating_add(height);
        }

        for (section, measured_height) in self.post_editor.iter_mut().zip(post_heights) {
            if y == bottom {
                break;
            }
            let height = measured_height.min(bottom - y);
            if height == 0 {
                continue;
            }
            section
                .component
                .render(Rect::new(area.x, y, area.width, height), buf);
            y = y.saturating_add(height);
        }
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.render(area, buf);
        }
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        match self.focus {
            FocusArea::Editor => self.editor.handle_event(event),
            FocusArea::Selector => self
                .selector
                .as_mut()
                .map_or(EventResult::Ignored, |selector| {
                    selector.handle_event(event)
                }),
            FocusArea::Overlay => self
                .overlay
                .as_mut()
                .map_or(EventResult::Ignored, |overlay| overlay.handle_event(event)),
            FocusArea::Widget => EventResult::Ignored,
        }
    }

    fn invalidate(&mut self) {
        for section in &mut self.pre_editor {
            section.component.invalidate();
        }
        self.editor.invalidate();
        for section in &mut self.post_editor {
            section.component.invalidate();
        }
        if let Some(selector) = self.selector.as_mut() {
            selector.invalidate();
        }
        if let Some(title) = self.dialog_title.as_mut() {
            title.invalidate();
        }
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.invalidate();
        }
    }
}

// ---------------------------------------------------------------------------
// InteractiveRuntime
// ---------------------------------------------------------------------------

/// Persistent transcript display preferences applied to every projection.
#[derive(Clone, Copy, Default)]
struct DisplayPreferences {
    /// Whether tool blocks render expanded.
    tools_expanded: bool,
    /// Whether thinking blocks are hidden behind a static label.
    hide_thinking: bool,
}

/// Live interactive runtime.
///
/// Owns:
/// - `tui` — the sole stdout owner.
/// - `input` — the sole stdin owner (`crossterm::EventStream`).
/// - `editor` — the live, stateful editor (preserved across frames).
/// - `view` — the [`ViewState`] snapshot mutated by events and actions.
/// - `mapper` / `input_state` — pure input dispatch state.
/// - `focus` — single-focus manager (used by selectors and overlays).
/// - `events` — the bridged session-event channel.
/// - `partial` — the partial-assistant watch receiver.
/// - `shutdown` — notify for graceful exit.
///
/// The caller owns the [`pi_tui::terminal::guard::TerminalGuard`] so it can
/// outlive the runtime and write restore bytes on process exit even if the
/// runtime itself panics.
struct SessionRebindSignal {
    next_generation: AtomicU64,
    pending_generation: AtomicU64,
    tx: mpsc::UnboundedSender<u64>,
}

impl SessionRebindSignal {
    fn new(tx: mpsc::UnboundedSender<u64>) -> Self {
        Self {
            next_generation: AtomicU64::new(1),
            pending_generation: AtomicU64::new(0),
            tx,
        }
    }

    fn begin(&self) {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        self.pending_generation.store(generation, Ordering::Release);
    }

    fn pending(&self) -> u64 {
        self.pending_generation.load(Ordering::Acquire)
    }

    fn signal_completion(&self) {
        let generation = self.pending();
        if generation != 0 {
            let _ = self.tx.send(generation);
        }
    }

    fn claim(&self, generation: u64) -> bool {
        self.pending_generation
            .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// Interactive terminal event loop and rendered session state.
#[allow(clippy::struct_excessive_bools)]
pub struct InteractiveRuntime<W: Write, S: SessionHost> {
    tui: Tui<W>,
    input: TerminalInput,
    session: Arc<S>,
    editor: Editor,
    view: ViewState,
    mapper: InputMapper,
    input_state: InputState,
    events: EventSubscription,
    partial: watch::Receiver<Option<Arc<AssistantMessage>>>,
    /// Generation handshake for bridge-driven replacement channel closure.
    session_rebind_signal: Arc<SessionRebindSignal>,
    session_rebind_rx: mpsc::UnboundedReceiver<u64>,
    session_events_closed_for_rebind: bool,
    session_rebind_channel_closed: bool,
    prompt_operations: PromptOperations,
    coalesce_deadline: Option<Instant>,
    /// When the current status phase began; `None` while no status is shown.
    /// Tokio's `Instant` so the paused test clock drives `elapsed_secs`.
    spinner_started: Option<tokio::time::Instant>,
    /// Braille spinner frame counter, advanced every [`SPINNER_TICK`].
    spinner_frame: usize,
    /// Persisted next-tick deadline for the spinner `select!` arm.
    ///
    /// WHY: `tokio::select!` drops the losing future each loop turn, so a
    /// fresh `sleep(SPINNER_TICK)` recreated every turn can be starved by
    /// busier arms and never fire. Storing the deadline keeps it alive across
    /// turns so the spinner ticks at a steady cadence.
    spinner_deadline: Option<tokio::time::Instant>,
    /// Kind the current spinner clock belongs to; `None` while no status.
    ///
    /// WHY: `set_status` was the only reset point and is easily bypassed (the
    /// host replaces `view.status` directly), so the elapsed clock could leak
    /// across unrelated status kinds — or survive a status gap when no tick
    /// fired. `reconcile_spinner_clock` is the single reset point now, called
    /// every loop turn via `arm_spinner_deadline` and on every tick, so a new
    /// kind (or a reappearance after a gap) counts up from 0s instead of
    /// inheriting a prior phase's time.
    spinner_kind: Option<StatusKind>,
    /// Cause for re-anchoring the next paint (full rows, no cell diff);
    /// set when an extension overlay opens over unrelated content so its
    /// first frame cannot be fragmented by the diff.
    pending_reanchor: Option<ReanchorCause>,
    pending_settle: Option<Vec<SettledBlock>>,
    shutdown: Arc<Notify>,
    exited: bool,
    exit_kind: InteractiveExit,
    last_error: Option<String>,
    shutdown_flag: Arc<std::sync::atomic::AtomicBool>,
    terminal_theme: TerminalTheme,
    /// Runtime theme generation: bumped on every theme switch so extension
    /// slots re-measure/re-render (flows to the host via `theme.update` and
    /// back on measure/render requests).
    theme_generation: u64,
    /// Set by [`Self::apply_theme`]; the event loop flushes it with
    /// `push_theme_to_host` so previews and restores reach extension slots
    /// while rapid highlight changes coalesce into one update.
    theme_push_pending: bool,
    pending_ui_reinject: Vec<UiEvent>,
    extension_runner: Option<Arc<ExtensionRuntimeSet>>,
    extension_events: Option<tokio::sync::broadcast::Receiver<ExtensionUiEvent>>,
    extension_requests: Option<mpsc::Receiver<HostUiRequest>>,
    extension_registry_changes: Option<watch::Receiver<u64>>,
    extension_slots: std::collections::HashMap<String, ProjectedExtensionSlot>,
    focused_extension_slot: Option<String>,
    effective_extension_shortcuts: Vec<EffectiveExtensionShortcut>,
    extension_action_rx: mpsc::UnboundedReceiver<Result<(), String>>,
    extension_action_tx: mpsc::UnboundedSender<Result<(), String>>,
    pending_extension_dialog: Option<PendingExtensionDialog>,
    extension_select_rx: mpsc::UnboundedReceiver<String>,
    extension_select_tx: mpsc::UnboundedSender<String>,
    display: DisplayPreferences,
    chat_prefix_cache: Option<Box<dyn Component>>,
    chat_prefix_len: usize,
    chat_tail_cache: Option<Box<dyn Component>>,
    chat_dirty: bool,
    /// Live selector component (replaces the editor while focused).
    active_selector: Option<Box<dyn Component>>,
    /// Kind of the active selector for confirm/cancel routing.
    active_selector_kind: Option<super::state::SelectorKind>,
    /// Pending editor submits emitted via `Editor::on_submit`.
    submit_rx: mpsc::UnboundedReceiver<String>,
    /// Sender retained so the editor callback stays valid across rebuilds.
    submit_tx: mpsc::UnboundedSender<String>,
    /// Pending selector confirm values.
    select_rx: mpsc::UnboundedReceiver<(super::state::SelectorKind, String)>,
    select_tx: mpsc::UnboundedSender<(super::state::SelectorKind, String)>,
    /// Pending selector cancels.
    cancel_rx: mpsc::UnboundedReceiver<()>,
    cancel_tx: mpsc::UnboundedSender<()>,
    /// Pending settings-row changes emitted by the live settings list.
    settings_change_rx: mpsc::UnboundedReceiver<(String, String)>,
    settings_change_tx: mpsc::UnboundedSender<(String, String)>,
    /// Pending live previews from the `/theme` selector.
    theme_preview_rx: mpsc::UnboundedReceiver<String>,
    theme_preview_tx: mpsc::UnboundedSender<String>,
    /// Theme snapshot restored when the `/theme` selector is cancelled.
    theme_preview_restore: Option<Arc<ResolvedTheme>>,
    /// Live first-run wizard state (drives the `FirstTimeSetup` overlay).
    first_run: Option<FirstRunWizardState>,
    /// `/debug` dump directory (agent dir, resolved once at construction).
    debug_dump_dir: std::path::PathBuf,
    /// In-flight `/import` awaiting its confirm dialog(s).
    pending_import: Option<PendingImport>,
    /// Stored-credential options backing the active `/logout` selector.
    logout_options: Vec<super::state::LogoutOption>,
    /// Set by the `before_session_invalidate` callback so the next
    /// `rebind_session_channels` resets extension-owned UI (ports upstream
    /// wiring `resetExtensionUI` on session invalidation).
    reset_ui_flag: Arc<std::sync::atomic::AtomicBool>,
    /// Editor placeholder saved while a built-in confirm/logout selector shows
    /// its prompt; restored when the selector closes.
    confirm_saved_placeholder: Option<String>,
}

/// In-flight `/import` awaiting its confirm dialog(s).
struct PendingImport {
    /// Import source path.
    path: String,
    /// Fallback cwd for the missing-cwd retry phase (`Some` once the first
    /// attempt hit a missing-cwd error and we re-confirm with the fallback).
    retry_cwd: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionOperationKind {
    Prompt,
    Bash,
}

/// Completion of one session operation owned by the interactive runtime.
struct PromptCompletion {
    id: u64,
    epoch: u64,
    kind: SessionOperationKind,
    result: Result<(), String>,
}

/// Runtime-owned session tasks plus their per-session abort signals.
///
/// `epoch` advances before session replacement or runtime exit. Results from an
/// older epoch are drained but never projected onto the replacement session.
struct PromptOperations {
    epoch: u64,
    next_id: u64,
    tasks: JoinSet<PromptCompletion>,
    aborts: std::collections::BTreeMap<u64, oneshot::Sender<()>>,
    bash_operation: Option<u64>,
}

#[derive(Debug)]
struct PendingExtensionDialog {
    request: HostUiRequest,
    saved_editor_text: Option<String>,
    saved_editor_placeholder: String,
    deadline: Option<Instant>,
}

/// Live first-run wizard state (family → mode → analytics).
struct FirstRunWizardState {
    step: usize,
    selected: usize,
    family: Option<String>,
    mode: Option<ThemeMode>,
    pre_theme: Arc<ResolvedTheme>,
}

#[derive(Clone, Debug)]
struct EffectiveExtensionShortcut {
    key: String,
    dispatch_key: String,
    parsed: ParsedKeyId,
    description: Option<String>,
    source: Option<String>,
}

#[derive(Clone, Debug)]
struct ProjectedExtensionSlot {
    placement: SlotPlacement,
    generation: u64,
    focusable: bool,
}

impl PromptOperations {
    fn new() -> Self {
        Self {
            epoch: 0,
            next_id: 0,
            tasks: JoinSet::new(),
            aborts: std::collections::BTreeMap::new(),
            bash_operation: None,
        }
    }
}

/// Build the live editor with the runtime's fixed options and submit hook.
fn build_initial_editor(
    options: &InteractiveRuntimeOptions,
    submit_tx: mpsc::UnboundedSender<String>,
) -> Editor {
    let mut editor = Editor::new(
        &pi_tui::components::editor::EditorTheme::default(),
        &EditorOptions {
            padding_x: 1,
            autocomplete_max_visible: 5,
            terminal_rows: options.size.1,
        },
    );
    editor.on_submit = Some(Box::new(move |text: String| {
        let _ = submit_tx.send(text);
    }));
    editor
}

impl<W: Write, S: SessionHost> InteractiveRuntime<W, S> {
    /// Construct the runtime around an already-active [`Tui`] and
    /// [`TerminalInput`].
    ///
    /// The caller is responsible for activating the
    /// [`pi_tui::terminal::guard::TerminalGuard`] before this call and dropping
    /// it after the runtime exits.
    ///
    /// # Panics
    ///
    /// Never. Construction is infallible.
    #[must_use]
    // A flat channel-and-field initialization list; splitting it would add
    // indirection without hiding any behaviour.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn new(
        tui: Tui<W>,
        input: TerminalInput,
        session: Arc<S>,
        options: &InteractiveRuntimeOptions,
    ) -> Self {
        let mut view = Self::seed_view(options);

        let events = session.subscribe();
        let partial = session.partial_rx();
        let snapshot = session.snapshot();
        project_snapshot(&mut view, &snapshot, None);
        view.messages = project_messages(&session.messages());
        let hide_thinking = session.hide_thinking_block();
        apply_display_preferences(&mut view.messages, false, hide_thinking);
        let extension_runner = session.host_extension_runner();
        let extension_events = extension_runner
            .as_ref()
            .map(|runner| runner.subscribe_ui());
        let (extension_registry_changes, effective_extension_shortcuts) =
            subscribe_and_snapshot_shortcuts(extension_runner.as_ref());
        let extension_requests = extension_runner
            .as_ref()
            .and_then(|runner| runner.take_ui_requests());
        let initial_extension_slots = extension_runner
            .as_ref()
            .map_or_else(Vec::new, |runner| runner.current_slots());
        view.extension_shortcuts = shortcut_hints(&effective_extension_shortcuts);

        let (submit_tx, submit_rx) = mpsc::unbounded_channel::<String>();
        let (select_tx, select_rx) =
            mpsc::unbounded_channel::<(super::state::SelectorKind, String)>();
        let (cancel_tx, cancel_rx) = mpsc::unbounded_channel::<()>();
        let (settings_change_tx, settings_change_rx) =
            mpsc::unbounded_channel::<(String, String)>();
        let (theme_preview_tx, theme_preview_rx) = mpsc::unbounded_channel::<String>();
        let (extension_select_tx, extension_select_rx) = mpsc::unbounded_channel::<String>();
        let (extension_action_tx, extension_action_rx) = mpsc::unbounded_channel();
        let (session_rebind_tx, session_rebind_rx) = mpsc::unbounded_channel();
        let session_rebind_signal = Arc::new(SessionRebindSignal::new(session_rebind_tx));

        let editor = build_initial_editor(options, submit_tx.clone());
        let agent_dir = crate::core::config::get_agent_dir();
        // Process-global table for TUI components + mapper snapshot for app.* ids.
        let keybindings = crate::core::keybindings::install_app_keybindings(&agent_dir);

        let mut runtime = Self {
            tui,
            input,
            session,
            editor,
            view,
            mapper: super::input::InputMapper::with_keybindings(keybindings),
            input_state: InputState::new(options.double_escape),
            events,
            session_rebind_signal,
            session_rebind_rx,
            session_events_closed_for_rebind: false,
            session_rebind_channel_closed: false,
            partial,
            prompt_operations: PromptOperations::new(),
            coalesce_deadline: None,
            spinner_started: None,
            pending_reanchor: None,
            spinner_frame: 0,
            spinner_deadline: None,
            spinner_kind: None,
            pending_settle: None,
            shutdown: Arc::new(Notify::new()),
            exited: false,
            exit_kind: InteractiveExit::Clean,
            last_error: None,
            shutdown_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            terminal_theme: options.terminal_theme,
            theme_generation: 0,
            pending_ui_reinject: options.pending_ui_events.iter().rev().cloned().collect(),
            extension_runner,
            extension_events,
            extension_requests,
            extension_registry_changes,
            extension_slots: std::collections::HashMap::new(),
            focused_extension_slot: None,
            effective_extension_shortcuts,
            extension_action_rx,
            extension_action_tx,
            pending_extension_dialog: None,
            extension_select_rx,
            extension_select_tx,
            display: DisplayPreferences {
                tools_expanded: false,
                hide_thinking,
            },
            chat_prefix_cache: None,
            chat_prefix_len: usize::MAX,
            chat_tail_cache: None,
            chat_dirty: true,
            active_selector: None,
            active_selector_kind: None,
            submit_rx,
            submit_tx,
            select_rx,
            select_tx,
            cancel_rx,
            cancel_tx,
            settings_change_rx,
            settings_change_tx,
            theme_preview_rx,
            theme_preview_tx,
            theme_preview_restore: None,
            theme_push_pending: false,
            first_run: None,
            debug_dump_dir: agent_dir,
            pending_import: None,
            logout_options: Vec::new(),
            reset_ui_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            confirm_saved_placeholder: None,
        };
        for slot in initial_extension_slots {
            runtime.project_extension_slot(slot);
        }
        runtime
    }

    /// Build the initial [`ViewState`] from startup options and install the
    /// startup theme as the thread-local current.
    fn seed_view(options: &InteractiveRuntimeOptions) -> ViewState {
        let mut view = ViewState::empty();
        view.theme = options.theme.clone();
        super::theme::set_current(options.theme.clone());
        view.width = options.size.0;
        view.height = options.size.1;
        view.quiet = options.quiet;
        view.resize(options.size.0, options.size.1);
        view
    }

    // ----- Public accessors (driver seam) -----

    /// Borrow the view state (tests / driver seam).
    pub fn view(&self) -> &ViewState {
        &self.view
    }

    /// Last row occupied by the current terminal viewport.
    #[must_use]
    pub fn viewport_bottom_row(&self) -> u16 {
        self.view.height.saturating_sub(1)
    }

    /// Mutably borrow the view state (tests / driver seam).
    pub fn view_mut(&mut self) -> &mut ViewState {
        &mut self.view
    }

    /// Borrow the live editor (tests / driver seam).
    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    /// Mutably borrow the live editor (tests / driver seam).
    pub fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    /// Borrow the input mapper state (tests).
    pub fn input_state(&self) -> &InputState {
        &self.input_state
    }

    /// Last recorded session error message, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Current terminal background polarity used by automatic theme selection.
    #[must_use]
    pub fn terminal_theme(&self) -> TerminalTheme {
        self.terminal_theme
    }

    fn requery_terminal_theme(&mut self) {
        let colorfgbg = std::env::var("COLORFGBG").ok();
        self.terminal_theme = detect_terminal_theme(
            self.tui.capabilities().dark_background,
            colorfgbg.as_deref(),
        );
    }

    /// Signal the runtime to exit at the next loop turn (signal handler hook).
    pub fn request_shutdown(&self) {
        self.shutdown_flag
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.shutdown.notify_one();
    }

    /// Shared shutdown notify (for registering multiple signal sources).
    pub fn shutdown_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.shutdown)
    }

    /// Borrow the underlying [`Tui`] (for suspend / resume / reprobe).
    pub fn tui(&self) -> &Tui<W> {
        &self.tui
    }

    /// Mutably borrow the underlying [`Tui`].
    pub fn tui_mut(&mut self) -> &mut Tui<W> {
        &mut self.tui
    }

    /// Borrow the input handle (driver seam).
    pub fn input(&self) -> &TerminalInput {
        &self.input
    }

    /// Mutably borrow the input handle (driver seam).
    pub fn input_mut(&mut self) -> &mut TerminalInput {
        &mut self.input
    }

    // ----- Main loop -----

    async fn initialize_run(&mut self) -> bool {
        // Hand the host the startup theme + catalog before serving events so
        // `ctx.ui.theme` / `getAllThemes` observe real data immediately.
        self.push_theme_to_host().await;
        // Warm the ui.state mirror so getEditorText / getToolsExpanded are
        // defined before any extension control arrives.
        self.push_ui_state_to_host().await;
        self.refresh_footer().await;
        if crate::core::platform::first_run::should_run_first_time_setup_on_host(None, None) {
            self.open_first_run_wizard();
            self.push_theme_to_host().await;
        }
        if let Err(error) = self.paint_frame() {
            self.exit_kind = InteractiveExit::IoFailure;
            self.last_error = Some(error.to_string());
            return false;
        }
        true
    }

    /// Latched shutdown check catches notifications fired before the select
    /// arm was awaiting (Ctrl+Z, signal handler, etc.). Only forces Clean when
    /// no more-specific exit was already set (Suspend sets `exit_kind` + exited
    /// without using this flag). Returns true when the loop must stop.
    fn take_latched_shutdown(&mut self) -> bool {
        if !self
            .shutdown_flag
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return false;
        }
        if !self.exited {
            self.exit_kind = InteractiveExit::Clean;
            self.exited = true;
        }
        true
    }

    /// Run the main event loop until shutdown is requested or stdin closes.
    ///
    /// Returns the exit reason; the caller drops the runtime and the
    /// [`pi_tui::terminal::guard::TerminalGuard`] in that order.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] only when a terminal write fails irrecoverably.
    // A flat `select!` event loop: one arm per event source. Splitting arms
    // into methods would hide the loop's shape without removing any behaviour.
    #[allow(clippy::too_many_lines)]
    pub async fn run(&mut self) -> io::Result<InteractiveExit> {
        if !self.initialize_run().await {
            return Ok(self.exit_kind);
        }

        while !self.exited {
            if self.take_latched_shutdown() {
                break;
            }

            // Re-inject events preserved by resize coalescing before pulling
            // new ones, so ordering across the storm is preserved.
            if let Some(event) = self.pending_ui_reinject.pop() {
                if let Err(err) = self.handle_ui_event(event).await {
                    self.fail_io(&err);
                    break;
                }
                if !self.settle_pending() {
                    break;
                }
                continue;
            }

            let coalesce_wait = self.coalesce_wait(Instant::now());
            let (spinner_active, spinner_deadline) = self.arm_spinner_deadline();

            tokio::select! {
                biased;

                () = self.shutdown.notified() => {
                    self.exit_kind = InteractiveExit::Clean;
                    self.exited = true;
                }
                ui = self.input.recv() => {
                    if let Some(event) = ui {
                        if let Err(err) = self.handle_ui_event(event).await {
                            self.fail_io(&err);
                        }
                    } else {
                        // stdin EOF: clean exit.
                        self.exit_kind = InteractiveExit::Clean;
                        self.exited = true;
                    }
                }
                ev = self.events.rx.recv(), if !self.session_events_closed_for_rebind => {
                    if let Some(event) = ev {
                        self.handle_session_event(&event);
                        if event_refreshes_footer(&event) {
                            self.refresh_footer().await;
                        }
                    } else if self.session_rebind_signal.pending() != 0 {
                        self.session_events_closed_for_rebind = true;
                    } else {
                        self.exit_kind = InteractiveExit::SessionEnded;
                        self.exited = true;
                    }
                }
                generation = self.session_rebind_rx.recv(), if !self.session_rebind_channel_closed => {
                    match generation {
                        Some(generation) => {
                            self.handle_session_rebind_completion(generation).await;
                        }
                        None => {
                            self.session_rebind_channel_closed = true;
                        }
                    }
                }
                extension_event = recv_extension_event(&mut self.extension_events) => {
                    self.handle_extension_stream_event(extension_event).await;
                }
                registry_changed =
                    wait_extension_registry_change(&mut self.extension_registry_changes) => {
                    self.handle_extension_registry_change(registry_changed);
                }
                extension_request = recv_extension_request(&mut self.extension_requests) => {
                    if let Some(extension_request) = extension_request {
                        self.begin_extension_dialog(extension_request).await;
                    } else {
                        self.extension_requests = None;
                    }
                }
                () = wait_extension_deadline(
                    self.pending_extension_dialog.as_ref().and_then(|dialog| dialog.deadline),
                ), if self.pending_extension_dialog.as_ref().and_then(|dialog| dialog.deadline).is_some() => {
                    self.cancel_extension_dialog().await;
                }
                changed = self.partial.changed(), if !self.session_events_closed_for_rebind => {
                    if changed.is_ok() {
                        self.handle_partial_update();
                    }
                }
                completion = self.prompt_operations.tasks.join_next(), if !self.prompt_operations.tasks.is_empty() => {
                    if let Some(completion) = completion
                        && self.handle_prompt_completion(completion)
                    {
                        self.refresh_footer().await;
                    }
                }
                () = tokio::time::sleep_until(spinner_deadline), if spinner_active => {
                    // Advance from the fired deadline (not `now`) so the cadence
                    // does not drift under load; the next turn reuses this value.
                    self.spinner_deadline = Some(spinner_deadline + SPINNER_TICK);
                    if self.tick_status_indicator() {
                        self.arm_coalescer();
                    }
                }
                () = tokio::time::sleep(coalesce_wait) => {
                    if self.coalesce_deadline.is_some() {
                        self.coalesce_deadline = None;
                        if let Err(err) = self.paint_frame() {
                            self.fail_io(&err);
                        }
                    }
                }
                extension_result = self.extension_action_rx.recv() => {
                    if let Some(Err(error)) = extension_result {
                        self.last_error = Some(error);
                    }
                }
            }

            self.end_loop_turn().await;
        }

        Ok(self.finish_run().await)
    }

    /// Time until the coalescer deadline, or effectively-forever when idle.
    fn coalesce_wait(&self, now: Instant) -> Duration {
        self.coalesce_deadline
            .map_or(Duration::from_hours(1), |deadline| {
                deadline.saturating_duration_since(now)
            })
    }

    /// Single reset point for the spinner clock.
    ///
    /// WHY: when `view.status` becomes `None`, the loop previously cleared only
    /// the next-tick deadline and left `spinner_started`, `spinner_frame`, and
    /// `spinner_kind` intact, so a status reappearing later could inherit the
    /// previous phase's elapsed seconds when no tick fired during the gap. The
    /// same leak occurs on an A→B→A kind flip before the timer wins. Clearing
    /// every clock field on `None`, and resetting them on a kind change, from
    /// this one function closes both holes. It is called from two entry points
    /// — `arm_spinner_deadline` (every loop turn) and `tick_status_indicator`
    /// (for direct unit-test callers) — so neither path can bypass the reset.
    fn reconcile_spinner_clock(&mut self) {
        match &self.view.status {
            None => {
                self.spinner_started = None;
                self.spinner_frame = 0;
                self.spinner_kind = None;
                self.spinner_deadline = None;
            }
            Some(status) => {
                if self.spinner_kind != Some(status.kind) {
                    self.spinner_started = None;
                    self.spinner_frame = 0;
                    self.spinner_kind = Some(status.kind);
                    self.spinner_deadline = None;
                }
            }
        }
    }

    /// Persist the spinner tick deadline across loop turns. `select!` drops
    /// the losing future each turn, so a fresh `sleep(SPINNER_TICK)` recreated
    /// every turn can be starved by busier arms and never fire; the stored
    /// deadline (see `spinner_deadline`) keeps it alive. Reconciliation runs
    /// here every turn (the loop's natural per-turn entry point); with the
    /// clock already matching the visible status, this only seeds and returns
    /// the deadline while a status is shown.
    fn arm_spinner_deadline(&mut self) -> (bool, tokio::time::Instant) {
        self.reconcile_spinner_clock();
        if self.view.status.is_some() {
            let deadline = *self
                .spinner_deadline
                .get_or_insert_with(|| tokio::time::Instant::now() + SPINNER_TICK);
            (true, deadline)
        } else {
            (false, tokio::time::Instant::now())
        }
    }

    /// Per-turn epilogue: flush a pending theme push (previews/restores mark
    /// the theme dirty without pushing inline, so extension slots track the
    /// switch while rapid changes coalesce) and settle as its own transaction.
    async fn end_loop_turn(&mut self) {
        if self.theme_push_pending {
            self.push_theme_to_host().await;
        }
        self.settle_pending();
    }

    /// Record an unrecoverable terminal I/O failure and request exit.
    fn fail_io(&mut self, err: &io::Error) {
        self.exit_kind = InteractiveExit::IoFailure;
        self.last_error = Some(err.to_string());
        self.exited = true;
    }

    /// Commit any pending settle transaction; returns `false` on I/O failure.
    fn settle_pending(&mut self) -> bool {
        if let Some(blocks) = self.pending_settle.take()
            && let Err(err) = self.commit_settle(blocks)
        {
            self.fail_io(&err);
            return false;
        }
        true
    }

    async fn finish_run(&mut self) -> InteractiveExit {
        // A prompt owns AgentSession turn cleanup until it settles. Abort and
        // drain before returning so dropping the runtime cannot detach a turn.
        self.quiesce_prompt_operations().await;

        // Final paint so the last view-state mutation is visible.
        if matches!(
            self.exit_kind,
            InteractiveExit::Clean | InteractiveExit::SessionEnded
        ) {
            let _ = self.paint_frame();
        }
        self.exit_kind
    }

    // -----------------------------------------------------------------------
    // Event handlers
    // -----------------------------------------------------------------------

    async fn handle_ui_event(&mut self, event: UiEvent) -> io::Result<()> {
        let Some(event) = self.intercept_terminal_input(event).await else {
            return Ok(());
        };
        // First-run wizard owns all keys while its overlay is up; a focused
        // extension slot must not steal them.
        if self.first_run.is_some()
            && let UiEvent::Key(key_event) = &event
        {
            if key_event.kind != crossterm::event::KeyEventKind::Release {
                let code = key_event.code;
                self.handle_first_run_key(code).await;
                self.paint_frame()?;
            }
            return Ok(());
        }
        if self.route_extension_input(&event) {
            return Ok(());
        }
        // Swap the editor (and active selector) into a throwaway-built
        // InteractiveRoot so we can route the event, then recover both.
        let saved_editor = std::mem::replace(&mut self.editor, Editor::with_defaults());
        let saved_selector = self.active_selector.take();
        let mut root = self.build_root(saved_editor, saved_selector);
        let editor_result = root.handle_event(&event);
        self.recover_root(root);
        // Re-attach on_submit after the swap (Editor does not preserve it
        // through with_defaults temporary).
        self.ensure_editor_on_submit();

        // Refresh view.editor.text from the live buffer so the mapper sees
        // the freshest value.
        let live_text = self.editor.get_text();
        // Expanded form (paste markers resolved) feeds submission paths so
        // followUp/submit carry real content, not the collapsed marker.
        let expanded_text = self.editor.get_expanded_text();
        self.view.editor.text.clone_from(&live_text);
        let (_line, col) = self.editor.get_cursor();
        self.view.editor.cursor = col;

        // Drain editor on_submit notifications first (plain Enter).
        let mut actions: Vec<ViewAction> = Vec::new();
        while let Ok(text) = self.submit_rx.try_recv() {
            actions.push(ViewAction::Submit { text });
            if self.pending_extension_dialog.is_none() {
                actions.push(ViewAction::ClearEditor);
            }
        }
        while let Ok((selector, value)) = self.select_rx.try_recv() {
            actions.push(ViewAction::SelectConfirmed { selector, value });
        }
        while let Ok(value) = self.extension_select_rx.try_recv() {
            self.finish_extension_selection(value).await;
        }
        while self.cancel_rx.try_recv().is_ok() {
            if self.pending_extension_dialog.is_some() {
                self.cancel_extension_dialog().await;
            } else {
                actions.push(ViewAction::SelectCancelled);
            }
        }
        let mut settings_mutated = false;
        while let Ok((id, value)) = self.settings_change_rx.try_recv() {
            self.handle_settings_change(&id, &value).await;
            settings_mutated = true;
        }
        while let Ok(selection) = self.theme_preview_rx.try_recv() {
            self.preview_theme_selection(&selection);
            settings_mutated = true;
        }

        // Map app-level keys (skipped when the focused component already
        // handled the event — including selector confirm/cancel).
        actions.extend(self.mapper.map(
            &event,
            &self.view,
            &live_text,
            &expanded_text,
            &mut self.input_state,
            editor_result.is_handled(),
        ));

        let mut needs_immediate_repaint = editor_result.needs_render() || settings_mutated;
        for action in actions {
            let outcome = self.dispatch_action(action).await;
            if matches!(outcome, ActionOutcome::Repaint) {
                needs_immediate_repaint = true;
            }
            if matches!(outcome, ActionOutcome::Exit) {
                self.exited = true;
                self.exit_kind = InteractiveExit::Clean;
            }
            if matches!(outcome, ActionOutcome::Suspend) {
                self.exited = true;
                self.exit_kind = InteractiveExit::Suspend;
            }
            if matches!(outcome, ActionOutcome::ExternalEditor) {
                self.exited = true;
                self.exit_kind = InteractiveExit::ExternalEditor;
            }
        }

        if needs_immediate_repaint {
            // Input-driven paints BYPASS the coalescer (per master plan D9).
            self.paint_frame()?;
        }
        Ok(())
    }

    fn handle_session_event(&mut self, event: &AgentSessionEvent) {
        project_event(&mut self.view, event);
        apply_display_preferences(
            &mut self.view.messages,
            self.display.tools_expanded,
            self.display.hide_thinking,
        );
        if matches!(event, AgentSessionEvent::MessageUpdate { .. }) {
            self.chat_dirty = true;
        } else {
            self.chat_prefix_cache = None;
            self.chat_prefix_len = usize::MAX;
            self.chat_dirty = true;
        }
        self.arm_coalescer();
    }

    fn handle_partial_update(&mut self) {
        let partial = self.partial.borrow_and_update().clone();
        if let Some(message) = partial {
            // Replace the streaming assistant tail (or push if none yet).
            let mut found = false;
            for item in &mut self.view.messages {
                if let MessageView::Assistant(view) = item
                    && view.streaming
                {
                    view.message = (*message).clone();
                    found = true;
                    break;
                }
            }
            if !found {
                self.view
                    .messages
                    .push(MessageView::streaming_assistant((*message).clone()));
            }
            self.view.streaming = true;
            apply_display_preferences(
                &mut self.view.messages,
                self.display.tools_expanded,
                self.display.hide_thinking,
            );
            self.chat_dirty = true;
            self.arm_coalescer();
        } else {
            // Stream ended; the next MessageEnd event will finalize the tail.
            self.arm_coalescer();
        }
    }

    // -----------------------------------------------------------------------
    // Action dispatch
    // -----------------------------------------------------------------------

    /// Flip thinking-block visibility, persist it, and reproject messages.
    fn toggle_thinking(&mut self) -> ActionOutcome {
        self.display.hide_thinking = !self.display.hide_thinking;
        if let Err(error) = self
            .session
            .set_hide_thinking_block(self.display.hide_thinking)
        {
            self.last_error = Some(error);
        }
        self.reapply_display_preferences()
    }

    /// Flip tool/bash expansion and reproject messages.
    fn toggle_tool_expand(&mut self) -> ActionOutcome {
        self.display.tools_expanded = !self.display.tools_expanded;
        self.reapply_display_preferences()
    }

    fn reapply_display_preferences(&mut self) -> ActionOutcome {
        apply_display_preferences(
            &mut self.view.messages,
            self.display.tools_expanded,
            self.display.hide_thinking,
        );
        self.chat_dirty = true;
        ActionOutcome::Repaint
    }

    async fn dispatch_action(&mut self, action: ViewAction) -> ActionOutcome {
        match action {
            ViewAction::None | ViewAction::Consumed => ActionOutcome::None,
            ViewAction::ExternalEditor => ActionOutcome::ExternalEditor,
            ViewAction::Render | ViewAction::OpenSettingsSubmenu { .. } => ActionOutcome::Repaint,
            ViewAction::ToggleThinking => self.toggle_thinking(),
            ViewAction::ToggleToolExpand => self.toggle_tool_expand(),
            ViewAction::Submit { text } => self.submit_text(text, false).await,
            ViewAction::SubmitBash {
                command,
                exclude_from_context,
            } => self.dispatch_bash(&command, exclude_from_context).await,
            ViewAction::Interrupt => self.dispatch_interrupt().await,
            ViewAction::ClearEditor => self.clear_editor(),
            ViewAction::Exit => ActionOutcome::Exit,
            ViewAction::Suspend => ActionOutcome::Suspend,
            ViewAction::CycleThinking { .. } => {
                self.record_err(self.session.cycle_thinking_level().await);
                self.refresh_footer().await;
                ActionOutcome::Repaint
            }
            ViewAction::CycleModel { forward } => {
                self.record_err(self.session.cycle_model(forward).await);
                self.refresh_footer().await;
                ActionOutcome::Repaint
            }
            ViewAction::OpenModelSelector => {
                self.open_selector(super::state::SelectorKind::Model).await
            }
            ViewAction::OpenSettings => {
                self.open_selector(super::state::SelectorKind::Settings)
                    .await
            }
            ViewAction::OpenSessionPicker => {
                self.open_selector(super::state::SelectorKind::Session)
                    .await
            }
            ViewAction::OpenTreeSelector => {
                self.open_selector(super::state::SelectorKind::Tree).await
            }
            ViewAction::OpenForkSelector => {
                self.open_selector(super::state::SelectorKind::Fork).await
            }
            ViewAction::OpenTrustSelector => {
                self.open_selector(super::state::SelectorKind::Trust).await
            }
            ViewAction::OpenLogin { .. } => self.open_overlay(OverlayKind::Login),
            ViewAction::Logout => self.handle_logout_command().await,
            ViewAction::OpenScopedModels => {
                self.open_selector(super::state::SelectorKind::ScopedModels)
                    .await
            }
            ViewAction::OpenConfigSelector => {
                self.open_selector(super::state::SelectorKind::Config).await
            }
            ViewAction::ToggleShortcutHelp => self.toggle_shortcut_help(),
            ViewAction::ShowChangelog => self.open_overlay(OverlayKind::Changelog),
            ViewAction::Paste { text } => self.paste_text(&text),
            ViewAction::QueueFollowUp { text } => self.queue_follow_up(text).await,
            ViewAction::DequeueFollowUp => self.dequeue_follow_up(),
            ViewAction::CopyLastAssistant => self.copy_last_assistant().await,
            ViewAction::Reload => self.handle_reload_action().await,
            ViewAction::SlashCommand { name, args } => self.submit_slash_command(name, args).await,
            ViewAction::SelectConfirmed { selector, value } => {
                self.handle_select_confirmed(selector, value).await
            }
            ViewAction::SelectCancelled => {
                let was_import = matches!(
                    self.active_selector_kind,
                    Some(
                        super::state::SelectorKind::ImportConfirm
                            | super::state::SelectorKind::ImportCwdConfirm
                    )
                );
                self.restore_theme_preview();
                self.close_selector();
                if was_import {
                    self.pending_import = None;
                    self.push_notice("import", "Import cancelled".to_owned());
                }
                ActionOutcome::Repaint
            }
            ViewAction::FocusChanged { area } => {
                self.view.focus = area;
                ActionOutcome::Repaint
            }
            ViewAction::ShowOverlay { kind } => self.open_overlay(kind),
            ViewAction::DismissOverlay => self.dismiss_overlay(),
            ViewAction::NewSession => self.replace_session(SessionReplacement::New).await,
            ViewAction::Fork => self.replace_session(SessionReplacement::Fork).await,
            ViewAction::Clone => self.replace_session(SessionReplacement::Clone).await,
            ViewAction::Compact { instructions } => {
                self.record_err(self.session.compact(instructions.as_deref()).await);
                ActionOutcome::None
            }
            ViewAction::Resize { width, height } => self.handle_resize(width, height),
        }
    }

    /// `/reload` (hotkey path): reload host resources, re-probe the terminal
    /// background, re-resolve the theme, and refresh app keybindings.
    async fn handle_reload_action(&mut self) -> ActionOutcome {
        let snapshot = self.session.snapshot();
        if snapshot.is_streaming() {
            self.push_notice(
                "reload",
                "Wait for the current response to finish before reloading.".to_owned(),
            );
            return ActionOutcome::Repaint;
        }
        if snapshot.is_compacting() {
            self.push_notice(
                "reload",
                "Wait for compaction to finish before reloading.".to_owned(),
            );
            return ActionOutcome::Repaint;
        }
        self.reset_extension_ui();
        if self.pending_extension_dialog.is_some() {
            self.cancel_extension_dialog().await;
        }
        match self.session.reload().await {
            Ok(messages) => {
                for message in messages {
                    self.push_notice("reload", message);
                }
            }
            Err(error) => self.last_error = Some(error),
        }
        self.rebind_extension_channels().await;
        if let Ok(Some(dark)) = self.input.requery_background(self.tui.outer_mut()).await {
            self.tui.capabilities_mut().set_dark_background(Some(dark));
        }
        self.requery_terminal_theme();
        self.apply_theme_from_settings();
        self.push_theme_to_host().await;
        let keybindings = crate::core::keybindings::reload_app_keybindings(&self.debug_dump_dir);
        self.mapper.set_keybindings(keybindings);
        ActionOutcome::Repaint
    }

    /// Route a built-in slash command by `name` + `args`. Returns `Some` when
    /// `name` is a recognized built-in (fully handled here) so the command never
    /// reaches the LLM; `None` lets the caller fall through to extension dispatch
    /// or the prompt path.
    async fn dispatch_builtin_command(&mut self, name: &str, args: &str) -> Option<ActionOutcome> {
        use super::state::SelectorKind;
        let outcome = match name {
            "quit" => ActionOutcome::Exit,
            "debug" => self.handle_debug_command(),
            "theme" => self.open_selector(SelectorKind::Theme).await,
            "settings" => self.open_selector(SelectorKind::Settings).await,
            "model" => self.open_selector(SelectorKind::Model).await,
            "scoped-models" => self.open_selector(SelectorKind::ScopedModels).await,
            "export" => self.handle_export_command(args).await,
            "import" => self.handle_import_command(args),
            "share" => self.handle_share_command().await,
            "copy" => self.copy_last_assistant().await,
            "name" => self.handle_name_command(args).await,
            "session" => self.handle_session_command().await,
            "changelog" => self.handle_changelog_command(),
            "hotkeys" => self.handle_hotkeys_command(),
            "fork" => self.open_selector(SelectorKind::Fork).await,
            "clone" => self.replace_session(SessionReplacement::Clone).await,
            "tree" => self.open_selector(SelectorKind::Tree).await,
            "trust" => self.open_selector(SelectorKind::Trust).await,
            "login" => self.open_selector(SelectorKind::Auth).await,
            "logout" => self.handle_logout_command().await,
            "new" => self.replace_session(SessionReplacement::New).await,
            "compact" => {
                let instructions = (!args.is_empty()).then_some(args);
                self.record_err(self.session.compact(instructions).await);
                ActionOutcome::None
            }
            "resume" => self.open_selector(SelectorKind::Session).await,
            "reload" => self.handle_reload_action().await,
            _ => return None,
        };
        Some(outcome)
    }

    /// `/export [path]`: JSONL when the path ends in `.jsonl`, else HTML.
    async fn handle_export_command(&mut self, args: &str) -> ActionOutcome {
        let path = parse_path_argument(args);
        // Case-sensitive `.jsonl` suffix, matching upstream `endsWith(".jsonl")`.
        #[allow(clippy::case_sensitive_file_extension_comparisons)]
        let is_jsonl = path
            .as_deref()
            .is_some_and(|value| value.ends_with(".jsonl"));
        let result = if is_jsonl {
            self.session.export_jsonl(path.as_deref()).await
        } else {
            self.session.export_html(path.as_deref()).await
        };
        match result {
            Ok(written) => self.push_notice("export", format!("Session exported to: {written}")),
            Err(error) => {
                self.push_notice("export", format!("Failed to export session: {error}"));
            }
        }
        ActionOutcome::Repaint
    }

    /// `/import <path>`: replace the current session from a JSONL file.
    fn handle_import_command(&mut self, args: &str) -> ActionOutcome {
        let Some(path) = parse_path_argument(args) else {
            self.push_notice("import", "Usage: /import <path.jsonl>".to_owned());
            return ActionOutcome::Repaint;
        };
        self.open_import_confirm(path)
    }

    /// Reset extension-owned UI state to defaults (ports `resetExtensionUI`).
    ///
    /// Clears extension working-message/visibility overrides, extension footer
    /// statuses, and restores the hidden-thinking label so extension state does
    /// not bleed across a session replacement (`/new`, `/fork`, `/clone`,
    /// `/import`, `/resume`) or `/reload`.
    fn reset_extension_ui(&mut self) {
        self.view.footer.extension_statuses.clear();
        self.view.working_message = None;
        self.view.working_visible = true;
        if let Some(status) = self.view.status.as_mut()
            && status.kind == StatusKind::Working
        {
            DEFAULT_WORKING_MESSAGE.clone_into(&mut status.message);
        }
        for message in &mut self.view.messages {
            if let MessageView::Assistant(view) = message {
                "Thinking…".clone_into(&mut view.hidden_thinking_label);
            }
        }
        self.chat_dirty = true;
    }

    /// `/logout`: list stored credentials and open the removal selector, or
    /// notice when none are stored (ports `showOAuthSelector("logout")`).
    async fn handle_logout_command(&mut self) -> ActionOutcome {
        let options = match self.session.logout_provider_options().await {
            Ok(options) => options,
            Err(error) => {
                self.last_error = Some(error);
                return ActionOutcome::Repaint;
            }
        };
        if options.is_empty() {
            self.push_notice(
                "logout",
                "No stored credentials to remove. /logout only removes credentials saved by /login; environment variables and models.json config are unchanged.".to_owned(),
            );
            return ActionOutcome::Repaint;
        }
        let items = options
            .iter()
            .map(|option| {
                pi_tui::components::SelectItem::new(option.id.clone(), option.name.clone())
            })
            .collect();
        self.logout_options = options;
        self.install_confirm_selector(
            super::state::SelectorKind::Logout,
            "Select a credential to remove",
            items,
        );
        ActionOutcome::Repaint
    }

    /// Apply a `/logout` selection: remove the chosen credential and report
    /// (message wording per credential kind, ports the OAuth-selector callback).
    async fn handle_logout_confirm(&mut self, provider_id: &str) -> ActionOutcome {
        let option = self
            .logout_options
            .iter()
            .find(|option| option.id == provider_id)
            .cloned();
        self.logout_options.clear();
        self.close_selector();
        let Some(option) = option else {
            return ActionOutcome::Repaint;
        };
        match self.session.logout(&option.id).await {
            Ok(()) => {
                let message = if option.is_oauth {
                    format!("Logged out of {}", option.name)
                } else {
                    format!(
                        "Removed stored API key for {}. Environment variables and models.json config are unchanged.",
                        option.name
                    )
                };
                self.push_notice("logout", message);
            }
            Err(error) => self.push_notice("logout", format!("Logout failed: {error}")),
        }
        ActionOutcome::Repaint
    }

    /// Show the `/import` replace-session confirmation (ports upstream
    /// `showExtensionConfirm("Import session", ...)`).
    fn open_import_confirm(&mut self, path: String) -> ActionOutcome {
        let prompt = format!("Import session — Replace current session with {path}?");
        self.pending_import = Some(PendingImport {
            path,
            retry_cwd: None,
        });
        self.install_confirm_selector(
            super::state::SelectorKind::ImportConfirm,
            &prompt,
            Self::confirm_items(),
        );
        ActionOutcome::Repaint
    }

    /// Resolve the `/import` replace confirmation: `"true"` runs the import,
    /// anything else cancels.
    async fn handle_import_confirm(&mut self, value: &str) -> ActionOutcome {
        self.close_selector();
        let Some(pending) = self.pending_import.take() else {
            return ActionOutcome::Repaint;
        };
        if value != "true" {
            self.push_notice("import", "Import cancelled".to_owned());
            return ActionOutcome::Repaint;
        }
        self.run_import(pending.path, None).await
    }

    /// Resolve the missing-cwd retry confirmation: `"true"` retries the import
    /// in the fallback cwd (ports `promptForMissingSessionCwd`).
    async fn handle_import_cwd_confirm(&mut self, value: &str) -> ActionOutcome {
        self.close_selector();
        let Some(pending) = self.pending_import.take() else {
            return ActionOutcome::Repaint;
        };
        if value != "true" {
            self.push_notice("import", "Import cancelled".to_owned());
            return ActionOutcome::Repaint;
        }
        self.run_import(pending.path, pending.retry_cwd).await
    }

    /// Perform the import and project the outcome; on missing-cwd, open the
    /// fallback-cwd retry confirmation.
    async fn run_import(&mut self, path: String, cwd_override: Option<String>) -> ActionOutcome {
        match self
            .session
            .import_jsonl(&path, cwd_override.as_deref())
            .await
        {
            Ok(true) => {
                self.rebind_session_channels().await;
                self.refresh_footer().await;
                self.push_notice("import", format!("Session imported from: {path}"));
            }
            Ok(false) => {
                self.push_notice("import", "Import cancelled".to_owned());
            }
            Err(ImportError::MissingCwd { fallback_cwd }) => {
                let prompt =
                    format!("Session cwd not found — continue in current cwd {fallback_cwd}?");
                self.pending_import = Some(PendingImport {
                    path,
                    retry_cwd: Some(fallback_cwd),
                });
                self.install_confirm_selector(
                    super::state::SelectorKind::ImportCwdConfirm,
                    &prompt,
                    Self::confirm_items(),
                );
            }
            Err(error) => {
                self.push_notice(
                    "import",
                    format!("Failed to import session: {}", error.message()),
                );
            }
        }
        ActionOutcome::Repaint
    }

    /// Install a built-in confirm/logout selector: save the editor placeholder,
    /// show the prompt, and route confirms/cancels through the selector channel
    /// under `kind`.
    fn install_confirm_selector(
        &mut self,
        kind: super::state::SelectorKind,
        prompt: &str,
        items: Vec<pi_tui::components::SelectItem>,
    ) {
        if self.confirm_saved_placeholder.is_none() {
            self.confirm_saved_placeholder = Some(self.view.editor.placeholder.clone());
        }
        prompt.clone_into(&mut self.view.editor.placeholder);
        self.active_selector = Some(self.build_select_list(kind, items));
        self.active_selector_kind = Some(kind);
        self.view.focus = FocusArea::Selector;
        self.view.overlay = None;
        self.view.extension_overlay_slot = None;
        self.input_state.reset_taps();
    }

    /// Yes/No items for a built-in confirm selector (`"true"`/`"false"` values).
    fn confirm_items() -> Vec<pi_tui::components::SelectItem> {
        vec![
            pi_tui::components::SelectItem::new("true", "Yes"),
            pi_tui::components::SelectItem::new("false", "No"),
        ]
    }

    /// `/share`: export to a temp HTML and upload it as a secret gist.
    async fn handle_share_command(&mut self) -> ActionOutcome {
        match self.session.share().await {
            Ok((viewer_url, gist_url)) => {
                self.push_notice(
                    "share",
                    format!("Share URL: {viewer_url}\nGist: {gist_url}"),
                );
            }
            Err(error) => self.push_notice("share", format!("Failed to create gist: {error}")),
        }
        ActionOutcome::Repaint
    }

    /// `/name [text]`: set the session display name, or show the current one.
    async fn handle_name_command(&mut self, args: &str) -> ActionOutcome {
        let name = args.trim();
        if name.is_empty() {
            match self.view.footer.session_name.clone() {
                Some(current) => self.push_notice("name", format!("Session name: {current}")),
                None => self.push_notice("name", "Usage: /name <name>".to_owned()),
            }
            return ActionOutcome::Repaint;
        }
        match self.session.set_session_name(name).await {
            Ok(stored) => {
                let stored_name = stored.clone().unwrap_or_else(|| name.to_owned());
                if stored.as_deref() != Some(name) {
                    self.push_notice(
                        "name",
                        format!("Session name was normalized from {name:?} to {stored_name:?}"),
                    );
                }
                self.push_notice("name", format!("Session name set: {stored_name}"));
            }
            Err(error) => {
                self.push_notice("name", format!("Failed to set session name: {error}"));
            }
        }
        ActionOutcome::Repaint
    }

    /// `/session`: show session info and stats.
    async fn handle_session_command(&mut self) -> ActionOutcome {
        let stats = self.session.session_stats().await;
        let name = self.view.footer.session_name.clone();
        self.push_notice("session", format_session_info(&stats, name.as_deref()));
        ActionOutcome::Repaint
    }

    /// `/changelog`: open the release-notes overlay from `CHANGELOG.md`.
    fn handle_changelog_command(&mut self) -> ActionOutcome {
        use crate::core::config::get_changelog_path;
        use crate::core::update::changelog::{normalize_changelog_links, parse_changelog};
        let entries = parse_changelog(&get_changelog_path());
        let markdown = if entries.is_empty() {
            "No changelog entries found.".to_owned()
        } else {
            entries
                .iter()
                .rev()
                .map(|entry| normalize_changelog_links(&entry.content, &entry.version()))
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        self.view.overlay = Some(Overlay {
            kind: OverlayKind::Changelog,
            lines: markdown.lines().map(str::to_owned).collect(),
            height: 1,
        });
        self.view.extension_overlay_slot = None;
        self.view.focus = FocusArea::Overlay;
        self.input_state.reset_taps();
        ActionOutcome::Repaint
    }

    /// `/hotkeys`: open the keyboard-shortcut overlay.
    fn handle_hotkeys_command(&mut self) -> ActionOutcome {
        self.open_overlay(OverlayKind::ShortcutHelp)
    }

    /// Append a local command-result message to the transcript.
    fn push_notice(&mut self, label: &str, text: String) {
        self.view
            .messages
            .push(MessageView::Custom(super::messages::CustomMessageView {
                custom_type: label.to_owned(),
                text,
            }));
        self.chat_dirty = true;
    }

    async fn submit_slash_command(&mut self, name: String, args: String) -> ActionOutcome {
        if let Some(outcome) = self.dispatch_builtin_command(&name, &args).await {
            return outcome;
        }
        if let Some(runner) = self.extension_runner.as_ref()
            && runner
                .registry()
                .commands()
                .iter()
                .any(|command| command.name == name)
        {
            let runner = Arc::clone(runner);
            let result_tx = self.extension_action_tx.clone();
            tokio::spawn(async move {
                let result = runner
                    .execute_command(&name, &args)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string());
                let _ = result_tx.send(result);
            });
            return ActionOutcome::Repaint;
        }

        let command = if args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {args}")
        };
        self.submit_text(command, false).await
    }

    async fn dispatch_bash(&mut self, command: &str, exclude_from_context: bool) -> ActionOutcome {
        if !self
            .enqueue_bash(command.to_owned(), exclude_from_context)
            .await
        {
            self.last_error = Some("a bash command is already running".to_owned());
            return ActionOutcome::Repaint;
        }
        self.view.editor.border = EditorBorder::Bash;
        ActionOutcome::Repaint
    }

    async fn dispatch_interrupt(&mut self) -> ActionOutcome {
        self.set_status(SessionStatus {
            kind: StatusKind::Working,
            frame: 0,
            elapsed_secs: 0,
            message: "Aborting…".to_owned(),
        });
        self.record_err(self.session.abort().await);
        self.refresh_footer().await;
        ActionOutcome::Repaint
    }

    fn clear_editor(&mut self) -> ActionOutcome {
        self.editor.set_text("");
        self.view.editor.text.clear();
        self.view.editor.cursor = 0;
        ActionOutcome::Repaint
    }

    fn toggle_shortcut_help(&mut self) -> ActionOutcome {
        if self.view.overlay.is_some() {
            self.view.overlay = None;
            self.view.focus = FocusArea::Editor;
        } else {
            self.view.overlay = Some(Overlay {
                kind: OverlayKind::ShortcutHelp,
                lines: Vec::new(),
                height: 1,
            });
            self.view.extension_overlay_slot = None;
            self.view.focus = FocusArea::Overlay;
        }
        self.view.extension_overlay_slot = None;
        self.input_state.reset_taps();
        ActionOutcome::Repaint
    }

    fn paste_text(&mut self, text: &str) -> ActionOutcome {
        if text.is_empty() {
            return ActionOutcome::None;
        }
        self.editor.insert_text_at_cursor(text);
        self.view.editor.text = self.editor.get_text();
        ActionOutcome::Repaint
    }

    async fn queue_follow_up(&mut self, text: String) -> ActionOutcome {
        self.record_err(self.session.follow_up(&text).await);
        self.view.pending.follow_up.push(PendingMessage {
            kind: PendingKind::FollowUp,
            text,
        });
        ActionOutcome::Repaint
    }

    fn dequeue_follow_up(&mut self) -> ActionOutcome {
        let Some(message) = self.view.pending.follow_up.pop() else {
            return ActionOutcome::None;
        };
        self.editor.set_text(&message.text);
        self.view.editor.text = self.editor.get_text();
        ActionOutcome::Repaint
    }

    async fn copy_last_assistant(&mut self) -> ActionOutcome {
        match self.session.last_assistant_text().await {
            Ok(Some(text)) if !text.is_empty() => {
                if crate::core::platform::clipboard::copy_to_clipboard(&text).is_ok() {
                    self.set_status(SessionStatus {
                        kind: StatusKind::Working,
                        frame: 0,
                        elapsed_secs: 0,
                        message: "Copied last assistant message".to_owned(),
                    });
                } else {
                    self.last_error = Some("Failed to copy to clipboard".to_owned());
                }
            }
            Ok(_) => self.set_status(SessionStatus {
                kind: StatusKind::Working,
                frame: 0,
                elapsed_secs: 0,
                message: "No assistant text to copy".to_owned(),
            }),
            Err(error) => self.last_error = Some(error),
        }
        ActionOutcome::Repaint
    }

    fn dismiss_overlay(&mut self) -> ActionOutcome {
        self.view.overlay = None;
        self.view.extension_overlay_slot = None;
        self.view.focus = FocusArea::Editor;
        self.input_state.reset_taps();
        ActionOutcome::Repaint
    }

    async fn replace_session(&mut self, replacement: SessionReplacement) -> ActionOutcome {
        self.quiesce_prompt_operations().await;
        if self.pending_extension_dialog.is_some() {
            self.cancel_extension_dialog().await;
        }
        match replacement {
            SessionReplacement::New => match self.session.new_session().await {
                Ok(outcome) if !outcome.cancelled => {
                    self.rebind_session_channels().await;
                    self.refresh_footer().await;
                    self.push_notice("new", "✓ New session started".to_owned());
                }
                Ok(_) => {}
                Err(error) => self.record_err(Err(error)),
            },
            SessionReplacement::Fork => match self.session.fork("").await {
                Ok(outcome) if !outcome.cancelled => {
                    self.rebind_session_channels().await;
                    self.refresh_footer().await;
                    self.push_notice("fork", "Forked to new session".to_owned());
                }
                Ok(_) => {}
                Err(error) => self.record_err(Err(error)),
            },
            SessionReplacement::Clone => match <S as SessionHost>::clone(&self.session).await {
                Ok(CloneOutcome::Cloned) => {
                    self.rebind_session_channels().await;
                    self.refresh_footer().await;
                    self.push_notice("clone", "Cloned to new session".to_owned());
                }
                Ok(CloneOutcome::NothingToClone) => {
                    self.push_notice("clone", "Nothing to clone yet".to_owned());
                }
                Ok(CloneOutcome::Cancelled) => {}
                Err(error) => self.record_err(Err(error)),
            },
        }
        ActionOutcome::Repaint
    }

    async fn submit_text(&mut self, text: String, force_follow_up: bool) -> ActionOutcome {
        if let Some(dialog) = self.pending_extension_dialog.as_ref() {
            match &dialog.request {
                HostUiRequest::Input { id, .. } => {
                    let response = HostUiResponse::Input {
                        id: *id,
                        value: Some(text),
                    };
                    self.finish_extension_dialog(response).await;
                    return ActionOutcome::Repaint;
                }
                HostUiRequest::Editor { id, .. } => {
                    let response = HostUiResponse::Editor {
                        id: *id,
                        value: Some(text),
                    };
                    self.finish_extension_dialog(response).await;
                    return ActionOutcome::Repaint;
                }
                HostUiRequest::Select { .. } | HostUiRequest::Confirm { .. } => {}
            }
        }
        let trimmed = text.trim().to_owned();
        if trimmed.is_empty() {
            return ActionOutcome::None;
        }
        if let Some((name, args)) = parse_slash_command(&trimmed)
            && let Some(outcome) = self.dispatch_builtin_command(name, args).await
        {
            return outcome;
        }

        // `!`/`!!` bash prefix routes directly to execute_bash.
        if let Some(stripped) = trimmed.strip_prefix("!!") {
            let cmd = stripped.trim().to_owned();
            if !cmd.is_empty() {
                return self.dispatch_bash(&cmd, true).await;
            }
        } else if let Some(stripped) = trimmed.strip_prefix('!') {
            let cmd = stripped.trim().to_owned();
            if !cmd.is_empty() {
                return self.dispatch_bash(&cmd, false).await;
            }
        }

        let is_slash = trimmed.starts_with('/');
        let snapshot = self.session.snapshot();
        // Always go through prompt so extension-command dispatch and input
        // transforms run before any steering / follow-up queueing.
        let opts = if snapshot.is_streaming() && !is_slash {
            PromptOptions {
                streaming_behavior: Some(if force_follow_up {
                    StreamingBehavior::FollowUp
                } else {
                    StreamingBehavior::Steer
                }),
                ..PromptOptions::default()
            }
        } else if force_follow_up {
            PromptOptions {
                streaming_behavior: Some(StreamingBehavior::FollowUp),
                ..PromptOptions::default()
            }
        } else {
            PromptOptions::default()
        };
        self.enqueue_prompt(trimmed, opts).await;
        ActionOutcome::None
    }

    /// Enqueue a prompt without holding the UI loop for the full agent turn.
    ///
    /// Admission polls the prompt exactly once before returning. This preserves
    /// submit order and lets a rapid second submit observe the first prompt's
    /// streaming/preflight state, while all later polling belongs to the task.
    async fn enqueue_prompt(&mut self, text: String, opts: PromptOptions) {
        let id = self.prompt_operations.next_id;
        self.prompt_operations.next_id = id.wrapping_add(1);
        let epoch = self.prompt_operations.epoch;
        let session = Arc::clone(&self.session);
        let abort = self.session.abort();
        let (abort_tx, mut abort_rx) = oneshot::channel();
        let (admitted_tx, admitted_rx) = oneshot::channel();

        self.prompt_operations.tasks.spawn(async move {
            let mut prompt = session.prompt(&text, opts);
            let first_poll = poll_fn(|cx| {
                Poll::Ready(match prompt.as_mut().poll(cx) {
                    Poll::Ready(result) => Some(result),
                    Poll::Pending => None,
                })
            })
            .await;
            let _ = admitted_tx.send(());

            let result = if let Some(result) = first_poll {
                result
            } else {
                tokio::select! {
                    result = &mut prompt => result,
                    _ = &mut abort_rx => {
                        let abort_result = abort.await;
                        let prompt_result = prompt.await;
                        prompt_result.and(abort_result)
                    }
                }
            };
            PromptCompletion {
                id,
                epoch,
                kind: SessionOperationKind::Prompt,
                result,
            }
        });
        self.prompt_operations.aborts.insert(id, abort_tx);

        // This waits only for one poll (preflight admission), never for the
        // provider stream or AgentSettled cleanup.
        let _ = admitted_rx.await;
    }

    async fn enqueue_bash(&mut self, command: String, exclude_from_context: bool) -> bool {
        if self.prompt_operations.bash_operation.is_some() {
            return false;
        }
        let id = self.prompt_operations.next_id;
        self.prompt_operations.next_id = id.wrapping_add(1);
        let epoch = self.prompt_operations.epoch;
        let session = Arc::clone(&self.session);
        let abort = self.session.abort();
        let (abort_tx, mut abort_rx) = oneshot::channel();
        let (admitted_tx, admitted_rx) = oneshot::channel();

        self.prompt_operations.tasks.spawn(async move {
            let mut execution = session.execute_bash(&command, exclude_from_context);
            let first_poll = poll_fn(|cx| {
                Poll::Ready(match execution.as_mut().poll(cx) {
                    Poll::Ready(result) => Some(result),
                    Poll::Pending => None,
                })
            })
            .await;
            let _ = admitted_tx.send(());
            let result = if let Some(result) = first_poll {
                result
            } else {
                tokio::select! {
                    result = &mut execution => result,
                    _ = &mut abort_rx => {
                        let abort_result = abort.await;
                        let execution_result = execution.await;
                        execution_result.and(abort_result)
                    }
                }
            };
            PromptCompletion {
                id,
                epoch,
                kind: SessionOperationKind::Bash,
                result,
            }
        });
        self.prompt_operations.aborts.insert(id, abort_tx);
        self.prompt_operations.bash_operation = Some(id);
        let _ = admitted_rx.await;
        true
    }

    fn handle_prompt_completion(
        &mut self,
        completion: Result<PromptCompletion, JoinError>,
    ) -> bool {
        match completion {
            Ok(completion) => {
                self.prompt_operations.aborts.remove(&completion.id);
                if completion.kind == SessionOperationKind::Bash {
                    self.prompt_operations.bash_operation = None;
                }
                if completion.epoch != self.prompt_operations.epoch {
                    return false;
                }
                let refresh_footer = completion.kind == SessionOperationKind::Bash;
                self.record_err(completion.result);
                refresh_footer
            }
            Err(error) => {
                self.prompt_operations
                    .aborts
                    .retain(|_, abort| !abort.is_closed());
                if self
                    .prompt_operations
                    .bash_operation
                    .is_some_and(|id| !self.prompt_operations.aborts.contains_key(&id))
                {
                    self.prompt_operations.bash_operation = None;
                }
                if !error.is_cancelled() {
                    self.record_err(Err(format!("session operation failed: {error}")));
                }
                false
            }
        }
    }

    /// Abort every session operation against the session it captured, then
    /// await its cleanup before session replacement or runtime exit.
    async fn quiesce_prompt_operations(&mut self) {
        self.prompt_operations.epoch = self.prompt_operations.epoch.wrapping_add(1);
        for (_, abort) in std::mem::take(&mut self.prompt_operations.aborts) {
            let _ = abort.send(());
        }
        self.prompt_operations.bash_operation = None;
        while self.prompt_operations.tasks.join_next().await.is_some() {}
    }

    async fn handle_select_confirmed(
        &mut self,
        selector: super::state::SelectorKind,
        value: String,
    ) -> ActionOutcome {
        match selector {
            super::state::SelectorKind::Model
            | super::state::SelectorKind::Tree
            | super::state::SelectorKind::Trust
            | super::state::SelectorKind::Settings
            | super::state::SelectorKind::Config
            | super::state::SelectorKind::ScopedModels
            | super::state::SelectorKind::Auth => {
                self.close_selector();
                ActionOutcome::Repaint
            }
            super::state::SelectorKind::Theme => {
                self.theme_preview_restore = None;
                let storage = super::theme::theme_selection_to_storage(&value);
                let (_, mode) = self.session.theme_settings();
                self.record_err(self.session.persist_theme(&storage, mode));
                self.apply_theme_from_settings();
                self.close_selector();
                self.push_theme_to_host().await;
                ActionOutcome::Repaint
            }
            super::state::SelectorKind::Session => {
                self.quiesce_prompt_operations().await;
                if self.pending_extension_dialog.is_some() {
                    self.cancel_extension_dialog().await;
                }
                match self.session.switch_session(&value).await {
                    Ok(outcome) if !outcome.cancelled => {
                        self.rebind_session_channels().await;
                        self.refresh_footer().await;
                        self.push_notice("resume", "Resumed session".to_owned());
                    }
                    Ok(_) => {}
                    Err(error) => self.record_err(Err(error)),
                }
                self.close_selector();
                ActionOutcome::Repaint
            }
            super::state::SelectorKind::Fork => {
                self.quiesce_prompt_operations().await;
                if self.pending_extension_dialog.is_some() {
                    self.cancel_extension_dialog().await;
                }
                match self.session.fork(&value).await {
                    Ok(outcome) if !outcome.cancelled => {
                        if let Some(text) = outcome.selected_text {
                            self.editor.set_text(&text);
                            self.view.editor.text = self.editor.get_text();
                        }
                        self.rebind_session_channels().await;
                        self.refresh_footer().await;
                        self.push_notice("fork", "Forked to new session".to_owned());
                    }
                    Ok(_) => {}
                    Err(error) => self.record_err(Err(error)),
                }
                self.close_selector();
                ActionOutcome::Repaint
            }
            super::state::SelectorKind::ImportConfirm => self.handle_import_confirm(&value).await,
            super::state::SelectorKind::ImportCwdConfirm => {
                self.handle_import_cwd_confirm(&value).await
            }
            super::state::SelectorKind::Logout => self.handle_logout_confirm(&value).await,
        }
    }

    fn close_selector(&mut self) {
        if let Some(placeholder) = self.confirm_saved_placeholder.take() {
            self.view.editor.placeholder = placeholder;
        }
        self.view.overlay = None;
        self.view.extension_overlay_slot = None;
        self.active_selector = None;
        self.active_selector_kind = None;
        self.view.focus = FocusArea::Editor;
        self.input_state.reset_taps();
    }

    /// Coalesce consecutive resize events into a single [`Txn::Reanchor`].
    /// Non-resize events queued during the storm are pushed back onto the
    /// channel so they redeliver on the next loop turn.
    fn handle_resize(&mut self, width: u16, height: u16) -> ActionOutcome {
        self.tui.note_resize(width, height);
        self.view.resize(width, height);

        // Drain queued events. Only Resize events coalesce; everything else
        // is preserved in `pending_ui_reinject` for the next loop iteration
        // (in arrival order — the loop pops from the back, so we push in
        // reverse).
        let mut preserved: Vec<UiEvent> = Vec::new();
        while let Ok(next) = self.input.receiver_mut().try_recv() {
            match next {
                UiEvent::Resize { width, height } => {
                    self.tui.note_resize(width, height);
                    self.view.resize(width, height);
                }
                other => preserved.push(other),
            }
        }
        for event in preserved.into_iter().rev() {
            self.pending_ui_reinject.push(event);
        }

        let result = self.commit_reanchor();
        if result.is_err() {
            self.exited = true;
            self.exit_kind = InteractiveExit::IoFailure;
        }
        ActionOutcome::Repaint
    }

    fn open_overlay(&mut self, kind: OverlayKind) -> ActionOutcome {
        self.view.overlay = Some(Overlay {
            kind,
            lines: Vec::new(),
            height: 1,
        });
        self.view.extension_overlay_slot = None;
        self.view.focus = FocusArea::Overlay;
        self.input_state.reset_taps();
        ActionOutcome::Repaint
    }

    async fn open_selector(&mut self, kind: super::state::SelectorKind) -> ActionOutcome {
        match self.load_selector_component(kind).await {
            Ok(component) => {
                self.active_selector = Some(component);
                self.active_selector_kind = Some(kind);
                self.view.focus = FocusArea::Selector;
                self.view.overlay = None;
                self.view.extension_overlay_slot = None;
                self.input_state.reset_taps();
            }
            Err(error) => self.last_error = Some(error),
        }
        ActionOutcome::Repaint
    }

    // Large exhaustive selector dispatch; adding a variant tips the line count.
    #[allow(clippy::too_many_lines)]
    async fn load_selector_component(
        &mut self,
        kind: super::state::SelectorKind,
    ) -> Result<Box<dyn Component>, String> {
        use pi_tui::components::SelectItem;

        match kind {
            super::state::SelectorKind::Model => {
                let entries = self.session.get_model_entries().await?;
                let items = entries
                    .into_iter()
                    .map(|entry| {
                        SelectItem::new(entry.value, entry.label)
                            .with_description(entry.description.unwrap_or_default())
                    })
                    .collect();
                Ok(self.build_select_list(kind, items))
            }
            super::state::SelectorKind::Session => {
                let entries = self.session.get_session_entries().await?;
                let items = entries
                    .into_iter()
                    .map(|entry| {
                        SelectItem::new(entry.value, entry.label)
                            .with_description(entry.description.unwrap_or_default())
                    })
                    .collect();
                Ok(self.build_select_list(kind, items))
            }
            super::state::SelectorKind::Tree => {
                let entries = self.session.get_tree_entries().await?;
                Ok(self.build_tree_select_list(kind, entries))
            }
            super::state::SelectorKind::Fork => {
                let entries = self.session.get_fork_entries().await?;
                Ok(self.build_tree_select_list(kind, entries))
            }
            super::state::SelectorKind::Auth => {
                let entries = self.session.get_auth_entries().await?;
                let items = entries
                    .into_iter()
                    .map(|entry| {
                        SelectItem::new(entry.value, entry.label)
                            .with_description(entry.description.unwrap_or_default())
                    })
                    .collect();
                Ok(self.build_select_list(kind, items))
            }
            super::state::SelectorKind::ScopedModels => {
                let (entries, enabled) = self.session.get_scoped_models_entries().await?;
                let items = entries
                    .into_iter()
                    .map(|entry| {
                        let mark = if enabled.get(&entry.value).copied().unwrap_or(false) {
                            "[x]"
                        } else {
                            "[ ]"
                        };
                        SelectItem::new(entry.value, format!("{mark} {}", entry.label))
                            .with_description(entry.description.unwrap_or_default())
                    })
                    .collect();
                Ok(self.build_select_list(kind, items))
            }
            super::state::SelectorKind::Trust => {
                let rows = self.session.get_trust_entries().await?;
                Ok(self.build_settings_list(kind, rows))
            }
            super::state::SelectorKind::Settings => {
                let rows = self.session.get_settings_entries().await?;
                Ok(self.build_settings_list(kind, rows))
            }
            super::state::SelectorKind::Config => {
                let rows = self.session.get_config_entries().await?;
                Ok(self.build_settings_list(kind, rows))
            }
            super::state::SelectorKind::Theme => {
                self.theme_preview_restore = Some(self.view.theme.clone());
                let items = super::theme::theme_selector_values()
                    .into_iter()
                    .map(|value| SelectItem::new(value.clone(), value))
                    .collect();
                let mut list = pi_tui::components::SelectList::new(
                    items,
                    super::selectors::SELECTOR_MAX_VISIBLE,
                    super::theme::select_list_theme(),
                );
                list.set_selected_index(0);
                let select_tx = self.select_tx.clone();
                list.on_select = Some(Box::new(move |item| {
                    let _ = select_tx.send((kind, item.value.clone()));
                }));
                let cancel_tx = self.cancel_tx.clone();
                list.on_cancel = Some(Box::new(move || {
                    let _ = cancel_tx.send(());
                }));
                let preview_tx = self.theme_preview_tx.clone();
                list.on_selection_change = Some(Box::new(move |item| {
                    let _ = preview_tx.send(item.value.clone());
                }));
                Ok(Box::new(list))
            }
            super::state::SelectorKind::ImportConfirm
            | super::state::SelectorKind::ImportCwdConfirm
            | super::state::SelectorKind::Logout => {
                Err("confirm/logout selectors are installed by their command handlers".to_owned())
            }
        }
    }

    fn build_select_list(
        &self,
        kind: super::state::SelectorKind,
        items: Vec<pi_tui::components::SelectItem>,
    ) -> Box<dyn Component> {
        let mut list = pi_tui::components::SelectList::new(
            items,
            super::selectors::SELECTOR_MAX_VISIBLE,
            super::theme::select_list_theme(),
        );
        list.set_selected_index(0);
        let select_tx = self.select_tx.clone();
        list.on_select = Some(Box::new(move |item| {
            let _ = select_tx.send((kind, item.value.clone()));
        }));
        let cancel_tx = self.cancel_tx.clone();
        list.on_cancel = Some(Box::new(move || {
            let _ = cancel_tx.send(());
        }));
        Box::new(list)
    }

    fn build_tree_select_list(
        &self,
        kind: super::state::SelectorKind,
        entries: Vec<super::state::TreeEntry>,
    ) -> Box<dyn Component> {
        let items = entries
            .into_iter()
            .map(|entry| {
                let label = format!("{}{}", "  ".repeat(entry.depth), entry.label);
                pi_tui::components::SelectItem::new(entry.value, label)
            })
            .collect();
        self.build_select_list(kind, items)
    }

    fn build_settings_list(
        &self,
        _kind: super::state::SelectorKind,
        rows: Vec<super::state::SettingsRow>,
    ) -> Box<dyn Component> {
        let items = rows
            .into_iter()
            .map(|row| pi_tui::components::SettingItem {
                id: row.id,
                label: row.label,
                description: row.description,
                current_value: row.current_value,
                values: row.values,
                submenu: None,
            })
            .collect();
        let change_tx = self.settings_change_tx.clone();
        let cancel_tx = self.cancel_tx.clone();
        Box::new(pi_tui::components::SettingsList::new(
            items,
            super::selectors::SELECTOR_MAX_VISIBLE,
            super::theme::settings_list_theme(),
            move |id: &str, value: &str| {
                let _ = change_tx.send((id.to_owned(), value.to_owned()));
            },
            move || {
                let _ = cancel_tx.send(());
            },
            &pi_tui::components::SettingsListOptions::default(),
        ))
    }

    fn build_extension_select_list(
        &mut self,
        title: &str,
        items: Vec<pi_tui::components::SelectItem>,
    ) -> Box<dyn Component> {
        let mut list = pi_tui::components::SelectList::new(
            items,
            super::selectors::SELECTOR_MAX_VISIBLE,
            super::theme::select_list_theme(),
        );
        list.set_selected_index(0);
        let select_tx = self.extension_select_tx.clone();
        list.on_select = Some(Box::new(move |item| {
            let _ = select_tx.send(item.value.clone());
        }));
        let cancel_tx = self.cancel_tx.clone();
        list.on_cancel = Some(Box::new(move || {
            let _ = cancel_tx.send(());
        }));
        title.clone_into(&mut self.view.editor.placeholder);
        Box::new(list)
    }

    async fn begin_extension_dialog(&mut self, request: HostUiRequest) {
        if self.pending_extension_dialog.is_some() {
            self.cancel_extension_dialog().await;
        }
        let deadline = dialog_timeout(&request).map(|timeout| Instant::now() + timeout);
        let saved_editor_placeholder = self.view.editor.placeholder.clone();
        let mut saved_editor_text = None;
        match &request {
            HostUiRequest::Select { request, .. } => {
                let items = request
                    .options
                    .iter()
                    .map(|option| {
                        pi_tui::components::SelectItem::new(option.clone(), option.clone())
                    })
                    .collect();
                self.active_selector =
                    Some(self.build_extension_select_list(&request.title, items));
                self.active_selector_kind = None;
                self.view.focus = FocusArea::Selector;
            }
            HostUiRequest::Confirm { request, .. } => {
                let items = vec![
                    pi_tui::components::SelectItem::new("true", "Yes")
                        .with_description(request.message.clone()),
                    pi_tui::components::SelectItem::new("false", "No"),
                ];
                self.active_selector =
                    Some(self.build_extension_select_list(&request.title, items));
                self.active_selector_kind = None;
                self.view.focus = FocusArea::Selector;
            }
            HostUiRequest::Input { request, .. } => {
                saved_editor_text = Some(self.editor.get_text());
                self.editor.set_text("");
                self.view.editor.text.clear();
                self.view.editor.placeholder = request
                    .placeholder
                    .clone()
                    .unwrap_or_else(|| request.title.clone());
                self.view.focus = FocusArea::Editor;
            }
            HostUiRequest::Editor { request, .. } => {
                saved_editor_text = Some(self.editor.get_text());
                let prefill = request.prefill.clone().unwrap_or_default();
                self.editor.set_text(&prefill);
                self.view.editor.text = prefill;
                self.view.editor.placeholder.clone_from(&request.title);
                self.view.focus = FocusArea::Editor;
            }
        }
        self.pending_extension_dialog = Some(PendingExtensionDialog {
            request,
            saved_editor_text,
            saved_editor_placeholder,
            deadline,
        });
        self.input_state.reset_taps();
        self.arm_coalescer();
    }

    async fn finish_extension_selection(&mut self, value: String) {
        let Some(dialog) = self.pending_extension_dialog.as_ref() else {
            return;
        };
        let response = match &dialog.request {
            HostUiRequest::Select { id, .. } => HostUiResponse::Select {
                id: *id,
                value: Some(value),
            },
            HostUiRequest::Confirm { id, .. } => HostUiResponse::Confirm {
                id: *id,
                confirmed: value == "true",
            },
            HostUiRequest::Input { .. } | HostUiRequest::Editor { .. } => return,
        };
        self.finish_extension_dialog(response).await;
    }

    async fn cancel_extension_dialog(&mut self) {
        let Some(dialog) = self.pending_extension_dialog.as_ref() else {
            return;
        };
        let response = default_extension_dialog_response(&dialog.request);
        self.finish_extension_dialog(response).await;
    }

    async fn finish_extension_dialog(&mut self, response: HostUiResponse) {
        let dialog = self.pending_extension_dialog.take();
        if let Some(runner) = &self.extension_runner
            && let Err(error) = runner.respond_ui(response).await
        {
            self.last_error = Some(error.to_string());
        }
        if let Some(dialog) = dialog {
            if let Some(saved) = dialog.saved_editor_text {
                self.editor.set_text(&saved);
                self.view.editor.text = saved;
            }
            self.view.editor.placeholder = dialog.saved_editor_placeholder;
        }
        self.close_selector();
        self.arm_coalescer();
    }

    async fn handle_extension_stream_event(&mut self, event: Option<ExtensionUiEvent>) {
        if let Some(event) = event {
            self.handle_extension_event(event).await;
        } else {
            self.extension_events = None;
        }
    }

    async fn handle_extension_event(&mut self, event: ExtensionUiEvent) {
        match event {
            ExtensionUiEvent::Notify(notification) => {
                let severity = match notification.level {
                    NotifyLevel::Info | NotifyLevel::Warning => DiagnosticSeverity::Warning,
                    NotifyLevel::Error => DiagnosticSeverity::Error,
                };
                self.view.diagnostics.entries.push(StartupDiagnostic {
                    severity,
                    source: "extension".to_owned(),
                    message: notification.message,
                });
            }
            ExtensionUiEvent::Slot(slot) => self.project_extension_slot(slot),
            ExtensionUiEvent::Dispose { key } => self.dispose_extension_slot(&key),
            ExtensionUiEvent::ThemeSet(set) => self.handle_extension_theme_set(set).await,
            ExtensionUiEvent::UiControl(control) => {
                self.handle_extension_ui_control(control).await;
            }
        }
        self.arm_coalescer();
    }

    /// Apply a fire-and-forget extension UI control (`ui.setStatus`, …).
    async fn handle_extension_ui_control(&mut self, control: UiControl) {
        match control {
            UiControl::SetStatus { key, text } => match text {
                Some(text) if !text.is_empty() => {
                    self.view.footer.extension_statuses.insert(key, text);
                }
                _ => {
                    self.view.footer.extension_statuses.remove(&key);
                }
            },
            UiControl::SetWorkingMessage { message } => {
                self.view.working_message.clone_from(&message);
                if let Some(status) = self.view.status.as_mut()
                    && status.kind == StatusKind::Working
                {
                    status.message = message.unwrap_or_else(|| DEFAULT_WORKING_MESSAGE.to_owned());
                }
            }
            UiControl::SetWorkingVisible { visible } => {
                self.view.working_visible = visible;
                if visible {
                    // Surface only while streaming; never spuriously while idle.
                    if self.view.streaming
                        && !self
                            .view
                            .status
                            .as_ref()
                            .is_some_and(|status| status.kind == StatusKind::Working)
                    {
                        let message = self
                            .view
                            .working_message
                            .clone()
                            .unwrap_or_else(|| DEFAULT_WORKING_MESSAGE.to_owned());
                        self.set_status(SessionStatus {
                            kind: StatusKind::Working,
                            frame: 0,
                            elapsed_secs: 0,
                            message,
                        });
                    }
                } else if self
                    .view
                    .status
                    .as_ref()
                    .is_some_and(|status| status.kind == StatusKind::Working)
                {
                    self.view.status = None;
                }
            }
            UiControl::SetWorkingIndicator { options } => {
                // Upstream: empty `frames` hides the indicator. Custom frames
                // are not portable to the native braille spinner (ledgered).
                let hide = options
                    .as_ref()
                    .and_then(|value| value.get("frames"))
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(Vec::is_empty);
                if hide
                    && self
                        .view
                        .status
                        .as_ref()
                        .is_some_and(|status| status.kind == StatusKind::Working)
                {
                    self.view.status = None;
                }
            }
            UiControl::SetHiddenThinkingLabel { label } => {
                let label = label.unwrap_or_else(|| "Thinking…".to_owned());
                for message in &mut self.view.messages {
                    if let MessageView::Assistant(view) = message {
                        view.hidden_thinking_label.clone_from(&label);
                    }
                }
                self.chat_dirty = true;
            }
            UiControl::SetTitle { title } => {
                let sequence = encode_osc0_set_title(title.as_deref().unwrap_or(""));
                if let Err(error) = self.tui.outer_mut().write_all(&sequence) {
                    self.last_error = Some(format!("write terminal title: {error}"));
                }
            }
            UiControl::PasteToEditor { text } => {
                let _ = self.paste_text(&text);
            }
            UiControl::SetEditorText { text } => {
                self.editor.set_text(&text);
                self.view.editor.text = text;
            }
            UiControl::SetToolsExpanded { expanded } => {
                self.display.tools_expanded = expanded;
                let _ = self.reapply_display_preferences();
            }
        }
        self.push_ui_state_to_host().await;
    }

    /// Mirror editor text + tool expansion to the extension host.
    async fn push_ui_state_to_host(&mut self) {
        let Some(runner) = self.extension_runner.as_ref() else {
            return;
        };
        let state = UiStateWire {
            editor_text: self.editor.get_expanded_text(),
            tools_expanded: self.display.tools_expanded,
        };
        Arc::clone(runner).push_ui_state(&state).await;
    }

    /// Re-resolve the active theme from settings + detected terminal polarity
    /// and apply it. Called on `/reload` and after settings-driven changes.
    pub(crate) fn apply_theme_from_settings(&mut self) {
        let (raw, mode) = self.session.theme_settings();
        let resolved =
            super::theme::resolve_active_theme(raw.as_deref(), mode, self.terminal_theme);
        self.apply_theme(resolved);
    }

    /// Install `resolved` as the live theme: thread-local current, view theme,
    /// generation bump, and memoized chat-line cache invalidation. No-op when
    /// the theme is unchanged. The event loop flushes the pending host push.
    fn apply_theme(&mut self, resolved: Arc<ResolvedTheme>) {
        if *resolved == *self.view.theme {
            return;
        }
        super::theme::set_current(resolved.clone());
        self.view.theme = resolved;
        self.theme_generation = self.theme_generation.wrapping_add(1);
        self.theme_push_pending = true;
        // Memoized chat lines carry baked-in ANSI colors; drop them all.
        self.chat_prefix_cache = None;
        self.chat_prefix_len = usize::MAX;
        self.chat_tail_cache = None;
        self.chat_dirty = true;
        self.arm_coalescer();
    }

    /// Apply an extension `setTheme` request forwarded by the host.
    ///
    /// String form (`persist == true`): persist the raw setting plus its
    /// inferred polarity mode, then resolve through the theme engine. The
    /// host's failure fallback (`persist == false`) and the `Theme`-object
    /// form apply without persistence (upstream `setThemeInstance`).
    async fn handle_extension_theme_set(&mut self, set: ThemeSet) {
        if let Some(wire) = set.theme {
            self.apply_theme(Arc::new(resolved_theme_from_wire(&wire)));
            self.push_theme_to_host().await;
            return;
        }
        let Some(name) = set.name else {
            return;
        };
        let resolved = if set.persist {
            let mode = theme_mode_for_name(&name);
            if let Err(error) = self.session.persist_theme(&name, mode) {
                self.last_error = Some(error);
            }
            super::theme::resolve_active_theme(Some(&name), mode, self.terminal_theme)
        } else {
            super::theme::load_or_dark(&name, super::theme::ColorMode::Truecolor)
        };
        self.apply_theme(resolved);
        self.push_theme_to_host().await;
    }

    /// Push the active theme, catalog, and generation to the extension host.
    async fn push_theme_to_host(&mut self) {
        self.theme_push_pending = false;
        let Some(runner) = self.extension_runner.as_ref() else {
            return;
        };
        let (_, mode) = self.session.theme_settings();
        let update = build_theme_update(
            &self.view.theme,
            mode,
            self.terminal_theme,
            self.theme_generation,
        );
        Arc::clone(runner).push_theme_update(&update).await;
    }

    /// Apply one settings-row change through the host, then refresh
    /// theme-derived state when the row affects the theme.
    async fn handle_settings_change(&mut self, id: &str, value: &str) {
        if let Err(error) = self.session.apply_settings_change(id, value) {
            self.last_error = Some(error);
            return;
        }
        if matches!(id, "theme" | "themeMode") {
            self.apply_theme_from_settings();
            self.push_theme_to_host().await;
        }
        self.arm_coalescer();
    }

    /// Preview a highlighted `/theme` selection without persisting.
    fn preview_theme_selection(&mut self, selection: &str) {
        let storage = super::theme::theme_selection_to_storage(selection);
        let (_, mode) = self.session.theme_settings();
        let resolved =
            super::theme::resolve_active_theme(Some(&storage), mode, self.terminal_theme);
        self.apply_theme(resolved);
    }

    /// Restore the pre-`/theme` theme after a cancelled selector.
    fn restore_theme_preview(&mut self) {
        if let Some(previous) = self.theme_preview_restore.take() {
            self.apply_theme(previous);
        }
    }

    /// `/debug`: write a support dump (rendered frame + transcript JSONL) and
    /// surface the written path in the transcript.
    fn handle_debug_command(&mut self) -> ActionOutcome {
        use crate::core::platform::debug_dump::{DebugDumpInput, write_debug_dump_with};
        let (width, height) = (self.view.width, self.view.height);
        let buffer = super::view::render_view(&self.view, width, height);
        let rendered_lines = buffer_plain_lines(&buffer, width, height);
        let messages: Vec<String> = self
            .session
            .messages()
            .iter()
            .filter_map(|message| serde_json::to_string(message).ok())
            .collect();
        let timestamp = jiff::Timestamp::now().to_string();
        let input = DebugDumpInput {
            timestamp: &timestamp,
            width,
            height,
            rendered_lines: &rendered_lines,
            messages: &messages,
        };
        let written = write_debug_dump_with(&input, &self.debug_dump_dir);
        match written {
            Ok(path) => {
                self.view
                    .messages
                    .push(MessageView::Custom(super::messages::CustomMessageView {
                        custom_type: "debug".to_owned(),
                        text: format!("✓ Debug log written\n{}", path.display()),
                    }));
                self.chat_dirty = true;
            }
            Err(error) => self.last_error = Some(format!("debug dump failed: {error}")),
        }
        ActionOutcome::Repaint
    }

    // ----- First-run wizard -----

    /// Open the first-time-setup overlay and seed the wizard state.
    fn open_first_run_wizard(&mut self) {
        self.first_run = Some(FirstRunWizardState {
            step: super::startup::FIRST_RUN_STEP_FAMILY,
            selected: 0,
            family: None,
            mode: None,
            pre_theme: self.view.theme.clone(),
        });
        self.view.overlay = Some(Overlay {
            kind: OverlayKind::FirstTimeSetup,
            lines: Vec::new(),
            height: 1,
        });
        self.view.extension_overlay_slot = None;
        self.view.focus = FocusArea::Overlay;
        self.input_state.reset_taps();
        self.sync_first_run_view();
        self.preview_first_run_selection();
    }

    fn first_run_option_count(step: usize) -> usize {
        match step {
            super::startup::FIRST_RUN_STEP_FAMILY => {
                super::startup::first_run_family_options().len()
            }
            super::startup::FIRST_RUN_STEP_MODE => super::startup::first_run_mode_options().len(),
            super::startup::FIRST_RUN_STEP_ANALYTICS => {
                super::startup::first_run_analytics_options().len()
            }
            _ => 0,
        }
    }

    /// Mirror the wizard state into [`ViewState`] for composition.
    fn sync_first_run_view(&mut self) {
        if let Some(state) = self.first_run.as_ref() {
            self.view.first_run_step = Some(state.step);
            self.view.first_run_selected = state.selected;
            self.view.first_run_family = state.family.clone();
            self.view.first_run_mode = state.mode;
        } else {
            self.view.first_run_step = None;
            self.view.first_run_selected = 0;
            self.view.first_run_family = None;
            self.view.first_run_mode = None;
        }
    }

    /// Live-preview the highlighted wizard option (family and mode steps).
    fn preview_first_run_selection(&mut self) {
        let Some(state) = self.first_run.as_ref() else {
            return;
        };
        let family = match state.step {
            super::startup::FIRST_RUN_STEP_FAMILY => super::startup::first_run_family_options()
                .get(state.selected)
                .copied()
                .map(str::to_owned),
            _ => state.family.clone(),
        };
        let mode = match state.step {
            super::startup::FIRST_RUN_STEP_MODE => super::startup::first_run_mode_options()
                .get(state.selected)
                .map(|(mode, _)| *mode),
            _ => state.mode,
        }
        .unwrap_or(ThemeMode::Auto);
        let Some(family) = family else {
            return;
        };
        let storage = super::theme::theme_selection_to_storage(&family);
        let resolved =
            super::theme::resolve_active_theme(Some(&storage), mode, self.terminal_theme);
        self.apply_theme(resolved);
    }

    /// Route a key to the first-run wizard: Up/Down move the highlight (with
    /// live preview), Enter advances/persists, Esc cancels and restores the
    /// pre-wizard theme.
    async fn handle_first_run_key(&mut self, code: crossterm::event::KeyCode) {
        use crossterm::event::KeyCode;
        let Some(mut state) = self.first_run.take() else {
            return;
        };
        let count = Self::first_run_option_count(state.step).max(1);
        match code {
            KeyCode::Up => {
                state.selected = (state.selected + count - 1) % count;
                self.first_run = Some(state);
                self.preview_first_run_selection();
            }
            KeyCode::Down => {
                state.selected = (state.selected + 1) % count;
                self.first_run = Some(state);
                self.preview_first_run_selection();
            }
            KeyCode::Esc => {
                self.apply_theme(state.pre_theme);
                let _ = self.dismiss_overlay();
                self.push_theme_to_host().await;
            }
            KeyCode::Enter => match state.step {
                super::startup::FIRST_RUN_STEP_FAMILY => {
                    state.family = super::startup::first_run_family_options()
                        .get(state.selected)
                        .copied()
                        .map(str::to_owned);
                    state.step = super::startup::FIRST_RUN_STEP_MODE;
                    state.selected = 0;
                    self.first_run = Some(state);
                    self.preview_first_run_selection();
                }
                super::startup::FIRST_RUN_STEP_MODE => {
                    state.mode = super::startup::first_run_mode_options()
                        .get(state.selected)
                        .map(|(mode, _)| *mode);
                    state.step = super::startup::FIRST_RUN_STEP_ANALYTICS;
                    state.selected = 0;
                    self.first_run = Some(state);
                }
                _ => {
                    let share_analytics = super::startup::first_run_analytics_options()
                        .get(state.selected)
                        .is_some_and(|(share, _)| *share);
                    let family = state.family.clone().unwrap_or_else(|| "default".to_owned());
                    let selection = crate::core::platform::first_run::FirstRunSelection {
                        theme: super::theme::theme_selection_to_storage(&family),
                        theme_mode: state.mode.unwrap_or(ThemeMode::Auto),
                        share_analytics,
                    };
                    self.record_err(self.session.persist_first_run(&selection));
                    let _ = self.dismiss_overlay();
                    self.apply_theme_from_settings();
                    self.push_theme_to_host().await;
                }
            },
            _ => {
                self.first_run = Some(state);
            }
        }
        self.sync_first_run_view();
        self.arm_coalescer();
    }

    fn project_extension_slot(&mut self, slot: SanitizedSlot) {
        // Compute whether the previous frame already showed this exact overlay
        // BEFORE dispose wipes `view.extension_overlay_slot`. A republish with
        // a different height/anchor/width reshapes the overlay over rows whose
        // previous content is unrelated, so it must re-anchor just like an open.
        let same_geometry = self
            .view
            .extension_overlay_slot
            .as_ref()
            .is_some_and(|prev| {
                prev.key == slot.key
                    && prev.height == slot.height
                    && prev.overlay_options == slot.overlay_options
            });
        self.dispose_extension_slot(&slot.key);
        let non_capturing = slot
            .overlay_options
            .as_ref()
            .is_some_and(|options| options.non_capturing);
        let captures_focus = slot.focusable && !non_capturing;
        if captures_focus {
            for widget in self
                .view
                .widgets_above
                .iter_mut()
                .chain(self.view.widgets_below.iter_mut())
            {
                widget.focused = false;
            }
        }
        let widget = WidgetSlot {
            slot: slot.clone(),
            focused: captures_focus,
        };
        match slot.placement {
            SlotPlacement::Footer | SlotPlacement::BelowEditor => {
                self.view.widgets_below.push(widget);
            }
            SlotPlacement::Overlay => {
                self.view.overlay = Some(Overlay {
                    kind: OverlayKind::Extension,
                    height: slot.height,
                    lines: Vec::new(),
                });
                self.view.extension_overlay_slot = Some(slot.clone());
                if !same_geometry {
                    // An open OR a reshape covers rows whose previous content
                    // is unrelated; re-anchor so the first frame is not
                    // fragmented by the cell diff (codex PRRT …VM-tM).
                    self.pending_reanchor = Some(ReanchorCause::OverlayOpen);
                }
            }
            SlotPlacement::Header
            | SlotPlacement::AboveEditor
            | SlotPlacement::Editor
            | SlotPlacement::MessageRenderer => self.view.widgets_above.push(widget),
        }
        if captures_focus {
            self.focused_extension_slot = Some(slot.key.clone());
            self.view.focus = if slot.placement == SlotPlacement::Overlay {
                FocusArea::Overlay
            } else {
                FocusArea::Widget
            };
        }
        self.extension_slots.insert(
            slot.key,
            ProjectedExtensionSlot {
                placement: slot.placement,
                generation: slot.generation,
                focusable: captures_focus,
            },
        );
    }

    fn dispose_extension_slot(&mut self, key: &str) {
        self.view.widgets_above.retain(|slot| slot.slot.key != key);
        self.view.widgets_below.retain(|slot| slot.slot.key != key);
        if matches!(
            self.extension_slots.remove(key).map(|slot| slot.placement),
            Some(SlotPlacement::Overlay)
        ) && self
            .view
            .overlay
            .as_ref()
            .is_some_and(|overlay| overlay.kind == OverlayKind::Extension)
        {
            self.view.overlay = None;
            self.view.extension_overlay_slot = None;
            self.view.focus = FocusArea::Editor;
        }
        if self.focused_extension_slot.as_deref() == Some(key) {
            self.focused_extension_slot = None;
            self.view.focus = FocusArea::Editor;
        }
    }

    async fn rebind_extension_channels(&mut self) {
        if self.pending_extension_dialog.is_some() {
            self.cancel_extension_dialog().await;
        }
        self.extension_runner = self.session.host_extension_runner();
        let (registry_changes, shortcuts) =
            subscribe_and_snapshot_shortcuts(self.extension_runner.as_ref());
        self.extension_registry_changes = registry_changes;
        self.effective_extension_shortcuts = shortcuts;
        let current_slots = self
            .extension_runner
            .as_ref()
            .map_or_else(Vec::new, |runner| runner.current_slots());
        self.extension_events = self
            .extension_runner
            .as_ref()
            .map(|runner| runner.subscribe_ui());
        if let Some(runner) = &self.extension_runner {
            if let Some(requests) = runner.take_ui_requests() {
                self.extension_requests = Some(requests);
            }
        } else {
            self.extension_requests = None;
        }
        self.pending_extension_dialog = None;
        self.extension_slots.clear();
        self.focused_extension_slot = None;
        self.view.extension_overlay_slot = None;
        self.view.extension_shortcuts = shortcut_hints(&self.effective_extension_shortcuts);
        self.view.widgets_above.clear();
        self.view.widgets_below.clear();
        if self
            .view
            .overlay
            .as_ref()
            .is_some_and(|overlay| overlay.kind == OverlayKind::Extension)
        {
            self.view.overlay = None;
        }
        for slot in current_slots {
            self.project_extension_slot(slot);
        }
    }

    fn handle_extension_registry_change(&mut self, channel_open: bool) {
        if !channel_open {
            self.extension_registry_changes = None;
            return;
        }
        self.refresh_extension_shortcuts();
        self.arm_coalescer();
    }

    fn refresh_extension_shortcuts(&mut self) {
        self.effective_extension_shortcuts = self
            .extension_runner
            .as_ref()
            .map_or_else(Vec::new, |runner| {
                build_effective_extension_shortcuts(&runner.raw_shortcuts())
            });
        self.view.extension_shortcuts = shortcut_hints(&self.effective_extension_shortcuts);
    }

    fn route_extension_input(&mut self, event: &UiEvent) -> bool {
        if !matches!(event, UiEvent::Key(_) | UiEvent::Paste(_)) {
            return false;
        }
        if let Some(key) = self.focused_extension_slot.clone()
            && let Some(slot) = self.extension_slots.get(&key)
            && slot.focusable
            && let Some(runner) = self.extension_runner.as_ref()
        {
            let request = UiEventRequest {
                key,
                generation: slot.generation,
                event: ui_event_wire(event),
                data: encode_terminal_input(event),
            };
            let runner = Arc::clone(runner);
            let result_tx = self.extension_action_tx.clone();
            tokio::spawn(async move {
                let result = runner
                    .send_ui_event(request)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string());
                let _ = result_tx.send(result);
            });
            return true;
        }

        let UiEvent::Key(key_event) = event else {
            return false;
        };
        if key_event.kind == crossterm::event::KeyEventKind::Release {
            return false;
        }
        let Some(shortcut) = self
            .effective_extension_shortcuts
            .iter()
            .find(|shortcut| key_matches_parsed(key_event, &shortcut.parsed))
        else {
            return false;
        };
        let Some(runner) = self.extension_runner.as_ref() else {
            return false;
        };
        let runner = Arc::clone(runner);
        let key = shortcut.dispatch_key.clone();
        let result_tx = self.extension_action_tx.clone();
        tokio::spawn(async move {
            let result = runner
                .execute_shortcut(key)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string());
            let _ = result_tx.send(result);
        });
        true
    }

    async fn intercept_terminal_input(&mut self, event: UiEvent) -> Option<UiEvent> {
        let Some(runner) = &self.extension_runner else {
            return Some(event);
        };
        if !runner.has_terminal_input_handlers() {
            return Some(event);
        }
        let Some(data) = encode_terminal_input(&event) else {
            return Some(event);
        };
        match runner.terminal_input(&data).await {
            Ok(result) if result.consume => None,
            Ok(result) => result
                .data
                .filter(|rewritten| rewritten != &data)
                .map_or(Some(event), |rewritten| {
                    Some(decode_terminal_input(rewritten))
                }),
            Err(_) => Some(event),
        }
    }

    fn ensure_editor_on_submit(&mut self) {
        if self.editor.on_submit.is_none() {
            let submit_tx = self.submit_tx.clone();
            self.editor.on_submit = Some(Box::new(move |text: String| {
                let _ = submit_tx.send(text);
            }));
        }
    }

    async fn handle_session_rebind_completion(&mut self, generation: u64) {
        if !self.session_rebind_signal.claim(generation) {
            return;
        }
        self.rebind_session_channels().await;
        self.refresh_footer().await;
        self.session_events_closed_for_rebind = false;
    }

    /// Rebind event/partial subscriptions and reload the transcript after a
    /// session replacement. Used by production rebind callback and tests.
    pub async fn rebind_session_channels(&mut self) {
        if self
            .reset_ui_flag
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            self.reset_extension_ui();
        }
        self.events = self.session.subscribe();
        self.partial = self.session.partial_rx();
        let snapshot = self.session.snapshot();
        project_snapshot(&mut self.view, &snapshot, None);
        self.view.messages = project_messages(&self.session.messages());
        apply_display_preferences(
            &mut self.view.messages,
            self.display.tools_expanded,
            self.display.hide_thinking,
        );
        self.chat_prefix_cache = None;
        self.chat_prefix_len = usize::MAX;
        self.chat_tail_cache = None;
        self.chat_dirty = true;
        self.rebind_extension_channels().await;
    }

    async fn refresh_footer(&mut self) {
        let snapshot = self.session.footer_snapshot().await;
        project_footer(&mut self.view, &snapshot);
    }

    /// Clear selector focus before process suspension.
    pub fn close_selector_for_suspend(&mut self) {
        self.close_selector();
        self.exited = false;
    }

    fn set_status(&mut self, status: SessionStatus) {
        // Only assign here. The spinner clock restart on a kind change lives
        // in `reconcile_spinner_clock` — the single reset point every status
        // transition funnels through, including direct `view.status`
        // replacements that bypass this method (reached via
        // `arm_spinner_deadline` in the loop and `tick_status_indicator` for
        // direct callers).
        self.view.status = Some(status);
    }

    /// Advance the status spinner one frame and refresh its elapsed-seconds
    /// counter. Called on every [`SPINNER_TICK`] while a status is visible.
    /// Returns `false` when nothing changed, so the caller can skip scheduling
    /// a repaint for idle sub-second ticks.
    fn tick_status_indicator(&mut self) -> bool {
        // Reconcile first so direct unit-test callers get the same reset the
        // run loop applies each turn via `arm_spinner_deadline` — one function,
        // two entry points. The `None` arm below stays a plain early return.
        self.reconcile_spinner_clock();
        let Some(status) = self.view.status.as_mut() else {
            return false;
        };
        let started = *self
            .spinner_started
            .get_or_insert_with(tokio::time::Instant::now);
        let elapsed_secs = started.elapsed().as_secs();
        self.spinner_frame =
            (self.spinner_frame + 1) % pi_tui::components::DEFAULT_LOADER_FRAMES.len();
        if status.frame == self.spinner_frame && status.elapsed_secs == elapsed_secs {
            return false;
        }
        status.frame = self.spinner_frame;
        status.elapsed_secs = elapsed_secs;
        true
    }

    /// Record an async session-action error into `last_error` so the UI can
    /// surface it on the next paint. Never panics.
    fn record_err(&mut self, result: Result<(), String>) {
        if let Err(e) = result {
            self.last_error = Some(e);
        }
    }

    // -----------------------------------------------------------------------
    // Painting
    // -----------------------------------------------------------------------

    fn refresh_chat_caches(&mut self) {
        let prefix_len = self.view.messages.len().saturating_sub(1);
        if self.chat_prefix_cache.is_none() || self.chat_prefix_len != prefix_len {
            let mut messages = std::mem::take(&mut self.view.messages);
            let tail = messages.split_off(prefix_len);
            self.view.messages = messages;
            self.chat_prefix_cache = Some(extract_chat_component(&self.view));
            let mut messages = std::mem::take(&mut self.view.messages);
            messages.extend(tail);
            self.view.messages = messages;
            self.chat_prefix_len = prefix_len;
            self.chat_dirty = true;
        }

        if self.chat_tail_cache.is_none() || self.chat_dirty {
            let mut messages = std::mem::take(&mut self.view.messages);
            let tail = messages.split_off(prefix_len);
            let prefix = messages;
            self.view.messages = tail;
            self.chat_tail_cache = Some(extract_chat_component(&self.view));
            let mut tail = std::mem::take(&mut self.view.messages);
            let mut all = prefix;
            all.append(&mut tail);
            self.view.messages = all;
            self.chat_dirty = false;
        }
    }

    fn build_root(
        &mut self,
        editor: Editor,
        selector: Option<Box<dyn Component>>,
    ) -> InteractiveRoot {
        self.refresh_chat_caches();
        let prefix = self
            .chat_prefix_cache
            .take()
            .unwrap_or_else(empty_chat_component);
        let tail = self
            .chat_tail_cache
            .take()
            .unwrap_or_else(empty_chat_component);
        let dialog_title = self.pending_extension_dialog.as_ref().map(|dialog| {
            Box::new(pi_tui::components::Text::with_padding(
                super::theme::bold(&self.view.theme.fg(
                    super::theme::ThemeColor::Accent,
                    &extension_dialog_title(&dialog.request),
                )),
                1,
                0,
            )) as Box<dyn Component>
        });
        InteractiveRoot::build_with_chat(
            &mut self.view,
            editor,
            selector,
            dialog_title,
            prefix,
            tail,
        )
    }

    fn recover_root(&mut self, mut root: InteractiveRoot) {
        self.chat_prefix_cache = root.take_section("chat-prefix");
        self.chat_tail_cache = root.take_section("chat-tail");
        self.editor = std::mem::replace(root.editor_mut(), Editor::with_defaults());
        self.active_selector = root.selector.take();
    }

    fn arm_coalescer(&mut self) {
        if self.coalesce_deadline.is_none() {
            self.coalesce_deadline = Some(Instant::now() + BACKGROUND_COALESCE_WINDOW);
        }
    }

    fn paint_frame(&mut self) -> io::Result<()> {
        let saved_editor = std::mem::replace(&mut self.editor, Editor::with_defaults());
        let saved_selector = self.active_selector.take();
        let mut root = self.build_root(saved_editor, saved_selector);
        let txn = self
            .pending_reanchor
            .take()
            .map_or(Txn::Frame, Txn::Reanchor);
        let result = self.tui.commit(txn, &mut root);
        self.recover_root(root);
        self.ensure_editor_on_submit();
        result
    }

    fn commit_settle(&mut self, blocks: Vec<SettledBlock>) -> io::Result<()> {
        let saved_editor = std::mem::replace(&mut self.editor, Editor::with_defaults());
        let saved_selector = self.active_selector.take();
        let mut root = self.build_root(saved_editor, saved_selector);
        let result = self.tui.commit(Txn::Settle(blocks), &mut root);
        self.recover_root(root);
        self.ensure_editor_on_submit();
        result
    }

    fn commit_reanchor(&mut self) -> io::Result<()> {
        let saved_editor = std::mem::replace(&mut self.editor, Editor::with_defaults());
        let saved_selector = self.active_selector.take();
        let mut root = self.build_root(saved_editor, saved_selector);
        // A resize reanchor already repaints full rows, so a queued overlay-open
        // reanchor is subsumed — drop it or the next normal frame does an extra,
        // unrelated full-row reanchor (CodeRabbit review body).
        self.pending_reanchor = None;
        let result = self
            .tui
            .commit(Txn::Reanchor(ReanchorCause::Resize), &mut root);
        self.recover_root(root);
        self.ensure_editor_on_submit();
        result
    }

    // -----------------------------------------------------------------------
    // Test driver seam
    // -----------------------------------------------------------------------

    /// Advance one UI event without running the full event loop. Returns the
    /// list of dispatch outcomes; the caller may then assert on view state.
    ///
    /// This is the test driver seam: a fake [`TerminalInput`] can be injected
    /// via [`InteractiveRuntime::new`], and tests call `step_ui` to feed
    /// scripted key sequences while observing the view and session host.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures from the underlying [`Tui::commit`].
    pub async fn step_ui(&mut self, event: UiEvent) -> io::Result<()> {
        self.handle_ui_event(event).await
    }

    /// Advance one session event without running the full event loop.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures from the underlying [`Tui::commit`].
    pub fn step_session_event(
        &mut self,
        event: impl std::borrow::Borrow<AgentSessionEvent>,
    ) -> std::future::Ready<io::Result<()>> {
        self.handle_session_event(event.borrow());
        std::future::ready(Ok(()))
    }

    /// Force a single paint (tests / driver seam).
    ///
    /// # Errors
    ///
    /// Propagates I/O failures from the underlying [`Tui::commit`].
    pub fn paint_now(&mut self) -> io::Result<()> {
        self.paint_frame()
    }

    /// Force a coalesced paint tick (tests). Clears the deadline and commits.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures from the underlying [`Tui::commit`].
    pub fn flush_coalescer(&mut self) -> io::Result<()> {
        self.coalesce_deadline = None;
        self.paint_frame()
    }

    /// Enqueue a settle transaction for the next loop turn (tests / driver).
    pub fn enqueue_settle(&mut self, blocks: Vec<SettledBlock>) {
        self.pending_settle = Some(blocks);
    }
}

// ---------------------------------------------------------------------------
// Pure projection helpers
// ---------------------------------------------------------------------------

/// Default working-indicator text when no extension override is set.
const DEFAULT_WORKING_MESSAGE: &str = "Working…";

/// Working-indicator status for agent-start / streaming projection, honoring the
/// extension `workingVisible` toggle and `workingMessage` override.
fn working_start_status(view: &ViewState) -> Option<SessionStatus> {
    if !view.working_visible {
        return None;
    }
    Some(SessionStatus {
        kind: StatusKind::Working,
        frame: 0,
        elapsed_secs: 0,
        message: view
            .working_message
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKING_MESSAGE.to_owned()),
    })
}

/// Apply a [`SessionSnapshot`] to [`ViewState`]. `partial` may overwrite the
/// streaming tail when present.
fn project_snapshot(
    view: &mut ViewState,
    snapshot: &SessionSnapshot,
    partial: Option<&Arc<AssistantMessage>>,
) {
    view.streaming = snapshot.is_streaming();
    let streaming_status = working_start_status(view);
    view.status = match snapshot.activity {
        SessionActivity::Streaming => streaming_status,
        SessionActivity::Compacting => Some(SessionStatus {
            kind: StatusKind::Compaction,
            frame: 0,
            elapsed_secs: 0,
            message: "Compacting…".to_owned(),
        }),
        SessionActivity::Retrying => Some(SessionStatus {
            kind: StatusKind::Retry,
            frame: 0,
            elapsed_secs: 0,
            message: "Retrying…".to_owned(),
        }),
        SessionActivity::Summarizing => Some(SessionStatus {
            kind: StatusKind::BranchSummary,
            frame: 0,
            elapsed_secs: 0,
            message: "Summarizing…".to_owned(),
        }),
        SessionActivity::Idle => None,
    };

    view.pending.steering = snapshot
        .steering
        .iter()
        .map(|t| PendingMessage {
            kind: PendingKind::Steering,
            text: t.clone(),
        })
        .collect();
    view.pending.follow_up = snapshot
        .follow_up
        .iter()
        .map(|t| PendingMessage {
            kind: PendingKind::FollowUp,
            text: t.clone(),
        })
        .collect();
    view.pending.follow_up_mode = snapshot.follow_up_mode;

    view.footer.model_id.clone_from(&snapshot.model_id);
    view.footer.flags.reasoning = snapshot.reasoning;

    if let Some(message) = partial {
        let has_streaming = view
            .messages
            .iter_mut()
            .any(|m| matches!(m, MessageView::Assistant(v) if v.streaming));
        if !has_streaming {
            view.messages
                .push(MessageView::streaming_assistant((**message).clone()));
        }
    }
}

fn project_footer(view: &mut ViewState, snapshot: &SessionFooterSnapshot) {
    let footer = &mut view.footer;
    footer.total_input = snapshot.total_input;
    footer.total_output = snapshot.total_output;
    footer.total_cache_read = snapshot.total_cache_read;
    footer.total_cache_write = snapshot.total_cache_write;
    footer.total_cost = snapshot.total_cost;
    footer.context_window = snapshot.context_window;
    footer.context_percent = snapshot.context_percent;
    footer.provider.clone_from(&snapshot.provider);
    footer.provider_count = snapshot.provider_count;
    footer.thinking_level = snapshot.thinking_level;
    footer.flags.billing = if snapshot.subscription {
        BillingMode::Subscription
    } else {
        BillingMode::Metered
    };
    footer.flags.auto_compact = snapshot.auto_compact;
    view.editor.border = if snapshot.bash_running {
        EditorBorder::Bash
    } else if snapshot.thinking_level == pi_ai::ModelThinkingLevel::Off {
        EditorBorder::Muted
    } else {
        EditorBorder::Thinking(snapshot.thinking_level)
    };
}

const fn event_refreshes_footer(event: &AgentSessionEvent) -> bool {
    matches!(
        event,
        AgentSessionEvent::AgentSettled
            | AgentSessionEvent::CompactionEnd { .. }
            | AgentSessionEvent::ThinkingLevelChanged { .. }
    )
}

/// Project a single [`AgentSessionEvent`] into [`ViewState`] mutations.
fn project_event(view: &mut ViewState, event: &AgentSessionEvent) {
    use crate::core::agent_session::events::AgentSessionEvent as Event;

    match event {
        Event::AgentStart => {
            view.streaming = true;
            view.status = working_start_status(view);
        }
        Event::AgentEnd { will_retry, .. } => {
            if !will_retry {
                view.streaming = false;
                view.status = None;
            }
        }
        Event::AgentSettled => {
            view.streaming = false;
            view.status = None;
        }
        Event::TurnStart
        | Event::TurnEnd { .. }
        | Event::SessionBeforeSwitch { .. }
        | Event::SessionBeforeFork { .. }
        | Event::SessionStart { .. }
        | Event::SessionShutdown { .. }
        | Event::ModelSelect { .. }
        | Event::BashExecutionUpdate { .. } => {}
        Event::SummarizationRetryScheduled {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        } => project_summarization_retry_scheduled(
            view,
            *attempt,
            *max_attempts,
            *delay_ms,
            error_message,
        ),
        Event::SummarizationRetryAttemptStart { source } => {
            project_summarization_retry_attempt_start(view, *source);
        }
        Event::SummarizationRetryFinished => {
            // Matches interactive-mode.ts:3242-3245: clear the retry
            // indicator.
            view.status = None;
        }
        Event::MessageStart { message } => project_message_start(view, message),
        Event::MessageUpdate { message, .. } => {
            project_assistant_message(view, message, false);
        }
        Event::MessageEnd { message } => project_assistant_message(view, message, true),
        Event::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => project_tool_start(view, tool_call_id, tool_name, args),
        Event::ToolExecutionUpdate {
            tool_call_id,
            partial_result,
            ..
        } => update_tool_message(
            view,
            tool_call_id,
            Some(partial_result),
            false,
            super::tool_renderer::ToolPhase::Pending,
        ),
        Event::ToolExecutionEnd {
            tool_call_id,
            result,
            is_error,
            ..
        } => project_tool_end(view, tool_call_id, result, *is_error),
        Event::QueueUpdate {
            steering,
            follow_up,
        } => project_queue(view, steering, follow_up),
        Event::CompactionStart { reason } => project_compaction_start(view, *reason),
        Event::CompactionEnd { .. } | Event::AutoRetryEnd { .. } => view.status = None,
        Event::EntryAppended { entry } => project_entry(view, entry),
        Event::SessionInfoChanged { name } => view.footer.session_name.clone_from(name),
        Event::ThinkingLevelChanged { level } => {
            view.footer.thinking_level = *level;
        }
        Event::AutoRetryStart {
            attempt,
            max_attempts,
            delay_ms,
            ..
        } => {
            view.status = Some(SessionStatus {
                kind: StatusKind::Retry,
                frame: 0,
                elapsed_secs: 0,
                message: format!(
                    "Retrying ({}/{}) in {}s",
                    attempt,
                    max_attempts,
                    delay_ms / 1000
                ),
            });
        }
    }
}

fn project_message_start(view: &mut ViewState, message: &pi_agent::AgentMessage) {
    let Some(view_message) = message_view_from_agent(message) else {
        return;
    };
    if matches!(view_message, MessageView::Assistant(_)) {
        let has_streaming = view
            .messages
            .iter()
            .any(|message| matches!(message, MessageView::Assistant(item) if item.streaming));
        if !has_streaming {
            view.messages.push(view_message);
        }
    } else {
        view.messages.push(view_message);
    }
}

fn project_assistant_message(
    view: &mut ViewState,
    message: &pi_agent::AgentMessage,
    finished: bool,
) {
    let pi_agent::AgentMessage::Llm(boxed) = message else {
        return;
    };
    let pi_ai::Message::Assistant(assistant_message) = boxed.as_ref() else {
        return;
    };

    for message in &mut view.messages {
        if let MessageView::Assistant(assistant) = message
            && assistant.streaming
        {
            assistant.streaming = !finished;
            assistant.message.clone_from(assistant_message);
            return;
        }
    }

    if finished {
        view.messages
            .push(MessageView::Assistant(AssistantMessageView {
                message: assistant_message.clone(),
                hide_thinking: false,
                hidden_thinking_label: String::new(),
                streaming: false,
            }));
    } else {
        view.messages
            .push(MessageView::streaming_assistant(assistant_message.clone()));
    }
}

fn project_tool_start(
    view: &mut ViewState,
    tool_call_id: &str,
    tool_name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
) {
    let args_value = serde_json::Value::Object(args.clone());
    let args_summary = summarize_tool_args(&args_value);
    view.messages
        .push(MessageView::Tool(super::messages::ToolMessageView {
            renderer: tool_name.to_owned(),
            state: super::tool_renderer::ToolState {
                call: super::tool_renderer::ToolCallView {
                    name: tool_name.to_owned(),
                    id: tool_call_id.to_owned(),
                    args_summary,
                    raw_args: args_value,
                },
                result: None,
                expanded: false,
                phase: super::tool_renderer::ToolPhase::Pending,
            },
        }));
}

fn project_tool_end(
    view: &mut ViewState,
    tool_call_id: &str,
    result: &pi_agent::AgentToolResult,
    is_error: bool,
) {
    let phase = if is_error {
        super::tool_renderer::ToolPhase::Error
    } else {
        super::tool_renderer::ToolPhase::Success
    };
    update_tool_message(view, tool_call_id, Some(result), is_error, phase);
}

fn project_queue(view: &mut ViewState, steering: &[String], follow_up: &[String]) {
    view.pending.steering = steering
        .iter()
        .map(|text| PendingMessage {
            kind: PendingKind::Steering,
            text: text.clone(),
        })
        .collect();
    view.pending.follow_up = follow_up
        .iter()
        .map(|text| PendingMessage {
            kind: PendingKind::FollowUp,
            text: text.clone(),
        })
        .collect();
}

/// Project `summarization_retry_scheduled` (interactive-mode.ts:3222-3228):
/// showError(errorMessage) then a retry status indicator with attempt,
/// maxAttempts, delayMs.
fn project_summarization_retry_scheduled(
    view: &mut ViewState,
    attempt: u32,
    max_attempts: u32,
    delay_ms: u64,
    error_message: &str,
) {
    view.messages
        .push(MessageView::Custom(super::messages::CustomMessageView {
            custom_type: "error".to_owned(),
            text: format!("Error: {error_message}"),
        }));
    view.status = Some(SessionStatus {
        kind: StatusKind::Retry,
        frame: 0,
        elapsed_secs: 0,
        message: format!(
            "Retrying ({attempt}/{max_attempts}) in {}s",
            delay_ms / 1000
        ),
    });
}

/// Project `summarization_retry_attempt_start` (interactive-mode.ts:3231-3239):
/// clear the retry indicator, then show a branch-summary indicator when
/// source is `BranchSummary`, else a compaction indicator with reason.
fn project_summarization_retry_attempt_start(
    view: &mut ViewState,
    source: crate::core::agent_session::events::SummarizationRetrySource,
) {
    match source {
        crate::core::agent_session::events::SummarizationRetrySource::BranchSummary => {
            view.status = Some(SessionStatus {
                kind: StatusKind::BranchSummary,
                frame: 0,
                elapsed_secs: 0,
                message: "Summarizing…".to_owned(),
            });
        }
        crate::core::agent_session::events::SummarizationRetrySource::Compaction { reason } => {
            project_compaction_start(view, reason);
        }
    }
}

fn project_compaction_start(
    view: &mut ViewState,
    reason: crate::core::agent_session::events::CompactionReason,
) {
    let message = match reason {
        crate::core::agent_session::events::CompactionReason::Manual => "Compacting…",
        crate::core::agent_session::events::CompactionReason::Threshold => "Auto-compacting…",
        crate::core::agent_session::events::CompactionReason::Overflow => "Overflow auto-compact…",
    };
    view.status = Some(SessionStatus {
        kind: StatusKind::Compaction,
        frame: 0,
        elapsed_secs: 0,
        message: message.to_owned(),
    });
}

fn project_entry(view: &mut ViewState, entry: &crate::core::sessions::SessionEntry) {
    let Some(view_message) = message_view_from_entry(entry) else {
        return;
    };
    match &view_message {
        MessageView::User(user) => {
            let already_present = view.messages.iter().rev().any(
                |message| matches!(message, MessageView::User(item) if item.text == user.text),
            );
            if !already_present {
                view.messages.push(view_message);
            }
        }
        MessageView::Assistant(_) => {
            // Assistants stream via MessageUpdate/partial.
        }
        MessageView::Tool(_)
        | MessageView::Bash(_)
        | MessageView::Custom(_)
        | MessageView::Compaction(_)
        | MessageView::Branch(_)
        | MessageView::Skill(_) => view.messages.push(view_message),
    }
}

// ---------------------------------------------------------------------------
// Message projection helpers
// ---------------------------------------------------------------------------

fn project_messages(messages: &[pi_agent::AgentMessage]) -> Vec<MessageView> {
    messages
        .iter()
        .filter_map(message_view_from_agent)
        .collect()
}

/// Flatten a rendered frame buffer into `(text, visible width)` rows for the
/// `/debug` dump. Wide-glyph continuation cells are skipped like the snapshot
/// helpers; trailing spaces are trimmed.
fn buffer_plain_lines(buffer: &Buffer, width: u16, height: u16) -> Vec<(String, u16)> {
    use ratatui::buffer::CellDiffOption;
    let mut out = Vec::with_capacity(usize::from(height));
    for row in 0..height {
        let mut line = String::new();
        for x in 0..width {
            if let Some(cell) = buffer.cell((x, row)) {
                if cell.diff_option == CellDiffOption::Skip {
                    continue;
                }
                line.push_str(cell.symbol());
            } else {
                line.push(' ');
            }
        }
        let trimmed = line.trim_end();
        let visible = u16::try_from(trimmed.chars().count()).unwrap_or(u16::MAX);
        out.push((trimmed.to_owned(), visible));
    }
    out
}

/// Resolve the theme to install at startup from persisted settings and the
/// probed terminal polarity.
fn startup_theme<S: SessionHost + ?Sized>(
    session: &S,
    terminal: TerminalTheme,
) -> Arc<ResolvedTheme> {
    let (raw_theme, theme_mode) = session.theme_settings();
    super::theme::resolve_active_theme(raw_theme.as_deref(), theme_mode, terminal)
}

/// Polarity mode implied by a raw theme setting an extension just set:
/// pairs mean auto; plain names pin their own polarity (upstream
/// "disable auto-sync"); unpaired custom names pin Dark (mode is inert).
fn theme_mode_for_name(raw: &str) -> ThemeMode {
    if super::theme::parse_theme_pair(raw).is_some() {
        ThemeMode::Auto
    } else if raw == "light" || raw.ends_with("-light") {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    }
}

fn wire_color(
    value: super::theme::ThemeSlotValue,
    mode: super::theme::ColorMode,
) -> ThemeColorValue {
    use super::theme::{ThemeSlotValue, rgb_to_256};
    match value {
        ThemeSlotValue::Empty => ThemeColorValue::Text(String::new()),
        ThemeSlotValue::Indexed(index) => ThemeColorValue::Index(index),
        ThemeSlotValue::Rgb(rgb) => match mode {
            super::theme::ColorMode::Truecolor => {
                let super::theme::Rgb(r, g, b) = rgb;
                ThemeColorValue::Text(format!("#{r:02x}{g:02x}{b:02x}"))
            }
            super::theme::ColorMode::Palette256 => ThemeColorValue::Index(rgb_to_256(rgb)),
        },
    }
}

fn slot_value_from_wire(value: &ThemeColorValue) -> super::theme::ThemeSlotValue {
    use super::theme::ThemeSlotValue;
    match value {
        ThemeColorValue::Index(index) => ThemeSlotValue::Indexed(*index),
        ThemeColorValue::Text(text) => {
            super::theme::parse_hex_color(text).map_or(ThemeSlotValue::Empty, ThemeSlotValue::Rgb)
        }
    }
}

/// Serialize a resolved theme into the extension wire shape (per-slot
/// `"" | "#rrggbb" | index` values keyed by schema slot name).
fn theme_wire_from_resolved(theme: &ResolvedTheme, source_path: Option<String>) -> ThemeWire {
    let mode = theme.mode();
    let fg = super::theme::fg_slot_names()
        .iter()
        .map(|(slot, name)| ((*name).to_owned(), wire_color(theme.fg_value(*slot), mode)))
        .collect();
    let bg = super::theme::bg_slot_names()
        .iter()
        .map(|(slot, name)| ((*name).to_owned(), wire_color(theme.bg_value(*slot), mode)))
        .collect();
    ThemeWire {
        name: (!theme.name.is_empty()).then(|| theme.name.to_string()),
        source_path,
        color_mode: match mode {
            super::theme::ColorMode::Truecolor => "truecolor".to_owned(),
            super::theme::ColorMode::Palette256 => "256color".to_owned(),
        },
        fg,
        bg,
    }
}

/// Build a theme from the extension `setTheme` object form. Unknown slots
/// are ignored; missing slots stay empty (reset).
fn resolved_theme_from_wire(wire: &ThemeWire) -> ResolvedTheme {
    let mode = if wire.color_mode == "256color" {
        super::theme::ColorMode::Palette256
    } else {
        super::theme::ColorMode::Truecolor
    };
    let fg = super::theme::fg_slot_names()
        .iter()
        .filter_map(|(slot, name)| {
            wire.fg
                .get(*name)
                .map(|value| (*slot, slot_value_from_wire(value)))
        });
    let bg = super::theme::bg_slot_names()
        .iter()
        .filter_map(|(slot, name)| {
            wire.bg
                .get(*name)
                .map(|value| (*slot, slot_value_from_wire(value)))
        });
    ResolvedTheme::from_value_slots(fg, bg, mode, wire.name.clone().unwrap_or_default())
}

/// Assemble the `theme.update` payload: active theme, polarity context,
/// generation, and the full discovered catalog.
fn build_theme_update(
    active: &ResolvedTheme,
    mode: ThemeMode,
    terminal: TerminalTheme,
    theme_generation: u64,
) -> ThemeUpdate {
    let themes = super::theme::available_themes(super::theme::ColorMode::Truecolor)
        .into_iter()
        .map(|(info, resolved)| {
            let path = info.path.map(|path| path.to_string_lossy().into_owned());
            ThemeCatalogEntry {
                name: info.name,
                path: path.clone(),
                file_stem: info.file_stem,
                theme: theme_wire_from_resolved(&resolved, path),
            }
        })
        .collect();
    ThemeUpdate {
        theme: theme_wire_from_resolved(active, None),
        terminal_theme: match terminal {
            TerminalTheme::Dark => "dark".to_owned(),
            TerminalTheme::Light => "light".to_owned(),
        },
        theme_mode: mode.as_str().to_owned(),
        theme_generation,
        themes,
    }
}

fn extract_chat_component(view: &ViewState) -> Box<dyn Component> {
    compose(view)
        .sections
        .into_iter()
        .find(|section| section.label == "chat")
        .map_or_else(empty_chat_component, |section| section.component)
}

fn empty_chat_component() -> Box<dyn Component> {
    Box::new(pi_tui::components::Text::new(String::new()))
}

fn apply_display_preferences(
    messages: &mut [MessageView],
    tools_expanded: bool,
    hide_thinking: bool,
) {
    for message in messages {
        match message {
            MessageView::Assistant(view) => view.hide_thinking = hide_thinking,
            MessageView::Tool(view) => view.state.expanded = tools_expanded,
            MessageView::Bash(view) => view.expanded = tools_expanded,
            MessageView::User(_)
            | MessageView::Custom(_)
            | MessageView::Compaction(_)
            | MessageView::Branch(_)
            | MessageView::Skill(_) => {}
        }
    }
}

fn message_view_from_agent(message: &pi_agent::AgentMessage) -> Option<MessageView> {
    match message {
        pi_agent::AgentMessage::Llm(boxed) => match boxed.as_ref() {
            pi_ai::Message::User(user) => {
                Some(MessageView::User(super::messages::UserMessageView {
                    text: user_message_text(user),
                }))
            }
            pi_ai::Message::Assistant(am) => Some(MessageView::Assistant(
                super::messages::AssistantMessageView {
                    message: am.clone(),
                    hide_thinking: false,
                    hidden_thinking_label: String::new(),
                    streaming: false,
                },
            )),
            pi_ai::Message::ToolResult(_) => None,
        },
        pi_agent::AgentMessage::Custom(custom) => Some(message_view_from_custom(custom)),
    }
}

fn message_view_from_custom(custom: &pi_agent::CustomAgentMessage) -> MessageView {
    let text = custom
        .payload
        .get("text")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            custom
                .payload
                .get("content")
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("")
        .to_owned();
    match custom.role.as_str() {
        "bashExecution" => bash_message_view(custom, &text),
        "compactionSummary" => MessageView::Compaction(super::messages::CompactionSummaryView {
            summary: custom
                .payload
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&text)
                .to_owned(),
            tokens_before: custom
                .payload
                .get("tokensBefore")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0),
        }),
        "branchSummary" => MessageView::Branch(super::messages::BranchSummaryView {
            summary: custom
                .payload
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&text)
                .to_owned(),
            from_id: custom
                .payload
                .get("fromId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("root")
                .to_owned(),
        }),
        "skillInvocation" => MessageView::Skill(super::messages::SkillInvocationView {
            name: custom
                .payload
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("skill")
                .to_owned(),
            text,
        }),
        other => MessageView::Custom(super::messages::CustomMessageView {
            custom_type: other.to_owned(),
            text,
        }),
    }
}

fn bash_message_view(custom: &pi_agent::CustomAgentMessage, text: &str) -> MessageView {
    let command = custom
        .payload
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    let output = custom
        .payload
        .get("output")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(text)
        .to_owned();
    MessageView::Bash(super::messages::BashMessageView {
        command,
        output,
        expanded: false,
        exit_code: custom
            .payload
            .get("exitCode")
            .and_then(serde_json::Value::as_i64)
            .map(clamp_i64_to_i32),
        cancelled: custom
            .payload
            .get("cancelled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        truncated: custom
            .payload
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        full_output_path: custom
            .payload
            .get("fullOutputPath")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

fn message_view_from_entry(entry: &crate::core::sessions::SessionEntry) -> Option<MessageView> {
    use crate::core::sessions::SessionEntry;
    match entry {
        SessionEntry::Message(m) => message_view_from_agent(&m.message),
        SessionEntry::Compaction(c) => Some(MessageView::Compaction(
            super::messages::CompactionSummaryView {
                summary: c.summary.clone(),
                tokens_before: c.tokens_before,
            },
        )),
        SessionEntry::BranchSummary(b) => {
            Some(MessageView::Branch(super::messages::BranchSummaryView {
                summary: b.summary.clone(),
                from_id: b.from_id.clone(),
            }))
        }
        SessionEntry::CustomMessage(m) => {
            let text = match &m.content {
                crate::core::messages::CustomMessageContent::Text(s) => s.clone(),
                crate::core::messages::CustomMessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|b| match b {
                        pi_ai::UserContent::Text(t) => Some(t.text.as_str()),
                        pi_ai::UserContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            };
            Some(MessageView::Custom(super::messages::CustomMessageView {
                custom_type: m.custom_type.clone(),
                text,
            }))
        }
        SessionEntry::Custom(c) => Some(MessageView::Custom(super::messages::CustomMessageView {
            custom_type: c.custom_type.clone(),
            text: c
                .data
                .as_ref()
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned(),
        })),
        _ => None,
    }
}

fn user_message_text(user: &pi_ai::UserMessage) -> String {
    match &user.content {
        pi_ai::UserMessageContent::Text(s) => s.clone(),
        pi_ai::UserMessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                pi_ai::UserContent::Text(t) => Some(t.text.as_str()),
                pi_ai::UserContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn clamp_i64_to_i32(value: i64) -> i32 {
    match i32::try_from(value) {
        Ok(value) => value,
        Err(_) if value.is_negative() => i32::MIN,
        Err(_) => i32::MAX,
    }
}

fn summarize_tool_args(args: &serde_json::Value) -> String {
    match args {
        serde_json::Value::Object(map) if map.len() == 1 => map.iter().next().map_or_else(
            || args.to_string(),
            |(key, value)| match value {
                serde_json::Value::String(text) => format!("{key}={text}"),
                other => format!("{key}={other}"),
            },
        ),
        other => other.to_string(),
    }
}

fn tool_result_view(
    result: &pi_agent::AgentToolResult,
    is_error: bool,
) -> super::tool_renderer::ToolResultView {
    // The Edit tool stores the numbered diff in `details["diff"]` while
    // `content` carries only the "Successfully replaced …" sentence. The diff
    // is display-oriented and strictly more informative, so prefer it when
    // present; EditRenderer routes this text through `diff_lines`, whose
    // leading-marker colourization recognizes the numbered `+`/`-` lines.
    let mut text = String::new();
    if let Some(diff) = result.details.get("diff").and_then(|v| v.as_str())
        && !diff.is_empty()
    {
        text.push_str(diff);
    }
    if text.is_empty() {
        for content in &result.content {
            if let pi_ai::ToolResultContent::Text(t) = content {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t.text);
            }
        }
    }
    super::tool_renderer::ToolResultView {
        text,
        truncated: false,
        full_output_path: None,
        images: Vec::new(),
        error: if is_error {
            Some(
                result
                    .details
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool error")
                    .to_owned(),
            )
        } else {
            None
        },
    }
}

fn update_tool_message(
    view: &mut ViewState,
    tool_call_id: &str,
    result: Option<&pi_agent::AgentToolResult>,
    is_error: bool,
    phase: super::tool_renderer::ToolPhase,
) {
    for message in view.messages.iter_mut().rev() {
        if let MessageView::Tool(tool) = message
            && tool.state.call.id == tool_call_id
        {
            if let Some(result) = result {
                tool.state.result = Some(tool_result_view(result, is_error));
            }
            tool.state.phase = phase;
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Settle policy helpers
// ---------------------------------------------------------------------------

/// Build a [`SettledBlock::Lines`] from a slice of styled lines.
#[cfg(test)]
fn settled_lines(lines: Vec<Line<'static>>) -> SettledBlock {
    SettledBlock::Lines(lines)
}

#[allow(dead_code)]
fn settled_raw(rows: u16, bytes: Vec<u8>, fallback: Vec<Line<'static>>) -> SettledBlock {
    SettledBlock::Raw {
        rows,
        bytes,
        kitty_id: None,
        fallback,
    }
}

// ---------------------------------------------------------------------------
// Test driver seam: SharedWriter + helpers
// ---------------------------------------------------------------------------

/// Shared-buffer writer for tests so the [`pi_tui::terminal::guard::TerminalGuard`]
/// and [`Tui`] can write to the same in-memory sink without owning the same
/// `Vec`.
#[derive(Clone, Default)]
pub struct SharedWriter {
    inner: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl SharedWriter {
    /// Construct a fresh shared writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the bytes written so far.
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("shared writer poisoned"))?;
        guard.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Debug for SharedWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedWriter").finish_non_exhaustive()
    }
}

/// Build a [`TerminalInput`] backed by an in-memory channel for tests.
#[must_use]
pub fn mock_input(rx: mpsc::UnboundedReceiver<UiEvent>) -> TerminalInput {
    TerminalInput::mock(rx)
}

// ---------------------------------------------------------------------------
// Production adapter: AgentSessionHost + run_interactive_mode
// ---------------------------------------------------------------------------

use std::io::IsTerminal;

use crate::core::agent_session::bash::ExecuteBashOptions;
use crate::core::agent_session::{AgentSession, AgentSessionEventListener};
use crate::core::agent_session_runtime::{
    AgentSessionRuntime, AgentSessionRuntimeError, ForkPosition, NewSessionOptions,
    SwitchSessionOptions,
};
use pi_tui::terminal::{
    TerminalGuard, install_panic_emergency_hook, write_emergency_restore_bytes,
};

/// Production [`SessionHost`] over a live `Arc<AgentSession>` and the
/// owning `Arc<AgentSessionRuntime>`.
///
/// All async session methods route to the real `AgentSession`. New / fork /
/// switch / clone go through `AgentSessionRuntime` so the replacement
/// pipeline runs (teardown → apply → rebind). The host clones the `Arc`s so
/// it is `'static` and cheap to share with the runtime.
#[derive(Clone)]
pub struct AgentSessionHost {
    session: Arc<std::sync::RwLock<Arc<AgentSession>>>,
    runtime: Arc<AgentSessionRuntime>,
}

impl AgentSessionHost {
    /// Construct a new host around the live runtime + its current session.
    #[must_use]
    pub fn new(runtime: Arc<AgentSessionRuntime>) -> Self {
        let session = runtime.session();
        Self {
            session: Arc::new(std::sync::RwLock::new(session)),
            runtime,
        }
    }

    /// Snapshot the underlying session Arc (for rebind wiring).
    #[must_use]
    pub fn session(&self) -> Arc<AgentSession> {
        self.read_session()
    }

    /// Refresh the cached session Arc from the runtime (after a replacement).
    pub fn refresh(&self) {
        let next = self.runtime.session();
        if let Ok(mut guard) = self.session.write() {
            guard.clone_from(&next);
        }
    }

    fn read_session(&self) -> Arc<AgentSession> {
        self.session.read().map_or_else(
            |poisoned| Arc::clone(&*poisoned.into_inner()),
            |guard| Arc::clone(&*guard),
        )
    }
}

impl SessionHost for AgentSessionHost {
    fn snapshot(&self) -> SessionSnapshot {
        let session = self.read_session();
        let model = session.model();
        let thinking = session.thinking_level();
        let activity = if session.is_streaming() {
            SessionActivity::Streaming
        } else if session.is_compacting() {
            SessionActivity::Compacting
        } else if session.is_retrying() {
            SessionActivity::Retrying
        } else if session.is_summarizing() {
            SessionActivity::Summarizing
        } else {
            SessionActivity::Idle
        };
        let (steering, follow_up) = session.pending_messages();
        SessionSnapshot {
            activity,
            bash_running: session.is_bash_running(),
            thinking_level_label: format!("{thinking:?}").to_lowercase(),
            model_id: model.id.clone(),
            reasoning: model.reasoning,
            steering,
            follow_up,
            follow_up_mode: match session.follow_up_mode() {
                pi_agent::QueueMode::All => super::state::QueueMode::All,
                pi_agent::QueueMode::OneAtATime => super::state::QueueMode::OneAtATime,
            },
        }
    }

    fn footer_snapshot(&self) -> BoxFuture<'_, SessionFooterSnapshot> {
        let session = self.read_session();
        Box::pin(async move {
            let model = session.model();
            let stats = session.get_session_stats().await;
            let context = stats.context_usage;
            let runtime = session.model_runtime_handle();
            let subscription = runtime
                .as_ref()
                .is_some_and(|runtime| runtime.is_using_oauth(&model.provider));
            let provider_count = runtime.as_ref().map_or(1, |runtime| {
                runtime
                    .get_models(None)
                    .into_iter()
                    .map(|model| model.provider)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    .max(1)
            });
            SessionFooterSnapshot {
                total_input: stats.tokens.input,
                total_output: stats.tokens.output,
                total_cache_read: stats.tokens.cache_read,
                total_cache_write: stats.tokens.cache_write,
                total_cost: stats.cost,
                context_window: context.map_or(model.context_window, |usage| usage.context_window),
                context_percent: context.and_then(|usage| usage.percent),
                provider: Some(model.provider),
                provider_count,
                thinking_level: session.thinking_level(),
                bash_running: session.is_bash_running(),
                subscription,
                auto_compact: session.auto_compaction_enabled(),
            }
        })
    }

    fn subscribe(&self) -> EventSubscription {
        let (tx, rx) = mpsc::unbounded_channel::<AgentSessionEvent>();
        let session = self.read_session();
        let listener: AgentSessionEventListener = Arc::new(move |event: &AgentSessionEvent| {
            let _ = tx.send(event.clone());
        });
        let unsubscribe = session.subscribe_arc_listener(listener);
        EventSubscription {
            rx,
            unsubscribe: Some(Box::new(unsubscribe)),
        }
    }

    fn partial_rx(&self) -> watch::Receiver<Option<Arc<AssistantMessage>>> {
        self.read_session().agent().partial()
    }

    fn prompt(&self, text: &str, opts: PromptOptions) -> BoxFuture<'_, Result<(), String>> {
        let session = self.read_session();
        let text = text.to_owned();
        Box::pin(async move { session.prompt(&text, opts).await.map_err(|e| e.to_string()) })
    }

    fn steer(&self, text: &str) -> BoxFuture<'_, Result<(), String>> {
        let session = self.read_session();
        let text = text.to_owned();
        Box::pin(async move { session.steer(&text, Vec::new()).map_err(|e| e.to_string()) })
    }

    fn follow_up(&self, text: &str) -> BoxFuture<'_, Result<(), String>> {
        let session = self.read_session();
        let text = text.to_owned();
        Box::pin(async move {
            session
                .follow_up(&text, Vec::new())
                .map_err(|e| e.to_string())
        })
    }

    fn abort(&self) -> BoxFuture<'static, Result<(), String>> {
        let session = self.read_session();
        Box::pin(async move {
            session.abort().await;
            Ok(())
        })
    }

    fn compact(&self, instructions: Option<&str>) -> BoxFuture<'_, Result<(), String>> {
        let session = self.read_session();
        let instructions = instructions.map(str::to_owned);
        Box::pin(async move {
            session
                .compact(instructions.as_deref())
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }

    fn cycle_thinking_level(&self) -> BoxFuture<'_, Result<(), String>> {
        let session = self.read_session();
        Box::pin(async move {
            session
                .cycle_thinking_level()
                .await
                .ok_or_else(|| "model does not support thinking".to_owned())
                .map(|_| ())
        })
    }

    fn cycle_model(&self, forward: bool) -> BoxFuture<'_, Result<(), String>> {
        let session = self.read_session();
        Box::pin(async move {
            let direction = if forward {
                crate::core::agent_session::model::CycleDirection::Forward
            } else {
                crate::core::agent_session::model::CycleDirection::Backward
            };
            session
                .cycle_model(direction)
                .await
                .ok_or_else(|| "only one model available".to_owned())
                .map(|_| ())
        })
    }

    fn reload(&self) -> BoxFuture<'_, Result<Vec<String>, String>> {
        let session = self.read_session();
        Box::pin(async move {
            let diagnostics = session.reload().await.map_err(|error| error.to_string())?;
            Ok(diagnostics
                .into_iter()
                .map(|diagnostic| diagnostic.to_string())
                .collect())
        })
    }

    fn messages(&self) -> Vec<pi_agent::AgentMessage> {
        self.read_session().messages()
    }

    fn host_extension_runner(&self) -> Option<Arc<ExtensionRuntimeSet>> {
        self.read_session().host_extension_runner()
    }

    fn hide_thinking_block(&self) -> bool {
        self.read_session()
            .lock_settings()
            .get_hide_thinking_block()
    }

    fn set_hide_thinking_block(&self, hide: bool) -> Result<(), String> {
        self.read_session()
            .lock_settings()
            .set_hide_thinking_block(hide);
        Ok(())
    }
    fn theme_settings(&self) -> (Option<String>, ThemeMode) {
        let session = self.read_session();
        let settings = session.lock_settings();
        (settings.get_theme(), settings.get_theme_mode())
    }

    fn persist_theme(&self, theme: &str, mode: ThemeMode) -> Result<(), String> {
        let session = self.read_session();
        let mut settings = session.lock_settings();
        settings.set_theme(theme);
        settings.set_theme_mode(mode);
        Ok(())
    }

    fn apply_settings_change(&self, id: &str, value: &str) -> Result<(), String> {
        let session = self.read_session();
        let mut settings = session.lock_settings();
        match id {
            "theme" => settings.set_theme(&super::theme::theme_selection_to_storage(value)),
            "themeMode" => {
                let mode = match value {
                    "auto" => ThemeMode::Auto,
                    "dark" => ThemeMode::Dark,
                    "light" => ThemeMode::Light,
                    other => return Err(format!("unknown theme mode: {other}")),
                };
                settings.set_theme_mode(mode);
            }
            "compaction.enabled" => settings.set_compaction_enabled(value == "on"),
            "retry.enabled" => settings.set_retry_enabled(value == "on"),
            "doubleEscapeAction" => {
                use crate::core::settings::DoubleEscapeAction as Action;
                let action = match value {
                    "fork" => Action::Fork,
                    "tree" => Action::Tree,
                    "none" => Action::None,
                    other => return Err(format!("unknown double-escape action: {other}")),
                };
                settings.set_double_escape_action(action);
            }
            "quietStartup" => settings.set_quiet_startup(value == "on"),
            "showImages" => settings.set_show_images(value == "on"),
            other => return Err(format!("unknown setting: {other}")),
        }
        Ok(())
    }

    fn persist_first_run(
        &self,
        selection: &crate::core::platform::first_run::FirstRunSelection,
    ) -> Result<(), String> {
        let session = self.read_session();
        let mut settings = session.lock_settings();
        crate::core::platform::first_run::persist_first_run_selection(&mut settings, selection)
    }

    fn external_editor_command(&self) -> String {
        self.read_session()
            .lock_settings()
            .get_external_editor_command()
    }

    fn get_model_entries(
        &self,
    ) -> BoxFuture<'_, Result<Vec<super::state::ModelSelectorEntry>, String>> {
        let session = self.read_session();
        Box::pin(async move {
            let models = session
                .model_runtime_handle()
                .map_or_else(|| vec![session.model()], |runtime| runtime.get_models(None));
            Ok(models
                .into_iter()
                .map(|m| super::state::ModelSelectorEntry {
                    value: format!("{}/{}", m.provider, m.id),
                    label: if m.name.is_empty() {
                        m.id.clone()
                    } else {
                        m.name.clone()
                    },
                    description: Some(m.provider.clone()),
                })
                .collect())
        })
    }

    fn get_session_entries(
        &self,
    ) -> BoxFuture<'_, Result<Vec<super::state::SessionPickerEntry>, String>> {
        let session = self.read_session();
        Box::pin(async move {
            let cwd = session.cwd.clone();
            let session_dir = {
                let manager = session.session_manager();
                let sm = manager.lock().await;
                sm.get_session_dir().to_owned()
            };
            let dir = if session_dir.is_empty() {
                crate::core::config::get_sessions_dir()
            } else {
                std::path::PathBuf::from(session_dir)
            };
            let infos = crate::core::sessions::list_sessions_for_cwd(&cwd, &dir, true, None).await;
            Ok(infos
                .into_iter()
                .map(|info| {
                    let label = info
                        .name
                        .clone()
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| {
                            if info.first_message.is_empty() {
                                info.path.clone()
                            } else {
                                info.first_message.chars().take(80).collect()
                            }
                        });
                    super::state::SessionPickerEntry {
                        value: info.path,
                        label,
                        description: Some(format!("{} msgs", info.message_count)),
                    }
                })
                .collect())
        })
    }

    fn get_tree_entries(&self) -> BoxFuture<'_, Result<Vec<super::state::TreeEntry>, String>> {
        let session = self.read_session();
        Box::pin(async move {
            let manager = session.session_manager();
            let sm = manager.lock().await;
            let tree = sm.get_tree();
            let mut out = Vec::new();
            flatten_tree_nodes(&tree, 0, &mut out);
            Ok(out)
        })
    }

    fn get_fork_entries(&self) -> BoxFuture<'_, Result<Vec<super::state::TreeEntry>, String>> {
        let session = self.read_session();
        Box::pin(async move {
            let users = session.get_user_messages_for_forking().await;
            Ok(users
                .into_iter()
                .map(|u| super::state::TreeEntry {
                    value: u.entry_id,
                    label: u.text.chars().take(80).collect(),
                    depth: 0,
                })
                .collect())
        })
    }

    fn get_trust_entries(&self) -> BoxFuture<'_, Result<Vec<super::state::SettingsRow>, String>> {
        let session = self.read_session();
        Box::pin(async move {
            let settings = session.lock_settings();
            let trust = settings.get_default_project_trust();
            Ok(vec![super::state::SettingsRow {
                id: "defaultProjectTrust".to_owned(),
                label: "Default project trust".to_owned(),
                description: Some("Trust policy for newly discovered project dirs".to_owned()),
                current_value: format!("{trust:?}").to_lowercase(),
                values: Some(vec![
                    "ask".to_owned(),
                    "always".to_owned(),
                    "never".to_owned(),
                ]),
            }])
        })
    }

    fn get_auth_entries(
        &self,
    ) -> BoxFuture<'_, Result<Vec<super::state::AuthSelectorEntry>, String>> {
        let session = self.read_session();
        Box::pin(async move {
            let mut out = Vec::new();
            if let Some(runtime) = session.model_runtime_handle() {
                for provider in runtime.get_registered_provider_ids() {
                    let configured = runtime.has_configured_auth(&provider);
                    out.push(super::state::AuthSelectorEntry {
                        value: provider.clone(),
                        label: provider.clone(),
                        description: Some(if configured {
                            "configured".to_owned()
                        } else {
                            "not configured".to_owned()
                        }),
                    });
                }
            }
            if out.is_empty() {
                let model = session.model();
                out.push(super::state::AuthSelectorEntry {
                    value: model.provider.clone(),
                    label: model.provider.clone(),
                    description: Some("active provider".to_owned()),
                });
            }
            Ok(out)
        })
    }

    fn get_scoped_models_entries(&self) -> BoxFuture<'_, Result<ScopedModelEntries, String>> {
        let session = self.read_session();
        Box::pin(async move {
            let scoped = session.scoped_models();
            let mut enabled = std::collections::BTreeMap::new();
            let entries = scoped
                .into_iter()
                .map(|sm| {
                    let value = format!("{}/{}", sm.model.provider, sm.model.id);
                    enabled.insert(value.clone(), true);
                    super::state::ModelSelectorEntry {
                        value,
                        label: if sm.model.name.is_empty() {
                            sm.model.id.clone()
                        } else {
                            sm.model.name.clone()
                        },
                        description: Some(sm.model.provider.clone()),
                    }
                })
                .collect();
            Ok((entries, enabled))
        })
    }

    fn get_settings_entries(
        &self,
    ) -> BoxFuture<'_, Result<Vec<super::state::SettingsRow>, String>> {
        let session = self.read_session();
        Box::pin(async move {
            let settings = session.lock_settings();
            Ok(vec![
                super::state::SettingsRow {
                    id: "theme".to_owned(),
                    label: "Theme".to_owned(),
                    description: Some("Color scheme".to_owned()),
                    current_value: super::theme::storage_name_to_display(
                        settings.get_theme().as_deref(),
                    ),
                    values: Some(super::theme::theme_selector_values()),
                },
                super::state::SettingsRow {
                    id: "themeMode".to_owned(),
                    label: "Theme mode".to_owned(),
                    description: Some("Auto matches the terminal background".to_owned()),
                    current_value: settings.get_theme_mode().as_str().to_owned(),
                    values: Some(vec![
                        "auto".to_owned(),
                        "dark".to_owned(),
                        "light".to_owned(),
                    ]),
                },
                super::state::SettingsRow {
                    id: "compaction.enabled".to_owned(),
                    label: "Auto-compact".to_owned(),
                    description: Some("Automatically compact long contexts".to_owned()),
                    current_value: if settings.get_compaction_enabled() {
                        "on".to_owned()
                    } else {
                        "off".to_owned()
                    },
                    values: Some(vec!["on".to_owned(), "off".to_owned()]),
                },
                super::state::SettingsRow {
                    id: "retry.enabled".to_owned(),
                    label: "Auto-retry".to_owned(),
                    description: Some("Retry transient provider errors".to_owned()),
                    current_value: if settings.get_retry_enabled() {
                        "on".to_owned()
                    } else {
                        "off".to_owned()
                    },
                    values: Some(vec!["on".to_owned(), "off".to_owned()]),
                },
                super::state::SettingsRow {
                    id: "doubleEscapeAction".to_owned(),
                    label: "Double-Esc action".to_owned(),
                    description: Some("tree / fork / none".to_owned()),
                    current_value: settings.get_double_escape_action().as_str().to_owned(),
                    values: Some(vec![
                        "tree".to_owned(),
                        "fork".to_owned(),
                        "none".to_owned(),
                    ]),
                },
            ])
        })
    }

    fn get_config_entries(&self) -> BoxFuture<'_, Result<Vec<super::state::SettingsRow>, String>> {
        let session = self.read_session();
        Box::pin(async move {
            let settings = session.lock_settings();
            Ok(vec![
                super::state::SettingsRow {
                    id: "quietStartup".to_owned(),
                    label: "Quiet startup".to_owned(),
                    description: Some("Suppress logo/header on launch".to_owned()),
                    current_value: if settings.get_quiet_startup() {
                        "on".to_owned()
                    } else {
                        "off".to_owned()
                    },
                    values: Some(vec!["on".to_owned(), "off".to_owned()]),
                },
                super::state::SettingsRow {
                    id: "showImages".to_owned(),
                    label: "Show images".to_owned(),
                    description: Some("Render inline images in the transcript".to_owned()),
                    current_value: if settings.get_show_images() {
                        "on".to_owned()
                    } else {
                        "off".to_owned()
                    },
                    values: Some(vec!["on".to_owned(), "off".to_owned()]),
                },
            ])
        })
    }

    fn execute_bash(
        &self,
        command: &str,
        exclude_from_context: bool,
    ) -> BoxFuture<'_, Result<(), String>> {
        let session = self.read_session();
        let command = command.to_owned();
        Box::pin(async move {
            let opts = ExecuteBashOptions {
                exclude_from_context,
                ..ExecuteBashOptions::default()
            };
            session
                .execute_bash(command.as_str(), None::<fn(&str)>, opts)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    }

    fn new_session(&self) -> BoxFuture<'_, Result<SwitchOutcome, String>> {
        let runtime = Arc::clone(&self.runtime);
        let host_session = Arc::clone(&self.session);
        Box::pin(async move {
            let outcome = runtime
                .new_session(NewSessionOptions::default())
                .await
                .map_err(|err| runtime_err_to_string(&err))?;
            if !outcome.cancelled
                && let Ok(mut guard) = host_session.write()
            {
                *guard = runtime.session();
            }
            Ok(outcome)
        })
    }

    fn fork(&self, entry_id: &str) -> BoxFuture<'_, Result<ForkOutcome, String>> {
        let runtime = Arc::clone(&self.runtime);
        let host_session = Arc::clone(&self.session);
        let entry_id = entry_id.to_owned();
        Box::pin(async move {
            let outcome = runtime
                .fork(&entry_id, ForkPosition::Before)
                .await
                .map_err(|err| runtime_err_to_string(&err))?;
            if !outcome.cancelled
                && let Ok(mut guard) = host_session.write()
            {
                *guard = runtime.session();
            }
            Ok(outcome)
        })
    }

    fn clone(&self) -> BoxFuture<'_, Result<CloneOutcome, String>> {
        let runtime = Arc::clone(&self.runtime);
        let host_session = Arc::clone(&self.session);
        Box::pin(async move {
            let leaf = {
                let session = runtime.session();
                let manager = session.session_manager();
                let sm = manager.lock().await;
                sm.get_leaf_id().map(str::to_owned)
            };
            let Some(leaf) = leaf else {
                return Ok(CloneOutcome::NothingToClone);
            };
            let outcome = runtime
                .fork(&leaf, ForkPosition::At)
                .await
                .map_err(|err| runtime_err_to_string(&err))?;
            if !outcome.cancelled
                && let Ok(mut guard) = host_session.write()
            {
                *guard = runtime.session();
            }
            Ok(if outcome.cancelled {
                CloneOutcome::Cancelled
            } else {
                CloneOutcome::Cloned
            })
        })
    }

    fn switch_session(&self, path: &str) -> BoxFuture<'_, Result<SwitchOutcome, String>> {
        let runtime = Arc::clone(&self.runtime);
        let host_session = Arc::clone(&self.session);
        let path = path.to_owned();
        Box::pin(async move {
            let outcome = runtime
                .switch_session(&path, SwitchSessionOptions::default())
                .await
                .map_err(|err| runtime_err_to_string(&err))?;
            if !outcome.cancelled
                && let Ok(mut guard) = host_session.write()
            {
                *guard = runtime.session();
            }
            Ok(outcome)
        })
    }

    fn export_html(&self, path: Option<&str>) -> BoxFuture<'_, Result<String, String>> {
        let session = self.read_session();
        let path = path.map(str::to_owned);
        Box::pin(async move {
            session
                .export_to_html(path.as_deref(), None)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn set_session_name(&self, name: &str) -> BoxFuture<'_, Result<Option<String>, String>> {
        let session = self.read_session();
        let name = name.to_owned();
        Box::pin(async move {
            session
                .set_session_name(&name)
                .await
                .map_err(|e| e.to_string())?;
            Ok(session.session_name().await)
        })
    }

    fn logout_provider_options(
        &self,
    ) -> BoxFuture<'_, Result<Vec<super::state::LogoutOption>, String>> {
        let session = self.read_session();
        Box::pin(async move {
            let Some(runtime) = session.model_runtime_handle() else {
                return Ok(Vec::new());
            };
            let credentials = runtime
                .list_credentials()
                .await
                .map_err(|e| e.to_string())?;
            let mut options: Vec<super::state::LogoutOption> = credentials
                .into_iter()
                .map(|info| super::state::LogoutOption {
                    // Upstream prefers the provider display name; the runtime's
                    // provider catalog is not addressable by id here, so fall
                    // back to the id (upstream's `?? providerId` behavior).
                    name: info.provider_id.clone(),
                    id: info.provider_id,
                    is_oauth: matches!(info.kind, pi_ai::auth::CredentialKind::Oauth),
                })
                .collect();
            options.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(options)
        })
    }

    fn logout(&self, provider_id: &str) -> BoxFuture<'_, Result<(), String>> {
        let session = self.read_session();
        let provider_id = provider_id.to_owned();
        Box::pin(async move {
            let runtime = session
                .model_runtime_handle()
                .ok_or_else(|| "No model runtime available".to_owned())?;
            runtime
                .logout(&provider_id)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn last_assistant_text(&self) -> BoxFuture<'_, Result<Option<String>, String>> {
        let session = self.read_session();
        Box::pin(async move { Ok(session.get_last_assistant_text()) })
    }

    fn export_jsonl(&self, path: Option<&str>) -> BoxFuture<'_, Result<String, String>> {
        let session = self.read_session();
        let path = path.map(str::to_owned);
        Box::pin(async move {
            session
                .export_to_jsonl(path.as_deref())
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn import_jsonl(
        &self,
        path: &str,
        cwd_override: Option<&str>,
    ) -> BoxFuture<'_, Result<bool, ImportError>> {
        let runtime = Arc::clone(&self.runtime);
        let host_session = Arc::clone(&self.session);
        let path = path.to_owned();
        let cwd_override = cwd_override.map(str::to_owned);
        Box::pin(async move {
            let outcome = runtime
                .import_from_jsonl(&path, cwd_override.as_deref())
                .await
                .map_err(|err| match err {
                    AgentSessionRuntimeError::MissingSessionCwd => ImportError::MissingCwd {
                        fallback_cwd: runtime.cwd(),
                    },
                    AgentSessionRuntimeError::ImportNotFound(inner) => {
                        ImportError::FileNotFound(inner.to_string())
                    }
                    other => ImportError::Other(runtime_err_to_string(&other)),
                })?;
            if !outcome.cancelled
                && let Ok(mut guard) = host_session.write()
            {
                *guard = runtime.session();
            }
            Ok(!outcome.cancelled)
        })
    }

    fn share(&self) -> BoxFuture<'_, Result<(String, String), String>> {
        let session = self.read_session();
        Box::pin(async move {
            // Unique, unpredictable temp dir per call (mirrors share.rs
            // `TemporaryShareFile`): avoids collisions between concurrent shares
            // and pre-created-symlink races on a guessable path.
            let directory = std::env::temp_dir().join(format!("pi-share-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&directory).map_err(|error| error.to_string())?;
            let html_path = directory.join("session.html");
            let path_str = html_path.to_string_lossy().into_owned();
            // Export via the live session so shared HTML carries system prompt
            // and tools, matching the `/export` path.
            let exported = session.export_to_html(Some(&path_str), None).await;
            let cancel = CancellationToken::new();
            let shared = match exported {
                Ok(_) => crate::core::share::share_html_file(&html_path, &cancel)
                    .await
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let _ = std::fs::remove_file(&html_path);
            let _ = std::fs::remove_dir(&directory);
            let shared = shared?;
            Ok((shared.viewer_url, shared.gist_url))
        })
    }

    fn session_stats(&self) -> BoxFuture<'_, crate::core::agent_session::stats::SessionStats> {
        let session = self.read_session();
        Box::pin(async move { session.get_session_stats().await })
    }
}

/// Map a runtime error into a `String` for [`SessionHost`] consumers.
fn runtime_err_to_string(err: &AgentSessionRuntimeError) -> String {
    err.to_string()
}

fn flatten_tree_nodes(
    nodes: &[crate::core::sessions::SessionTreeNode],
    depth: usize,
    out: &mut Vec<super::state::TreeEntry>,
) {
    for node in nodes {
        let id = node.entry.id().unwrap_or("").to_owned();
        let label = node
            .label
            .clone()
            .unwrap_or_else(|| tree_entry_label(&node.entry));
        if !id.is_empty() {
            out.push(super::state::TreeEntry {
                value: id,
                label,
                depth,
            });
        }
        flatten_tree_nodes(&node.children, depth.saturating_add(1), out);
    }
}

fn tree_entry_label(entry: &crate::core::sessions::SessionEntry) -> String {
    use crate::core::sessions::SessionEntry;
    match entry {
        SessionEntry::Message(message) => {
            let text =
                crate::core::agent_session::tree::extract_user_message_text_pub(&message.message);
            if text.is_empty() {
                message.message.role().to_owned()
            } else {
                text.chars().take(80).collect()
            }
        }
        SessionEntry::Compaction(compaction) => format!(
            "compaction: {}",
            compaction.summary.chars().take(40).collect::<String>()
        ),
        SessionEntry::BranchSummary(branch) => format!(
            "branch: {}",
            branch.summary.chars().take(40).collect::<String>()
        ),
        SessionEntry::Custom(custom) => format!("custom:{}", custom.custom_type),
        SessionEntry::CustomMessage(custom) => {
            format!("custom_message:{}", custom.custom_type)
        }
        SessionEntry::Label(label) => format!("label:{}", label.id),
        SessionEntry::SessionInfo(info) => format!("session_info:{}", info.id),
        SessionEntry::ThinkingLevelChange(change) => {
            format!("thinking:{}", change.thinking_level)
        }
        SessionEntry::ModelChange(change) => {
            format!("model:{}/{}", change.provider, change.model_id)
        }
        SessionEntry::Unknown(_) => "unknown".to_owned(),
    }
}

/// Initial terminal size before raw mode (an ioctl, not an escape probe).
///
/// Returns `(80, 24)` when the size cannot be queried (non-tty stdout).
fn initial_terminal_size() -> (u16, u16) {
    match crossterm::terminal::size() {
        Ok((width, height)) => (width.clamp(20, 1024), height.clamp(1, 256)),
        Err(_) => (80, 24),
    }
}

fn install_product_panic_emergency_hook<W>(
    emergency: Arc<std::sync::atomic::AtomicBool>,
    writer: W,
) -> Arc<dyn Fn() + Send + Sync>
where
    W: Write + Send + 'static,
{
    let writer = std::sync::Mutex::new(writer);
    let restore: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        if let Ok(mut writer) = writer.lock() {
            let _ = write_emergency_restore_bytes(&mut *writer);
        }
    });
    install_panic_emergency_hook(emergency, Arc::clone(&restore));
    restore
}

/// Run interactive mode end-to-end against a real [`AgentSessionRuntime`].
///
/// Wires (in order):
/// 1. `io::stdout()` handle + initial ioctl size.
/// 2. Panic emergency-restore hook and [`TerminalGuard`] viewport/activation.
/// 3. [`Tui<Stdout>`] construction with the cached capabilities + size.
/// 4. [`TerminalInput::spawn`] (sole `EventStream` owner).
/// 5. [`AgentSessionHost`] wrapping the runtime.
/// 6. [`InteractiveRuntime::run`] to completion.
///
/// On exit the runtime is dropped, then the guard (which writes the restore
/// bytes via its `Drop` impl). Returns the process exit code.
///
/// # Errors
///
/// Returns an error string when terminal initialization fails. The caller
/// should surface it on stderr and exit nonzero.
#[allow(clippy::too_many_lines)]
pub async fn run_interactive_mode(
    runtime: Arc<AgentSessionRuntime>,
    mut options: InteractiveRuntimeOptions,
) -> Result<u8, String> {
    use std::io::stdout;
    if !stdout().is_terminal() {
        return Err("interactive mode requires a tty".to_owned());
    }

    // 1. Capture the real terminal size before enabling raw mode. The guard
    // parks the cursor below this viewport on every normal restore.
    let size = initial_terminal_size();
    let mut guard = TerminalGuard::new(stdout());
    guard.set_viewport_bottom_row(size.1.saturating_sub(1));
    let _panic_restore = install_product_panic_emergency_hook(guard.emergency_flag(), stdout());
    let enable_kitty = !cfg!(windows);
    guard
        .activate(enable_kitty)
        .map_err(|e| format!("terminal activation failed: {e}"))?;

    options.pending_ui_events = probe_terminal(guard.writer_mut(), &mut options.caps)
        .map_err(|error| format!("terminal probe failed: {error}"))?;
    let colorfgbg = std::env::var("COLORFGBG").ok();
    options.terminal_theme =
        detect_terminal_theme(options.caps.dark_background, colorfgbg.as_deref());
    set_kitty_protocol_active(options.caps.kitty_keyboard());

    // 2. Tui takes a separate stdout handle (Stdout is a cheap cloneable
    //    handle to the same underlying stream). No stdout clone of the
    //    process's stdout fd — both handles write to the OS stream, but Tui
    //    is the sole writer of paint bytes (guard only wrote mode setup).
    let stdout_writer = stdout();
    let viewport_height = options.viewport_height.max(1).min(size.1);
    let tui = Tui::new(
        stdout_writer,
        ratatui::layout::Size::new(size.0, size.1),
        ratatui::layout::Position::ORIGIN,
        viewport_height,
        options.caps.clone(),
    )
    .map_err(|e| format!("tui initialization failed: {e}"))?;

    // 3. Spawn the sole TerminalInput task.
    let input = TerminalInput::spawn();

    // 4. Wire the host and runtime. Session replacement rebinds the host's
    //    cached session Arc; InteractiveRuntime also rebinds events/partial
    //    via an interior rebind signal.
    let host = AgentSessionHost::new(Arc::clone(&runtime));
    let host_arc = Arc::new(host);

    // Initial bind: emits the stored session_start{startup} to extensions
    // and runs bind-time resource discovery. Bind errors are non-fatal
    // extension errors (the session survives with base resources).
    let _ = host_arc
        .session()
        .bind_extensions(crate::core::agent_session::ExtensionBindings {
            mode: Some(crate::core::agent_session::ExtensionMode::Tui),
            ..Default::default()
        })
        .await;

    // Resolve the startup theme from settings + the just-probed terminal
    // polarity (replaces the static dark default).
    options.theme = startup_theme(host_arc.as_ref(), options.terminal_theme);
    let mut rt = InteractiveRuntime::new(tui, input, host_arc, &options);
    // Bridge replacements dispose the old session from a task outside this
    // loop. Mark that closure before teardown, then rebind after the host
    // points at the replacement.
    {
        let host_for_rebind = Arc::clone(&rt.session);
        let rebind_signal = Arc::clone(&rt.session_rebind_signal);
        runtime.set_rebind_session(Some(Arc::new(move |_session| {
            let host_for_rebind = Arc::clone(&host_for_rebind);
            let rebind_signal = Arc::clone(&rebind_signal);
            Box::pin(async move {
                host_for_rebind.refresh();
                let _ = host_for_rebind
                    .session()
                    .bind_extensions(crate::core::agent_session::ExtensionBindings {
                        mode: Some(crate::core::agent_session::ExtensionMode::Tui),
                        ..Default::default()
                    })
                    .await;
                rebind_signal.signal_completion();
            })
        })));
    }
    {
        let rebind_signal = Arc::clone(&rt.session_rebind_signal);
        runtime.set_before_session_replacement(Some(Arc::new(move || {
            rebind_signal.begin();
        })));
    }

    // Reset extension-owned UI on every session invalidation (ports upstream
    // `setBeforeSessionInvalidate(() => resetExtensionUI())`). The synchronous
    // teardown callback sets a flag that the runtime applies on its next
    // `rebind_session_channels`, covering /new, /fork, /clone, /import, /resume.
    {
        let reset_flag = Arc::clone(&rt.reset_ui_flag);
        runtime.set_before_session_invalidate(Some(Arc::new(move || {
            reset_flag.store(true, std::sync::atomic::Ordering::Release);
        })));
    }

    // 5. Drive the loop. Suspend restores the terminal, raises SIGTSTP on
    //    Unix, then resumes/resizes and re-enters run() without exiting.
    let exit = loop {
        let exit = rt.run().await;
        // Resize events update the runtime view while the guard remains owned
        // here. Synchronize before every path that can restore terminal modes.
        guard.set_viewport_bottom_row(rt.viewport_bottom_row());
        let exit = exit.map_err(|e| format!("runtime loop: {e}"))?;
        match exit {
            InteractiveExit::Suspend => {
                // Drop active selector focus so resume returns to the editor.
                rt.close_selector_for_suspend();
                // Restore modes, suspend the process, then re-activate using
                // the terminal dimensions observed after SIGCONT.
                guard
                    .suspend()
                    .map_err(|e| format!("terminal suspend failed: {e}"))?;
                let size = initial_terminal_size();
                guard.set_viewport_bottom_row(size.1.saturating_sub(1));
                guard
                    .resume(enable_kitty)
                    .map_err(|e| format!("terminal resume failed: {e}"))?;
                // Reanchor without a clear and retain the runtime's clamped
                // view row as the source for the next normal restore.
                let _ = rt
                    .step_ui(UiEvent::Resize {
                        width: size.0,
                        height: size.1,
                    })
                    .await;
                guard.set_viewport_bottom_row(rt.viewport_bottom_row());
                // Rebind channels in case a replacement happened while we
                // were suspended (defensive; replacement normally rebinds
                // via the host callback + next action).
                rt.rebind_session_channels().await;
            }
            InteractiveExit::ExternalEditor => {
                run_external_editor_handoff(&mut rt, &mut guard, enable_kitty).await?;
            }
            other => break other,
        }
    };

    // 6. Drop runtime first so any final paint commits before guard restore.
    drop(rt);
    runtime.set_rebind_session(None);
    runtime.set_before_session_invalidate(None);
    runtime.set_before_session_replacement(None);
    // 7. Guard restores on Drop. Convert exit kind to a process exit code.
    let code = match exit {
        InteractiveExit::Clean
        | InteractiveExit::SessionEnded
        | InteractiveExit::Suspend
        | InteractiveExit::ExternalEditor => 0u8,
        InteractiveExit::IoFailure | InteractiveExit::DrawDeadlock => 1u8,
    };

    guard.restore();
    Ok(code)
}

/// Hand the terminal to the configured external editor, then restore the
/// interactive session and apply the edited prompt text.
async fn run_external_editor_handoff<W, G, S>(
    rt: &mut InteractiveRuntime<W, S>,
    guard: &mut TerminalGuard<G>,
    enable_kitty: bool,
) -> Result<(), String>
where
    W: Write,
    G: Write,
    S: SessionHost,
{
    let initial = rt.editor.get_expanded_text();
    let editor_command = rt.session.external_editor_command();
    rt.input
        .pause()
        .await
        .map_err(|e| format!("pause terminal input for editor: {e}"))?;
    guard.restore();

    let cancel = CancellationToken::new();
    let cancel_on_shutdown = cancel.clone();
    let shutdown = Arc::clone(&rt.shutdown);
    let watcher = tokio::spawn(async move {
        shutdown.notified().await;
        cancel_on_shutdown.cancel();
    });
    let edited = edit_text_in_external_editor(&editor_command, &initial, &cancel)
        .await
        .map_err(|error| error.to_string());
    watcher.abort();

    guard
        .resume(enable_kitty)
        .map_err(|e| format!("terminal resume after editor failed: {e}"))?;
    rt.input
        .resume(Vec::new())
        .await
        .map_err(|e| format!("resume terminal input after editor: {e}"))?;
    rt.exited = false;
    rt.exit_kind = InteractiveExit::Clean;
    match edited {
        Ok(EditOutcome::Changed(text)) => {
            rt.editor.set_text(&text);
            rt.view.editor.text = text;
        }
        Ok(EditOutcome::Unchanged | EditOutcome::Aborted) => {}
        Err(error) => rt.last_error = Some(error),
    }
    let size = initial_terminal_size();
    guard.set_viewport_bottom_row(size.1.saturating_sub(1));
    let _ = rt
        .step_ui(UiEvent::Resize {
            width: size.0,
            height: size.1,
        })
        .await;
    guard.set_viewport_bottom_row(rt.viewport_bottom_row());
    Ok(())
}

/// Extension trait so the host can subscribe with an [`Arc<EventListener>`].
/// [`AgentSession::subscribe`] takes `Fn(&Event)` (not `Arc`) and returns an
/// unsubscribe closure. We adapt by cloning the Arc into a wrapped fn.
trait AgentSessionSubscribeExt {
    fn subscribe_arc_listener(
        &self,
        listener: AgentSessionEventListener,
    ) -> Box<dyn FnOnce() + Send + Sync>;
}

impl AgentSessionSubscribeExt for AgentSession {
    fn subscribe_arc_listener(
        &self,
        listener: AgentSessionEventListener,
    ) -> Box<dyn FnOnce() + Send + Sync> {
        let unsubscribe = self.subscribe(move |event: &AgentSessionEvent| {
            listener(event);
        });
        Box::new(unsubscribe)
    }
}

async fn recv_extension_event(
    receiver: &mut Option<tokio::sync::broadcast::Receiver<ExtensionUiEvent>>,
) -> Option<ExtensionUiEvent> {
    match receiver {
        Some(receiver) => loop {
            match receiver.recv().await {
                Ok(event) => return Some(event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        },
        None => std::future::pending().await,
    }
}

async fn recv_extension_request(
    receiver: &mut Option<mpsc::Receiver<HostUiRequest>>,
) -> Option<HostUiRequest> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn wait_extension_registry_change(receiver: &mut Option<watch::Receiver<u64>>) -> bool {
    match receiver {
        Some(receiver) => receiver.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

async fn wait_extension_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

fn dialog_timeout(request: &HostUiRequest) -> Option<Duration> {
    let timeout_ms = match request {
        HostUiRequest::Select { request, .. } => request.options_meta.timeout_ms,
        HostUiRequest::Confirm { request, .. } => request.options_meta.timeout_ms,
        HostUiRequest::Input { request, .. } => request.options_meta.timeout_ms,
        HostUiRequest::Editor { .. } => None,
    }?;
    Some(Duration::from_millis(timeout_ms))
}

fn extension_dialog_title(request: &HostUiRequest) -> String {
    match request {
        HostUiRequest::Select { request, .. } => request.title.clone(),
        HostUiRequest::Confirm { request, .. } => {
            format!("{}\n{}", request.title, request.message)
        }
        HostUiRequest::Input { request, .. } => request.title.clone(),
        HostUiRequest::Editor { request, .. } => request.title.clone(),
    }
}

const RESERVED_EXTENSION_SHORTCUTS: &[&str] = &[
    "escape",
    "ctrl+c",
    "ctrl+d",
    "ctrl+z",
    "shift+tab",
    "ctrl+p",
    "shift+ctrl+p",
    "ctrl+l",
    "ctrl+o",
    "ctrl+t",
    "ctrl+g",
    "ctrl+x",
    "alt+enter",
    "enter",
    "ctrl+k",
];

fn subscribe_and_snapshot_shortcuts(
    runner: Option<&Arc<ExtensionRuntimeSet>>,
) -> (
    Option<watch::Receiver<u64>>,
    Vec<EffectiveExtensionShortcut>,
) {
    let changes = runner.map(|runner| runner.subscribe_registry_changes());
    let shortcuts = runner.map_or_else(Vec::new, |runner| {
        build_effective_extension_shortcuts(&runner.raw_shortcuts())
    });
    (changes, shortcuts)
}

fn build_effective_extension_shortcuts(
    registrations: &[pi_ext::adapters::ShortcutRegistration],
) -> Vec<EffectiveExtensionShortcut> {
    let reserved = RESERVED_EXTENSION_SHORTCUTS
        .iter()
        .filter_map(|key| parse_key_id(key).ok())
        .map(|key| key.canonical_id())
        .collect::<Vec<_>>();
    let mut effective = Vec::<EffectiveExtensionShortcut>::new();
    for registration in registrations {
        let Ok(parsed) = parse_key_id(&registration.key) else {
            continue;
        };
        let key = parsed.canonical_id().as_str().to_owned();
        if reserved.iter().any(|reserved| reserved.as_str() == key) {
            continue;
        }
        effective.retain(|shortcut| shortcut.key != key);
        effective.push(EffectiveExtensionShortcut {
            key,
            dispatch_key: registration.key.clone(),
            parsed,
            description: registration.description.clone(),
            source: registration.extension_path.clone(),
        });
    }
    effective
}

fn shortcut_hints(shortcuts: &[EffectiveExtensionShortcut]) -> Vec<super::state::ShortcutHint> {
    shortcuts
        .iter()
        .map(|shortcut| super::state::ShortcutHint {
            key: shortcut.key.clone(),
            action: shortcut
                .description
                .clone()
                .or_else(|| shortcut.source.clone())
                .unwrap_or_else(|| "Extension shortcut".to_owned()),
        })
        .collect()
}

fn ui_event_wire(event: &UiEvent) -> UiEventWire {
    match event {
        UiEvent::Key(key) => {
            let (code, modifiers) = pi_tui::keys::normalize_event(key)
                .unwrap_or_else(|| (format!("{:?}", key.code), key.modifiers));
            UiEventWire::Key {
                code,
                modifiers: KeyModifiersWire {
                    shift: modifiers
                        .contains(crossterm::event::KeyModifiers::SHIFT)
                        .then_some(true),
                    alt: modifiers
                        .contains(crossterm::event::KeyModifiers::ALT)
                        .then_some(true),
                    ctrl: modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL)
                        .then_some(true),
                    super_key: modifiers
                        .contains(crossterm::event::KeyModifiers::SUPER)
                        .then_some(true),
                },
                kind: match key.kind {
                    crossterm::event::KeyEventKind::Press => KeyEventKindWire::Press,
                    crossterm::event::KeyEventKind::Repeat => KeyEventKindWire::Repeat,
                    crossterm::event::KeyEventKind::Release => KeyEventKindWire::Release,
                },
            }
        }
        UiEvent::Paste(text) => UiEventWire::Paste { text: text.clone() },
        UiEvent::FocusGained => UiEventWire::FocusGained,
        UiEvent::FocusLost => UiEventWire::FocusLost,
        UiEvent::Resize { width, height } => UiEventWire::Resize {
            width: *width,
            height: *height,
        },
    }
}

fn default_extension_dialog_response(request: &HostUiRequest) -> HostUiResponse {
    match request {
        HostUiRequest::Select { id, .. } => HostUiResponse::Select {
            id: *id,
            value: None,
        },
        HostUiRequest::Confirm { id, .. } => HostUiResponse::Confirm {
            id: *id,
            confirmed: false,
        },
        HostUiRequest::Input { id, .. } => HostUiResponse::Input {
            id: *id,
            value: None,
        },
        HostUiRequest::Editor { id, .. } => HostUiResponse::Editor {
            id: *id,
            value: None,
        },
    }
}

fn encode_terminal_input(event: &UiEvent) -> Option<String> {
    match event {
        UiEvent::Paste(text) => Some(text.clone()),
        UiEvent::Key(key) => encode_key_event(key),
        UiEvent::FocusGained | UiEvent::FocusLost | UiEvent::Resize { .. } => None,
    }
}

fn decode_terminal_input(data: String) -> UiEvent {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let key = match data.as_str() {
        "\r" | "\n" => Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        "\t" => Some(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        "\u{7f}" | "\u{8}" => Some(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        "\u{1b}" => Some(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        "\u{1b}[A" => Some(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        "\u{1b}[B" => Some(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        "\u{1b}[C" => Some(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
        "\u{1b}[D" => Some(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
        "\u{1b}[H" => Some(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
        "\u{1b}[F" => Some(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
        "\u{1b}[3~" => Some(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
        "\u{1b}[Z" => Some(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
        _ if data.starts_with('\u{1b}') && data.chars().count() == 2 => data
            .chars()
            .nth(1)
            .map(|character| KeyEvent::new(KeyCode::Char(character), KeyModifiers::ALT)),
        _ => {
            let mut characters = data.chars();
            match (characters.next(), characters.next()) {
                (Some(character), None) if (character as u32) < 0x20 => {
                    let letter = char::from((character as u8) | 0x60);
                    Some(KeyEvent::new(KeyCode::Char(letter), KeyModifiers::CONTROL))
                }
                (Some(character), None) => {
                    Some(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                }
                _ => None,
            }
        }
    };
    key.map_or(UiEvent::Paste(data), UiEvent::Key)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use futures::future::BoxFuture;
    use pi_ai::{AssistantContent, AssistantMessage, TextContent};
    use pi_tui::component::UiEvent;
    use pi_tui::terminal::caps::TerminalCapabilities;
    use pi_tui::terminal::writer::Tui;
    use ratatui::layout::{Position, Size};
    use tokio::sync::{Mutex, mpsc, watch};

    use super::*;
    use crate::core::agent_session::events::AgentSessionEvent;
    use crate::modes::interactive::state::SelectorKind;

    type TestResult = Result<(), String>;

    /// The startup closure in `cli::entry` offloads detection with
    /// `tokio::task::spawn_blocking`; the blocking pool must yield exactly the
    /// options a direct synchronous call produces (the tmux hyperlink probe is
    /// cached process-wide, so a second detection observes the same answer).
    #[tokio::test]
    async fn detect_offloaded_matches_sync_detect() -> TestResult {
        let sync = InteractiveRuntimeOptions::detect();
        let offloaded = tokio::task::spawn_blocking(InteractiveRuntimeOptions::detect)
            .await
            .map_err(|error| format!("spawn_blocking join failed: {error}"))?;
        assert_eq!(sync.caps, offloaded.caps);
        assert_eq!(sync.terminal_theme, offloaded.terminal_theme);
        Ok(())
    }

    /// Records every action dispatched to it; tests assert on the call log.
    #[derive(Default)]
    struct ActionLog {
        prompts: Mutex<Vec<String>>,
        bash_started: Notify,
        bash_release: Notify,
        prompt_behaviors: Mutex<Vec<Option<StreamingBehavior>>>,
        aborts: Mutex<u32>,
        compacts: Mutex<Vec<Option<String>>>,
        cycles: Mutex<u32>,
        reloads: Mutex<u32>,
        bashes: Mutex<Vec<(String, bool)>>,
        new_sessions: Mutex<u32>,
        forks: Mutex<Vec<String>>,
        clones: Mutex<u32>,
        switches: Mutex<Vec<String>>,
        logout_ids: Mutex<Vec<String>>,
        imports: Mutex<Vec<String>>,
        shares: Mutex<u32>,
        follows: Mutex<Vec<String>>,
        steers: Mutex<Vec<String>>,
        last_text: Mutex<Option<String>>,
        themes: std::sync::Mutex<Vec<(String, ThemeMode)>>,
        settings_changes: std::sync::Mutex<Vec<(String, String)>>,
        first_runs: std::sync::Mutex<Vec<crate::core::platform::first_run::FirstRunSelection>>,
    }

    struct FakeHost {
        log: Arc<ActionLog>,
        partial_tx: watch::Sender<Option<Arc<AssistantMessage>>>,
        snapshot: Arc<std::sync::Mutex<SessionSnapshot>>,
        event_senders: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<AgentSessionEvent>>>>,
        stream_chunks: Arc<AtomicUsize>,
        logout_options: Arc<std::sync::Mutex<Vec<super::state::LogoutOption>>>,
        clone_nothing: Arc<std::sync::atomic::AtomicBool>,
        cancel_new: Arc<std::sync::atomic::AtomicBool>,
        cancel_fork: Arc<std::sync::atomic::AtomicBool>,
        cancel_switch: Arc<std::sync::atomic::AtomicBool>,
        fork_selected_text: Arc<std::sync::Mutex<Option<String>>>,
        reload_diagnostics: Arc<std::sync::Mutex<Vec<String>>>,
        extension_runner: Option<Arc<ExtensionRuntimeSet>>,
    }

    impl FakeHost {
        fn new() -> (Self, Arc<ActionLog>) {
            let log = Arc::new(ActionLog::default());
            let (partial_tx, _partial_rx) = watch::channel(None);
            let host = Self {
                log: Arc::clone(&log),
                partial_tx,
                snapshot: Arc::new(std::sync::Mutex::new(SessionSnapshot::default())),
                event_senders: Arc::new(std::sync::Mutex::new(Vec::new())),
                stream_chunks: Arc::new(AtomicUsize::new(0)),
                logout_options: Arc::new(std::sync::Mutex::new(Vec::new())),
                clone_nothing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                cancel_new: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                cancel_fork: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                cancel_switch: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                fork_selected_text: Arc::new(std::sync::Mutex::new(None)),
                reload_diagnostics: Arc::new(std::sync::Mutex::new(Vec::new())),
                extension_runner: None,
            };
            (host, log)
        }

        fn set_stream_chunks(&self, chunks: usize) {
            self.stream_chunks.store(chunks, Ordering::SeqCst);
        }

        fn set_logout_options(&self, options: Vec<super::state::LogoutOption>) {
            *self
                .logout_options
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = options;
        }

        fn set_clone_nothing(&self, nothing: bool) {
            self.clone_nothing.store(nothing, Ordering::SeqCst);
        }

        fn set_cancel_new(&self, cancel: bool) {
            self.cancel_new.store(cancel, Ordering::SeqCst);
        }

        fn set_cancel_fork(&self, cancel: bool) {
            self.cancel_fork.store(cancel, Ordering::SeqCst);
        }

        fn set_cancel_switch(&self, cancel: bool) {
            self.cancel_switch.store(cancel, Ordering::SeqCst);
        }

        fn set_fork_selected_text(&self, text: Option<String>) {
            *self
                .fork_selected_text
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = text;
        }

        fn set_reload_diagnostics(&self, diagnostics: Vec<String>) {
            *self
                .reload_diagnostics
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = diagnostics;
        }
    }

    impl SessionHost for FakeHost {
        fn snapshot(&self) -> SessionSnapshot {
            self.snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn host_extension_runner(&self) -> Option<Arc<ExtensionRuntimeSet>> {
            self.extension_runner.clone()
        }

        fn theme_settings(&self) -> (Option<String>, ThemeMode) {
            let themes = self
                .log
                .themes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            themes
                .last()
                .map_or((None, ThemeMode::Auto), |(name, mode)| {
                    (Some(name.clone()), *mode)
                })
        }

        fn persist_theme(&self, theme: &str, mode: ThemeMode) -> Result<(), String> {
            self.log
                .themes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((theme.to_owned(), mode));
            Ok(())
        }

        fn apply_settings_change(&self, id: &str, value: &str) -> Result<(), String> {
            self.log
                .settings_changes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((id.to_owned(), value.to_owned()));
            if id == "theme" {
                let (_, mode) = self.theme_settings();
                self.persist_theme(
                    &crate::modes::interactive::theme::theme_selection_to_storage(value),
                    mode,
                )?;
            }
            Ok(())
        }

        fn persist_first_run(
            &self,
            selection: &crate::core::platform::first_run::FirstRunSelection,
        ) -> Result<(), String> {
            self.log
                .first_runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(selection.clone());
            Ok(())
        }

        fn subscribe(&self) -> EventSubscription {
            let (tx, rx) = mpsc::unbounded_channel();
            self.event_senders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(tx);
            EventSubscription {
                rx,
                unsubscribe: None,
            }
        }

        fn partial_rx(&self) -> watch::Receiver<Option<Arc<AssistantMessage>>> {
            self.partial_tx.subscribe()
        }

        fn prompt(&self, text: &str, opts: PromptOptions) -> BoxFuture<'_, Result<(), String>> {
            let log = Arc::clone(&self.log);
            let owned = text.to_owned();
            let partial_tx = self.partial_tx.clone();
            let snapshot = Arc::clone(&self.snapshot);
            let stream_chunks = Arc::clone(&self.stream_chunks);
            Box::pin(async move {
                log.prompts.lock().await.push(owned);
                log.prompt_behaviors
                    .lock()
                    .await
                    .push(opts.streaming_behavior);
                if opts.streaming_behavior.is_some() {
                    return Ok(());
                }

                let chunks = stream_chunks.load(Ordering::SeqCst);
                if chunks == 0 {
                    return Ok(());
                }
                snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .activity = SessionActivity::Streaming;
                for index in 0..chunks {
                    let text = if index + 1 == chunks {
                        "<<Done>>".to_owned()
                    } else {
                        format!("stream-chunk-{index:02}")
                    };
                    let mut message = AssistantMessage::new("test", "test", "test", 0);
                    message
                        .content
                        .push(AssistantContent::Text(TextContent::new(text)));
                    partial_tx.send_replace(Some(Arc::new(message)));
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                snapshot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .activity = SessionActivity::Idle;
                Ok(())
            })
        }

        fn steer(&self, text: &str) -> BoxFuture<'_, Result<(), String>> {
            let log = Arc::clone(&self.log);
            let owned = text.to_owned();
            Box::pin(async move {
                log.steers.lock().await.push(owned);
                Ok(())
            })
        }

        fn follow_up(&self, text: &str) -> BoxFuture<'_, Result<(), String>> {
            let log = Arc::clone(&self.log);
            let owned = text.to_owned();
            Box::pin(async move {
                log.follows.lock().await.push(owned);
                Ok(())
            })
        }

        fn abort(&self) -> BoxFuture<'static, Result<(), String>> {
            let log = Arc::clone(&self.log);
            Box::pin(async move {
                *log.aborts.lock().await += 1;
                log.bash_release.notify_one();
                Ok(())
            })
        }

        fn compact(&self, instructions: Option<&str>) -> BoxFuture<'_, Result<(), String>> {
            let log = Arc::clone(&self.log);
            let instructions = instructions.map(str::to_owned);
            Box::pin(async move {
                log.compacts.lock().await.push(instructions);
                Ok(())
            })
        }

        fn cycle_thinking_level(&self) -> BoxFuture<'_, Result<(), String>> {
            let log = Arc::clone(&self.log);
            Box::pin(async move {
                *log.cycles.lock().await += 1;
                Ok(())
            })
        }

        fn cycle_model(&self, _forward: bool) -> BoxFuture<'_, Result<(), String>> {
            let log = Arc::clone(&self.log);
            Box::pin(async move {
                *log.cycles.lock().await += 1;
                Ok(())
            })
        }

        fn reload(&self) -> BoxFuture<'_, Result<Vec<String>, String>> {
            let log = Arc::clone(&self.log);
            let diagnostics = Arc::clone(&self.reload_diagnostics);
            Box::pin(async move {
                *log.reloads.lock().await += 1;
                Ok(diagnostics
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone())
            })
        }

        fn execute_bash(&self, command: &str, exclude: bool) -> BoxFuture<'_, Result<(), String>> {
            let log = Arc::clone(&self.log);
            let owned = command.to_owned();
            Box::pin(async move {
                let should_wait = owned == "hang";
                log.bashes.lock().await.push((owned, exclude));
                if should_wait {
                    log.bash_started.notify_one();
                    log.bash_release.notified().await;
                }
                Ok(())
            })
        }

        fn new_session(&self) -> BoxFuture<'_, Result<SwitchOutcome, String>> {
            let log = Arc::clone(&self.log);
            let cancel = self.cancel_new.load(Ordering::SeqCst);
            Box::pin(async move {
                *log.new_sessions.lock().await += 1;
                Ok(SwitchOutcome { cancelled: cancel })
            })
        }

        fn fork(&self, entry_id: &str) -> BoxFuture<'_, Result<ForkOutcome, String>> {
            let log = Arc::clone(&self.log);
            let owned = entry_id.to_owned();
            let cancel = self.cancel_fork.load(Ordering::SeqCst);
            let selected_text = self
                .fork_selected_text
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            Box::pin(async move {
                log.forks.lock().await.push(owned);
                Ok(ForkOutcome {
                    cancelled: cancel,
                    selected_text,
                })
            })
        }

        fn clone(&self) -> BoxFuture<'_, Result<CloneOutcome, String>> {
            if self.clone_nothing.load(Ordering::SeqCst) {
                return Box::pin(async { Ok(CloneOutcome::NothingToClone) });
            }
            let log = Arc::clone(&self.log);
            Box::pin(async move {
                *log.clones.lock().await += 1;
                Ok(CloneOutcome::Cloned)
            })
        }

        fn switch_session(&self, path: &str) -> BoxFuture<'_, Result<SwitchOutcome, String>> {
            let log = Arc::clone(&self.log);
            let owned = path.to_owned();
            let cancel = self.cancel_switch.load(Ordering::SeqCst);
            Box::pin(async move {
                log.switches.lock().await.push(owned);
                Ok(SwitchOutcome { cancelled: cancel })
            })
        }

        fn export_html(&self, _path: Option<&str>) -> BoxFuture<'_, Result<String, String>> {
            Box::pin(async { Ok("<html></html>".to_owned()) })
        }

        fn export_jsonl(&self, path: Option<&str>) -> BoxFuture<'_, Result<String, String>> {
            let owned = path.map(str::to_owned);
            Box::pin(async move { Ok(owned.unwrap_or_else(|| "session.jsonl".to_owned())) })
        }

        fn import_jsonl(
            &self,
            path: &str,
            _cwd_override: Option<&str>,
        ) -> BoxFuture<'_, Result<bool, ImportError>> {
            let log = Arc::clone(&self.log);
            let owned = path.to_owned();
            Box::pin(async move {
                log.imports.lock().await.push(owned);
                Ok(true)
            })
        }

        fn share(&self) -> BoxFuture<'_, Result<(String, String), String>> {
            let log = Arc::clone(&self.log);
            Box::pin(async move {
                *log.shares.lock().await += 1;
                Ok((
                    "https://viewer.example/abc".to_owned(),
                    "https://gist.github.com/u/abc".to_owned(),
                ))
            })
        }

        fn session_stats(&self) -> BoxFuture<'_, crate::core::agent_session::stats::SessionStats> {
            Box::pin(async {
                crate::core::agent_session::stats::SessionStats {
                    session_file: None,
                    session_id: "test-session".to_owned(),
                    user_messages: 0,
                    assistant_messages: 0,
                    tool_calls: 0,
                    tool_results: 0,
                    total_messages: 0,
                    tokens: crate::core::agent_session::stats::SessionTokenTotals::default(),
                    cost: 0.0,
                    context_usage: None,
                }
            })
        }

        fn set_session_name(&self, name: &str) -> BoxFuture<'_, Result<Option<String>, String>> {
            // Mirror the real manager's whitespace normalization so tests can
            // exercise the normalization warning.
            let normalized = name.split_whitespace().collect::<Vec<_>>().join(" ");
            Box::pin(async move { Ok((!normalized.is_empty()).then_some(normalized)) })
        }

        fn logout(&self, provider_id: &str) -> BoxFuture<'_, Result<(), String>> {
            let log = Arc::clone(&self.log);
            let id = provider_id.to_owned();
            Box::pin(async move {
                log.logout_ids.lock().await.push(id);
                Ok(())
            })
        }

        fn logout_provider_options(
            &self,
        ) -> BoxFuture<'_, Result<Vec<super::state::LogoutOption>, String>> {
            let options = self
                .logout_options
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            Box::pin(async move { Ok(options) })
        }

        fn messages(&self) -> Vec<pi_agent::AgentMessage> {
            Vec::new()
        }

        fn get_model_entries(
            &self,
        ) -> BoxFuture<'_, Result<Vec<super::state::ModelSelectorEntry>, String>> {
            Box::pin(async {
                Ok(vec![super::state::ModelSelectorEntry {
                    value: "test/model".to_owned(),
                    label: "Test Model".to_owned(),
                    description: None,
                }])
            })
        }

        fn get_session_entries(
            &self,
        ) -> BoxFuture<'_, Result<Vec<super::state::SessionPickerEntry>, String>> {
            Box::pin(async {
                Ok(vec![super::state::SessionPickerEntry {
                    value: "/tmp/sess.jsonl".to_owned(),
                    label: "fixture session".to_owned(),
                    description: None,
                }])
            })
        }

        fn get_tree_entries(&self) -> BoxFuture<'_, Result<Vec<super::state::TreeEntry>, String>> {
            Box::pin(async {
                Ok(vec![super::state::TreeEntry {
                    value: "root".to_owned(),
                    label: "root".to_owned(),
                    depth: 0,
                }])
            })
        }

        fn get_fork_entries(&self) -> BoxFuture<'_, Result<Vec<super::state::TreeEntry>, String>> {
            Box::pin(async {
                Ok(vec![super::state::TreeEntry {
                    value: "user-1".to_owned(),
                    label: "hello".to_owned(),
                    depth: 0,
                }])
            })
        }

        fn get_trust_entries(
            &self,
        ) -> BoxFuture<'_, Result<Vec<super::state::SettingsRow>, String>> {
            Box::pin(async {
                Ok(vec![super::state::SettingsRow {
                    id: "defaultProjectTrust".to_owned(),
                    label: "Default project trust".to_owned(),
                    description: None,
                    current_value: "ask".to_owned(),
                    values: Some(vec![
                        "ask".to_owned(),
                        "always".to_owned(),
                        "never".to_owned(),
                    ]),
                }])
            })
        }

        fn get_auth_entries(
            &self,
        ) -> BoxFuture<'_, Result<Vec<super::state::AuthSelectorEntry>, String>> {
            Box::pin(async {
                Ok(vec![super::state::AuthSelectorEntry {
                    value: "anthropic".to_owned(),
                    label: "Anthropic".to_owned(),
                    description: Some("configured".to_owned()),
                }])
            })
        }

        fn get_scoped_models_entries(
            &self,
        ) -> BoxFuture<
            '_,
            Result<
                (
                    Vec<super::state::ModelSelectorEntry>,
                    std::collections::BTreeMap<String, bool>,
                ),
                String,
            >,
        > {
            Box::pin(async {
                let mut enabled = std::collections::BTreeMap::new();
                enabled.insert("test/model".to_owned(), true);
                Ok((
                    vec![super::state::ModelSelectorEntry {
                        value: "test/model".to_owned(),
                        label: "Test Model".to_owned(),
                        description: None,
                    }],
                    enabled,
                ))
            })
        }

        fn get_settings_entries(
            &self,
        ) -> BoxFuture<'_, Result<Vec<super::state::SettingsRow>, String>> {
            Box::pin(async {
                Ok(vec![super::state::SettingsRow {
                    id: "theme".to_owned(),
                    label: "Theme".to_owned(),
                    description: None,
                    current_value: "dark".to_owned(),
                    values: Some(vec!["dark".to_owned(), "light".to_owned()]),
                }])
            })
        }

        fn get_config_entries(
            &self,
        ) -> BoxFuture<'_, Result<Vec<super::state::SettingsRow>, String>> {
            Box::pin(async {
                Ok(vec![super::state::SettingsRow {
                    id: "quietStartup".to_owned(),
                    label: "Quiet startup".to_owned(),
                    description: None,
                    current_value: "off".to_owned(),
                    values: Some(vec!["on".to_owned(), "off".to_owned()]),
                }])
            })
        }

        fn last_assistant_text(&self) -> BoxFuture<'_, Result<Option<String>, String>> {
            let log = Arc::clone(&self.log);
            Box::pin(async move {
                let t = log.last_text.lock().await.clone();
                Ok(t)
            })
        }
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> UiEvent {
        UiEvent::Key(KeyEvent::new(code, mods))
    }

    fn try_make_runtime()
    -> Result<(InteractiveRuntime<SharedWriter, FakeHost>, Arc<ActionLog>), String> {
        let writer = SharedWriter::new();
        let caps = TerminalCapabilities::default();
        let tui = Tui::new(writer, Size::new(80, 24), Position::ORIGIN, 8, caps)
            .map_err(|error| format!("tui construction: {error}"))?;
        let (_tx, rx) = mpsc::unbounded_channel::<UiEvent>();
        let input = TerminalInput::mock(rx);
        let (host, log) = FakeHost::new();
        let options = InteractiveRuntimeOptions {
            size: (80, 24),
            ..InteractiveRuntimeOptions::default()
        };
        let mut rt = InteractiveRuntime::new(tui, input, Arc::new(host), &options);
        let _ = rt.paint_now();
        Ok((rt, log))
    }

    #[tokio::test]
    async fn bash_stays_interruptible_and_rejects_overlap() -> Result<(), String> {
        let (mut rt, log) = try_make_runtime()?;
        let _ = rt.dispatch_bash("hang", false).await;
        let _ = rt.dispatch_bash("second", false).await;
        assert_eq!(
            rt.last_error.as_deref(),
            Some("a bash command is already running")
        );
        tokio::time::timeout(Duration::from_secs(1), log.bash_started.notified())
            .await
            .map_err(|_| "bash operation did not start".to_owned())?;
        assert_eq!(rt.view.editor.border, EditorBorder::Bash);

        let _ = rt.dispatch_interrupt().await;
        assert_eq!(*log.aborts.lock().await, 1);
        assert_eq!(
            log.bashes.lock().await.as_slice(),
            &[("hang".to_owned(), false)]
        );
        let completion = tokio::time::timeout(
            Duration::from_secs(1),
            rt.prompt_operations.tasks.join_next(),
        )
        .await
        .map_err(|_| "bash operation did not finish after abort".to_owned())?
        .ok_or_else(|| "bash operation task was missing".to_owned())?;
        assert!(rt.handle_prompt_completion(completion));
        assert_eq!(rt.view.editor.border, EditorBorder::Muted);
        Ok(())
    }

    fn make_runtime() -> (InteractiveRuntime<SharedWriter, FakeHost>, Arc<ActionLog>) {
        match try_make_runtime() {
            Ok(runtime) => runtime,
            Err(error) => std::panic::resume_unwind(Box::new(error)),
        }
    }

    #[tokio::test]
    async fn extension_theme_set_applies_persists_and_bumps_generation() {
        let (mut rt, log) = make_runtime();
        assert_eq!(rt.view.theme.name, "dark");
        let generation = rt.theme_generation;

        // String form with persist: applies, persists name + inferred mode.
        rt.handle_extension_event(ExtensionUiEvent::ThemeSet(ThemeSet {
            name: Some("classic-light".to_owned()),
            theme: None,
            persist: true,
        }))
        .await;
        assert_eq!(rt.view.theme.name, "classic-light");
        assert_eq!(rt.theme_generation, generation + 1);
        {
            let themes = log
                .themes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(
                *themes,
                vec![("classic-light".to_owned(), ThemeMode::Light)]
            );
        }

        // Object form: applies without persistence.
        let mut fg = std::collections::BTreeMap::new();
        fg.insert(
            "text".to_owned(),
            pi_ext::protocol::ThemeColorValue::Text("#010203".to_owned()),
        );
        rt.handle_extension_event(ExtensionUiEvent::ThemeSet(ThemeSet {
            name: None,
            theme: Some(ThemeWire {
                name: Some("inmem".to_owned()),
                source_path: None,
                color_mode: "truecolor".to_owned(),
                fg,
                bg: std::collections::BTreeMap::new(),
            }),
            persist: false,
        }))
        .await;
        assert_eq!(rt.view.theme.name, "inmem");
        assert_eq!(
            rt.view.theme.fg_rgb(super::super::theme::ThemeColor::Text),
            super::super::theme::Rgb(1, 2, 3)
        );
        assert_eq!(rt.theme_generation, generation + 2);

        // Host failure fallback: literal dark, still no new persistence.
        rt.handle_extension_event(ExtensionUiEvent::ThemeSet(ThemeSet {
            name: Some("dark".to_owned()),
            theme: None,
            persist: false,
        }))
        .await;
        assert_eq!(rt.view.theme.name, "dark");
        let persisted = log
            .themes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert_eq!(persisted, 1);
    }

    #[tokio::test]
    async fn dispatch_submit_calls_prompt_on_host() {
        let (mut rt, log) = make_runtime();
        let _ = rt
            .dispatch_action(ViewAction::Submit {
                text: "hello".to_owned(),
            })
            .await;
        let prompts = log.prompts.lock().await.clone();
        assert_eq!(prompts, vec!["hello".to_owned()]);
    }

    #[tokio::test]
    async fn dispatch_quit_exits_without_prompting() {
        let (mut rt, log) = make_runtime();
        let outcome = rt
            .dispatch_action(ViewAction::Submit {
                text: "/quit".to_owned(),
            })
            .await;
        assert_eq!(outcome, ActionOutcome::Exit);
        assert!(log.prompts.lock().await.is_empty());
    }

    #[tokio::test]
    async fn dispatch_interrupt_calls_abort() {
        let (mut rt, log) = make_runtime();
        let _ = rt.dispatch_action(ViewAction::Interrupt).await;
        assert_eq!(*log.aborts.lock().await, 1);
    }

    #[tokio::test]
    async fn dispatch_compact_passes_through() {
        let (mut rt, log) = make_runtime();
        let _ = rt
            .dispatch_action(ViewAction::Compact {
                instructions: Some("focus on tools".to_owned()),
            })
            .await;
        assert_eq!(
            *log.compacts.lock().await,
            vec![Some("focus on tools".to_owned())]
        );
    }

    #[tokio::test]
    async fn dispatch_bash_routes_to_execute_bash() {
        let (mut rt, log) = make_runtime();
        let _ = rt
            .dispatch_action(ViewAction::SubmitBash {
                command: "ls".to_owned(),
                exclude_from_context: true,
            })
            .await;
        let bashes = log.bashes.lock().await.clone();
        assert_eq!(bashes, vec![("ls".to_owned(), true)]);
    }

    #[tokio::test]
    async fn dispatch_slash_command_with_args() {
        let (mut rt, log) = make_runtime();
        let _ = rt
            .dispatch_action(ViewAction::SlashCommand {
                // Non-builtin command: falls through to the prompt path, which
                // reconstructs `/{name} {args}` for extension/LLM dispatch.
                name: "explain".to_owned(),
                args: "this diff".to_owned(),
            })
            .await;
        let prompts = log.prompts.lock().await.clone();
        assert_eq!(prompts, vec!["/explain this diff".to_owned()]);
    }

    #[tokio::test]
    async fn dispatch_bang_prefix_routes_to_bash_not_prompt() {
        let (mut rt, log) = make_runtime();
        let _ = rt
            .dispatch_action(ViewAction::Submit {
                text: "!ls -la".to_owned(),
            })
            .await;
        let bashes = log.bashes.lock().await.clone();
        assert_eq!(bashes, vec![("ls -la".to_owned(), false)]);
        let prompts = log.prompts.lock().await.clone();
        assert!(prompts.is_empty());
    }

    #[tokio::test]
    async fn dispatch_double_bang_routes_to_excluded_bash() {
        let (mut rt, log) = make_runtime();
        let _ = rt
            .dispatch_action(ViewAction::Submit {
                text: "!!rm -rf /tmp/x".to_owned(),
            })
            .await;
        let bashes = log.bashes.lock().await.clone();
        assert_eq!(bashes, vec![("rm -rf /tmp/x".to_owned(), true)]);
    }

    #[tokio::test]
    async fn dispatch_clear_editor_empties_view() {
        let (mut rt, _log) = make_runtime();
        rt.view.editor.text = "draft".to_owned();
        rt.editor.set_text("draft");
        let _ = rt.dispatch_action(ViewAction::ClearEditor).await;
        assert!(rt.view.editor.text.is_empty());
        assert!(rt.editor.get_text().is_empty());
    }

    #[tokio::test]
    async fn dispatch_open_overlay_sets_focus_to_overlay() {
        let (mut rt, _log) = make_runtime();
        let _ = rt
            .dispatch_action(ViewAction::ShowOverlay {
                kind: OverlayKind::ShortcutHelp,
            })
            .await;
        assert_eq!(rt.view.focus, FocusArea::Overlay);
        assert!(rt.view.overlay.is_some());
    }

    #[tokio::test]
    async fn dispatch_dismiss_overlay_restores_editor_focus() {
        let (mut rt, _log) = make_runtime();
        let _ = rt
            .dispatch_action(ViewAction::ShowOverlay {
                kind: OverlayKind::Changelog,
            })
            .await;
        assert_eq!(rt.view.focus, FocusArea::Overlay);
        let _ = rt.dispatch_action(ViewAction::DismissOverlay).await;
        assert_eq!(rt.view.focus, FocusArea::Editor);
        assert!(rt.view.overlay.is_none());
    }

    #[tokio::test]
    async fn project_event_agent_start_sets_streaming_status() {
        let mut view = ViewState::empty();
        project_event(&mut view, &AgentSessionEvent::AgentStart);
        assert!(view.streaming);
        assert!(view.status.is_some());
    }

    #[tokio::test]
    async fn project_event_agent_end_clears_status() {
        let mut view = ViewState::empty();
        project_event(&mut view, &AgentSessionEvent::AgentStart);
        project_event(
            &mut view,
            &AgentSessionEvent::AgentEnd {
                messages: Vec::new(),
                will_retry: false,
            },
        );
        assert!(!view.streaming);
        assert!(view.status.is_none());
    }

    #[test]
    fn project_snapshot_projects_summarizing_and_pending_queues() {
        let mut view = ViewState::empty();
        let snapshot = SessionSnapshot {
            activity: SessionActivity::Summarizing,
            steering: vec!["steer".to_owned()],
            follow_up: vec!["later".to_owned()],
            follow_up_mode: super::state::QueueMode::All,
            ..SessionSnapshot::default()
        };
        project_snapshot(&mut view, &snapshot, None);
        assert_eq!(
            view.status.as_ref().map(|status| status.kind),
            Some(StatusKind::BranchSummary)
        );
        assert_eq!(view.pending.steering[0].text, "steer");
        assert_eq!(view.pending.follow_up[0].text, "later");
        assert_eq!(view.pending.follow_up_mode, super::state::QueueMode::All);
    }

    #[test]
    fn project_footer_sets_stats_billing_and_border_from_one_snapshot() {
        let mut view = ViewState::empty();
        project_footer(
            &mut view,
            &SessionFooterSnapshot {
                total_input: 10,
                total_output: 20,
                total_cache_read: 30,
                total_cache_write: 40,
                total_cost: 1.25,
                context_window: 200,
                context_percent: Some(50.0),
                provider: Some("provider".to_owned()),
                provider_count: 2,
                thinking_level: pi_ai::ModelThinkingLevel::High,
                subscription: true,
                auto_compact: false,
                ..SessionFooterSnapshot::default()
            },
        );
        assert_eq!(view.footer.total_input, 10);
        assert_eq!(view.footer.total_output, 20);
        assert_eq!(view.footer.total_cache_read, 30);
        assert_eq!(view.footer.total_cache_write, 40);
        assert!((view.footer.total_cost - 1.25).abs() <= f64::EPSILON);
        assert_eq!(view.footer.context_percent, Some(50.0));
        assert_eq!(view.footer.provider.as_deref(), Some("provider"));
        assert_eq!(view.footer.provider_count, 2);
        assert_eq!(view.footer.flags.billing, BillingMode::Subscription);
        assert!(!view.footer.flags.auto_compact);
        assert_eq!(
            view.editor.border,
            EditorBorder::Thinking(pi_ai::ModelThinkingLevel::High)
        );
    }

    #[tokio::test]
    async fn project_event_queue_update_syncs_pending_lists() {
        let mut view = ViewState::empty();
        project_event(
            &mut view,
            &AgentSessionEvent::QueueUpdate {
                steering: vec!["s1".to_owned()],
                follow_up: vec!["f1".to_owned(), "f2".to_owned()],
            },
        );
        assert_eq!(view.pending.steering.len(), 1);
        assert_eq!(view.pending.follow_up.len(), 2);
    }

    #[tokio::test]
    async fn project_event_compaction_start_sets_status() -> Result<(), String> {
        let mut view = ViewState::empty();
        project_event(
            &mut view,
            &AgentSessionEvent::CompactionStart {
                reason: crate::core::agent_session::events::CompactionReason::Manual,
            },
        );
        let status = view.status.as_ref().ok_or("compaction status not set")?;
        assert_eq!(status.kind, StatusKind::Compaction);
        Ok(())
    }

    #[tokio::test]
    async fn project_event_auto_retry_start_sets_retry_status() -> Result<(), String> {
        let mut view = ViewState::empty();
        project_event(
            &mut view,
            &AgentSessionEvent::AutoRetryStart {
                attempt: 2,
                max_attempts: 5,
                delay_ms: 2000,
                error_message: "x".to_owned(),
            },
        );
        let status = view.status.as_ref().ok_or("retry status not set")?;
        assert_eq!(status.kind, StatusKind::Retry);
        Ok(())
    }

    #[tokio::test]
    async fn project_event_summarization_retry_scheduled_sets_retry_status_and_error()
    -> Result<(), String> {
        let mut view = ViewState::empty();
        project_event(
            &mut view,
            &AgentSessionEvent::SummarizationRetryScheduled {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 2000,
                error_message: "overloaded".to_owned(),
            },
        );
        let status = view.status.as_ref().ok_or("retry status not set")?;
        assert_eq!(status.kind, StatusKind::Retry);
        assert!(status.message.contains("Retrying (1/3)"));
        // showError equivalent: an error message is pushed to the chat.
        let last = view.messages.last().ok_or("no message pushed")?;
        match last {
            MessageView::Custom(custom) => {
                assert_eq!(custom.custom_type, "error");
                assert!(custom.text.contains("overloaded"));
            }
            _ => return Err(format!("expected Custom error message, got {last:?}")),
        }
        Ok(())
    }

    #[tokio::test]
    async fn project_event_summarization_retry_attempt_start_branch_summary() -> Result<(), String>
    {
        let mut view = ViewState::empty();
        project_event(
            &mut view,
            &AgentSessionEvent::SummarizationRetryAttemptStart {
                source: crate::core::agent_session::events::SummarizationRetrySource::BranchSummary,
            },
        );
        let status = view
            .status
            .as_ref()
            .ok_or("branch-summary status not set")?;
        assert_eq!(status.kind, StatusKind::BranchSummary);
        Ok(())
    }

    #[tokio::test]
    async fn project_event_summarization_retry_attempt_start_compaction() -> Result<(), String> {
        let mut view = ViewState::empty();
        project_event(
            &mut view,
            &AgentSessionEvent::SummarizationRetryAttemptStart {
                source: crate::core::agent_session::events::SummarizationRetrySource::Compaction {
                    reason: crate::core::agent_session::events::CompactionReason::Manual,
                },
            },
        );
        let status = view.status.as_ref().ok_or("compaction status not set")?;
        assert_eq!(status.kind, StatusKind::Compaction);
        Ok(())
    }

    #[tokio::test]
    async fn project_event_summarization_retry_finished_clears_status() -> Result<(), String> {
        let mut view = ViewState::empty();
        view.status = Some(SessionStatus {
            kind: StatusKind::Retry,
            frame: 0,
            elapsed_secs: 0,
            message: "Retrying…".to_owned(),
        });
        project_event(&mut view, &AgentSessionEvent::SummarizationRetryFinished);
        assert!(
            view.status.is_none(),
            "status should be cleared after retry finished"
        );
        Ok(())
    }

    #[tokio::test]
    async fn project_snapshot_streaming_state_projects_to_view() -> Result<(), String> {
        let mut view = ViewState::empty();
        let snap = SessionSnapshot {
            activity: SessionActivity::Streaming,
            ..SessionSnapshot::default()
        };
        project_snapshot(&mut view, &snap, None);
        assert!(view.streaming);
        let status = view.status.as_ref().ok_or("working status not set")?;
        assert_eq!(status.kind, StatusKind::Working);
        Ok(())
    }

    #[tokio::test]
    async fn step_ui_ctrl_l_opens_model_selector() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        rt.step_ui(key(KeyCode::Char('l'), KeyModifiers::CONTROL))
            .await
            .map_err(|error| format!("model selector step failed: {error}"))?;
        assert_eq!(rt.view.focus, FocusArea::Selector);
        Ok(())
    }

    #[tokio::test]
    async fn step_ui_ctrl_z_requests_suspend() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        rt.step_ui(key(KeyCode::Char('z'), KeyModifiers::CONTROL))
            .await
            .map_err(|error| format!("suspend step failed: {error}"))?;
        assert!(rt.exited);
        assert_eq!(rt.exit_kind, InteractiveExit::Suspend);
        Ok(())
    }

    #[tokio::test]
    async fn step_ui_resize_updates_tui_size_cache() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        rt.step_ui(UiEvent::Resize {
            width: 100,
            height: 40,
        })
        .await
        .map_err(|error| format!("resize step failed: {error}"))?;
        assert_eq!(rt.tui.size(), Size::new(100, 40));
        assert_eq!(rt.view.width, 100);
        assert_eq!(rt.view.height, 40);
        Ok(())
    }

    #[tokio::test]
    async fn step_ui_paste_inserts_into_editor() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        rt.step_ui(UiEvent::Paste("hello paste".to_owned()))
            .await
            .map_err(|error| format!("paste step failed: {error}"))?;
        assert_eq!(rt.editor.get_text(), "hello paste");
        assert_eq!(rt.view.editor.text, "hello paste");
        Ok(())
    }

    #[tokio::test]
    async fn step_session_event_agent_start_marks_streaming() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        rt.step_session_event(AgentSessionEvent::AgentStart)
            .await
            .map_err(|error| format!("session event step failed: {error}"))?;
        assert!(rt.view.streaming);
        assert!(rt.view.status.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn flush_coalescer_clears_deadline_and_paints() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        rt.arm_coalescer();
        assert!(rt.coalesce_deadline.is_some());
        rt.flush_coalescer()
            .map_err(|error| format!("coalescer flush failed: {error}"))?;
        assert!(rt.coalesce_deadline.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn prompt_stream_paints_an_intermediate_chunk_before_done() -> Result<(), String> {
        let writer = SharedWriter::new();
        let captured = writer.clone();
        let tui = Tui::new(
            writer,
            Size::new(80, 24),
            Position::ORIGIN,
            8,
            TerminalCapabilities::default(),
        )
        .map_err(|error| format!("tui construction failed: {error}"))?;
        let (_input_tx, input_rx) = mpsc::unbounded_channel::<UiEvent>();
        let input = TerminalInput::mock(input_rx);
        let (host, _log) = FakeHost::new();
        host.set_stream_chunks(16);
        let options = InteractiveRuntimeOptions {
            size: (80, 24),
            ..InteractiveRuntimeOptions::default()
        };
        let mut rt = InteractiveRuntime::new(tui, input, Arc::new(host), &options);

        let _ = rt
            .dispatch_action(ViewAction::Submit {
                text: "stream".to_owned(),
            })
            .await;
        let shutdown_flag = Arc::clone(&rt.shutdown_flag);
        let shutdown = Arc::clone(&rt.shutdown);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(75)).await;
            shutdown_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            shutdown.notify_one();
        });

        let exit = tokio::time::timeout(Duration::from_millis(500), rt.run())
            .await
            .map_err(|_| "runtime blocked on prompt".to_owned())?
            .map_err(|error| format!("runtime failed: {error}"))?;
        assert_eq!(exit, InteractiveExit::Clean);

        let output = String::from_utf8_lossy(&captured.snapshot()).into_owned();
        let intermediate = output
            .find("stream-chunk-")
            .ok_or("no intermediate streaming frame")?;
        let done = output.rfind("Done").ok_or("no final Done frame")?;
        assert!(
            intermediate < done,
            "intermediate frame must be written before Done"
        );
        Ok(())
    }

    #[tokio::test]
    async fn rapid_second_submit_reenters_prompt_with_streaming_behavior() -> Result<(), String> {
        let writer = SharedWriter::new();
        let tui = Tui::new(
            writer,
            Size::new(80, 24),
            Position::ORIGIN,
            8,
            TerminalCapabilities::default(),
        )
        .map_err(|error| format!("tui construction failed: {error}"))?;
        let (_input_tx, input_rx) = mpsc::unbounded_channel::<UiEvent>();
        let input = TerminalInput::mock(input_rx);
        let (host, log) = FakeHost::new();
        host.set_stream_chunks(16);
        let mut rt = InteractiveRuntime::new(
            tui,
            input,
            Arc::new(host),
            &InteractiveRuntimeOptions::default(),
        );

        let _ = rt
            .dispatch_action(ViewAction::Submit {
                text: "first".to_owned(),
            })
            .await;
        let _ = rt
            .dispatch_action(ViewAction::Submit {
                text: "second".to_owned(),
            })
            .await;

        assert_eq!(
            *log.prompt_behaviors.lock().await,
            vec![None, Some(StreamingBehavior::Steer)]
        );
        rt.quiesce_prompt_operations().await;
        Ok(())
    }

    #[tokio::test]
    async fn session_replacement_aborts_and_drains_prompt_operations() -> Result<(), String> {
        let writer = SharedWriter::new();
        let tui = Tui::new(
            writer,
            Size::new(80, 24),
            Position::ORIGIN,
            8,
            TerminalCapabilities::default(),
        )
        .map_err(|error| format!("tui construction failed: {error}"))?;
        let (_input_tx, input_rx) = mpsc::unbounded_channel::<UiEvent>();
        let input = TerminalInput::mock(input_rx);
        let (host, log) = FakeHost::new();
        host.set_stream_chunks(8);
        let mut rt = InteractiveRuntime::new(
            tui,
            input,
            Arc::new(host),
            &InteractiveRuntimeOptions::default(),
        );
        let _ = rt
            .dispatch_action(ViewAction::Submit {
                text: "old session".to_owned(),
            })
            .await;

        let _ = rt.dispatch_action(ViewAction::NewSession).await;

        assert_eq!(*log.aborts.lock().await, 1);
        assert_eq!(*log.new_sessions.lock().await, 1);
        assert!(rt.prompt_operations.tasks.is_empty());
        assert!(rt.prompt_operations.aborts.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn viewport_bottom_row_tracks_terminal_resize() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        assert_eq!(rt.viewport_bottom_row(), 23);

        rt.step_ui(UiEvent::Resize {
            width: 100,
            height: 41,
        })
        .await
        .map_err(|error| format!("resize step failed: {error}"))?;

        assert_eq!(rt.viewport_bottom_row(), 40);
        Ok(())
    }
    #[test]
    fn editor_only_repaint_reuses_long_transcript_chat_components() -> io::Result<()> {
        let (mut rt, _log) = make_runtime();
        rt.view.messages = (0..1_000)
            .map(|index| {
                MessageView::User(crate::modes::interactive::messages::UserMessageView {
                    text: format!("message {index} with **markdown**"),
                })
            })
            .collect();
        rt.chat_prefix_cache = None;
        rt.chat_prefix_len = usize::MAX;
        rt.chat_tail_cache = None;
        rt.chat_dirty = true;
        rt.paint_frame()?;

        let prefix_before = rt
            .chat_prefix_cache
            .as_deref()
            .map(|component| std::ptr::from_ref(component).cast::<()>())
            .ok_or_else(|| io::Error::other("missing prefix cache"))?;
        let tail_before = rt
            .chat_tail_cache
            .as_deref()
            .map(|component| std::ptr::from_ref(component).cast::<()>())
            .ok_or_else(|| io::Error::other("missing tail cache"))?;

        rt.editor.set_text("editor-only change");
        rt.view.editor.text = "editor-only change".to_owned();
        rt.paint_frame()?;

        assert_eq!(rt.chat_prefix_len, 999);
        assert_eq!(
            rt.chat_prefix_cache
                .as_deref()
                .map(|component| std::ptr::from_ref(component).cast::<()>()),
            Some(prefix_before)
        );
        assert_eq!(
            rt.chat_tail_cache
                .as_deref()
                .map(|component| std::ptr::from_ref(component).cast::<()>()),
            Some(tail_before)
        );
        Ok(())
    }

    #[test]
    fn installed_product_panic_hook_emits_complete_restore_sequence() -> io::Result<()> {
        const CHILD_ENV: &str = "PI_TEST_PRODUCT_PANIC_HOOK_PATH";
        if let Some(path) = std::env::var_os(CHILD_ENV) {
            let writer = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            let emergency = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let _restore = install_product_panic_emergency_hook(emergency, writer);
            // The fixture MUST execute the installed panic hook;
            // `resume_unwind` deliberately bypasses hooks, so an explicit
            // panic is the only honest trigger. Test-only lint exception.
            #[allow(clippy::panic)]
            {
                panic!("intentional product panic-hook fixture");
            }
        }

        let directory = tempfile::tempdir()?;
        let capture = directory.path().join("panic-restore.bin");
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "modes::interactive::runtime::tests::installed_product_panic_hook_emits_complete_restore_sequence",
                "--nocapture",
            ])
            .env(CHILD_ENV, &capture)
            .output()?;
        assert!(
            !output.status.success(),
            "panic fixture unexpectedly succeeded"
        );
        assert_eq!(
            std::fs::read(capture)?,
            b"\x1b[?2026l\x1b[<u\x1b[?2004l\x1b[?1004l\x1b[?2031l\x1b[?25h\x1b[0m"
        );
        Ok(())
    }

    #[tokio::test]
    async fn shared_writer_aggregates_writes_from_two_handles() -> Result<(), String> {
        let writer = SharedWriter::new();
        let mut a = writer.clone();
        let mut b = writer.clone();
        a.write_all(b"hello")
            .map_err(|error| format!("first write failed: {error}"))?;
        b.write_all(b" world")
            .map_err(|error| format!("second write failed: {error}"))?;
        assert_eq!(writer.snapshot(), b"hello world");
        Ok(())
    }

    #[tokio::test]
    async fn open_overlay_then_dismiss_restores_focus_and_clears_state() {
        let (mut rt, _log) = make_runtime();
        rt.input_state
            .set_last_sigint_for_test(Some(std::time::Instant::now()));
        let _ = rt
            .dispatch_action(ViewAction::ShowOverlay {
                kind: OverlayKind::Login,
            })
            .await;
        let _ = rt.dispatch_action(ViewAction::DismissOverlay).await;
        assert!(rt.input_state.last_sigint().is_none());
        assert!(rt.input_state.last_escape().is_none());
        assert_eq!(rt.view.focus, FocusArea::Editor);
    }

    #[tokio::test]
    async fn select_confirmed_session_invokes_switch_session() {
        let (mut rt, log) = make_runtime();
        let _ = rt
            .dispatch_action(ViewAction::SelectConfirmed {
                selector: SelectorKind::Session,
                value: "/tmp/sess.json".to_owned(),
            })
            .await;
        let switches = log.switches.lock().await.clone();
        assert_eq!(switches, vec!["/tmp/sess.json".to_owned()]);
        assert_eq!(rt.view.focus, FocusArea::Editor);
    }

    #[tokio::test]
    async fn select_cancelled_restores_editor_focus() {
        let (mut rt, _log) = make_runtime();
        rt.view.focus = FocusArea::Selector;
        let _ = rt.dispatch_action(ViewAction::SelectCancelled).await;
        assert_eq!(rt.view.focus, FocusArea::Editor);
    }

    #[tokio::test]
    async fn draw_timeout_constant_matches_master_plan() {
        assert_eq!(DRAW_TIMEOUT, Duration::from_secs(5));
    }

    #[tokio::test]
    async fn coalesce_window_constant_matches_master_plan() {
        assert_eq!(BACKGROUND_COALESCE_WINDOW, Duration::from_millis(16));
    }

    #[tokio::test]
    async fn enqueue_settle_runs_on_next_loop_turn() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        rt.enqueue_settle(vec![settled_lines(vec![Line::raw("settled")])]);
        assert!(rt.pending_settle.is_some());
        // Simulate the loop post-turn processing.
        if let Some(blocks) = rt.pending_settle.take() {
            rt.commit_settle(blocks)
                .map_err(|error| format!("settle commit failed: {error}"))?;
        }
        assert!(rt.pending_settle.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn request_shutdown_exits_main_loop_cleanly() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        rt.request_shutdown();
        let exit = tokio::time::timeout(Duration::from_millis(500), rt.run())
            .await
            .map_err(|_| "runtime did not return after shutdown".to_owned())?
            .map_err(|error| format!("runtime shutdown failed: {error}"))?;
        assert_eq!(exit, InteractiveExit::Clean);
        Ok(())
    }

    /// The paused-clock counterpart to the frozen-spinner regression: with a
    /// status visible, each [`SPINNER_TICK`] must advance `view.status.frame`,
    /// and a full second of ticks must surface in `elapsed_secs`.
    #[tokio::test(start_paused = true)]
    async fn spinner_tick_advances_status_frame() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        rt.view.status = Some(SessionStatus {
            kind: StatusKind::Working,
            frame: 0,
            elapsed_secs: 0,
            message: "Working…".to_owned(),
        });
        let frames = pi_tui::components::DEFAULT_LOADER_FRAMES.len();
        for expected in 1..=2_usize {
            tokio::time::advance(SPINNER_TICK).await;
            assert!(
                rt.tick_status_indicator(),
                "tick {expected} reported no change"
            );
            let status = rt.view.status.as_ref().ok_or("status vanished")?;
            assert_eq!(status.frame, expected % frames);
        }
        // 13 more ticks cross the 1-second boundary (15 × 80 ms = 1.2 s).
        for _ in 0..13 {
            tokio::time::advance(SPINNER_TICK).await;
            rt.tick_status_indicator();
        }
        let status = rt.view.status.as_ref().ok_or("status vanished")?;
        assert_eq!(status.elapsed_secs, 1);
        assert_eq!(status.frame, 15 % frames);
        Ok(())
    }

    /// T2: the persisted spinner deadline advances by exactly one
    /// [`SPINNER_TICK`] per tick, so a `select!` arm that recreates its sleep
    /// every turn cannot starve the cadence. (Select starvation itself is not
    /// unit-testable; this harness pins the deadline-advance invariant the
    /// fix relies on — see `spinner_deadline`.)
    #[tokio::test(start_paused = true)]
    async fn spinner_deadline_advances_one_tick_per_frame() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        rt.view.status = Some(SessionStatus {
            kind: StatusKind::Working,
            frame: 0,
            elapsed_secs: 0,
            message: "Working…".to_owned(),
        });
        // Seed the deadline the way the run loop does — `arm_spinner_deadline`
        // reconciles the spinner clock for the new status (sets the kind) and
        // seeds the next deadline — then simulate ticks.
        rt.arm_spinner_deadline();
        let mut expected = rt.spinner_deadline.ok_or("deadline seeded")?;
        for tick in 1..=2_usize {
            tokio::time::advance(SPINNER_TICK).await;
            // Mirrors the run-loop arm body: advance from the fired deadline.
            let fired = rt
                .spinner_deadline
                .ok_or("deadline persists across turns")?;
            rt.spinner_deadline = Some(fired + SPINNER_TICK);
            assert!(
                rt.tick_status_indicator(),
                "tick {tick} should report a change"
            );
            expected += SPINNER_TICK;
            assert_eq!(rt.spinner_deadline, Some(expected));
        }
        Ok(())
    }

    /// T3+T6: replacing `view.status` with a different kind resets the spinner
    /// clock inside `tick_status_indicator` (the single, unbypassable point),
    /// so a new phase counts up from 0s even when `set_status` is bypassed.
    #[tokio::test(start_paused = true)]
    async fn spinner_clock_resets_on_kind_change_in_tick() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        rt.view.status = Some(SessionStatus {
            kind: StatusKind::Working,
            frame: 0,
            elapsed_secs: 0,
            message: "Working…".to_owned(),
        });
        // Run the Working clock past the 1-second boundary (15 × 80 ms).
        for _ in 0..15 {
            tokio::time::advance(SPINNER_TICK).await;
            rt.tick_status_indicator();
        }
        let working = rt.view.status.as_ref().ok_or("status vanished")?;
        assert_eq!(working.elapsed_secs, 1, "Working clock should reach 1s");

        // Host bypasses `set_status` and writes a different kind directly.
        rt.view.status = Some(SessionStatus {
            kind: StatusKind::Retry,
            frame: 9,
            elapsed_secs: 5,
            message: "Retrying…".to_owned(),
        });
        tokio::time::advance(SPINNER_TICK).await;
        assert!(
            rt.tick_status_indicator(),
            "tick after kind change should report a change"
        );
        let status = rt.view.status.as_ref().ok_or("status vanished")?;
        assert_eq!(
            status.elapsed_secs, 0,
            "clock must restart for the new kind"
        );
        assert_eq!(status.frame, 1, "frame must restart at 1 for the new kind");
        Ok(())
    }

    /// R3: when `view.status` becomes `None`, the spinner clock must be fully
    /// cleared so a same-kind status reappearing later — with no tick fired
    /// during the gap — counts up from 0s instead of inheriting the old time.
    /// The run loop reconciles every turn via `arm_spinner_deadline`; this
    /// test drives that path the way the loop would.
    #[tokio::test(start_paused = true)]
    async fn spinner_clock_resets_after_status_gap() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        rt.view.status = Some(SessionStatus {
            kind: StatusKind::Working,
            frame: 0,
            elapsed_secs: 0,
            message: "Working…".to_owned(),
        });
        // Run the Working clock past the 1-second boundary, reconciling and
        // ticking each turn as the run loop does (15 × 80 ms).
        for _ in 0..15 {
            rt.arm_spinner_deadline();
            tokio::time::advance(SPINNER_TICK).await;
            rt.tick_status_indicator();
        }
        let working = rt.view.status.as_ref().ok_or("status vanished")?;
        assert_eq!(working.elapsed_secs, 1, "Working clock should reach 1s");

        // Status disappears; the loop reconciles next turn, clearing the clock.
        rt.view.status = None;
        rt.arm_spinner_deadline();
        // No tick fires during the gap.

        // A new SAME-kind Working status appears.
        rt.view.status = Some(SessionStatus {
            kind: StatusKind::Working,
            frame: 0,
            elapsed_secs: 0,
            message: "Working again…".to_owned(),
        });
        rt.arm_spinner_deadline();
        tokio::time::advance(SPINNER_TICK).await;
        rt.tick_status_indicator();

        let status = rt.view.status.as_ref().ok_or("status vanished")?;
        assert_eq!(
            status.elapsed_secs, 0,
            "clock must restart from 0 after a status gap"
        );
        Ok(())
    }

    /// R3: an A→B→A kind flip before any tick must still reset the clock for
    /// the returning kind. Without reconciling on each transition the clock
    /// would inherit phase A's elapsed time (and a stale frame) when the timer
    /// finally fires.
    #[tokio::test(start_paused = true)]
    async fn spinner_clock_resets_on_kind_round_trip() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        rt.view.status = Some(SessionStatus {
            kind: StatusKind::Working,
            frame: 0,
            elapsed_secs: 0,
            message: "Working…".to_owned(),
        });
        for _ in 0..15 {
            rt.arm_spinner_deadline();
            tokio::time::advance(SPINNER_TICK).await;
            rt.tick_status_indicator();
        }
        assert_eq!(
            rt.view
                .status
                .as_ref()
                .ok_or("status vanished")?
                .elapsed_secs,
            1,
            "Working clock should reach 1s"
        );

        // A → B: flip to Retry, reconciling for the new kind before any tick.
        rt.view.status = Some(SessionStatus {
            kind: StatusKind::Retry,
            frame: 9,
            elapsed_secs: 5,
            message: "Retrying…".to_owned(),
        });
        rt.arm_spinner_deadline();

        // B → A: flip straight back to Working, still before any tick.
        rt.view.status = Some(SessionStatus {
            kind: StatusKind::Working,
            frame: 0,
            elapsed_secs: 0,
            message: "Working again…".to_owned(),
        });
        rt.arm_spinner_deadline();
        tokio::time::advance(SPINNER_TICK).await;
        rt.tick_status_indicator();

        let status = rt.view.status.as_ref().ok_or("status vanished")?;
        assert_eq!(
            status.elapsed_secs, 0,
            "clock must restart from 0 after A→B→A"
        );
        assert_eq!(status.frame, 1, "frame must restart at 1 after A→B→A");
        Ok(())
    }

    #[tokio::test]
    async fn plain_enter_submits_via_on_submit_channel() -> Result<(), String> {
        let (mut rt, log) = try_make_runtime()?;
        rt.submit_tx
            .send("hello enter".to_owned())
            .map_err(|error| format!("submit channel closed: {error}"))?;
        rt.step_ui(key(KeyCode::F(24), KeyModifiers::NONE))
            .await
            .map_err(|error| format!("submit step failed: {error}"))?;
        let prompts = log.prompts.lock().await.clone();
        assert_eq!(prompts, vec!["hello enter".to_owned()]);
        Ok(())
    }

    #[tokio::test]
    async fn rebind_session_channels_reloads_snapshot() {
        let (mut rt, _log) = make_runtime();
        rt.view.streaming = true;
        rt.rebind_session_channels().await;
        assert!(!rt.view.streaming);
    }

    #[tokio::test]
    async fn open_model_selector_installs_component_and_focus() {
        let (mut rt, _log) = make_runtime();
        let outcome = rt.dispatch_action(ViewAction::OpenModelSelector).await;
        assert_eq!(outcome, ActionOutcome::Repaint);
        assert_eq!(rt.view.focus, FocusArea::Selector);
        assert!(rt.active_selector.is_some());
        assert_eq!(rt.active_selector_kind, Some(SelectorKind::Model));
    }

    #[tokio::test]
    async fn selector_confirm_channel_routes_to_switch_session() -> Result<(), String> {
        let (mut rt, log) = try_make_runtime()?;
        let _ = rt.dispatch_action(ViewAction::OpenSessionPicker).await;
        rt.select_tx
            .send((SelectorKind::Session, "/tmp/from-select.jsonl".to_owned()))
            .map_err(|error| format!("selector channel closed: {error}"))?;
        rt.step_ui(key(KeyCode::F(24), KeyModifiers::NONE))
            .await
            .map_err(|error| format!("selector step failed: {error}"))?;
        let switches = log.switches.lock().await.clone();
        assert_eq!(switches, vec!["/tmp/from-select.jsonl".to_owned()]);
        assert_eq!(rt.view.focus, FocusArea::Editor);
        assert!(rt.active_selector.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn streaming_submit_uses_prompt_with_steer_behavior() -> Result<(), String> {
        let writer = SharedWriter::new();
        let caps = TerminalCapabilities::default();
        let tui = Tui::new(writer, Size::new(80, 24), Position::ORIGIN, 8, caps)
            .map_err(|error| format!("tui construction failed: {error}"))?;
        let (_tx, rx) = mpsc::unbounded_channel::<UiEvent>();
        let input = TerminalInput::mock(rx);
        let (host, log) = FakeHost::new();
        *host
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SessionSnapshot {
            activity: SessionActivity::Streaming,
            ..SessionSnapshot::default()
        };
        let options = InteractiveRuntimeOptions {
            size: (80, 24),
            ..InteractiveRuntimeOptions::default()
        };
        let mut rt = InteractiveRuntime::new(tui, input, Arc::new(host), &options);
        let _ = rt
            .dispatch_action(ViewAction::Submit {
                text: "steer me".to_owned(),
            })
            .await;
        let prompts = log.prompts.lock().await.clone();
        assert_eq!(prompts, vec!["steer me".to_owned()]);
        assert!(log.steers.lock().await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn suspend_action_sets_suspend_exit_not_clean_shutdown() {
        let (mut rt, _log) = make_runtime();
        let outcome = rt.dispatch_action(ViewAction::Suspend).await;
        assert_eq!(outcome, ActionOutcome::Suspend);
        assert!(!rt.shutdown_flag.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn external_editor_action_requests_outer_terminal_handoff() {
        let (mut rt, _log) = make_runtime();
        let outcome = rt.dispatch_action(ViewAction::ExternalEditor).await;
        assert_eq!(outcome, ActionOutcome::ExternalEditor);
    }

    #[tokio::test]
    async fn display_toggles_update_existing_assistant_and_tool_messages() {
        let (mut rt, _log) = make_runtime();
        rt.view
            .messages
            .push(MessageView::Assistant(AssistantMessageView {
                message: AssistantMessage::new(
                    "test-api",
                    "test-provider",
                    "test-model",
                    pi_agent::now_millis(),
                ),
                hide_thinking: false,
                hidden_thinking_label: "Thinking hidden".to_owned(),
                streaming: false,
            }));
        project_event(
            &mut rt.view,
            &AgentSessionEvent::ToolExecutionStart {
                tool_call_id: "tool-1".to_owned(),
                tool_name: "read".to_owned(),
                args: serde_json::Map::new(),
            },
        );

        assert_eq!(
            rt.dispatch_action(ViewAction::ToggleThinking).await,
            ActionOutcome::Repaint
        );
        assert_eq!(
            rt.dispatch_action(ViewAction::ToggleToolExpand).await,
            ActionOutcome::Repaint
        );
        assert!(
            rt.view.messages.iter().any(
                |message| matches!(message, MessageView::Assistant(view) if view.hide_thinking)
            )
        );
        assert!(
            rt.view
                .messages
                .iter()
                .any(|message| matches!(message, MessageView::Tool(view) if view.state.expanded))
        );
    }

    #[test]
    fn effective_extension_shortcuts_reject_invalid_reserved_and_use_last_registration() {
        use pi_ext::adapters::ShortcutRegistration;

        let shortcuts = build_effective_extension_shortcuts(&[
            ShortcutRegistration {
                key: "ctrl+not-a-key".to_owned(),
                description: Some("invalid".to_owned()),
                extension_path: Some("invalid.ts".to_owned()),
            },
            ShortcutRegistration {
                key: "ctrl+c".to_owned(),
                description: Some("reserved".to_owned()),
                extension_path: Some("reserved.ts".to_owned()),
            },
            ShortcutRegistration {
                key: "alt+ctrl+y".to_owned(),
                description: Some("first".to_owned()),
                extension_path: Some("first.ts".to_owned()),
            },
            ShortcutRegistration {
                key: "CTRL+ALT+Y".to_owned(),
                description: Some("last".to_owned()),
                extension_path: Some("last.ts".to_owned()),
            },
        ]);

        assert_eq!(shortcuts.len(), 1);
        assert_eq!(shortcuts[0].key, "ctrl+alt+y");
        assert_eq!(shortcuts[0].dispatch_key, "CTRL+ALT+Y");
        assert_eq!(shortcuts[0].description.as_deref(), Some("last"));
        assert_eq!(shortcuts[0].source.as_deref(), Some("last.ts"));
        let non_reserved = KeyEvent::new(
            KeyCode::Char('y'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert!(key_matches_parsed(&non_reserved, &shortcuts[0].parsed));
        let hints = shortcut_hints(&shortcuts);
        assert_eq!(hints[0].action, "last");
    }

    #[tokio::test]
    async fn reserved_extension_conflict_falls_through_to_native_binding() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        rt.editor.set_text("draft");
        rt.view.editor.text = "draft".to_owned();
        rt.effective_extension_shortcuts =
            build_effective_extension_shortcuts(&[pi_ext::adapters::ShortcutRegistration {
                key: "ctrl+c".to_owned(),
                description: Some("must not run".to_owned()),
                extension_path: Some("extension.ts".to_owned()),
            }]);
        assert!(rt.effective_extension_shortcuts.is_empty());

        rt.step_ui(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .map_err(|error| format!("native fallthrough failed: {error}"))?;
        assert!(rt.editor.get_text().is_empty());
        Ok(())
    }

    #[test]
    fn focused_slot_projection_retains_generation_and_typed_key_payload() {
        let (mut rt, _log) = make_runtime();
        let slot = pi_ext::sanitize::sanitize_slot(&pi_ext::protocol::UiSlot {
            key: "editor.status".to_owned(),
            generation: 7,
            placement: SlotPlacement::AboveEditor,
            height: 1,
            runs: vec![vec![pi_ext::protocol::StyledRun {
                text: "focused".to_owned(),
                style: pi_ext::protocol::Style::default(),
            }]],
            focusable: true,
            cursor: None,
            overlay_options: None,
        });
        rt.project_extension_slot(slot);
        assert_eq!(rt.focused_extension_slot.as_deref(), Some("editor.status"));
        assert_eq!(rt.view.focus, FocusArea::Widget);
        assert_eq!(
            rt.extension_slots
                .get("editor.status")
                .map(|slot| slot.generation),
            Some(7)
        );

        let event = UiEvent::Key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::ALT,
            crossterm::event::KeyEventKind::Repeat,
        ));
        assert_eq!(
            ui_event_wire(&event),
            UiEventWire::Key {
                code: "enter".to_owned(),
                modifiers: KeyModifiersWire {
                    alt: Some(true),
                    ..KeyModifiersWire::default()
                },
                kind: KeyEventKindWire::Repeat,
            }
        );
        assert_eq!(
            encode_terminal_input(&event).as_deref(),
            Some("\u{1b}[13;3:2u")
        );
    }

    #[test]
    fn non_capturing_overlay_preserves_editor_focus_and_structured_metadata() -> Result<(), String>
    {
        let (mut rt, _log) = make_runtime();
        let link = pi_ext::protocol::Hyperlink {
            id: Some("docs".to_owned()),
            uri: "https://example.com/docs".to_owned(),
        };
        let slot = pi_ext::sanitize::sanitize_slot(&pi_ext::protocol::UiSlot {
            key: "overlay.help".to_owned(),
            generation: 3,
            placement: SlotPlacement::Overlay,
            height: 1,
            runs: vec![vec![pi_ext::protocol::StyledRun {
                text: "help".to_owned(),
                style: pi_ext::protocol::Style {
                    bold: Some(true),
                    link: Some(link.clone()),
                    ..pi_ext::protocol::Style::default()
                },
            }]],
            focusable: true,
            cursor: None,
            overlay_options: Some(pi_ext::protocol::OverlaySpec {
                non_capturing: true,
                ..pi_ext::protocol::OverlaySpec::default()
            }),
        });

        rt.project_extension_slot(slot);
        assert_eq!(rt.view.focus, FocusArea::Editor);
        assert!(rt.focused_extension_slot.is_none());
        let projected = rt
            .view
            .extension_overlay_slot
            .as_ref()
            .ok_or_else(|| "structured overlay was not projected".to_owned())?;
        assert_eq!(projected.lines[0][0].style.link.as_ref(), Some(&link));
        Ok(())
    }

    /// T7: republishing the same overlay key with a different geometry
    /// re-anchors (its reshaped rows cover unrelated previous content), while
    /// an identical-geometry republish does not (codex PRRT …VM-tM).
    #[test]
    fn overlay_reshape_republish_reanchors() {
        let (mut rt, _log) = make_runtime();
        let overlay = |height: u16, options: Option<pi_ext::protocol::OverlaySpec>| {
            // `sanitize_slot` derives the slot height from the run-line count,
            // so emit one run line per requested row.
            let runs = (0..height)
                .map(|_| {
                    vec![pi_ext::protocol::StyledRun {
                        text: "term".to_owned(),
                        style: pi_ext::protocol::Style::default(),
                    }]
                })
                .collect::<Vec<_>>();
            pi_ext::sanitize::sanitize_slot(&pi_ext::protocol::UiSlot {
                key: "overlay.term".to_owned(),
                generation: 1,
                placement: SlotPlacement::Overlay,
                height,
                runs,
                focusable: false,
                cursor: None,
                overlay_options: options,
            })
        };
        rt.project_extension_slot(overlay(3, None));
        assert_eq!(
            rt.pending_reanchor,
            Some(ReanchorCause::OverlayOpen),
            "overlay open must queue a reanchor"
        );
        rt.pending_reanchor = None;

        // Same key, identical geometry: no reanchor.
        rt.project_extension_slot(overlay(3, None));
        assert_eq!(
            rt.pending_reanchor, None,
            "identical-geometry republish must not reanchor"
        );

        // Same key, different height: reanchor (reshape over unrelated rows).
        rt.project_extension_slot(overlay(5, None));
        assert_eq!(
            rt.pending_reanchor,
            Some(ReanchorCause::OverlayOpen),
            "reshaped overlay must reanchor"
        );
    }

    /// B1: a resize reanchor (`commit_reanchor`) subsumes any queued
    /// overlay-open reanchor, so the next normal frame does not do an extra,
    /// unrelated full-row reanchor.
    #[test]
    fn commit_reanchor_clears_stale_pending_reanchor() -> TestResult {
        let (mut rt, _log) = make_runtime();
        rt.pending_reanchor = Some(ReanchorCause::OverlayOpen);
        rt.commit_reanchor()
            .map_err(|e| format!("commit_reanchor failed: {e}"))?;
        assert_eq!(
            rt.pending_reanchor, None,
            "resize reanchor must subsume the queued overlay reanchor"
        );
        Ok(())
    }

    /// T4: when an edit result carries the numbered diff in
    /// `details["diff"]`, that diff becomes the rendered text (it is strictly
    /// more informative than the "Successfully replaced …" sentence in
    /// `content`). Falls back to `content` when no diff detail exists.
    #[test]
    fn tool_result_view_prefers_diff_detail() {
        let with_diff = pi_agent::AgentToolResult {
            content: vec![pi_ai::ToolResultContent::Text(TextContent::new(
                "Successfully replaced 2 lines",
            ))],
            details: serde_json::json!({ "diff": "+1 added\n-2 removed\n 3 ctx" }),
            ..Default::default()
        };
        let view = tool_result_view(&with_diff, false);
        assert_eq!(view.text, "+1 added\n-2 removed\n 3 ctx");

        // No diff detail: fall back to the content sentence.
        let without_diff = pi_agent::AgentToolResult {
            content: vec![pi_ai::ToolResultContent::Text(TextContent::new(
                "Successfully replaced 2 lines",
            ))],
            details: serde_json::json!({}),
            ..Default::default()
        };
        let view = tool_result_view(&without_diff, false);
        assert_eq!(view.text, "Successfully replaced 2 lines");
    }

    #[tokio::test]
    async fn extension_input_dialog_temporarily_owns_then_restores_editor() {
        let (mut rt, _log) = make_runtime();
        rt.editor.set_text("draft prompt");
        rt.view.editor.text = "draft prompt".to_owned();
        rt.view.editor.placeholder = "Type a message…".to_owned();
        rt.begin_extension_dialog(HostUiRequest::Input {
            id: 17,
            request: pi_ext::protocol::InputRequest {
                title: "Extension input".to_owned(),
                placeholder: Some("value".to_owned()),
                options_meta: pi_ext::protocol::DialogOptions::default(),
            },
        })
        .await;
        assert_eq!(rt.editor.get_text(), "");
        assert_eq!(rt.view.editor.placeholder, "value");

        let outcome = rt.submit_text("answer".to_owned(), false).await;
        assert_eq!(outcome, ActionOutcome::Repaint);
        assert!(rt.pending_extension_dialog.is_none());
        assert_eq!(rt.editor.get_text(), "draft prompt");
        assert_eq!(rt.view.editor.placeholder, "Type a message…");
    }

    #[tokio::test]
    async fn reload_cancels_pending_extension_dialog_and_restores_editor() {
        let (mut rt, _log) = make_runtime();
        rt.editor.set_text("draft prompt");
        rt.view.editor.text = "draft prompt".to_owned();
        rt.view.editor.placeholder = "Type a message…".to_owned();
        rt.begin_extension_dialog(HostUiRequest::Input {
            id: 18,
            request: pi_ext::protocol::InputRequest {
                title: "Extension input".to_owned(),
                placeholder: None,
                options_meta: pi_ext::protocol::DialogOptions::default(),
            },
        })
        .await;

        let outcome = rt.dispatch_action(ViewAction::Reload).await;
        assert_eq!(outcome, ActionOutcome::Repaint);
        assert!(rt.pending_extension_dialog.is_none());
        assert_eq!(rt.editor.get_text(), "draft prompt");
        assert_eq!(rt.view.editor.placeholder, "Type a message…");
    }

    #[tokio::test]
    async fn interactive_reload_surfaces_nonfatal_extension_diagnostics() -> TestResult {
        let writer = SharedWriter::new();
        let caps = TerminalCapabilities::default();
        let tui = Tui::new(writer, Size::new(80, 24), Position::ORIGIN, 8, caps)
            .map_err(|error| error.to_string())?;
        let (_tx, rx) = mpsc::unbounded_channel::<UiEvent>();
        let input = TerminalInput::mock(rx);
        let (host, _log) = FakeHost::new();
        host.set_reload_diagnostics(vec![
            "Extension \"first.ts\" error: flag rejected".to_owned(),
            "Extension \"second.ts\" error: provider rejected".to_owned(),
        ]);
        let options = InteractiveRuntimeOptions {
            size: (80, 24),
            ..InteractiveRuntimeOptions::default()
        };
        let mut runtime = InteractiveRuntime::new(tui, input, Arc::new(host), &options);

        assert_eq!(
            runtime.dispatch_action(ViewAction::Reload).await,
            ActionOutcome::Repaint
        );
        let notices = runtime
            .view
            .messages
            .iter()
            .filter_map(|message| match message {
                MessageView::Custom(custom) if custom.custom_type == "reload" => {
                    Some(custom.text.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            notices,
            [
                "Extension \"first.ts\" error: flag rejected",
                "Extension \"second.ts\" error: provider rejected",
            ]
        );
        assert!(runtime.last_error.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn extension_confirmation_renders_title_and_message() {
        let (mut rt, _log) = make_runtime();
        rt.begin_extension_dialog(HostUiRequest::Confirm {
            id: 19,
            request: pi_ext::protocol::ConfirmRequest {
                title: "Verification confirm prompt".to_owned(),
                message: "Choose Yes".to_owned(),
                options_meta: pi_ext::protocol::DialogOptions::default(),
            },
        })
        .await;
        let editor = std::mem::replace(&mut rt.editor, Editor::with_defaults());
        let selector = rt.active_selector.take();
        let mut root = rt.build_root(editor, selector);
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);

        root.render(area, &mut buffer);

        let visible = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(visible.contains("Verification confirm prompt"));
        assert!(visible.contains("Choose Yes"));
    }

    #[tokio::test]
    async fn rebind_extension_channels_preserves_requests_across_same_source_reload() -> TestResult
    {
        let runner = ExtensionRuntimeSet::bind(Vec::new());
        let writer = SharedWriter::new();
        let caps = TerminalCapabilities::default();
        let tui = Tui::new(writer, Size::new(80, 24), Position::ORIGIN, 8, caps)
            .map_err(|error| error.to_string())?;
        let (_tx, rx) = mpsc::unbounded_channel::<UiEvent>();
        let input = TerminalInput::mock(rx);
        let (mut host, _log) = FakeHost::new();
        host.extension_runner = Some(runner);
        let options = InteractiveRuntimeOptions {
            size: (80, 24),
            ..InteractiveRuntimeOptions::default()
        };
        let mut rt = InteractiveRuntime::new(tui, input, Arc::new(host), &options);
        let (request_tx, request_rx) = mpsc::channel(1);
        rt.extension_requests = Some(request_rx);

        // Product reload rebinds this same source after the session refresh.
        assert_eq!(
            rt.dispatch_action(ViewAction::Reload).await,
            ActionOutcome::Repaint
        );
        assert!(rt.extension_requests.is_some());
        assert_eq!(
            rt.dispatch_action(ViewAction::Reload).await,
            ActionOutcome::Repaint
        );
        assert!(rt.extension_requests.is_some());

        request_tx
            .send(HostUiRequest::Confirm {
                id: 41,
                request: pi_ext::protocol::ConfirmRequest {
                    title: "Still connected".to_owned(),
                    message: "Receive after rebind".to_owned(),
                    options_meta: pi_ext::protocol::DialogOptions::default(),
                },
            })
            .await
            .map_err(|_| "preserved receiver disconnected".to_owned())?;
        let received = rt
            .extension_requests
            .as_mut()
            .ok_or_else(|| "receiver missing after second rebind".to_owned())?
            .recv()
            .await
            .ok_or_else(|| "request channel closed after second rebind".to_owned())?;
        assert!(matches!(received, HostUiRequest::Confirm { id: 41, .. }));
        Ok(())
    }

    #[tokio::test]
    async fn endpoint_retirement_refreshes_extension_shortcuts() -> TestResult {
        let (live, _live_host) =
            crate::core::extension_runtime_set::tests::make_runner(serde_json::json!({
                "shortcuts": [
                    {"key": "ctrl+y", "description": "live", "extensionPath": "live.ts"}
                ]
            }))
            .await
            .map_err(|error| error.to_string())?;
        let (dead, dead_host) =
            crate::core::extension_runtime_set::tests::make_runner(serde_json::json!({
                "shortcuts": [
                    {"key": "ctrl+y", "description": "dead", "extensionPath": "dead.ts"},
                    {"key": "ctrl+e", "description": "dead-only", "extensionPath": "dead.ts"}
                ]
            }))
            .await
            .map_err(|error| error.to_string())?;
        let runner = ExtensionRuntimeSet::bind(vec![
            (
                crate::core::extension_runtime_set::EndpointKind::TsCompat,
                live,
            ),
            (
                crate::core::extension_runtime_set::EndpointKind::Native,
                dead,
            ),
        ]);
        let writer = SharedWriter::new();
        let tui = Tui::new(
            writer,
            Size::new(80, 24),
            Position::ORIGIN,
            8,
            TerminalCapabilities::default(),
        )
        .map_err(|error| error.to_string())?;
        let (_tx, rx) = mpsc::unbounded_channel::<UiEvent>();
        let input = TerminalInput::mock(rx);
        let (mut host, _log) = FakeHost::new();
        host.extension_runner = Some(runner.clone());
        let mut rt = InteractiveRuntime::new(
            tui,
            input,
            Arc::new(host),
            &InteractiveRuntimeOptions {
                size: (80, 24),
                ..InteractiveRuntimeOptions::default()
            },
        );
        assert!(rt.effective_extension_shortcuts.iter().any(|shortcut| {
            shortcut.key == "ctrl+e" && shortcut.description.as_deref() == Some("dead-only")
        }));
        assert!(rt.effective_extension_shortcuts.iter().any(|shortcut| {
            shortcut.key == "ctrl+y" && shortcut.description.as_deref() == Some("dead")
        }));

        dead_host.close().await;
        let changed = tokio::time::timeout(
            Duration::from_secs(5),
            wait_extension_registry_change(&mut rt.extension_registry_changes),
        )
        .await
        .map_err(|_| "registry revision did not arrive".to_owned())?;
        assert!(changed, "registry revision channel closed");
        rt.refresh_extension_shortcuts();

        assert!(
            rt.effective_extension_shortcuts
                .iter()
                .all(|shortcut| shortcut.key != "ctrl+e"),
            "dead unique shortcut remained cached"
        );
        assert!(rt.effective_extension_shortcuts.iter().any(|shortcut| {
            shortcut.key == "ctrl+y" && shortcut.description.as_deref() == Some("live")
        }));
        runner.shutdown_once().await;
        Ok(())
    }
    #[test]
    fn extension_slot_update_and_dispose_projects_live_widgets() {
        let (mut rt, _log) = make_runtime();
        let slot = pi_ext::sanitize::sanitize_slot(&pi_ext::protocol::UiSlot {
            key: "status".to_owned(),
            generation: 1,
            placement: SlotPlacement::AboveEditor,
            height: 1,
            runs: vec![vec![pi_ext::protocol::StyledRun {
                text: "extension ready".to_owned(),
                style: pi_ext::protocol::Style::default(),
            }]],
            focusable: false,
            cursor: None,
            overlay_options: None,
        });
        rt.project_extension_slot(slot);
        assert_eq!(rt.view.widgets_above.len(), 1);
        assert_eq!(
            rt.view.widgets_above[0].slot.lines[0][0].text,
            "extension ready"
        );

        rt.dispose_extension_slot("status");
        assert!(rt.view.widgets_above.is_empty());
    }

    #[test]
    fn terminal_input_codec_covers_extension_rewrite_keyspace() -> Result<(), String> {
        let events = [
            UiEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            UiEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT)),
            UiEvent::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            UiEvent::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            UiEvent::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
            UiEvent::Key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)),
        ];
        for event in events {
            let encoded = encode_terminal_input(&event)
                .ok_or_else(|| format!("unsupported event: {event:?}"))?;
            assert_eq!(decode_terminal_input(encoded), event);
        }
        Ok(())
    }

    #[tokio::test]
    async fn copy_last_assistant_produces_feedback() {
        let (mut rt, log) = make_runtime();
        *log.last_text.lock().await = Some("assistant says hi".to_owned());
        let _ = rt.dispatch_action(ViewAction::CopyLastAssistant).await;
        let had_status =
            rt.view.status.as_ref().is_some_and(|s| {
                s.message.contains("Copied") || s.message.contains("No assistant")
            });
        let had_error = rt
            .last_error
            .as_ref()
            .is_some_and(|e| e.contains("clipboard") || e.contains("Failed"));
        assert!(had_status || had_error);
    }

    #[tokio::test]
    async fn project_event_message_start_user_appears_in_chat() {
        let mut view = ViewState::empty();
        let user = pi_agent::user_text("hi from user", std::iter::empty());
        project_event(
            &mut view,
            &AgentSessionEvent::MessageStart { message: user },
        );
        assert!(
            view.messages
                .iter()
                .any(|m| matches!(m, MessageView::User(_)))
        );
    }

    #[tokio::test]
    async fn project_event_tool_start_appears_in_chat() {
        let mut view = ViewState::empty();
        project_event(
            &mut view,
            &AgentSessionEvent::ToolExecutionStart {
                tool_call_id: "t1".to_owned(),
                tool_name: "read".to_owned(),
                args: serde_json::Map::from_iter([(
                    "path".to_owned(),
                    serde_json::Value::String("a.rs".to_owned()),
                )]),
            },
        );
        assert!(
            view.messages
                .iter()
                .any(|m| matches!(m, MessageView::Tool(_)))
        );
    }

    #[test]
    fn root_render_clips_overflow_and_keeps_editor_visible() {
        let mut view = ViewState::empty();
        for index in 0..30 {
            let message = pi_agent::user_text(
                format!("message {index}: {}", "overflow ".repeat(20)),
                std::iter::empty(),
            );
            project_event(&mut view, &AgentSessionEvent::MessageStart { message });
        }
        let mut editor = Editor::with_defaults();
        editor.set_text("EDITOR_VISIBLE");
        let mut root = InteractiveRoot::build(&view, editor, None);
        let area = Rect::new(0, 0, 80, 24);
        let mut buffer = Buffer::empty(area);

        root.render(area, &mut buffer);

        let visible = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(visible.contains("EDITOR_VISIBLE"));
    }

    /// T1: the live editor render path must paint the prompt marker — the
    /// regression was that `build_with_chat` dropped the composed editor
    /// section so interactive users never saw `❯`/`$`. Empty/normal input
    /// shows `❯`; bash-mode input (`!`-prefixed) shows `$`.
    ///
    /// The marker must sit beside the input text on the editor's first BODY
    /// row, not beside the top border. The editor is a bordered box whose top
    /// border occupies its first row, so the marker row must never itself carry
    /// a border glyph (`─`) and must be exactly one row below a `─` border row.
    #[test]
    fn live_editor_renders_prompt_marker() -> Result<(), String> {
        let area = Rect::new(0, 0, 80, 24);

        // Normal input → ❯ marker.
        let view = ViewState::empty();
        let editor = Editor::with_defaults();
        let mut root = InteractiveRoot::build(&view, editor, None);
        let mut buffer = Buffer::empty(area);
        root.render(area, &mut buffer);
        // The marker is painted at column 0; find the body row it lands on.
        let body_row = (0..area.height)
            .position(|y| buffer[(0, y)].symbol() == "❯")
            .ok_or("❯ marker missing at column 0")?;
        let body_row_u16 =
            u16::try_from(body_row).map_err(|_| "marker row overflow".to_string())?;
        let row: String = (0..area.width)
            .map(|x| buffer[(x, body_row_u16)].symbol().to_owned())
            .collect();
        if row.contains('─') {
            return Err(format!("❯ marker on the border row {body_row}: {row:?}"));
        }
        if body_row == 0 {
            return Err("❯ marker has no border row above it".to_string());
        }
        let above: String = (0..area.width)
            .map(|x| buffer[(x, body_row_u16 - 1)].symbol().to_owned())
            .collect();
        if !above.contains('─') {
            return Err(format!(
                "❯ marker body row {body_row} must sit one below the editor top border; row above: {above:?}"
            ));
        }

        // Bash-mode input flips the marker to `$` at the same body-row column.
        let mut bash_editor = Editor::with_defaults();
        bash_editor.set_text("!ls");
        let mut bash_root = InteractiveRoot::build(&view, bash_editor, None);
        let mut bash_buffer = Buffer::empty(area);
        bash_root.render(area, &mut bash_buffer);
        let bash_glyph = bash_buffer[(0, body_row_u16)].symbol();
        if bash_glyph != "$" {
            return Err(format!(
                "bash $ marker missing at column 0, body row {body_row}: got {bash_glyph:?}"
            ));
        }
        Ok(())
    }

    /// R2: `InteractiveRoot::measure` must allocate editor rows at the shifted
    /// width (width - 2), matching what `render_editor_with_marker` actually
    /// renders into. Otherwise wrapped input demands rows that were never
    /// allocated and clips the editor or footer. The two roots share an
    /// identical (empty) view, so every non-editor section cancels; the
    /// total-height difference must equal the editors' shifted-width measure
    /// difference, not their full-width difference (which hides the wrap).
    #[test]
    fn editor_measure_uses_shifted_width() -> Result<(), String> {
        // 39 graphemes fit a width-39 field (single line at width 40) but spill
        // past a width-37 field (two lines at width 38, i.e. width - 2).
        let mut long = Editor::with_defaults();
        long.set_text(&"x".repeat(39));
        let mut short = Editor::with_defaults();
        short.set_text("x");

        // Pre-compute the editors' shifted-width measures before they move
        // into the roots; `measure` is deterministic on (text, width).
        let editor_long_38 = long.measure(38);
        let editor_short_38 = short.measure(38);
        if editor_long_38 <= editor_short_38 {
            return Err(format!(
                "test setup broken: long editor ({editor_long_38}) must need more rows than short ({editor_short_38}) at width 38"
            ));
        }

        let view = ViewState::empty();
        let total_long = InteractiveRoot::build(&view, long, None).measure(40);
        let total_short = InteractiveRoot::build(&view, short, None).measure(40);

        assert_eq!(
            total_long - total_short,
            editor_long_38 - editor_short_38,
            "measure must allocate editor rows at the shifted (width-2) width"
        );
        Ok(())
    }

    #[tokio::test]
    async fn dispatch_slash_compact_without_instructions_calls_compact() {
        let (mut rt, log) = make_runtime();
        let outcome = rt
            .dispatch_action(ViewAction::SlashCommand {
                name: "compact".to_owned(),
                args: String::new(),
            })
            .await;
        assert_eq!(outcome, ActionOutcome::None);
        assert_eq!(*log.compacts.lock().await, vec![None]);
        assert!(log.prompts.lock().await.is_empty());
    }

    #[tokio::test]
    async fn dispatch_typed_compact_trims_custom_instructions() {
        let (mut rt, log) = make_runtime();
        let outcome = rt
            .dispatch_action(ViewAction::Submit {
                text: "  /compact   focus on tools   ".to_owned(),
            })
            .await;
        assert_eq!(outcome, ActionOutcome::None);
        assert_eq!(
            *log.compacts.lock().await,
            vec![Some("focus on tools".to_owned())]
        );
        assert!(log.prompts.lock().await.is_empty());
    }

    #[tokio::test]
    async fn dispatch_typed_fork_opens_user_message_selector() {
        let (mut rt, log) = make_runtime();
        let outcome = rt
            .dispatch_action(ViewAction::Submit {
                text: "/fork".to_owned(),
            })
            .await;
        assert_eq!(outcome, ActionOutcome::Repaint);
        assert_eq!(rt.view.focus, FocusArea::Selector);
        assert!(rt.active_selector.is_some());
        assert_eq!(rt.active_selector_kind, Some(SelectorKind::Fork));
        assert!(log.prompts.lock().await.is_empty());
    }

    #[tokio::test]
    async fn dispatch_typed_resume_opens_session_selector() {
        let (mut rt, log) = make_runtime();
        let outcome = rt
            .dispatch_action(ViewAction::Submit {
                text: "/resume".to_owned(),
            })
            .await;
        assert_eq!(outcome, ActionOutcome::Repaint);
        assert_eq!(rt.view.focus, FocusArea::Selector);
        assert!(rt.active_selector.is_some());
        assert_eq!(rt.active_selector_kind, Some(SelectorKind::Session));
        assert!(log.prompts.lock().await.is_empty());
    }

    #[tokio::test]
    async fn dispatch_typed_reload_awaits_host_and_repaints() {
        let (mut rt, log) = make_runtime();
        let outcome = rt
            .dispatch_action(ViewAction::Submit {
                text: "/reload".to_owned(),
            })
            .await;
        assert_eq!(outcome, ActionOutcome::Repaint);
        assert_eq!(*log.reloads.lock().await, 1);
        assert!(log.prompts.lock().await.is_empty());
    }

    #[tokio::test]
    async fn dispatch_unknown_slash_command_routes_through_prompt() {
        let (mut rt, log) = make_runtime();
        let outcome = rt
            .dispatch_action(ViewAction::SlashCommand {
                name: "foo".to_owned(),
                args: "custom args".to_owned(),
            })
            .await;
        assert_eq!(outcome, ActionOutcome::None);
        assert_eq!(
            *log.prompts.lock().await,
            vec!["/foo custom args".to_owned()]
        );
    }

    fn lock_plain<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[tokio::test]
    async fn debug_intercept_writes_dump_and_pushes_message() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let (mut rt, _log) = try_make_runtime()?;
        rt.debug_dump_dir = dir.path().to_path_buf();
        let outcome = rt.submit_text("/debug".to_owned(), false).await;
        assert_eq!(outcome, ActionOutcome::Repaint);
        let dump = dir.path().join("pi-debug.log");
        assert!(
            dump.exists(),
            "debug dump not written at {}",
            dump.display()
        );
        let Some(MessageView::Custom(custom)) = rt.view.messages.last() else {
            return Err("expected trailing custom debug message".to_owned());
        };
        assert_eq!(custom.custom_type, "debug");
        assert!(custom.text.starts_with("✓ Debug log written\n"));
        assert!(custom.text.contains("pi-debug.log"));
        Ok(())
    }

    #[tokio::test]
    async fn settings_change_theme_persists_and_applies() {
        let (mut rt, log) = make_runtime();
        rt.handle_settings_change("theme", "classic").await;
        assert_eq!(
            *lock_plain(&log.settings_changes),
            vec![("theme".to_owned(), "classic".to_owned())]
        );
        assert_eq!(
            lock_plain(&log.themes).last(),
            Some(&("classic-dark".to_owned(), ThemeMode::Auto))
        );
        assert!(rt.view.theme.name.starts_with("classic"));
    }

    #[tokio::test]
    async fn settings_selector_enter_cycles_value_through_host() -> Result<(), String> {
        let (mut rt, log) = try_make_runtime()?;
        let _ = rt.dispatch_action(ViewAction::OpenSettings).await;
        assert_eq!(rt.active_selector_kind, Some(SelectorKind::Settings));
        // Enter cycles the highlighted row (theme: dark → light) via on_change.
        rt.step_ui(key(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .map_err(|error| format!("settings step failed: {error}"))?;
        assert_eq!(
            *lock_plain(&log.settings_changes),
            vec![("theme".to_owned(), "light".to_owned())]
        );
        Ok(())
    }

    #[tokio::test]
    async fn theme_selector_preview_and_cancel_restore() -> Result<(), String> {
        let (mut rt, _log) = try_make_runtime()?;
        let original = rt.view.theme.name.clone();
        let outcome = rt.open_selector(SelectorKind::Theme).await;
        assert_eq!(outcome, ActionOutcome::Repaint);
        assert_eq!(rt.active_selector_kind, Some(SelectorKind::Theme));
        assert!(rt.theme_preview_restore.is_some());
        rt.preview_theme_selection("classic");
        assert!(rt.view.theme.name.starts_with("classic"));
        let _ = rt.dispatch_action(ViewAction::SelectCancelled).await;
        assert_eq!(rt.view.theme.name, original);
        assert!(rt.theme_preview_restore.is_none());
        assert_eq!(rt.view.focus, FocusArea::Editor);
        Ok(())
    }

    #[tokio::test]
    async fn theme_selector_confirm_persists_selection() {
        let (mut rt, log) = make_runtime();
        let _ = rt.open_selector(SelectorKind::Theme).await;
        let _ = rt
            .dispatch_action(ViewAction::SelectConfirmed {
                selector: SelectorKind::Theme,
                value: "motion".to_owned(),
            })
            .await;
        assert_eq!(
            lock_plain(&log.themes).last(),
            Some(&("motion-dark".to_owned(), ThemeMode::Auto))
        );
        assert!(rt.view.theme.name.starts_with("motion"));
        assert!(rt.theme_preview_restore.is_none());
        assert_eq!(rt.view.focus, FocusArea::Editor);
    }

    #[tokio::test]
    async fn typed_theme_command_opens_theme_selector() {
        let (mut rt, log) = make_runtime();
        let outcome = rt.submit_text("/theme".to_owned(), false).await;
        assert_eq!(outcome, ActionOutcome::Repaint);
        assert_eq!(rt.active_selector_kind, Some(SelectorKind::Theme));
        assert_eq!(rt.view.focus, FocusArea::Selector);
        assert!(log.prompts.lock().await.is_empty());
    }

    #[tokio::test]
    async fn first_run_wizard_persists_selection() {
        let (mut rt, log) = make_runtime();
        rt.open_first_run_wizard();
        assert_eq!(
            rt.view.overlay.as_ref().map(|overlay| overlay.kind),
            Some(OverlayKind::FirstTimeSetup)
        );
        assert_eq!(rt.view.focus, FocusArea::Overlay);
        assert_eq!(rt.view.first_run_step, Some(0));
        // Family: pick the second entry ("classic").
        rt.handle_first_run_key(KeyCode::Down).await;
        rt.handle_first_run_key(KeyCode::Enter).await;
        // Mode: keep Auto.
        rt.handle_first_run_key(KeyCode::Enter).await;
        // Analytics: choose "Don't share".
        rt.handle_first_run_key(KeyCode::Down).await;
        rt.handle_first_run_key(KeyCode::Enter).await;
        assert_eq!(
            *lock_plain(&log.first_runs),
            vec![crate::core::platform::first_run::FirstRunSelection {
                theme: "classic-dark".to_owned(),
                theme_mode: ThemeMode::Auto,
                share_analytics: false,
            }]
        );
        assert!(rt.first_run.is_none());
        assert!(rt.view.overlay.is_none());
        assert_eq!(rt.view.first_run_step, None);
        assert_eq!(rt.view.focus, FocusArea::Editor);
    }

    #[tokio::test]
    async fn first_run_wizard_esc_restores_pre_theme() {
        let (mut rt, log) = make_runtime();
        let original = rt.view.theme.name.clone();
        rt.open_first_run_wizard();
        rt.handle_first_run_key(KeyCode::Down).await;
        assert_ne!(rt.view.theme.name, original, "preview should change theme");
        rt.handle_first_run_key(KeyCode::Esc).await;
        assert_eq!(rt.view.theme.name, original);
        assert!(rt.first_run.is_none());
        assert!(rt.view.overlay.is_none());
        assert!(lock_plain(&log.first_runs).is_empty());
    }

    #[tokio::test]
    async fn all_registered_builtins_are_intercepted() {
        use crate::core::resources::slash::builtin_slash_commands;
        for command in builtin_slash_commands() {
            let (mut rt, log) = make_runtime();
            let input = format!("/{}", command.name);
            let _ = rt.submit_text(input, false).await;
            assert!(
                log.prompts.lock().await.is_empty(),
                "/{} leaked to the LLM prompt path instead of being intercepted",
                command.name
            );
        }
    }

    #[tokio::test]
    async fn slash_settings_opens_settings_selector() {
        let (mut rt, log) = make_runtime();
        let _ = rt.submit_text("/settings".to_owned(), false).await;
        assert_eq!(rt.active_selector_kind, Some(SelectorKind::Settings));
        assert!(log.prompts.lock().await.is_empty());
    }

    #[tokio::test]
    async fn slash_session_pushes_info_notice() {
        let (mut rt, _log) = make_runtime();
        let _ = rt.submit_text("/session".to_owned(), false).await;
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom)) if custom.custom_type == "session"
        ));
    }

    #[tokio::test]
    async fn slash_changelog_opens_changelog_overlay() {
        let (mut rt, _log) = make_runtime();
        let _ = rt.submit_text("/changelog".to_owned(), false).await;
        assert!(matches!(
            rt.view.overlay.as_ref().map(|overlay| overlay.kind),
            Some(OverlayKind::Changelog)
        ));
        assert_eq!(rt.view.focus, FocusArea::Overlay);
    }

    #[tokio::test]
    async fn slash_hotkeys_opens_shortcut_overlay() {
        let (mut rt, _log) = make_runtime();
        let _ = rt.submit_text("/hotkeys".to_owned(), false).await;
        assert!(matches!(
            rt.view.overlay.as_ref().map(|overlay| overlay.kind),
            Some(OverlayKind::ShortcutHelp)
        ));
    }

    #[tokio::test]
    async fn slash_clone_invokes_clone_backend() {
        let (mut rt, log) = make_runtime();
        let _ = rt.submit_text("/clone".to_owned(), false).await;
        assert_eq!(*log.clones.lock().await, 1);
        assert!(log.prompts.lock().await.is_empty());
    }

    #[tokio::test]
    async fn slash_new_invokes_new_session() {
        let (mut rt, log) = make_runtime();
        let _ = rt.submit_text("/new".to_owned(), false).await;
        assert_eq!(*log.new_sessions.lock().await, 1);
    }

    #[tokio::test]
    async fn slash_export_jsonl_routes_to_jsonl_backend() {
        let (mut rt, _log) = make_runtime();
        let _ = rt.submit_text("/export out.jsonl".to_owned(), false).await;
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "export" && custom.text.contains("out.jsonl")
        ));
    }

    #[tokio::test]
    async fn slash_import_without_path_shows_usage() {
        let (mut rt, log) = make_runtime();
        let _ = rt.submit_text("/import".to_owned(), false).await;
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "import" && custom.text.contains("Usage")
        ));
        assert!(log.imports.lock().await.is_empty());
    }

    #[tokio::test]
    async fn slash_name_with_arg_sets_and_confirms() {
        let (mut rt, _log) = make_runtime();
        let _ = rt.submit_text("/name my-session".to_owned(), false).await;
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "name" && custom.text.contains("my-session")
        ));
    }

    #[tokio::test]
    async fn reload_resets_extension_ui() -> TestResult {
        let (mut rt, _log) = try_make_runtime()?;
        rt.view.working_message = Some("Custom…".to_owned());
        rt.view.working_visible = false;
        rt.view
            .footer
            .extension_statuses
            .insert("ext".to_owned(), "busy".to_owned());
        let mut message = AssistantMessage::new("test", "test", "test", 0);
        message
            .content
            .push(AssistantContent::Text(TextContent::new("hi")));
        rt.view
            .messages
            .push(MessageView::Assistant(AssistantMessageView {
                message,
                hide_thinking: true,
                hidden_thinking_label: "Custom label".to_owned(),
                streaming: false,
            }));

        let outcome = rt.dispatch_action(ViewAction::Reload).await;

        assert_eq!(outcome, ActionOutcome::Repaint);
        assert!(rt.view.working_message.is_none());
        assert!(rt.view.working_visible);
        assert!(rt.view.footer.extension_statuses.is_empty());
        assert!(
            rt.view.messages.iter().all(|message| match message {
                MessageView::Assistant(view) => view.hidden_thinking_label == "Thinking…",
                _ => true,
            }),
            "hidden thinking label must reset to default"
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_invalidate_flag_resets_extension_ui() -> TestResult {
        let (mut rt, _log) = try_make_runtime()?;
        rt.view.working_message = Some("Custom…".to_owned());
        rt.view.working_visible = false;
        rt.view
            .footer
            .extension_statuses
            .insert("ext".to_owned(), "busy".to_owned());
        // A plain rebind (no invalidate) must NOT reset extension UI.
        rt.rebind_session_channels().await;
        assert_eq!(rt.view.working_message.as_deref(), Some("Custom…"));
        assert!(!rt.view.working_visible);
        assert!(rt.view.footer.extension_statuses.contains_key("ext"));
        // The before_session_invalidate callback sets the flag; next rebind resets.
        rt.reset_ui_flag.store(true, Ordering::Release);
        rt.rebind_session_channels().await;
        assert!(rt.view.working_message.is_none());
        assert!(rt.view.working_visible);
        assert!(rt.view.footer.extension_statuses.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn reload_guarded_while_streaming_and_compacting() -> TestResult {
        let (mut rt, log) = try_make_runtime()?;
        rt.session
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .activity = SessionActivity::Streaming;
        let _ = rt.dispatch_action(ViewAction::Reload).await;
        assert_eq!(*log.reloads.lock().await, 0);
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "reload" && custom.text.contains("current response")
        ));
        rt.session
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .activity = SessionActivity::Compacting;
        let _ = rt.dispatch_action(ViewAction::Reload).await;
        assert_eq!(*log.reloads.lock().await, 0);
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "reload" && custom.text.contains("compaction")
        ));
        Ok(())
    }

    #[tokio::test]
    async fn import_confirm_decline_cancels() -> TestResult {
        let (mut rt, log) = try_make_runtime()?;
        let _ = rt
            .submit_text("/import session.jsonl".to_owned(), false)
            .await;
        assert_eq!(rt.active_selector_kind, Some(SelectorKind::ImportConfirm));
        let _ = rt
            .dispatch_action(ViewAction::SelectConfirmed {
                selector: SelectorKind::ImportConfirm,
                value: "false".to_owned(),
            })
            .await;
        assert!(log.imports.lock().await.is_empty());
        assert!(rt.active_selector_kind.is_none());
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "import" && custom.text.contains("Import cancelled")
        ));
        Ok(())
    }

    #[tokio::test]
    async fn import_confirm_accept_runs_import() -> TestResult {
        let (mut rt, log) = try_make_runtime()?;
        let _ = rt
            .submit_text("/import session.jsonl".to_owned(), false)
            .await;
        let _ = rt
            .dispatch_action(ViewAction::SelectConfirmed {
                selector: SelectorKind::ImportConfirm,
                value: "true".to_owned(),
            })
            .await;
        assert_eq!(
            log.imports.lock().await.as_slice(),
            &["session.jsonl".to_owned()]
        );
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "import" && custom.text.contains("Session imported from")
        ));
        Ok(())
    }

    #[tokio::test]
    async fn logout_lists_credentials_and_removes_selected() -> TestResult {
        let (mut rt, log) = try_make_runtime()?;
        rt.session.set_logout_options(vec![
            super::state::LogoutOption {
                id: "anthropic".to_owned(),
                name: "Anthropic".to_owned(),
                is_oauth: true,
            },
            super::state::LogoutOption {
                id: "openai".to_owned(),
                name: "OpenAI".to_owned(),
                is_oauth: false,
            },
        ]);
        let _ = rt.dispatch_action(ViewAction::Logout).await;
        assert_eq!(rt.active_selector_kind, Some(SelectorKind::Logout));
        let _ = rt
            .dispatch_action(ViewAction::SelectConfirmed {
                selector: SelectorKind::Logout,
                value: "anthropic".to_owned(),
            })
            .await;
        assert_eq!(
            log.logout_ids.lock().await.as_slice(),
            &["anthropic".to_owned()]
        );
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "logout" && custom.text.contains("Logged out of Anthropic")
        ));
        // Second round exercises the API-key wording.
        let _ = rt.dispatch_action(ViewAction::Logout).await;
        let _ = rt
            .dispatch_action(ViewAction::SelectConfirmed {
                selector: SelectorKind::Logout,
                value: "openai".to_owned(),
            })
            .await;
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "logout"
                    && custom.text.contains("Removed stored API key for OpenAI")
        ));
        Ok(())
    }

    #[tokio::test]
    async fn logout_with_no_credentials_shows_notice() -> TestResult {
        let (mut rt, log) = try_make_runtime()?;
        let _ = rt.dispatch_action(ViewAction::Logout).await;
        assert!(rt.active_selector_kind.is_none());
        assert!(log.logout_ids.lock().await.is_empty());
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "logout" && custom.text.contains("No stored credentials")
        ));
        Ok(())
    }

    #[tokio::test]
    async fn name_warns_when_normalized() -> TestResult {
        let (mut rt, _log) = try_make_runtime()?;
        let _ = rt
            .submit_text("/name  spaced   out ".to_owned(), false)
            .await;
        let notices: Vec<String> = rt
            .view
            .messages
            .iter()
            .filter_map(|message| match message {
                MessageView::Custom(custom) if custom.custom_type == "name" => {
                    Some(custom.text.clone())
                }
                _ => None,
            })
            .collect();
        assert!(
            notices
                .iter()
                .any(|text| text.contains("was normalized from")),
            "expected normalization warning: {notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|text| text.contains("Session name set: spaced out")),
            "expected normalized set notice: {notices:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn clone_guard_shows_nothing_to_clone() -> TestResult {
        let (mut rt, log) = try_make_runtime()?;
        rt.session.set_clone_nothing(true);
        let _ = rt.submit_text("/clone".to_owned(), false).await;
        assert_eq!(*log.clones.lock().await, 0);
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "clone" && custom.text.contains("Nothing to clone yet")
        ));
        Ok(())
    }

    #[tokio::test]
    async fn clone_success_shows_notice() -> TestResult {
        let (mut rt, log) = try_make_runtime()?;
        let _ = rt.submit_text("/clone".to_owned(), false).await;
        assert_eq!(*log.clones.lock().await, 1);
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "clone" && custom.text.contains("Cloned to new session")
        ));
        Ok(())
    }

    #[tokio::test]
    async fn new_session_success_shows_notice() -> TestResult {
        let (mut rt, log) = try_make_runtime()?;
        let before = rt
            .session
            .event_senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let _ = rt.submit_text("/new".to_owned(), false).await;
        assert_eq!(*log.new_sessions.lock().await, 1);
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "new" && custom.text.contains("New session started")
        ));
        let after = rt
            .session
            .event_senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert_eq!(after, before + 1, "success rebinds subscriptions");
        Ok(())
    }

    #[tokio::test]
    async fn new_session_cancelled_suppresses_notice_and_rebind() -> TestResult {
        let (mut rt, log) = try_make_runtime()?;
        rt.session.set_cancel_new(true);
        let before = rt
            .session
            .event_senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let _ = rt.submit_text("/new".to_owned(), false).await;
        assert_eq!(*log.new_sessions.lock().await, 1);
        assert!(
            !rt.view.messages.iter().any(|message| matches!(
                message,
                MessageView::Custom(custom)
                    if custom.custom_type == "new" && custom.text.contains("New session started")
            )),
            "cancelled new session must not show success notice"
        );
        let after = rt
            .session
            .event_senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert_eq!(after, before, "cancelled new session must not rebind");
        Ok(())
    }

    #[tokio::test]
    async fn fork_success_shows_notice() -> TestResult {
        let (mut rt, log) = try_make_runtime()?;
        let before = rt
            .session
            .event_senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let _ = rt
            .dispatch_action(ViewAction::SelectConfirmed {
                selector: SelectorKind::Fork,
                value: "entry-1".to_owned(),
            })
            .await;
        assert_eq!(log.forks.lock().await.as_slice(), &["entry-1".to_owned()]);
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "fork" && custom.text.contains("Forked to new session")
        ));
        let after = rt
            .session
            .event_senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert_eq!(after, before + 1, "success rebinds subscriptions");
        Ok(())
    }

    #[tokio::test]
    async fn fork_success_prefills_selected_text() -> TestResult {
        let (mut rt, _log) = try_make_runtime()?;
        rt.session
            .set_fork_selected_text(Some("prefill text".to_owned()));
        let _ = rt
            .dispatch_action(ViewAction::SelectConfirmed {
                selector: SelectorKind::Fork,
                value: "entry-1".to_owned(),
            })
            .await;
        assert_eq!(rt.view.editor.text, "prefill text");
        Ok(())
    }

    #[tokio::test]
    async fn fork_cancelled_suppresses_notice_and_rebind() -> TestResult {
        let (mut rt, log) = try_make_runtime()?;
        rt.session.set_cancel_fork(true);
        let before = rt
            .session
            .event_senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let _ = rt
            .dispatch_action(ViewAction::SelectConfirmed {
                selector: SelectorKind::Fork,
                value: "entry-1".to_owned(),
            })
            .await;
        assert_eq!(log.forks.lock().await.as_slice(), &["entry-1".to_owned()]);
        assert!(
            !rt.view.messages.iter().any(|message| matches!(
                message,
                MessageView::Custom(custom)
                    if custom.custom_type == "fork" && custom.text.contains("Forked to new session")
            )),
            "cancelled fork must not show success notice"
        );
        let after = rt
            .session
            .event_senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert_eq!(after, before, "cancelled fork must not rebind");
        Ok(())
    }

    #[tokio::test]
    async fn resume_success_shows_notice() -> TestResult {
        let (mut rt, log) = try_make_runtime()?;
        let before = rt
            .session
            .event_senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let _ = rt
            .dispatch_action(ViewAction::SelectConfirmed {
                selector: SelectorKind::Session,
                value: "/tmp/sess.json".to_owned(),
            })
            .await;
        assert_eq!(
            log.switches.lock().await.as_slice(),
            &["/tmp/sess.json".to_owned()]
        );
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "resume" && custom.text.contains("Resumed session")
        ));
        let after = rt
            .session
            .event_senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert_eq!(after, before + 1, "success rebinds subscriptions");
        Ok(())
    }

    #[tokio::test]
    async fn resume_cancelled_suppresses_notice_and_rebind() -> TestResult {
        let (mut rt, log) = try_make_runtime()?;
        rt.session.set_cancel_switch(true);
        let before = rt
            .session
            .event_senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let _ = rt
            .dispatch_action(ViewAction::SelectConfirmed {
                selector: SelectorKind::Session,
                value: "/tmp/sess.json".to_owned(),
            })
            .await;
        assert_eq!(
            log.switches.lock().await.as_slice(),
            &["/tmp/sess.json".to_owned()]
        );
        assert!(
            !rt.view.messages.iter().any(|message| matches!(
                message,
                MessageView::Custom(custom)
                    if custom.custom_type == "resume" && custom.text.contains("Resumed session")
            )),
            "cancelled resume must not show success notice"
        );
        let after = rt
            .session
            .event_senders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        assert_eq!(after, before, "cancelled resume must not rebind");
        Ok(())
    }

    #[tokio::test]
    async fn export_uppercase_jsonl_uses_html() -> TestResult {
        let (mut rt, _log) = try_make_runtime()?;
        // Upstream `endsWith(".jsonl")` is case-sensitive → `.JSONL` exports HTML.
        let _ = rt.submit_text("/export out.JSONL".to_owned(), false).await;
        assert!(matches!(
            rt.view.messages.last(),
            Some(MessageView::Custom(custom))
                if custom.custom_type == "export" && custom.text.contains("<html></html>")
        ));
        Ok(())
    }

    #[tokio::test]
    async fn working_visible_false_suppresses_agent_start_status() {
        let mut view = ViewState::empty();
        view.working_visible = false;
        project_event(&mut view, &AgentSessionEvent::AgentStart);
        assert!(view.streaming);
        assert!(
            view.status.is_none(),
            "workingVisible=false must suppress the status at AgentStart"
        );
    }

    #[tokio::test]
    async fn working_message_override_applies_at_agent_start() -> Result<(), String> {
        let mut view = ViewState::empty();
        view.working_message = Some("Custom…".to_owned());
        project_event(&mut view, &AgentSessionEvent::AgentStart);
        let status = view
            .status
            .ok_or_else(|| "status present at AgentStart".to_owned())?;
        assert_eq!(status.kind, StatusKind::Working);
        assert_eq!(status.message, "Custom…");
        Ok(())
    }

    #[tokio::test]
    async fn set_working_visible_false_persists_and_clears_status() {
        let (mut rt, _log) = make_runtime();
        rt.view.streaming = true;
        rt.view.status = Some(SessionStatus {
            kind: StatusKind::Working,
            frame: 0,
            elapsed_secs: 0,
            message: "Working…".to_owned(),
        });
        rt.handle_extension_ui_control(UiControl::SetWorkingVisible { visible: false })
            .await;
        assert!(!rt.view.working_visible);
        assert!(rt.view.status.is_none());
        // Persisted flag is honored at the next AgentStart projection.
        project_event(&mut rt.view, &AgentSessionEvent::AgentStart);
        assert!(rt.view.status.is_none());
    }

    #[tokio::test]
    async fn set_working_message_does_not_spawn_idle_status() {
        let (mut rt, _log) = make_runtime();
        rt.handle_extension_ui_control(UiControl::SetWorkingMessage {
            message: Some("Deploying…".to_owned()),
        })
        .await;
        assert!(
            rt.view.status.is_none(),
            "setWorkingMessage must not spawn a status while idle"
        );
        assert_eq!(rt.view.working_message.as_deref(), Some("Deploying…"));
        // The stored override is applied when the agent actually starts.
        project_event(&mut rt.view, &AgentSessionEvent::AgentStart);
        assert_eq!(
            rt.view
                .status
                .as_ref()
                .map(|status| status.message.as_str()),
            Some("Deploying…")
        );
    }

    /// T3: hostile OSC/title bytes are stripped and the sink sees fixed framing.
    #[test]
    fn sanitize_terminal_title_strips_raw_controls() {
        let title = "safe\x07\x1b]1;evil\x07\r\n\x0cmiddle";
        assert_eq!(sanitize_terminal_title(title), "safe]1;evilmiddle");
    }

    #[test]
    fn sanitize_terminal_title_strips_c1_controls() {
        let title = "before\u{009b}after\u{0085}end";
        assert_eq!(sanitize_terminal_title(title), "beforeafterend");
    }

    #[test]
    fn sanitize_terminal_title_respects_utf8_byte_cap_without_splitting_scalar() {
        let one_byte = "a".repeat(MAX_TERMINAL_TITLE_BYTES);
        assert_eq!(
            sanitize_terminal_title(&one_byte).len(),
            MAX_TERMINAL_TITLE_BYTES
        );
        assert_eq!(
            sanitize_terminal_title(&format!("{one_byte}x")).len(),
            MAX_TERMINAL_TITLE_BYTES
        );

        let emoji = "\u{1f642}"; // 4 UTF-8 bytes
        let max_emojis = emoji.repeat(MAX_TERMINAL_TITLE_BYTES / emoji.len());
        assert_eq!(sanitize_terminal_title(&max_emojis), max_emojis);
        assert_eq!(
            sanitize_terminal_title(&format!("{max_emojis}a")).len(),
            MAX_TERMINAL_TITLE_BYTES
        );

        let prefix = "a".repeat(MAX_TERMINAL_TITLE_BYTES - 1);
        assert_eq!(sanitize_terminal_title(&format!("{prefix}{emoji}")), prefix);
    }

    #[test]
    #[allow(clippy::naive_bytecount)]
    fn encode_osc0_set_title_uses_fixed_framing_and_valid_payload() {
        let hostile = "pi\x07\x1b]1;break\x07\u{009b}ok";
        let sequence = encode_osc0_set_title(hostile);
        assert_eq!(&sequence[..4], b"\x1b]0;");
        assert_eq!(sequence.last().copied(), Some(OSC_BEL));
        let payload = &sequence[4..sequence.len() - 1];
        assert_eq!(payload, b"pi]1;breakok");
        assert!(payload.len() <= MAX_TERMINAL_TITLE_BYTES);
        assert!(std::str::from_utf8(payload).is_ok());
        assert!(!pi_ext::sanitize::contains_control_bytes(payload));
        assert_eq!(sequence.iter().filter(|&&b| b == OSC_BEL).count(), 1);
    }

    #[tokio::test]
    async fn set_title_control_writes_sanitized_osc0() -> Result<(), String> {
        let writer = SharedWriter::new();
        let sink = writer.clone();
        let caps = TerminalCapabilities::default();
        let tui = Tui::new(writer, Size::new(80, 24), Position::ORIGIN, 8, caps)
            .map_err(|error| format!("tui construction: {error}"))?;
        let (_tx, rx) = mpsc::unbounded_channel::<UiEvent>();
        let input = TerminalInput::mock(rx);
        let (host, _log) = FakeHost::new();
        let options = InteractiveRuntimeOptions {
            size: (80, 24),
            ..InteractiveRuntimeOptions::default()
        };
        let mut rt = InteractiveRuntime::new(tui, input, Arc::new(host), &options);
        let before = sink.snapshot().len();
        rt.handle_extension_ui_control(UiControl::SetTitle {
            title: Some("safe\x07\x1b]1;evil\x07\u{009b}ok".to_owned()),
        })
        .await;
        let written = &sink.snapshot()[before..];
        assert_eq!(
            written,
            encode_osc0_set_title("safe\x07\x1b]1;evil\x07\u{009b}ok")
        );
        Ok(())
    }
}
