//! App-level input dispatch: maps [`UiEvent`]s into [`ViewAction`]s.
//!
//! The runtime loop calls [`InputMapper::map`] with a [`ViewState`] snapshot
//! plus the [`EventResult`](pi_tui::component::EventResult) the focused
//! component returned for the same event. If the focused component consumed
//! the event, the mapper defers entirely; otherwise it resolves the closed set
//! of application keybindings (`app.*` ids from
//! [`crate::core::keybindings`]) through a [`KeybindingsManager`] so user
//! rebinds in `keybindings.json` apply.
//!
//! Double-tap timing for "press clear-chord twice within 500ms to exit" and
//! "press interrupt-chord twice within 500ms to open `/tree` or `/fork`" lives
//! in [`InputState`]; the runtime owns one instance for the lifetime of the
//! session and resets it whenever focus moves to or from a selector.
//!
//! Field and constant names mirror `.references/pi-2.0/packages/coding-agent/
//! src/modes/interactive/interactive-mode.ts` (key handler block) and
//! `core/keybindings.ts` defaults.

use std::time::{Duration, Instant};

use crossterm::event::KeyEvent;
use pi_tui::component::UiEvent;
use pi_tui::keybindings::KeybindingsManager;

use crate::core::keybindings::{app_keybindings_defaults, create_app_keybindings};
use crate::core::settings::DoubleEscapeAction;

use super::state::{FocusArea, OverlayKind, StatusKind, ViewAction, ViewState};

/// Default double-tap window for "exit on second tap" semantics.
///
/// Mirrors the TS literal `500` used in `handleCtrlC` and the double-Esc
/// handler (`interactive-mode.ts:3464` and `:2541`).
pub const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(500);

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
/// Holds a [`KeybindingsManager`] snapshot for `app.*` resolution. One
/// instance lives for the runtime's lifetime; call [`InputMapper::set_keybindings`]
/// after `/reload` so user rebinds take effect. Methods never touch the
/// terminal, the session, or any I/O beyond the already-loaded table.
#[derive(Debug, Clone)]
pub struct InputMapper {
    keybindings: KeybindingsManager,
}

impl Default for InputMapper {
    fn default() -> Self {
        Self::new()
    }
}

impl InputMapper {
    /// Construct a mapper with process agent-dir defaults + `keybindings.json`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            keybindings: create_app_keybindings(),
        }
    }

    /// Construct a mapper with shipped defaults only (no user file).
    #[must_use]
    pub fn with_defaults() -> Self {
        Self {
            keybindings: app_keybindings_defaults(),
        }
    }

    /// Construct a mapper from an explicit keybindings table (tests / reload).
    #[must_use]
    pub fn with_keybindings(keybindings: KeybindingsManager) -> Self {
        Self { keybindings }
    }

    /// Replace the keybindings table (e.g. after `/reload`).
    pub fn set_keybindings(&mut self, keybindings: KeybindingsManager) {
        self.keybindings = keybindings;
    }

    /// Borrow the active keybindings table.
    #[must_use]
    pub fn keybindings(&self) -> &KeybindingsManager {
        &self.keybindings
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
    /// `expanded_text` is the same buffer with paste markers resolved; use it
    /// for submission paths so followUp/submit carry the real pasted content.
    #[must_use]
    pub fn map(
        &self,
        event: &UiEvent,
        view: &ViewState,
        editor_text: &str,
        expanded_text: &str,
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
                    self.map_key(*key, view, editor_text, expanded_text, state, &mut out);
                }
            }
        }
        out
    }

    /// `app.clear` (default ctrl+c): double-tap within window → Exit;
    /// otherwise Interrupt. First tap also clears the editor.
    fn map_clear_key(state: &mut InputState, out: &mut Vec<ViewAction>) {
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

    fn map_key(
        &self,
        key: KeyEvent,
        view: &ViewState,
        editor_text: &str,
        expanded_text: &str,
        state: &mut InputState,
        out: &mut Vec<ViewAction>,
    ) {
        let kb = &self.keybindings;

        if kb.matches(&key, "app.clear") {
            Self::map_clear_key(state, out);
            return;
        }

        // app.exit (default ctrl+d): modal focus owns the chord. At the
        // focused editor, only a truly zero-length buffer exits; whitespace is
        // content and remains owned by forward-delete.
        if kb.matches(&key, "app.exit") {
            if view.focus == FocusArea::Editor
                && editor_text.is_empty()
                && view.overlay.is_none()
                && view.first_run_step.is_none()
            {
                out.push(ViewAction::AppExit);
            }
            return;
        }

        // app.suspend (default ctrl+z; unbound on Windows).
        if kb.matches(&key, "app.suspend") {
            out.push(ViewAction::Suspend);
            return;
        }

        // app.thinking.cycle (default shift+tab).
        if kb.matches(&key, "app.thinking.cycle") {
            out.push(ViewAction::CycleThinking { forward: true });
            return;
        }

        // Model cycle: check backward before forward so shift+ctrl+p does not
        // also match ctrl+p when both are claimed.
        if kb.matches(&key, "app.model.cycleBackward") {
            out.push(ViewAction::CycleModel { forward: false });
            return;
        }
        if kb.matches(&key, "app.model.cycleForward") {
            out.push(ViewAction::CycleModel { forward: true });
            return;
        }

        if kb.matches(&key, "app.model.select") {
            out.push(ViewAction::OpenModelSelector);
            return;
        }
        if kb.matches(&key, "app.tools.expand") {
            out.push(ViewAction::ToggleToolExpand);
            return;
        }
        if kb.matches(&key, "app.thinking.toggle") {
            out.push(ViewAction::ToggleThinking);
            return;
        }
        if kb.matches(&key, "app.editor.external") {
            out.push(ViewAction::ExternalEditor);
            return;
        }
        if kb.matches(&key, "app.message.copy") {
            out.push(ViewAction::CopyLastAssistant);
            return;
        }

        // app.message.followUp (default alt+enter): queue while streaming,
        // otherwise submit.
        if kb.matches(&key, "app.message.followUp") {
            let text = expanded_text.trim().to_owned();
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
            return;
        }

        if kb.matches(&key, "app.message.dequeue") {
            out.push(ViewAction::DequeueFollowUp);
            return;
        }

        // app.clipboard.pasteImage (default ctrl+v / alt+v on Windows):
        // empty Paste payload signals the runtime to pull a clipboard image.
        if kb.matches(&key, "app.clipboard.pasteImage") {
            out.push(ViewAction::Paste {
                text: String::new(),
            });
            return;
        }

        // Session actions: TS defaults are empty for new/tree/fork/resume so
        // they only fire when the user rebinds them in keybindings.json.
        if kb.matches(&key, "app.session.new") {
            out.push(ViewAction::NewSession);
            return;
        }
        if kb.matches(&key, "app.session.tree") {
            out.push(ViewAction::OpenTreeSelector);
            return;
        }
        if kb.matches(&key, "app.session.fork") {
            out.push(ViewAction::OpenForkSelector);
            return;
        }
        if kb.matches(&key, "app.session.resume") {
            out.push(ViewAction::OpenSessionPicker);
            return;
        }

        // app.session.toggleNamedFilter (default ctrl+n) is selector-local in
        // TS (session-selector.ts). The interactive editor has no named-filter
        // surface, so a match here is a deliberate no-op: leave the chord
        // reserved / rebound without opening a new session.
        if kb.matches(&key, "app.session.toggleNamedFilter") {
            return;
        }

        // app.interrupt (default escape): contextual dismiss / interrupt /
        // double-tap tree|fork / clear editor.
        if kb.matches(&key, "app.interrupt") {
            Self::map_escape(view, editor_text, state, out);
        }
    }

    fn map_escape(
        view: &ViewState,
        editor_text: &str,
        state: &mut InputState,
        out: &mut Vec<ViewAction>,
    ) {
        if view.overlay.is_some() {
            out.push(ViewAction::DismissOverlay);
            state.reset_taps();
            return;
        }
        if view.extension_dialog {
            out.push(ViewAction::SelectCancelled);
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use pi_ai::AssistantMessage;
    use pi_tui::component::UiEvent;
    use pi_tui::keybindings::KeybindingsConfig;
    use pi_tui::keys::KeyId;
    use tempfile::TempDir;

    use super::*;
    use crate::core::keybindings::{KEYBINDINGS_FILE_NAME, app_keybindings, load_app_keybindings};
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
            elapsed_secs: 0,
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
            elapsed_secs: 0,
            message: "x".to_owned(),
        });
        v
    }

    /// Mapper with shipped defaults only — isolated from the developer's
    /// real `~/.pi/agent/keybindings.json`.
    fn mapper() -> InputMapper {
        InputMapper::with_defaults()
    }

    #[test]
    fn resize_emits_resize_action() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(
            &UiEvent::Resize {
                width: 100,
                height: 30,
            },
            &view,
            "",
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
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(
            &UiEvent::Paste("hello".to_owned()),
            &view,
            "",
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
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(
            &UiEvent::Paste("hello".to_owned()),
            &view,
            "",
            "",
            &mut state,
            true,
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn empty_paste_is_ignored() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(
            &UiEvent::Paste(String::new()),
            &view,
            "",
            "",
            &mut state,
            false,
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn ctrl_c_first_tap_clears_and_interrupts() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = view_with_editor("draft");
        let actions = mapper.map(&ctrl('c'), &view, "draft", "draft", &mut state, false);
        assert_eq!(
            actions,
            vec![ViewAction::ClearEditor, ViewAction::Interrupt]
        );
    }

    #[test]
    fn ctrl_c_double_tap_within_window_exits() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = view_with_editor("x");
        let _ = mapper.map(&ctrl('c'), &view, "x", "x", &mut state, false);
        let actions = mapper.map(&ctrl('c'), &view, "x", "x", &mut state, false);
        assert_eq!(actions, vec![ViewAction::ClearEditor, ViewAction::Exit]);
    }

    #[test]
    fn ctrl_c_double_tap_outside_window_just_interrupts() {
        let mapper = mapper();
        let mut state = InputState::default();
        state.set_sigint_exit_window(Duration::from_nanos(1));
        let view = view_with_editor("x");
        let _ = mapper.map(&ctrl('c'), &view, "x", "x", &mut state, false);
        std::thread::sleep(Duration::from_millis(2));
        let actions = mapper.map(&ctrl('c'), &view, "x", "x", &mut state, false);
        assert_eq!(
            actions,
            vec![ViewAction::ClearEditor, ViewAction::Interrupt]
        );
    }

    #[test]
    fn ctrl_d_exits_only_for_zero_length_editor() {
        let mapper = mapper();
        let mut state = InputState::default();
        let empty = empty_view();
        assert_eq!(
            mapper.map(&ctrl('d'), &empty, "", "", &mut state, false),
            vec![ViewAction::AppExit]
        );

        for text in [" ", "   ", "\n", "x"] {
            let view = view_with_editor(text);
            let actions = mapper.map(&ctrl('d'), &view, text, text, &mut state, false);
            assert!(actions.is_empty(), "{text:?} must not be treated as empty");
        }
    }

    #[test]
    fn ctrl_d_does_not_exit_while_modal_view_owns_focus() {
        let mapper = mapper();
        let mut state = InputState::default();

        let mut selector = empty_view();
        selector.focus = FocusArea::Selector;
        assert!(
            mapper
                .map(&ctrl('d'), &selector, "", "", &mut state, false)
                .is_empty()
        );

        let mut overlay = view_with_overlay(OverlayKind::ShortcutHelp);
        overlay.focus = FocusArea::Overlay;
        assert!(
            mapper
                .map(&ctrl('d'), &overlay, "", "", &mut state, false)
                .is_empty()
        );

        let mut first_run = empty_view();
        first_run.first_run_step = Some(0);
        assert!(
            mapper
                .map(&ctrl('d'), &first_run, "", "", &mut state, false)
                .is_empty()
        );
    }

    #[test]
    fn ctrl_z_emits_suspend() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&ctrl('z'), &view, "", "", &mut state, false);
        if cfg!(windows) {
            assert!(actions.is_empty());
        } else {
            assert_eq!(actions, vec![ViewAction::Suspend]);
        }
    }

    #[test]
    fn shift_tab_cycles_thinking() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&shift(KeyCode::Tab), &view, "", "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::CycleThinking { forward: true }]);
    }

    #[test]
    fn ctrl_p_cycles_model_forward() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&ctrl('p'), &view, "", "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::CycleModel { forward: true }]);
    }

    #[test]
    fn ctrl_shift_p_cycles_model_backward() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&ctrl_shift('P'), &view, "", "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::CycleModel { forward: false }]);
    }

    #[test]
    fn app_keys_dispatch_table() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();

        assert_eq!(
            mapper.map(&ctrl('l'), &view, "", "", &mut state, false),
            vec![ViewAction::OpenModelSelector]
        );
        assert_eq!(
            mapper.map(&ctrl('o'), &view, "", "", &mut state, false),
            vec![ViewAction::ToggleToolExpand]
        );
        assert_eq!(
            mapper.map(&ctrl('t'), &view, "", "", &mut state, false),
            vec![ViewAction::ToggleThinking]
        );
        assert_eq!(
            mapper.map(&ctrl('g'), &view, "", "", &mut state, false),
            vec![ViewAction::ExternalEditor]
        );
        assert_eq!(
            mapper.map(&ctrl('x'), &view, "", "", &mut state, false),
            vec![ViewAction::CopyLastAssistant]
        );
    }

    #[test]
    fn default_chord_fires_action() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        assert_eq!(
            mapper.map(&ctrl('l'), &view, "", "", &mut state, false),
            vec![ViewAction::OpenModelSelector]
        );
        assert_eq!(
            mapper
                .keybindings()
                .get_keys("app.model.select")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["ctrl+l"]
        );
    }

    #[test]
    fn keybindings_json_rebind_moves_action() -> Result<(), Box<dyn std::error::Error>> {
        let temp = TempDir::new()?;
        std::fs::write(
            temp.path().join(KEYBINDINGS_FILE_NAME),
            r#"{"app.model.select": "ctrl+m"}"#,
        )?;
        let mapper = InputMapper::with_keybindings(load_app_keybindings(temp.path()));
        let mut state = InputState::default();
        let view = empty_view();

        // Old default no longer fires.
        assert!(
            mapper
                .map(&ctrl('l'), &view, "", "", &mut state, false)
                .is_empty()
        );
        // Rebound chord fires the action.
        assert_eq!(
            mapper.map(&ctrl('m'), &view, "", "", &mut state, false),
            vec![ViewAction::OpenModelSelector]
        );
        Ok(())
    }

    #[test]
    fn unbound_by_default_verified() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();

        // TS leaves these unbound at the editor level (slash/UI only).
        assert!(
            mapper
                .map(&ctrl('r'), &view, "", "", &mut state, false)
                .is_empty(),
            "ctrl+r must not be Reload by default"
        );
        assert!(
            mapper
                .map(&ctrl('b'), &view, "", "", &mut state, false)
                .is_empty(),
            "ctrl+b must not open session picker by default"
        );
        assert!(
            mapper
                .map(&ctrl('f'), &view, "", "", &mut state, false)
                .is_empty(),
            "ctrl+f must not open tree selector by default"
        );
        // Ctrl+N is app.session.toggleNamedFilter (selector-local); editor is no-op.
        assert!(
            mapper
                .map(&ctrl('n'), &view, "", "", &mut state, false)
                .is_empty(),
            "ctrl+n must not NewSession; named-filter is selector-local"
        );
        assert!(mapper.keybindings().get_keys("app.session.new").is_empty());
        assert!(mapper.keybindings().get_keys("app.session.tree").is_empty());
        assert!(mapper.keybindings().get_keys("app.session.fork").is_empty());
        assert!(
            mapper
                .keybindings()
                .get_keys("app.session.resume")
                .is_empty()
        );
        assert_eq!(
            mapper
                .keybindings()
                .get_keys("app.session.toggleNamedFilter")
                .iter()
                .map(KeyId::as_str)
                .collect::<Vec<_>>(),
            vec!["ctrl+n"]
        );
    }

    #[test]
    fn rebind_session_new_enables_new_session_chord() {
        let mut user = KeybindingsConfig::new();
        user.insert(
            "app.session.new".to_owned(),
            vec![KeyId::from_raw("ctrl+n")],
        );
        // Unbind named-filter so ctrl+n is free for session.new.
        user.insert("app.session.toggleNamedFilter".to_owned(), vec![]);
        let mgr = KeybindingsManager::new(app_keybindings(), user);
        let mapper = InputMapper::with_keybindings(mgr);
        let mut state = InputState::default();
        let view = empty_view();
        assert_eq!(
            mapper.map(&ctrl('n'), &view, "", "", &mut state, false),
            vec![ViewAction::NewSession]
        );
    }

    #[test]
    fn alt_enter_submits_when_not_streaming() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = view_with_editor("hello");
        let actions = mapper.map(
            &alt(KeyCode::Enter),
            &view,
            "hello",
            "hello",
            &mut state,
            false,
        );
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
        let mapper = mapper();
        let mut state = InputState::default();
        let view = view_streaming();
        let actions = mapper.map(
            &alt(KeyCode::Enter),
            &view,
            "more",
            "more",
            &mut state,
            false,
        );
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
    fn alt_enter_submits_expanded_paste_text() {
        // Collapsed buffer shows the paste marker; submission must carry the
        // expanded content, not the marker.
        let mapper = mapper();
        let mut state = InputState::default();
        let view = view_with_editor("[paste #1 +5 lines]");
        let actions = mapper.map(
            &alt(KeyCode::Enter),
            &view,
            "[paste #1 +5 lines]",
            "the real pasted body",
            &mut state,
            false,
        );
        assert_eq!(
            actions,
            vec![
                ViewAction::Submit {
                    text: "the real pasted body".to_owned()
                },
                ViewAction::ClearEditor,
            ]
        );
    }

    #[test]
    fn alt_enter_queues_expanded_paste_text_when_streaming() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = view_streaming();
        let actions = mapper.map(
            &alt(KeyCode::Enter),
            &view,
            "[paste #2 +9 lines]",
            "expanded queued content",
            &mut state,
            false,
        );
        assert_eq!(
            actions,
            vec![
                ViewAction::QueueFollowUp {
                    text: "expanded queued content".to_owned()
                },
                ViewAction::ClearEditor,
            ]
        );
    }

    #[test]
    fn alt_enter_empty_is_noop() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&alt(KeyCode::Enter), &view, "   ", "   ", &mut state, false);
        assert!(actions.is_empty());
    }

    #[test]
    fn alt_up_dequeues() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&alt(KeyCode::Up), &view, "", "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::DequeueFollowUp]);
    }

    #[test]
    fn esc_dismisses_overlay_first() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = view_with_overlay(OverlayKind::ShortcutHelp);
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "abc", "abc", &mut state, false);
        assert_eq!(actions, vec![ViewAction::DismissOverlay]);
    }

    #[test]
    fn esc_cancels_extension_dialog_before_clearing_editor() {
        let mapper = mapper();
        let mut state = InputState::default();
        let mut view = view_with_editor("draft");
        view.extension_dialog = true;
        let actions = mapper.map(
            &plain(KeyCode::Esc),
            &view,
            "draft",
            "draft",
            &mut state,
            false,
        );
        assert_eq!(actions, vec![ViewAction::SelectCancelled]);
    }

    #[test]
    fn esc_interrupts_when_streaming() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = view_streaming();
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "x", "x", &mut state, false);
        assert_eq!(actions, vec![ViewAction::Interrupt]);
    }

    #[test]
    fn esc_interrupts_when_compacting() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = view_with_status(StatusKind::Compaction);
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "x", "x", &mut state, false);
        assert_eq!(actions, vec![ViewAction::Interrupt]);
    }

    #[test]
    fn esc_clears_editor_when_nonempty_and_idle() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = view_with_editor("draft");
        let actions = mapper.map(
            &plain(KeyCode::Esc),
            &view,
            "draft",
            "draft",
            &mut state,
            false,
        );
        assert_eq!(actions, vec![ViewAction::ClearEditor]);
    }

    #[test]
    fn esc_double_tap_on_empty_opens_tree_when_configured() {
        let mapper = mapper();
        let mut state = InputState::new(DoubleEscapeAction::Tree);
        let view = empty_view();
        let _ = mapper.map(&plain(KeyCode::Esc), &view, "", "", &mut state, false);
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "", "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::OpenTreeSelector]);
    }

    #[test]
    fn esc_double_tap_on_empty_opens_fork_when_configured() {
        let mapper = mapper();
        let mut state = InputState::new(DoubleEscapeAction::Fork);
        let view = empty_view();
        let _ = mapper.map(&plain(KeyCode::Esc), &view, "", "", &mut state, false);
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "", "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::OpenForkSelector]);
    }

    #[test]
    fn default_state_double_esc_opens_tree() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let _ = mapper.map(&plain(KeyCode::Esc), &view, "", "", &mut state, false);
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "", "", &mut state, false);
        assert_eq!(actions, vec![ViewAction::OpenTreeSelector]);
    }

    #[test]
    fn esc_single_tap_on_empty_when_double_disabled_is_noop() {
        let mapper = mapper();
        let mut state = InputState::new(DoubleEscapeAction::None);
        let view = empty_view();
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "", "", &mut state, false);
        assert!(actions.is_empty());
        assert!(state.last_escape.is_none());
    }

    #[test]
    fn esc_double_tap_outside_window_records_new_tap() {
        let mapper = mapper();
        let mut state = InputState::new(DoubleEscapeAction::Tree);
        state.set_escape_double_window(Duration::from_nanos(1));
        let view = empty_view();
        let _ = mapper.map(&plain(KeyCode::Esc), &view, "", "", &mut state, false);
        std::thread::sleep(Duration::from_millis(2));
        let actions = mapper.map(&plain(KeyCode::Esc), &view, "", "", &mut state, false);
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
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&UiEvent::FocusGained, &view, "", "", &mut state, false);
        assert!(actions.is_empty());
        let actions = mapper.map(&UiEvent::FocusLost, &view, "", "", &mut state, false);
        assert!(actions.is_empty());
    }

    #[test]
    fn editor_consumed_silences_app_keys() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&ctrl('l'), &view, "", "", &mut state, true);
        assert!(actions.is_empty());
    }

    #[test]
    fn ctrl_v_emits_empty_paste_for_clipboard_image_path() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(&ctrl('v'), &view, "", "", &mut state, false);
        if cfg!(windows) {
            assert!(actions.is_empty());
        } else {
            assert_eq!(
                actions,
                vec![ViewAction::Paste {
                    text: String::new()
                }]
            );
        }
    }

    #[test]
    fn unknown_key_is_ignored() {
        let mapper = mapper();
        let mut state = InputState::default();
        let view = empty_view();
        let actions = mapper.map(
            &key(KeyCode::Char('a'), KeyModifiers::NONE),
            &view,
            "",
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
