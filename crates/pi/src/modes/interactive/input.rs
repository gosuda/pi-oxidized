//! App-level input dispatch: maps [`UiEvent`]s into [`ViewAction`]s.
//!
//! This module is pure (no I/O, no async, no terminal access). The runtime
//! loop calls [`InputMapper::map`] with a [`ViewState`] snapshot plus the
//! [`EventResult`](pi_tui::component::EventResult) the focused component
//! returned for the same event. If the focused component consumed the event,
//! the mapper defers entirely; otherwise it checks the small closed set of
//! application keybindings (Ctrl+C / Ctrl+D / Ctrl+Z / Esc / Shift+Tab /
//! Ctrl+P / Ctrl+L / Ctrl+O / Ctrl+T / Ctrl+G / Ctrl+X / Alt+Enter / Alt+Up)
//! and emits the matching semantic action.
//!
//! Double-tap timing for "press Ctrl+C twice within 500ms to exit" and
//! "press Esc twice within 500ms to open `/tree` or `/fork`" lives in
//! [`InputState`]; the runtime owns one instance for the lifetime of the
//! session and resets it whenever focus moves to or from a selector.
//!
//! Field and constant names mirror `.references/pi/packages/coding-agent/
//! src/modes/interactive/interactive-mode.ts` (key handler block) and
//! `core/keybindings.ts` defaults.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pi_tui::component::UiEvent;

use super::state::{OverlayKind, StatusKind, ViewAction, ViewState};

/// Default double-tap window for "exit on second tap" semantics.
///
/// Mirrors the TS literal `500` used in `handleCtrlC` and the double-Esc
/// handler (`interactive-mode.ts:3464` and `:2541`).
pub const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(500);

/// What double-Esc on an empty editor should do.
///
/// Ports `getDoubleEscapeAction()` from settings (`"none" | "tree" | "fork"`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DoubleEscapeAction {
    /// No double-Esc behaviour (single Esc still clears the editor).
    #[default]
    None,
    /// Open the branch tree (`/tree`) selector.
    Tree,
    /// Open the user-message fork (`/fork`) selector.
    Fork,
}

/// Mutable state carried across input events for double-tap detection.
///
/// Owned by the runtime; reset whenever focus moves to a selector / overlay
/// (a fresh tap window starts after the selector closes).
#[derive(Clone, Copy, Debug)]
pub struct InputState {
    last_sigint: Option<Instant>,
    last_escape: Option<Instant>,
    double_escape_action: DoubleEscapeAction,
    sigint_exit_window: Duration,
    escape_double_window: Duration,
}

impl Default for InputState {
    fn default() -> Self {
        Self::new(DoubleEscapeAction::default())
    }
}

impl InputState {
    /// Build a fresh state with the configured double-Esc action.
    #[must_use]
    pub fn new(double_escape_action: DoubleEscapeAction) -> Self {
        Self {
            last_sigint: None,
            last_escape: None,
            double_escape_action,
            sigint_exit_window: DOUBLE_TAP_WINDOW,
            escape_double_window: DOUBLE_TAP_WINDOW,
        }
    }

    /// Update the configured double-Esc action (e.g. after a settings reload).
    pub fn set_double_escape_action(&mut self, action: DoubleEscapeAction) {
        self.double_escape_action = action;
    }

    /// Override the Ctrl+C double-tap window (tests / settings override).
    pub fn set_sigint_exit_window(&mut self, window: Duration) {
        self.sigint_exit_window = window;
    }

    /// Override the Esc double-tap window (tests / settings override).
    pub fn set_escape_double_window(&mut self, window: Duration) {
        self.escape_double_window = window;
    }

    /// Drop both remembered taps (called after focus changes / overlays open).
    pub fn reset_taps(&mut self) {
        self.last_sigint = None;
        self.last_escape = None;
    }

    /// Last Ctrl+C timestamp (for tests).
    #[must_use]
    pub fn last_sigint(&self) -> Option<Instant> {
        self.last_sigint
    }

    /// Last Esc timestamp (for tests).
    #[must_use]
    pub fn last_escape(&self) -> Option<Instant> {
        self.last_escape
    }

    /// Set the last Ctrl+C timestamp (test-only seam).
    #[cfg(test)]
    pub(crate) fn set_last_sigint_for_test(&mut self, instant: Option<Instant>) {
        self.last_sigint = instant;
    }
}

/// Pure input mapper.
///
/// Stateless beyond the borrowed [`InputState`]; one instance lives for the
/// runtime's lifetime. Methods never touch the terminal, the session, or any
/// I/O.
#[derive(Debug, Default)]
pub struct InputMapper;

impl InputMapper {
    /// Construct a fresh mapper.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Translate one [`UiEvent`] into zero or more ordered [`ViewAction`]s.
    ///
    /// `editor_consumed` is `true` when the focused editor/component already
    /// returned `Consumed` or `Render` for this event; in that case the mapper
    /// only emits actions for non-key events (`Resize`, `Paste`) that the
    /// component cannot logically claim.
    ///
    /// `editor_text` is the live editor buffer at the moment the event was
    /// received — the snapshot in `view.editor.text` may lag by one paint.
    /// Callers should pass the editor's current `get_text()` so submit /
    /// bash-detection see the same string the user typed.
    #[must_use]
    pub fn map(
        &self,
        event: &UiEvent,
        view: &ViewState,
        editor_text: &str,
        state: &mut InputState,
        editor_consumed: bool,
    ) -> Vec<ViewAction> {
        let mut out = Vec::new();
        match event {
            UiEvent::Resize { width, height } => {
                out.push(ViewAction::Resize {
                    width: *width,
                    height: *height,
                });
            }
            UiEvent::Paste(text) => {
                // Bracketed paste is delivered to the editor first. If it was
                // not consumed (no editor focused), surface as a Paste action
                // so the runtime can splice it into the live buffer.
                if !editor_consumed && !text.is_empty() {
                    out.push(ViewAction::Paste { text: text.clone() });
                }
            }
            UiEvent::FocusGained | UiEvent::FocusLost => {
                // No app-level action; the runtime uses these as a heuristic
                // to re-probe terminal light/dark on FocusGained.
            }
            UiEvent::Key(key) => {
                if !editor_consumed {
                    Self::map_key(*key, view, editor_text, state, &mut out);
                }
            }
        }
        out
    }

    fn map_key(
        key: KeyEvent,
        view: &ViewState,
        editor_text: &str,
        state: &mut InputState,
        out: &mut Vec<ViewAction>,
    ) {
        let mods = key.modifiers;
        match (mods, key.code) {
            // Ctrl+C: double-tap within window → Exit; otherwise Interrupt.
            // First tap also clears the editor (mirrors `handleCtrlC`).
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                let now = Instant::now();
                let double = state
                    .last_sigint
                    .is_some_and(|t| now.duration_since(t) < state.sigint_exit_window);
                if double {
                    state.last_sigint = None;
                    out.push(ViewAction::ClearEditor);
                    out.push(ViewAction::Exit);
                } else {
                    state.last_sigint = Some(now);
                    out.push(ViewAction::ClearEditor);
                    out.push(ViewAction::Interrupt);
                }
            }
            // Ctrl+D: exit only when the editor is empty AND no overlay is up
            // (parity with `handleCtrlD`: "Only called when editor is empty").
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                if editor_text.trim().is_empty() && view.overlay.is_none() {
                    out.push(ViewAction::Exit);
                }
            }
            // Ctrl+Z: suspend (Windows no-op is handled by the runtime).
            (KeyModifiers::CONTROL, KeyCode::Char('z')) => {
                out.push(ViewAction::Suspend);
            }
            // Shift+Tab: cycle thinking level forward.
            (KeyModifiers::SHIFT, KeyCode::BackTab | KeyCode::Tab) => {
                out.push(ViewAction::CycleThinking { forward: true });
            }
            // Ctrl+P: cycle model forward.
            // Ctrl+P (lowercase, no shift): cycle model forward.
            // The SHIFT bit is checked exactly so Ctrl+Shift+P (which may
            // arrive as lowercase 'p' with SHIFT on some platforms) falls
            // through to the backward arm below.
            (mods, KeyCode::Char('p')) if mods == KeyModifiers::CONTROL => {
                out.push(ViewAction::CycleModel { forward: true });
            }
            // Ctrl+Shift+P (any case, with SHIFT bit): cycle backward.
            (mods, KeyCode::Char('P'))
                if mods.contains(KeyModifiers::CONTROL) && mods.contains(KeyModifiers::SHIFT) =>
            {
                out.push(ViewAction::CycleModel { forward: false });
            }
            (mods, KeyCode::Char('p'))
                if mods.contains(KeyModifiers::CONTROL) && mods.contains(KeyModifiers::SHIFT) =>
            {
                out.push(ViewAction::CycleModel { forward: false });
            }
            // Ctrl+L: open the model selector.
            (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                out.push(ViewAction::OpenModelSelector);
            }
            // Ctrl+O: toggle tool expansion.
            (KeyModifiers::CONTROL, KeyCode::Char('o')) => {
                out.push(ViewAction::ToggleToolExpand);
            }
            // Ctrl+T: toggle thinking block visibility.
            (KeyModifiers::CONTROL, KeyCode::Char('t')) => {
                out.push(ViewAction::ToggleThinking);
            }
            // Ctrl+G: open external editor.
            (KeyModifiers::CONTROL, KeyCode::Char('g')) => {
                out.push(ViewAction::ExternalEditor);
            }
            // Ctrl+X: copy last assistant message to clipboard.
            (KeyModifiers::CONTROL, KeyCode::Char('x')) => {
                out.push(ViewAction::CopyLastAssistant);
            }
            // Alt+Enter: queue a follow-up while streaming, otherwise submit.
            (KeyModifiers::ALT, KeyCode::Enter) => {
                let text = editor_text.trim().to_owned();
                if text.is_empty() {
                    return;
                }
                if view.streaming {
                    out.push(ViewAction::QueueFollowUp { text });
                    out.push(ViewAction::ClearEditor);
                } else {
                    out.push(ViewAction::Submit { text });
                    out.push(ViewAction::ClearEditor);
                }
            }
            // Alt+Up: restore the last queued follow-up to the editor.
            (KeyModifiers::ALT, KeyCode::Up) => {
                out.push(ViewAction::DequeueFollowUp);
            }
            // Ctrl+V / Alt+V: paste image from clipboard (runtime owns the
            // clipboard call; emit a Paste with empty payload so the runtime
            // knows it was a clipboard-image request).
            (mods, KeyCode::Char('v'))
                if mods == KeyModifiers::CONTROL || mods == KeyModifiers::ALT =>
            {
                out.push(ViewAction::Paste {
                    text: String::new(),
                });
            }
            // Ctrl+R: open reload (slash-command equivalent of /reload).
            (KeyModifiers::CONTROL, KeyCode::Char('r')) => {
                out.push(ViewAction::Reload);
            }
            // Ctrl+B: open the session picker (/resume).
            (KeyModifiers::CONTROL, KeyCode::Char('b')) => {
                out.push(ViewAction::OpenSessionPicker);
            }
            // Ctrl+F: open the tree (/tree) selector.
            (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
                out.push(ViewAction::OpenTreeSelector);
            }
            // Ctrl+N: new session.
            (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
                out.push(ViewAction::NewSession);
            }
            // Esc: dispatch contextually (overlay → dismiss; streaming →
            // interrupt; bash → interrupt; double-tap on empty → tree/fork;
            // otherwise clear editor).
            (_, KeyCode::Esc) => {
                Self::map_escape(view, editor_text, state, out);
            }
            _ => {}
        }
    }

    fn map_escape(
        view: &ViewState,
        editor_text: &str,
        state: &mut InputState,
        out: &mut Vec<ViewAction>,
    ) {
        // 1. Any active overlay dismisses first.
        if view.overlay.is_some() {
            out.push(ViewAction::DismissOverlay);
            state.reset_taps();
            return;
        }
        // 2. Streaming agent → interrupt (also restores queued messages,
        //    which the runtime does after the abort resolves).
        if view.streaming {
            out.push(ViewAction::Interrupt);
            state.reset_taps();
            return;
        }
        // 3. Compaction / retry / branch-summary / working → Esc cancels.
        if let Some(status) = &view.status {
            let cancel_kind = matches!(
                status.kind,
                StatusKind::Compaction
                    | StatusKind::Retry
                    | StatusKind::BranchSummary
                    | StatusKind::Working
            );
            if cancel_kind {
                out.push(ViewAction::Interrupt);
                state.reset_taps();
                return;
            }
        }
        // 4. Double-Esc on empty editor → open tree/fork per setting.
        if editor_text.trim().is_empty()
            && !matches!(state.double_escape_action, DoubleEscapeAction::None)
        {
            let now = Instant::now();
            let double = state
                .last_escape
                .is_some_and(|t| now.duration_since(t) < state.escape_double_window);
            if double {
                state.last_escape = None;
                match state.double_escape_action {
                    DoubleEscapeAction::Tree => out.push(ViewAction::OpenTreeSelector),
                    DoubleEscapeAction::Fork => out.push(ViewAction::OpenForkSelector),
                    DoubleEscapeAction::None => {}
                }
            } else {
                state.last_escape = Some(now);
            }
            return;
        }
        // 5. Single Esc with non-empty editor → clear.
        if !editor_text.trim().is_empty() {
            out.push(ViewAction::ClearEditor);
        }
        // Otherwise: no-op.
    }
}

/// Helper for tests / settings reload: build a state from the wire string.
///
/// Mirrors `getDoubleEscapeAction` default mapping (`"none" | "tree" | "fork"`).
#[must_use]
pub fn double_escape_action_from_str(s: &str) -> DoubleEscapeAction {
    match s {
        "tree" => DoubleEscapeAction::Tree,
        "fork" => DoubleEscapeAction::Fork,
        _ => DoubleEscapeAction::None,
    }
}

/// Re-exported so the runtime can construct the default overlay kind list.
#[must_use]
pub fn dismissable_overlay_kinds() -> &'static [OverlayKind] {
    &[
        OverlayKind::ShortcutHelp,
        OverlayKind::Changelog,
        OverlayKind::FirstTimeSetup,
        OverlayKind::Login,
        OverlayKind::Extension,
    ]
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use pi_ai::AssistantMessage;
    use pi_tui::component::UiEvent;

    use super::*;
    use crate::modes::interactive::messages::MessageView;
    use crate::modes::interactive::state::{
        EditorBorder, EditorView, FocusArea, Overlay, OverlayKind, SessionStatus, StatusKind,
        ViewState,
    };

    fn key(code: KeyCode, mods: KeyModifiers) -> UiEvent {
        UiEvent::Key(KeyEvent::new(code, mods))
    }

    fn ctrl(c: char) -> UiEvent {
        key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(code: KeyCode) -> UiEvent {
        key(code, KeyModifiers::ALT)
    }

    fn ctrl_shift(c: char) -> UiEvent {
        key(
            KeyCode::Char(c),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )
    }

    fn shift(code: KeyCode) -> UiEvent {
        key(code, KeyModifiers::SHIFT)
    }

    fn plain(code: KeyCode) -> UiEvent {
        key(code, KeyModifiers::NONE)
    }

    fn empty_view() -> ViewState {
        ViewState::empty()
    }

    fn view_with_editor(text: &str) -> ViewState {
        let mut v = empty_view();
        v.editor = EditorView {
            text: text.to_owned(),
            cursor: text.chars().count(),
            placeholder: String::new(),
            border: EditorBorder::Muted,
            paste_marker: None,
        };
        v
    }

    fn view_streaming() -> ViewState {
        let mut v = empty_view();
        v.streaming = true;
        v.status = Some(SessionStatus {
            kind: StatusKind::Working,
            frame: 0,
            message: "Working".to_owned(),
        });
        let assistant = AssistantMessage::new("anthropic", "test", "test", 0);
        v.messages.push(MessageView::streaming_assistant(assistant));
        v
    }

    fn view_with_overlay(kind: OverlayKind) -> ViewState {
        let mut v = empty_view();
        v.overlay = Some(Overlay {
            kind,
            lines: vec!["overlay".to_owned()],
            height: 3,
        });
        v
    }

    fn view_with_status(kind: StatusKind) -> ViewState {
        let mut v = empty_view();
        v.status = Some(SessionStatus {
            kind,
            frame: 0,
            message: "x".to_owned(),
        });
        v
    }

    #[test]
    fn resize_emits_resize_action() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(
            &UiEvent::Resize {
                width: 100,
                height: 30,
            },
            &view,
            "",
            &mut state,
            false,
        );
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            ViewAction::Resize {
                width: 100,
                height: 30
            }
        );
    }

    #[test]
    fn paste_when_editor_did_not_consume_emits_paste() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(
            &UiEvent::Paste("hello".to_owned()),
            &view,
            "",
            &mut state,
            false,
        );
        assert_eq!(
            actions,
            vec![ViewAction::Paste {
                text: "hello".to_owned()
            }]
        );
    }

    #[test]
    fn paste_consumed_by_editor_is_ignored_by_mapper() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(
            &UiEvent::Paste("hello".to_owned()),
            &view,
            "",
            &mut state,
            true,
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn empty_paste_is_ignored() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&UiEvent::Paste(String::new()), &view, "", &mut state, false);
        assert!(actions.is_empty());
    }

    #[test]
    fn ctrl_c_first_tap_clears_and_interrupts() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = view_with_editor("draft");
        let actions = mapper.map(&ctrl('c'), &view, "draft", &mut state, false);
        assert_eq!(
            actions,
            vec![ViewAction::ClearEditor, ViewAction::Interrupt]
        );
        assert!(state.last_sigint.is_some());
    }

    #[test]
    fn ctrl_c_double_tap_within_window_exits() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = view_with_editor("x");
        let _ = mapper.map(&ctrl('c'), &view, "x", &mut state, false);
        let actions = mapper.map(&ctrl('c'), &view, "x", &mut state, false);
        assert_eq!(actions, vec![ViewAction::ClearEditor, ViewAction::Exit]);
        assert!(state.last_sigint.is_none());
    }

    #[test]
    fn ctrl_c_double_tap_outside_window_just_interrupts() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        state.set_sigint_exit_window(Duration::from_nanos(1));
        let view = view_with_editor("x");
        let _ = mapper.map(&ctrl('c'), &view, "x", &mut state, false);
        std::thread::sleep(Duration::from_millis(2));
        let actions = mapper.map(&ctrl('c'), &view, "x", &mut state, false);
        assert_eq!(
            actions,
            vec![ViewAction::ClearEditor, ViewAction::Interrupt]
        );
    }

    #[test]
    fn ctrl_d_only_exits_when_editor_empty() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let empty = empty_view();
        let actions = mapper.map(&ctrl('d'), &empty, "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::Exit]);

        let view = view_with_editor("not empty");
        let actions = mapper.map(&ctrl('d'), &view, "not empty", &mut state, false);
        assert!(actions.is_empty());
    }

    #[test]
    fn ctrl_d_does_not_exit_when_overlay_open() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = view_with_overlay(OverlayKind::ShortcutHelp);
        let actions = mapper.map(&ctrl('d'), &view, "", &mut state, false);
        assert!(actions.is_empty());
    }

    #[test]
    fn ctrl_z_emits_suspend() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&ctrl('z'), &view, "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::Suspend]);
    }

    #[test]
    fn shift_tab_cycles_thinking() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&shift(KeyCode::Tab), &view, "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::CycleThinking { forward: true }]);
    }

    #[test]
    fn ctrl_p_cycles_model_forward() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&ctrl('p'), &view, "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::CycleModel { forward: true }]);
    }

    #[test]
    fn ctrl_shift_p_cycles_model_backward() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&ctrl_shift('P'), &view, "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::CycleModel { forward: false }]);
    }

    #[test]
    fn app_keys_dispatch_table() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();

        assert_eq!(
            mapper.map(&ctrl('l'), &view, "", &mut state, false),
            vec![ViewAction::OpenModelSelector]
        );
        assert_eq!(
            mapper.map(&ctrl('o'), &view, "", &mut state, false),
            vec![ViewAction::ToggleToolExpand]
        );
        assert_eq!(
            mapper.map(&ctrl('t'), &view, "", &mut state, false),
            vec![ViewAction::ToggleThinking]
        );
        assert_eq!(
            mapper.map(&ctrl('g'), &view, "", &mut state, false),
            vec![ViewAction::ExternalEditor]
        );
        assert_eq!(
            mapper.map(&ctrl('x'), &view, "", &mut state, false),
            vec![ViewAction::CopyLastAssistant]
        );
        assert_eq!(
            mapper.map(&ctrl('r'), &view, "", &mut state, false),
            vec![ViewAction::Reload]
        );
        assert_eq!(
            mapper.map(&ctrl('b'), &view, "", &mut state, false),
            vec![ViewAction::OpenSessionPicker]
        );
        assert_eq!(
            mapper.map(&ctrl('f'), &view, "", &mut state, false),
            vec![ViewAction::OpenTreeSelector]
        );
        assert_eq!(
            mapper.map(&ctrl('n'), &view, "", &mut state, false),
            vec![ViewAction::NewSession]
        );
    }

    #[test]
    fn alt_enter_submits_when_not_streaming() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = view_with_editor("hello");
        let actions = mapper.map(&alt(KeyCode::Enter), &view, "hello", &mut state, false);
        assert_eq!(
            actions,
            vec![
                ViewAction::Submit {
                    text: "hello".to_owned()
                },
                ViewAction::ClearEditor,
            ]
        );
    }

    #[test]
    fn alt_enter_queues_followup_when_streaming() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = view_streaming();
        let actions = mapper.map(&alt(KeyCode::Enter), &view, "more", &mut state, false);
        assert_eq!(
            actions,
            vec![
                ViewAction::QueueFollowUp {
                    text: "more".to_owned()
                },
                ViewAction::ClearEditor,
            ]
        );
    }

    #[test]
    fn alt_enter_empty_is_noop() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&alt(KeyCode::Enter), &view, "   ", &mut state, false);
        assert!(actions.is_empty());
    }

    #[test]
    fn alt_up_dequeues() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&alt(KeyCode::Up), &view, "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::DequeueFollowUp]);
    }

    #[test]
    fn esc_dismisses_overlay_first() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = view_with_overlay(OverlayKind::ShortcutHelp);
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "abc", &mut state, false);
        assert_eq!(actions, vec![ViewAction::DismissOverlay]);
    }

    #[test]
    fn esc_interrupts_when_streaming() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = view_streaming();
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "x", &mut state, false);
        assert_eq!(actions, vec![ViewAction::Interrupt]);
    }

    #[test]
    fn esc_interrupts_when_compacting() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = view_with_status(StatusKind::Compaction);
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "x", &mut state, false);
        assert_eq!(actions, vec![ViewAction::Interrupt]);
    }

    #[test]
    fn esc_clears_editor_when_nonempty_and_idle() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = view_with_editor("draft");
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "draft", &mut state, false);
        assert_eq!(actions, vec![ViewAction::ClearEditor]);
    }

    #[test]
    fn esc_double_tap_on_empty_opens_tree_when_configured() {
        let mapper = InputMapper::new();
        let mut state = InputState::new(DoubleEscapeAction::Tree);
        let view = empty_view();
        let _ = mapper.map(&plain(KeyCode::Esc), &view, "", &mut state, false);
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::OpenTreeSelector]);
    }

    #[test]
    fn esc_double_tap_on_empty_opens_fork_when_configured() {
        let mapper = InputMapper::new();
        let mut state = InputState::new(DoubleEscapeAction::Fork);
        let view = empty_view();
        let _ = mapper.map(&plain(KeyCode::Esc), &view, "", &mut state, false);
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::OpenForkSelector]);
    }

    #[test]
    fn esc_single_tap_on_empty_when_double_disabled_is_noop() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "", &mut state, false);
        assert!(actions.is_empty());
        assert!(state.last_escape.is_none());
    }

    #[test]
    fn esc_double_tap_outside_window_records_new_tap() {
        let mapper = InputMapper::new();
        let mut state = InputState::new(DoubleEscapeAction::Tree);
        state.set_escape_double_window(Duration::from_nanos(1));
        let view = empty_view();
        let _ = mapper.map(&plain(KeyCode::Esc), &view, "", &mut state, false);
        std::thread::sleep(Duration::from_millis(2));
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "", &mut state, false);
        assert!(actions.is_empty(), "{actions:?}");
        assert!(state.last_escape.is_some());
    }

    #[test]
    fn reset_taps_clears_state() {
        let mut state = InputState {
            last_sigint: Some(Instant::now()),
            last_escape: Some(Instant::now()),
            ..InputState::default()
        };
        state.reset_taps();
        assert!(state.last_sigint.is_none());
        assert!(state.last_escape.is_none());
    }

    #[test]
    fn focus_events_produce_no_actions() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&UiEvent::FocusGained, &view, "", &mut state, false);
        assert!(actions.is_empty());
        let actions = mapper.map(&UiEvent::FocusLost, &view, "", &mut state, false);
        assert!(actions.is_empty());
    }

    #[test]
    fn editor_consumed_silences_app_keys() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&ctrl('l'), &view, "", &mut state, true);
        assert!(actions.is_empty());
    }

    #[test]
    fn double_escape_action_from_str_parses() {
        assert_eq!(
            double_escape_action_from_str("tree"),
            DoubleEscapeAction::Tree
        );
        assert_eq!(
            double_escape_action_from_str("fork"),
            DoubleEscapeAction::Fork
        );
        assert_eq!(
            double_escape_action_from_str("none"),
            DoubleEscapeAction::None
        );
        assert_eq!(double_escape_action_from_str(""), DoubleEscapeAction::None);
        assert_eq!(
            double_escape_action_from_str("bogus"),
            DoubleEscapeAction::None
        );
    }

    #[test]
    fn ctrl_v_emits_empty_paste_for_clipboard_image_path() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&ctrl('v'), &view, "", &mut state, false);
        assert_eq!(
            actions,
            vec![ViewAction::Paste {
                text: String::new()
            }]
        );
    }

    #[test]
    fn unknown_key_is_ignored() {
        let mapper = InputMapper::new();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(
            &key(KeyCode::Char('a'), KeyModifiers::NONE),
            &view,
            "",
            &mut state,
            false,
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn focus_area_default_is_editor() {
        let view = empty_view();
        assert_eq!(view.focus, FocusArea::Editor);
    }
}
