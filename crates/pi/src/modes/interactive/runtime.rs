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
use std::task::Poll;
use std::time::{Duration, Instant};

use futures::future::{BoxFuture, poll_fn};
use pi_ai::AssistantMessage;
use pi_tui::component::{Component, EventResult, UiEvent};
use pi_tui::keys::{ParsedKeyId, encode_key_event, key_matches_parsed, parse_key_id};
use pi_tui::components::editor::{Editor, EditorOptions};
use pi_tui::terminal::caps::TerminalCapabilities;
use pi_tui::terminal::input::TerminalInput;
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
use crate::core::extension_host::{ExtensionUiEvent, HostExtensionRunner};
use crate::core::platform::external_editor::{EditOutcome, edit_text_in_external_editor};
use pi_ext::client::{HostUiRequest, HostUiResponse};
use pi_ext::protocol::{
    KeyEventKindWire, KeyModifiersWire, NotifyLevel, SlotPlacement, UiEventRequest, UiEventWire,
};
use pi_ext::sanitize::SanitizedSlot;

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

/// Bound on the runtime's incoming event channel. Matches the agent crate's
/// extension-queue capacity so a lagging consumer surfaces backpressure early.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

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
    fn reload(&self) -> BoxFuture<'_, Result<(), String>>;

    /// Returns the full transcript for the current session (used on rebind).
    fn messages(&self) -> Vec<pi_agent::AgentMessage>;

    /// Concrete extension host for interactive UI bridging, when enabled.
    fn host_extension_runner(&self) -> Option<Arc<HostExtensionRunner>> {
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

    /// Start a new session (replacement pipeline).
    fn new_session(&self) -> BoxFuture<'_, Result<(), String>>;

    /// Open the fork selector's confirmation; runtime supplies the entry id.
    fn fork(&self, entry_id: &str) -> BoxFuture<'_, Result<(), String>>;

    /// Clone the session at the current leaf.
    fn clone(&self) -> BoxFuture<'_, Result<(), String>>;

    /// Switch to a different session file (resume).
    fn switch_session(&self, path: &str) -> BoxFuture<'_, Result<(), String>>;

    /// Export the current session to HTML; runtime passes an optional path.
    fn export_html(&self, path: Option<&str>) -> BoxFuture<'_, Result<String, String>>;

    /// Set the session display name.
    fn set_session_name(&self, name: &str) -> BoxFuture<'_, Result<(), String>>;

    /// Log out of the active auth / open the login selector.
    fn logout(&self) -> BoxFuture<'_, Result<(), String>>;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TypedBuiltin<'a> {
    Compact(Option<&'a str>),
    Fork,
    Resume,
    Reload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionReplacement {
    New,
    Fork,
    Clone,
}

fn parse_typed_builtin(text: &str) -> Option<TypedBuiltin<'_>> {
    match text {
        "/compact" => Some(TypedBuiltin::Compact(None)),
        "/fork" => Some(TypedBuiltin::Fork),
        "/resume" => Some(TypedBuiltin::Resume),
        "/reload" => Some(TypedBuiltin::Reload),
        _ => text
            .strip_prefix("/compact ")
            .map(str::trim)
            .map(Some)
            .map(TypedBuiltin::Compact),
    }
}

impl Default for InteractiveRuntimeOptions {
    fn default() -> Self {
        Self {
            theme: super::theme::dark(),
            caps: TerminalCapabilities::default(),
            size: (80, 24),
            viewport_height: 24,
            quiet: false,
            double_escape: DoubleEscapeAction::None,
            hardware_cursor: false,
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
            focus: view.focus,
        }
    }

    fn build_with_chat(
        view: &mut ViewState,
        editor: Editor,
        selector: Option<Box<dyn Component>>,
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
        let middle_height = if self.focus == FocusArea::Selector {
            self.selector
                .as_mut()
                .map_or(0, |selector| selector.measure(width))
        } else {
            self.editor.measure(width)
        };
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
        let middle_height = if self.focus == FocusArea::Selector {
            self.selector
                .as_mut()
                .map_or(0, |selector| selector.measure(area.width))
        } else {
            self.editor.measure(area.width)
        };
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
            let middle_area = Rect::new(area.x, y, area.width, height);
            if self.focus == FocusArea::Selector {
                if let Some(selector) = self.selector.as_mut() {
                    selector.render(middle_area, buf);
                }
            } else {
                self.editor.render(middle_area, buf);
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
    prompt_operations: PromptOperations,
    coalesce_deadline: Option<Instant>,
    pending_settle: Option<Vec<SettledBlock>>,
    shutdown: Arc<Notify>,
    exited: bool,
    exit_kind: InteractiveExit,
    last_error: Option<String>,
    shutdown_flag: Arc<std::sync::atomic::AtomicBool>,
    pending_ui_reinject: Vec<UiEvent>,
    extension_runner: Option<Arc<HostExtensionRunner>>,
    extension_events: Option<tokio::sync::broadcast::Receiver<ExtensionUiEvent>>,
    extension_requests: Option<mpsc::Receiver<HostUiRequest>>,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tui: Tui<W>,
        input: TerminalInput,
        session: Arc<S>,
        options: &InteractiveRuntimeOptions,
    ) -> Self {
        let mut view = ViewState::empty();
        view.theme = options.theme.clone();
        view.width = options.size.0;
        view.height = options.size.1;
        view.quiet = options.quiet;
        view.resize(options.size.0, options.size.1);

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
        let extension_requests = extension_runner
            .as_ref()
            .and_then(|runner| runner.take_ui_requests());
        let initial_extension_slots = extension_runner
            .as_ref()
            .map_or_else(Vec::new, |runner| runner.current_slots());
        let effective_extension_shortcuts = extension_runner
            .as_ref()
            .map_or_else(Vec::new, |runner| build_effective_extension_shortcuts(&runner.raw_shortcuts()));
        view.extension_shortcuts = shortcut_hints(&effective_extension_shortcuts);

        let (submit_tx, submit_rx) = mpsc::unbounded_channel::<String>();
        let (select_tx, select_rx) =
            mpsc::unbounded_channel::<(super::state::SelectorKind, String)>();
        let (cancel_tx, cancel_rx) = mpsc::unbounded_channel::<()>();
        let (extension_select_tx, extension_select_rx) = mpsc::unbounded_channel::<String>();
        let (extension_action_tx, extension_action_rx) = mpsc::unbounded_channel();

        let mut editor = Editor::new(
            &pi_tui::components::editor::EditorTheme::default(),
            &EditorOptions {
                padding_x: 1,
                autocomplete_max_visible: 5,
                terminal_rows: options.size.1,
            },
        );
        let submit_tx_cb = submit_tx.clone();
        editor.on_submit = Some(Box::new(move |text: String| {
            let _ = submit_tx_cb.send(text);
        }));

        let mut runtime = Self {
            tui,
            input,
            session,
            editor,
            view,
            mapper: InputMapper::new(),
            input_state: InputState::new(options.double_escape),
            events,
            partial,
            prompt_operations: PromptOperations::new(),
            coalesce_deadline: None,
            pending_settle: None,
            shutdown: Arc::new(Notify::new()),
            exited: false,
            exit_kind: InteractiveExit::Clean,
            last_error: None,
            shutdown_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_ui_reinject: Vec::new(),
            extension_runner,
            extension_events,
            extension_requests,
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
        };
        for slot in initial_extension_slots {
            runtime.project_extension_slot(slot);
        }
        runtime
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
        self.refresh_footer().await;
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

            let now = Instant::now();
            let coalesce_wait = self
                .coalesce_deadline
                .map_or(Duration::from_hours(1), |deadline| {
                    deadline.saturating_duration_since(now)
                });

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
                ev = self.events.rx.recv() => {
                    if let Some(event) = ev {
                        self.handle_session_event(&event);
                        if event_refreshes_footer(&event) {
                            self.refresh_footer().await;
                        }
                    } else {
                        self.exit_kind = InteractiveExit::SessionEnded;
                        self.exited = true;
                    }
                }
                extension_event = recv_extension_event(&mut self.extension_events) => {
                    if let Some(extension_event) = extension_event {
                        self.handle_extension_event(extension_event);
                    } else {
                        self.extension_events = None;
                    }
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
                changed = self.partial.changed() => {
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

            // Run any pending settle as its own transaction.
            self.settle_pending();
        }

        Ok(self.finish_run().await)
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

        // Map app-level keys (skipped when the focused component already
        // handled the event — including selector confirm/cancel).
        actions.extend(self.mapper.map(
            &event,
            &self.view,
            &live_text,
            &mut self.input_state,
            editor_result.is_handled(),
        ));

        let mut needs_immediate_repaint = editor_result.needs_render();
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
            ViewAction::Logout => {
                self.record_err(self.session.logout().await);
                ActionOutcome::None
            }
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
            ViewAction::Reload => {
                if self.pending_extension_dialog.is_some() {
                    self.cancel_extension_dialog().await;
                }
                self.record_err(self.session.reload().await);
                self.rebind_extension_channels().await;
                ActionOutcome::Repaint
            }
            ViewAction::SlashCommand { name, args } => self.submit_slash_command(name, args).await,
            ViewAction::SelectConfirmed { selector, value } => {
                self.handle_select_confirmed(selector, value).await
            }
            ViewAction::SelectCancelled => {
                self.close_selector();
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

    async fn submit_slash_command(&mut self, name: String, args: String) -> ActionOutcome {
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
                        message: "Copied last assistant message".to_owned(),
                    });
                } else {
                    self.last_error = Some("Failed to copy to clipboard".to_owned());
                }
            }
            Ok(_) => self.set_status(SessionStatus {
                kind: StatusKind::Working,
                frame: 0,
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
        let result = match replacement {
            SessionReplacement::New => self.session.new_session().await,
            SessionReplacement::Fork => self.session.fork("").await,
            SessionReplacement::Clone => <S as SessionHost>::clone(&self.session).await,
        };
        self.record_err(result);
        self.rebind_session_channels().await;
        self.refresh_footer().await;
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
        if trimmed == "/quit" {
            return ActionOutcome::Exit;
        }
        if let Some(command) = parse_typed_builtin(&trimmed) {
            return match command {
                TypedBuiltin::Compact(instructions) => {
                    self.record_err(self.session.compact(instructions).await);
                    ActionOutcome::None
                }
                TypedBuiltin::Fork => self.open_selector(super::state::SelectorKind::Fork).await,
                TypedBuiltin::Resume => {
                    self.open_selector(super::state::SelectorKind::Session)
                        .await
                }
                TypedBuiltin::Reload => {
                    self.record_err(self.session.reload().await);
                    ActionOutcome::Repaint
                }
            };
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
            super::state::SelectorKind::Session => {
                self.quiesce_prompt_operations().await;
                if self.pending_extension_dialog.is_some() {
                    self.cancel_extension_dialog().await;
                }
                self.record_err(self.session.switch_session(&value).await);
                self.rebind_session_channels().await;
                self.refresh_footer().await;
                self.close_selector();
                ActionOutcome::Repaint
            }
            super::state::SelectorKind::Fork => {
                self.quiesce_prompt_operations().await;
                if self.pending_extension_dialog.is_some() {
                    self.cancel_extension_dialog().await;
                }
                self.record_err(self.session.fork(&value).await);
                self.rebind_session_channels().await;
                self.refresh_footer().await;
                self.close_selector();
                ActionOutcome::Repaint
            }
        }
    }

    fn close_selector(&mut self) {
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
        kind: super::state::SelectorKind,
        rows: Vec<super::state::SettingsRow>,
    ) -> Box<dyn Component> {
        let items = rows
            .into_iter()
            .map(|row| {
                pi_tui::components::SelectItem::new(
                    row.id,
                    format!("{}  {}", row.label, row.current_value),
                )
                .with_description(row.description.unwrap_or_default())
            })
            .collect();
        self.build_select_list(kind, items)
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

    fn handle_extension_event(&mut self, event: ExtensionUiEvent) {
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
        }
        self.arm_coalescer();
    }

    fn project_extension_slot(&mut self, slot: SanitizedSlot) {
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
        let current_slots = self
            .extension_runner
            .as_ref()
            .map_or_else(Vec::new, |runner| runner.current_slots());
        self.extension_events = self
            .extension_runner
            .as_ref()
            .map(|runner| runner.subscribe_ui());
        self.extension_requests = self
            .extension_runner
            .as_ref()
            .and_then(|runner| runner.take_ui_requests());
        self.pending_extension_dialog = None;
        self.extension_slots.clear();
        self.focused_extension_slot = None;
        self.view.extension_overlay_slot = None;
        self.effective_extension_shortcuts = self.extension_runner.as_ref().map_or_else(
            Vec::new,
            |runner| build_effective_extension_shortcuts(&runner.raw_shortcuts()),
        );
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

    /// Rebind event/partial subscriptions and reload the transcript after a
    /// session replacement. Used by production rebind callback and tests.
    pub async fn rebind_session_channels(&mut self) {
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
        self.view.status = Some(status);
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
        InteractiveRoot::build_with_chat(&mut self.view, editor, selector, prefix, tail)
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
        let result = self.tui.commit(Txn::Frame, &mut root);
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

/// Apply a [`SessionSnapshot`] to [`ViewState`]. `partial` may overwrite the
/// streaming tail when present.
fn project_snapshot(
    view: &mut ViewState,
    snapshot: &SessionSnapshot,
    partial: Option<&Arc<AssistantMessage>>,
) {
    view.streaming = snapshot.is_streaming();
    view.status = match snapshot.activity {
        SessionActivity::Streaming => Some(SessionStatus {
            kind: StatusKind::Working,
            frame: 0,
            message: "Working…".to_owned(),
        }),
        SessionActivity::Compacting => Some(SessionStatus {
            kind: StatusKind::Compaction,
            frame: 0,
            message: "Compacting…".to_owned(),
        }),
        SessionActivity::Retrying => Some(SessionStatus {
            kind: StatusKind::Retry,
            frame: 0,
            message: "Retrying…".to_owned(),
        }),
        SessionActivity::Summarizing => Some(SessionStatus {
            kind: StatusKind::BranchSummary,
            frame: 0,
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
            view.status = Some(SessionStatus {
                kind: StatusKind::Working,
                frame: 0,
                message: "Working…".to_owned(),
            });
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
        | Event::ModelSelect { .. } => {}
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
    let mut text = String::new();
    for content in &result.content {
        if let pi_ai::ToolResultContent::Text(t) = content {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&t.text);
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

    fn reload(&self) -> BoxFuture<'_, Result<(), String>> {
        let session = self.read_session();
        Box::pin(async move { session.reload().await.map_err(|e| e.to_string()) })
    }

    fn messages(&self) -> Vec<pi_agent::AgentMessage> {
        self.read_session().messages()
    }

    fn host_extension_runner(&self) -> Option<Arc<HostExtensionRunner>> {
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
                    current_value: settings.get_theme().unwrap_or_else(|| "default".to_owned()),
                    values: Some(vec!["dark".to_owned(), "light".to_owned()]),
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
                    current_value: format!("{:?}", settings.get_double_escape_action())
                        .to_lowercase(),
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

    fn new_session(&self) -> BoxFuture<'_, Result<(), String>> {
        let runtime = Arc::clone(&self.runtime);
        let host_session = Arc::clone(&self.session);
        Box::pin(async move {
            runtime
                .new_session(NewSessionOptions::default())
                .await
                .map(|_| ())
                .map_err(|err| runtime_err_to_string(&err))?;
            if let Ok(mut guard) = host_session.write() {
                *guard = runtime.session();
            }
            Ok(())
        })
    }

    fn fork(&self, entry_id: &str) -> BoxFuture<'_, Result<(), String>> {
        let runtime = Arc::clone(&self.runtime);
        let host_session = Arc::clone(&self.session);
        let entry_id = entry_id.to_owned();
        Box::pin(async move {
            runtime
                .fork(&entry_id, ForkPosition::Before)
                .await
                .map(|_| ())
                .map_err(|err| runtime_err_to_string(&err))?;
            if let Ok(mut guard) = host_session.write() {
                *guard = runtime.session();
            }
            Ok(())
        })
    }

    fn clone(&self) -> BoxFuture<'_, Result<(), String>> {
        let runtime = Arc::clone(&self.runtime);
        let host_session = Arc::clone(&self.session);
        Box::pin(async move {
            let leaf = {
                let session = runtime.session();
                let manager = session.session_manager();
                let sm = manager.lock().await;
                sm.get_leaf_id().map(str::to_owned)
            };
            let leaf =
                leaf.ok_or_else(|| "Cannot clone session: no current entry selected".to_owned())?;
            runtime
                .fork(&leaf, ForkPosition::At)
                .await
                .map(|_| ())
                .map_err(|err| runtime_err_to_string(&err))?;
            if let Ok(mut guard) = host_session.write() {
                *guard = runtime.session();
            }
            Ok(())
        })
    }

    fn switch_session(&self, path: &str) -> BoxFuture<'_, Result<(), String>> {
        let runtime = Arc::clone(&self.runtime);
        let host_session = Arc::clone(&self.session);
        let path = path.to_owned();
        Box::pin(async move {
            runtime
                .switch_session(&path, SwitchSessionOptions::default())
                .await
                .map(|_| ())
                .map_err(|err| runtime_err_to_string(&err))?;
            if let Ok(mut guard) = host_session.write() {
                *guard = runtime.session();
            }
            Ok(())
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

    fn set_session_name(&self, name: &str) -> BoxFuture<'_, Result<(), String>> {
        let session = self.read_session();
        let name = name.to_owned();
        Box::pin(async move {
            session
                .set_session_name(&name)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn logout(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn last_assistant_text(&self) -> BoxFuture<'_, Result<Option<String>, String>> {
        let session = self.read_session();
        Box::pin(async move { Ok(session.get_last_assistant_text()) })
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
pub async fn run_interactive_mode(
    runtime: Arc<AgentSessionRuntime>,
    options: InteractiveRuntimeOptions,
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

    // Rebind callback keeps AgentSessionHost's cached session Arc current and
    // binds the replacement session (emitting its stored
    // session_start{new|resume|fork}).
    {
        let host_for_rebind = Arc::clone(&host_arc);
        runtime.set_rebind_session(Some(Arc::new(move |_session| {
            let host_for_rebind = Arc::clone(&host_for_rebind);
            Box::pin(async move {
                host_for_rebind.refresh();
                let _ = host_for_rebind
                    .session()
                    .bind_extensions(crate::core::agent_session::ExtensionBindings {
                        mode: Some(crate::core::agent_session::ExtensionMode::Tui),
                        ..Default::default()
                    })
                    .await;
            })
        })));
    }

    let mut rt = InteractiveRuntime::new(tui, input, host_arc, &options);

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
    let initial = rt.editor.get_text();
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
        logouts: Mutex<u32>,
        follows: Mutex<Vec<String>>,
        steers: Mutex<Vec<String>>,
        last_text: Mutex<Option<String>>,
    }

    struct FakeHost {
        log: Arc<ActionLog>,
        partial_tx: watch::Sender<Option<Arc<AssistantMessage>>>,
        snapshot: Arc<std::sync::Mutex<SessionSnapshot>>,
        event_senders: Arc<std::sync::Mutex<Vec<mpsc::UnboundedSender<AgentSessionEvent>>>>,
        stream_chunks: Arc<AtomicUsize>,
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
            };
            (host, log)
        }

        fn set_stream_chunks(&self, chunks: usize) {
            self.stream_chunks.store(chunks, Ordering::SeqCst);
        }
    }

    impl SessionHost for FakeHost {
        fn snapshot(&self) -> SessionSnapshot {
            self.snapshot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
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

        fn reload(&self) -> BoxFuture<'_, Result<(), String>> {
            let log = Arc::clone(&self.log);
            Box::pin(async move {
                *log.reloads.lock().await += 1;
                Ok(())
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

        fn new_session(&self) -> BoxFuture<'_, Result<(), String>> {
            let log = Arc::clone(&self.log);
            Box::pin(async move {
                *log.new_sessions.lock().await += 1;
                Ok(())
            })
        }

        fn fork(&self, entry_id: &str) -> BoxFuture<'_, Result<(), String>> {
            let log = Arc::clone(&self.log);
            let owned = entry_id.to_owned();
            Box::pin(async move {
                log.forks.lock().await.push(owned);
                Ok(())
            })
        }

        fn clone(&self) -> BoxFuture<'_, Result<(), String>> {
            let log = Arc::clone(&self.log);
            Box::pin(async move {
                *log.clones.lock().await += 1;
                Ok(())
            })
        }

        fn switch_session(&self, path: &str) -> BoxFuture<'_, Result<(), String>> {
            let log = Arc::clone(&self.log);
            let owned = path.to_owned();
            Box::pin(async move {
                log.switches.lock().await.push(owned);
                Ok(())
            })
        }

        fn export_html(&self, _path: Option<&str>) -> BoxFuture<'_, Result<String, String>> {
            Box::pin(async { Ok("<html></html>".to_owned()) })
        }

        fn set_session_name(&self, _name: &str) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn logout(&self) -> BoxFuture<'_, Result<(), String>> {
            let log = Arc::clone(&self.log);
            Box::pin(async move {
                *log.logouts.lock().await += 1;
                Ok(())
            })
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
                name: "name".to_owned(),
                args: "my session".to_owned(),
            })
            .await;
        let prompts = log.prompts.lock().await.clone();
        assert_eq!(prompts, vec!["/name my session".to_owned()]);
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
        rt.effective_extension_shortcuts = build_effective_extension_shortcuts(&[
            pi_ext::adapters::ShortcutRegistration {
                key: "ctrl+c".to_owned(),
                description: Some("must not run".to_owned()),
                extension_path: Some("extension.ts".to_owned()),
            },
        ]);
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
            rt.extension_slots.get("editor.status").map(|slot| slot.generation),
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
    fn non_capturing_overlay_preserves_editor_focus_and_structured_metadata() {
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
            .expect("structured overlay");
        assert_eq!(projected.lines[0][0].style.link.as_ref(), Some(&link));
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
}
