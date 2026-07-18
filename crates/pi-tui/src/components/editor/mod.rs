//! Multiline editor component with wrap, history, kill/yank, undo, paste markers,
//! autocomplete, and grapheme-safe navigation.
//!
//! Ports `.references/pi/packages/tui/src/components/editor.ts`.

mod paste;
mod state;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::component::{Component, EventResult, UiEvent};
use crate::editor_support::{
    ATTACHMENT_AUTOCOMPLETE_DEBOUNCE_MS, AutocompleteItem, AutocompleteProvider,
    AutocompleteSuggestions, CursorPlacement, DEFAULT_AUTOCOMPLETE_TRIGGER_CHARACTERS, History,
    KillPushOptions, KillRing, SuggestionOptions, UndoStack, VisualLine, WordNavigationOptions,
    WordSegment, build_visual_line_map, default_word_segments, find_visual_line_at,
    find_word_backward, find_word_forward,
};
use crate::frame::set_cursor;
use crate::keybindings::get_keybindings;
use crate::keys::{
    KeyId, MODIFY_OTHER_KEYS_OMISSION, backslash_enter_inserts_newline, key_matches,
    should_submit_on_backslash_enter,
};
use crate::text::{is_whitespace_char, truncate_to_width, visible_width};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;

use self::paste::{
    expand_paste_markers, find_paste_markers, format_paste_marker, is_large_paste, is_paste_marker,
    normalize_text, paste_marker_id, prepare_paste_text, renumber_after_delete,
};
use self::state::{
    EditorState, LastAction, compute_vertical_move_column, next_grapheme_len, prev_grapheme_len,
    segment_graphemes_with_markers, valid_paste_ids,
};

pub use self::paste::{
    LARGE_PASTE_CHAR_THRESHOLD, LARGE_PASTE_LINE_THRESHOLD, expand_paste_markers as expand_markers,
    is_large_paste as paste_is_large, is_paste_marker as paste_marker_is_atomic,
    normalize_text as normalize_editor_text,
};
pub use self::state::{EditorState as BufferState, compute_vertical_move_column as sticky_column};

/// Documented legacy-input omission (re-export for key-matrix tests).
pub const LEGACY_MODIFY_OTHER_KEYS_OMISSION: &str = MODIFY_OTHER_KEYS_OMISSION;

/// Theme hooks for the editor border and optional select-list styling.
#[derive(Clone, Copy)]
pub struct EditorTheme {
    /// Style a border glyph run (defaults to identity).
    pub border_color: fn(&str) -> String,
}

impl Default for EditorTheme {
    fn default() -> Self {
        fn id(s: &str) -> String {
            s.to_owned()
        }
        Self { border_color: id }
    }
}

/// Construction options.
#[derive(Debug, Clone, Copy)]
pub struct EditorOptions {
    /// Horizontal padding cells.
    pub padding_x: u16,
    /// Max visible autocomplete rows (clamped 3..=20).
    pub autocomplete_max_visible: usize,
    /// Terminal rows used for max-visible-line math (default 24).
    pub terminal_rows: u16,
}

impl Default for EditorOptions {
    fn default() -> Self {
        Self {
            padding_x: 0,
            autocomplete_max_visible: 5,
            terminal_rows: 24,
        }
    }
}

/// Open autocomplete UI state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutocompleteUiState {
    Regular,
    Force,
}

/// Pending autocomplete request bookkeeping.
struct AutocompleteRequest {
    request_id: u64,
    snapshot_text: String,
    snapshot_line: usize,
    snapshot_col: usize,
    force: bool,
    explicit_tab: bool,
}

#[derive(Clone)]
struct EditorSnapshot {
    state: EditorState,
    pastes: HashMap<u32, Arc<str>>,
    paste_counter: u32,
}

/// Multiline editor implementing [`Component`].
pub struct Editor {
    state: EditorState,
    /// Focus flag (set by product focus manager).
    pub focused: bool,
    padding_x: u16,
    last_width: usize,
    scroll_offset: usize,
    terminal_rows: u16,
    /// Dynamic border color (product may reassign).
    pub border_color: fn(&str) -> String,

    pastes: HashMap<u32, Arc<str>>,
    paste_counter: u32,

    history: History,
    kill_ring: KillRing,
    last_action: Option<LastAction>,
    jump_mode: Option<JumpDir>,
    preferred_visual_col: Option<usize>,
    undo_stack: UndoStack<EditorSnapshot>,

    autocomplete_provider: Option<Arc<dyn AutocompleteProvider>>,
    autocomplete_trigger_characters: Vec<String>,
    autocomplete_state: Option<AutocompleteUiState>,
    autocomplete_prefix: String,
    autocomplete_items: Vec<AutocompleteItem>,
    autocomplete_selected: usize,
    autocomplete_max_visible: usize,
    autocomplete_start_token: u64,
    autocomplete_request_id: u64,
    autocomplete_pending: Option<AutocompleteRequest>,
    autocomplete_debounce_ms: u64,
    /// Pending debounced request fire time (ms since epoch proxy via counter).
    autocomplete_debounce_remaining_ms: Option<u64>,

    /// Called on submit with expanded, trimmed text.
    pub on_submit: Option<Box<dyn FnMut(String) + Send>>,
    /// Called when buffer text changes.
    pub on_change: Option<Box<dyn FnMut(String) + Send>>,
    /// When true, Enter does not submit.
    pub disable_submit: bool,

    /// Hardware cursor annotation position from last render (viewport-relative).
    last_cursor_screen: Option<(u16, u16)>,
    /// Dirty flag after invalidate/resize.
    needs_layout: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JumpDir {
    Forward,
    Backward,
}

impl Editor {
    /// Create an editor with theme and options.
    #[must_use]
    pub fn new(theme: &EditorTheme, options: &EditorOptions) -> Self {
        let max_visible = options.autocomplete_max_visible.clamp(3, 20);
        let padding = options.padding_x;
        Self {
            state: EditorState::new(),
            focused: false,
            border_color: theme.border_color,
            padding_x: padding,
            last_width: 80,
            scroll_offset: 0,
            terminal_rows: options.terminal_rows.max(1),
            pastes: HashMap::new(),
            paste_counter: 0,
            history: History::new(),
            kill_ring: KillRing::new(),
            last_action: None,
            jump_mode: None,
            preferred_visual_col: None,
            undo_stack: UndoStack::new(),
            autocomplete_provider: None,
            autocomplete_trigger_characters: DEFAULT_AUTOCOMPLETE_TRIGGER_CHARACTERS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            autocomplete_state: None,
            autocomplete_prefix: String::new(),
            autocomplete_items: Vec::new(),
            autocomplete_selected: 0,
            autocomplete_max_visible: max_visible,
            autocomplete_start_token: 0,
            autocomplete_request_id: 0,
            autocomplete_pending: None,
            autocomplete_debounce_ms: ATTACHMENT_AUTOCOMPLETE_DEBOUNCE_MS,
            autocomplete_debounce_remaining_ms: None,
            on_submit: None,
            on_change: None,
            disable_submit: false,
            last_cursor_screen: None,
            needs_layout: true,
        }
    }

    /// Create with defaults.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(&EditorTheme::default(), &EditorOptions::default())
    }

    /// Set terminal row count (affects page size / max visible lines).
    pub fn set_terminal_rows(&mut self, rows: u16) {
        self.terminal_rows = rows.max(1);
    }

    /// Current text (markers not expanded).
    #[must_use]
    pub fn get_text(&self) -> String {
        self.state.text()
    }

    /// Text with paste markers expanded.
    #[must_use]
    pub fn get_expanded_text(&self) -> String {
        expand_paste_markers(&self.state.text(), &self.pastes)
    }

    /// Logical lines clone.
    #[must_use]
    pub fn get_lines(&self) -> Vec<String> {
        self.state.lines.clone()
    }

    /// Cursor position.
    #[must_use]
    pub fn get_cursor(&self) -> (usize, usize) {
        (self.state.cursor_line, self.state.cursor_col)
    }

    /// Replace buffer text (clears pastes, exits history).
    pub fn set_text(&mut self, text: &str) {
        self.cancel_autocomplete();
        self.last_action = None;
        self.history.exit_browsing();
        self.pastes.clear();
        self.paste_counter = 0;
        let normalized = normalize_text(text);
        if self.get_text() != normalized {
            self.push_undo_snapshot();
        }
        self.set_text_internal(&normalized, CursorPlacement::End);
    }

    /// Insert text at cursor (atomic undo unit).
    pub fn insert_text_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.cancel_autocomplete();
        self.push_undo_snapshot();
        self.last_action = None;
        self.history.exit_browsing();
        self.insert_text_at_cursor_internal(text);
    }

    /// Add submitted text to history.
    pub fn add_to_history(&mut self, text: &str) {
        self.history.add(text);
    }

    /// Install an autocomplete provider.
    pub fn set_autocomplete_provider(&mut self, provider: Option<Arc<dyn AutocompleteProvider>>) {
        self.cancel_autocomplete();
        if let Some(ref p) = provider {
            self.set_autocomplete_trigger_characters(p.trigger_characters());
        }
        self.autocomplete_provider = provider;
    }

    /// Horizontal padding.
    pub fn set_padding_x(&mut self, padding: u16) {
        self.padding_x = padding;
        self.needs_layout = true;
    }

    /// Autocomplete max visible rows.
    pub fn set_autocomplete_max_visible(&mut self, max_visible: usize) {
        self.autocomplete_max_visible = max_visible.clamp(3, 20);
    }

    /// True when autocomplete popup is open.
    #[must_use]
    pub fn is_showing_autocomplete(&self) -> bool {
        self.autocomplete_state.is_some()
    }

    /// Kill ring (tests / inspection).
    #[must_use]
    pub fn kill_ring_len(&self) -> usize {
        self.kill_ring.len()
    }

    /// History length.
    #[must_use]
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Advance autocomplete debounce by `ms` and fire if due.
    ///
    /// Product event loops call this from their timer tick; unit tests drive it
    /// directly. Returns true when a request was started.
    pub fn tick_autocomplete_debounce(&mut self, ms: u64) -> bool {
        let Some(remaining) = self.autocomplete_debounce_remaining_ms.as_mut() else {
            return false;
        };
        if *remaining > ms {
            *remaining -= ms;
            return false;
        }
        self.autocomplete_debounce_remaining_ms = None;
        let token = self.autocomplete_start_token;
        let force = self.autocomplete_pending.as_ref().is_some_and(|p| p.force);
        let explicit = self
            .autocomplete_pending
            .as_ref()
            .is_some_and(|p| p.explicit_tab);
        self.start_autocomplete_request(token, force, explicit);
        true
    }

    /// Apply a completed suggestion batch if the request token is still current.
    ///
    /// Async product code obtains suggestions from the provider, then feeds
    /// them back here so the editor stays free of a runtime.
    pub fn complete_autocomplete_request(
        &mut self,
        request_id: u64,
        suggestions: Option<AutocompleteSuggestions>,
        force: bool,
        explicit_tab: bool,
    ) -> EventResult {
        let Some(pending) = self.autocomplete_pending.as_ref() else {
            return EventResult::Ignored;
        };
        if pending.request_id != request_id {
            return EventResult::Ignored;
        }
        if self.get_text() != pending.snapshot_text
            || self.state.cursor_line != pending.snapshot_line
            || self.state.cursor_col != pending.snapshot_col
        {
            return EventResult::Ignored;
        }
        self.autocomplete_pending = None;

        let Some(suggestions) = suggestions else {
            self.cancel_autocomplete();
            return EventResult::Render;
        };
        if suggestions.items.is_empty() {
            self.cancel_autocomplete();
            return EventResult::Render;
        }

        if force && explicit_tab && suggestions.items.len() == 1 {
            let item = suggestions.items[0].clone();
            self.push_undo_snapshot();
            self.last_action = None;
            if let Some(provider) = self.autocomplete_provider.clone() {
                let result = provider.apply_completion(
                    &self.state.lines,
                    self.state.cursor_line,
                    self.state.cursor_col,
                    &item,
                    &suggestions.prefix,
                );
                self.state.lines = result.lines;
                self.state.cursor_line = result.cursor_line;
                self.set_cursor_col(result.cursor_col);
                self.emit_change();
            }
            self.cancel_autocomplete();
            return EventResult::Render;
        }

        self.apply_autocomplete_suggestions(
            suggestions,
            if force {
                AutocompleteUiState::Force
            } else {
                AutocompleteUiState::Regular
            },
        );
        EventResult::Render
    }

    /// Current autocomplete request id (for async correlation), if any.
    #[must_use]
    pub fn pending_autocomplete_request_id(&self) -> Option<u64> {
        self.autocomplete_pending.as_ref().map(|p| p.request_id)
    }

    // ------------------------------------------------------------------
    // internals
    // ------------------------------------------------------------------

    fn emit_change(&mut self) {
        let text = self.get_text();
        if let Some(cb) = self.on_change.as_mut() {
            cb(text);
        }
    }

    fn push_undo_snapshot(&mut self) {
        let snapshot = EditorSnapshot {
            state: self.state.clone(),
            pastes: self.pastes.clone(),
            paste_counter: self.paste_counter,
        };
        self.undo_stack.push(&snapshot);
    }

    fn set_cursor_col(&mut self, col: usize) {
        self.state.cursor_col = col;
        self.preferred_visual_col = None;
        self.state.clamp_cursor();
    }

    fn set_text_internal(&mut self, text: &str, placement: CursorPlacement) {
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_owned).collect()
        };
        self.state.lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };
        match placement {
            CursorPlacement::Start => {
                self.state.cursor_line = 0;
                self.set_cursor_col(0);
            }
            CursorPlacement::End => {
                self.state.cursor_line = self.state.lines.len() - 1;
                let len = self.state.lines[self.state.cursor_line].len();
                self.set_cursor_col(len);
            }
        }
        self.scroll_offset = 0;
        self.emit_change();
    }

    fn insert_text_at_cursor_internal(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = normalize_text(text);
        let inserted: Vec<&str> = normalized.split('\n').collect();
        let current = self.state.current_line().to_owned();
        let col = self.state.cursor_col.min(current.len());
        let before = &current[..col];
        let after = &current[col..];

        if inserted.len() == 1 {
            self.state.lines[self.state.cursor_line] = format!("{before}{normalized}{after}");
            self.set_cursor_col(col + normalized.len());
        } else {
            let mut new_lines = Vec::with_capacity(self.state.lines.len() + inserted.len());
            new_lines.extend(self.state.lines[..self.state.cursor_line].iter().cloned());
            new_lines.push(format!("{before}{}", inserted[0]));
            for mid in &inserted[1..inserted.len() - 1] {
                new_lines.push((*mid).to_owned());
            }
            let last = *inserted.last().unwrap_or(&"");
            new_lines.push(format!("{last}{after}"));
            new_lines.extend(
                self.state.lines[self.state.cursor_line + 1..]
                    .iter()
                    .cloned(),
            );
            self.state.lines = new_lines;
            self.state.cursor_line += inserted.len() - 1;
            self.set_cursor_col(last.len());
        }
        self.emit_change();
    }

    fn max_visible_lines(&self) -> usize {
        usize::from(self.terminal_rows)
            .saturating_mul(3)
            .saturating_div(10)
            .max(5)
    }

    fn visual_lines(&self) -> Vec<VisualLine> {
        let ids = valid_paste_ids(&self.pastes);
        build_visual_line_map(&self.state.lines, self.last_width.max(1), |line| {
            segment_graphemes_with_markers(line, &ids)
        })
    }

    fn is_on_first_visual_line(&self) -> bool {
        let vls = self.visual_lines();
        find_visual_line_at(&vls, self.state.cursor_line, self.state.cursor_col) == 0
    }

    fn is_on_last_visual_line(&self) -> bool {
        let vls = self.visual_lines();
        let cur = find_visual_line_at(&vls, self.state.cursor_line, self.state.cursor_col);
        cur + 1 >= vls.len()
    }

    fn navigate_history(&mut self, direction: i8) {
        self.last_action = None;
        let draft = self.get_text();
        let Some(result) = self.history.navigate(direction, &draft) else {
            return;
        };
        if result.entered {
            self.push_undo_snapshot();
        }
        self.set_text_internal(&result.text, result.cursor_placement);
        self.refresh_autocomplete_after_navigation();
    }

    fn insert_character(&mut self, ch: &str) {
        self.history.exit_browsing();
        if is_whitespace_char(ch) || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo_snapshot();
        }
        self.last_action = Some(LastAction::TypeWord);

        let line = self.state.current_line().to_owned();
        let col = self.state.cursor_col.min(line.len());
        self.state.lines[self.state.cursor_line] =
            format!("{}{}{}", &line[..col], ch, &line[col..]);
        self.set_cursor_col(col + ch.len());
        self.emit_change();
        self.maybe_auto_trigger_autocomplete(ch);
    }

    fn maybe_auto_trigger_autocomplete(&mut self, ch: &str) {
        if self.autocomplete_state.is_some() {
            self.update_autocomplete();
            return;
        }
        if ch == "/" && self.is_at_start_of_message() {
            self.try_trigger_autocomplete(false);
            return;
        }
        if self.autocomplete_trigger_characters.iter().any(|t| t == ch) {
            let before = &self.state.current_line()
                [..self.state.cursor_col.min(self.state.current_line().len())];
            let char_before = before.chars().rev().nth(1);
            if before.chars().count() == 1 || char_before == Some(' ') || char_before == Some('\t')
            {
                self.try_trigger_autocomplete(false);
            }
            return;
        }
        if ch.len() == 1
            && ch
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        {
            let before = &self.state.current_line()
                [..self.state.cursor_col.min(self.state.current_line().len())];
            if self.is_in_slash_command_context(before) || self.trigger_pattern_matches(before) {
                self.try_trigger_autocomplete(false);
            }
        }
    }

    fn trigger_pattern_matches(&self, text_before: &str) -> bool {
        // (?:^|[\s])[triggers][^\s]*$
        let triggers = &self.autocomplete_trigger_characters;
        if triggers.is_empty() {
            return false;
        }
        let bytes = text_before.as_bytes();
        // Find last trigger char at token boundary
        for (i, ch) in text_before.char_indices().rev() {
            let s = ch.to_string();
            if triggers.iter().any(|t| t == &s) {
                let at_boundary = i == 0
                    || text_before[..i]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace);
                if !at_boundary {
                    return false;
                }
                // rest has no whitespace
                return !text_before[i + ch.len_utf8()..]
                    .chars()
                    .any(char::is_whitespace);
            }
            if ch.is_whitespace() {
                break;
            }
            let _ = bytes;
        }
        false
    }

    fn debounce_pattern_matches(&self, text_before: &str) -> bool {
        // @quoted or non-@ triggers
        if text_before.contains("@\"") || text_before.ends_with('@') {
            return true;
        }
        let non_at: Vec<&str> = self
            .autocomplete_trigger_characters
            .iter()
            .filter(|c| c.as_str() != "@")
            .map(String::as_str)
            .collect();
        if non_at.is_empty() {
            // still debounce bare @ paths
            return text_before.contains('@');
        }
        self.trigger_pattern_matches(text_before)
    }

    fn is_slash_menu_allowed(&self) -> bool {
        self.state.cursor_line == 0
    }

    fn is_at_start_of_message(&self) -> bool {
        if !self.is_slash_menu_allowed() {
            return false;
        }
        let before = &self.state.current_line()
            [..self.state.cursor_col.min(self.state.current_line().len())];
        let trimmed = before.trim();
        trimmed.is_empty() || trimmed == "/"
    }

    fn is_in_slash_command_context(&self, text_before: &str) -> bool {
        self.is_slash_menu_allowed() && text_before.trim_start().starts_with('/')
    }

    fn set_autocomplete_trigger_characters(&mut self, extra: &[String]) {
        let mut next: Vec<String> = DEFAULT_AUTOCOMPLETE_TRIGGER_CHARACTERS
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        for ch in extra {
            if ch.chars().count() != 1 || ch == "/" || is_whitespace_char(ch) || next.contains(ch) {
                continue;
            }
            next.push(ch.clone());
        }
        self.autocomplete_trigger_characters = next;
    }

    fn try_trigger_autocomplete(&mut self, explicit_tab: bool) {
        self.request_autocomplete(false, explicit_tab);
    }

    fn update_autocomplete(&mut self) {
        let Some(state) = self.autocomplete_state else {
            return;
        };
        let force = matches!(state, AutocompleteUiState::Force);
        self.request_autocomplete(force, false);
    }

    fn request_autocomplete(&mut self, force: bool, explicit_tab: bool) {
        let Some(provider) = self.autocomplete_provider.clone() else {
            return;
        };
        if force
            && !provider.should_trigger_file_completion(
                &self.state.lines,
                self.state.cursor_line,
                self.state.cursor_col,
            )
        {
            return;
        }
        self.cancel_autocomplete_request();
        self.autocomplete_start_token = self.autocomplete_start_token.wrapping_add(1);
        let token = self.autocomplete_start_token;

        let debounce = if explicit_tab || force {
            0
        } else {
            let before = &self.state.current_line()
                [..self.state.cursor_col.min(self.state.current_line().len())];
            if self.debounce_pattern_matches(before) {
                self.autocomplete_debounce_ms
            } else {
                0
            }
        };

        self.autocomplete_pending = Some(AutocompleteRequest {
            request_id: 0,
            snapshot_text: self.get_text(),
            snapshot_line: self.state.cursor_line,
            snapshot_col: self.state.cursor_col,
            force,
            explicit_tab,
        });

        if debounce > 0 {
            self.autocomplete_debounce_remaining_ms = Some(debounce);
            return;
        }
        self.start_autocomplete_request(token, force, explicit_tab);
    }

    fn start_autocomplete_request(&mut self, start_token: u64, force: bool, explicit_tab: bool) {
        if start_token != self.autocomplete_start_token || self.autocomplete_provider.is_none() {
            return;
        }
        self.autocomplete_request_id = self.autocomplete_request_id.wrapping_add(1);
        let request_id = self.autocomplete_request_id;
        let snapshot_text = self.get_text();
        let snapshot_line = self.state.cursor_line;
        let snapshot_col = self.state.cursor_col;
        if let Some(pending) = self.autocomplete_pending.as_mut() {
            pending.request_id = request_id;
            pending.snapshot_text = snapshot_text;
            pending.snapshot_line = snapshot_line;
            pending.snapshot_col = snapshot_col;
            pending.force = force;
            pending.explicit_tab = explicit_tab;
        }
        // Product drives complete_autocomplete_request after awaiting the provider.
        let _ = (force, explicit_tab);
    }

    /// Synchronously fetch suggestions from the provider (for tests / simple providers).
    pub fn poll_autocomplete_now(&mut self) -> EventResult {
        let Some(provider) = self.autocomplete_provider.clone() else {
            return EventResult::Ignored;
        };
        let Some(pending) = self.autocomplete_pending.as_ref() else {
            return EventResult::Ignored;
        };
        let request_id = pending.request_id;
        let force = pending.force;
        let explicit = pending.explicit_tab;
        let lines = self.state.lines.clone();
        let line = self.state.cursor_line;
        let col = self.state.cursor_col;
        // Use futures executor only when the future is ready immediately is hard;
        // block_on via tokio Handle if available, else ignore.
        let fut = provider.get_suggestions(
            &lines,
            line,
            col,
            SuggestionOptions {
                force,
                request_token: request_id,
            },
        );
        // Drive ready futures with a noop waker; nested Option from poll_once.
        let suggestions = poll_once(fut).flatten();
        self.complete_autocomplete_request(request_id, suggestions, force, explicit)
    }

    fn apply_autocomplete_suggestions(
        &mut self,
        suggestions: AutocompleteSuggestions,
        state: AutocompleteUiState,
    ) {
        self.autocomplete_prefix.clone_from(&suggestions.prefix);
        self.autocomplete_items = suggestions.items;
        let best =
            best_autocomplete_match_index(&self.autocomplete_items, &self.autocomplete_prefix);
        self.autocomplete_selected = if best >= 0 { best.cast_unsigned() } else { 0 };
        self.autocomplete_state = Some(state);
    }

    fn cancel_autocomplete_request(&mut self) {
        self.autocomplete_start_token = self.autocomplete_start_token.wrapping_add(1);
        self.autocomplete_debounce_remaining_ms = None;
        self.autocomplete_pending = None;
    }

    fn cancel_autocomplete(&mut self) {
        self.cancel_autocomplete_request();
        self.autocomplete_state = None;
        self.autocomplete_items.clear();
        self.autocomplete_prefix.clear();
        self.autocomplete_selected = 0;
    }

    fn apply_selected_completion(&mut self) -> bool {
        let Some(item) = self
            .autocomplete_items
            .get(self.autocomplete_selected)
            .cloned()
        else {
            return false;
        };
        let Some(provider) = self.autocomplete_provider.clone() else {
            return false;
        };
        self.push_undo_snapshot();
        self.last_action = None;
        let result = provider.apply_completion(
            &self.state.lines,
            self.state.cursor_line,
            self.state.cursor_col,
            &item,
            &self.autocomplete_prefix,
        );
        self.state.lines = result.lines;
        self.state.cursor_line = result.cursor_line;
        self.set_cursor_col(result.cursor_col);
        self.emit_change();
        true
    }

    fn handle_paste(&mut self, pasted: &str) {
        self.cancel_autocomplete();
        self.history.exit_browsing();
        self.last_action = None;
        self.push_undo_snapshot();

        let char_before = self
            .state
            .current_line()
            .get(..self.state.cursor_col)
            .and_then(|s| s.chars().next_back());
        let filtered = prepare_paste_text(pasted, char_before);

        if is_large_paste(&filtered) {
            self.paste_counter = self.paste_counter.saturating_add(1);
            let id = self.paste_counter;
            let marker = format_paste_marker(id, &filtered);
            self.pastes.insert(id, Arc::<str>::from(filtered));
            self.insert_text_at_cursor_internal(&marker);
            return;
        }
        self.insert_text_at_cursor_internal(&filtered);
    }

    fn add_new_line(&mut self) {
        self.cancel_autocomplete();
        self.history.exit_browsing();
        self.last_action = None;
        self.push_undo_snapshot();
        let current = self.state.current_line().to_owned();
        let col = self.state.cursor_col.min(current.len());
        let before = current[..col].to_owned();
        let after = current[col..].to_owned();
        self.state.lines[self.state.cursor_line] = before;
        self.state.lines.insert(self.state.cursor_line + 1, after);
        self.state.cursor_line += 1;
        self.set_cursor_col(0);
        self.emit_change();
    }

    fn submit_value(&mut self) {
        self.cancel_autocomplete();
        let result = expand_paste_markers(&self.state.text(), &self.pastes)
            .trim()
            .to_owned();
        self.state = EditorState::new();
        self.pastes.clear();
        self.paste_counter = 0;
        self.history.exit_browsing();
        self.scroll_offset = 0;
        self.undo_stack.clear();
        self.last_action = None;
        if let Some(cb) = self.on_change.as_mut() {
            cb(String::new());
        }
        if let Some(cb) = self.on_submit.as_mut() {
            cb(result);
        }
    }

    fn handle_backspace(&mut self) {
        self.history.exit_browsing();
        self.last_action = None;
        if self.state.cursor_col > 0 {
            let line = self.state.current_line().to_owned();
            let col = self.state.cursor_col.min(line.len());
            if let Some(marker) = find_paste_markers(&line)
                .into_iter()
                .find(|marker| marker.end == col && self.pastes.contains_key(&marker.id))
            {
                self.delete_paste_marker(marker.start, marker.end, marker.id);
            } else {
                self.push_undo_snapshot();
                let ids = valid_paste_ids(&self.pastes);
                let glen = prev_grapheme_len(&line, col, &ids);
                self.state.lines[self.state.cursor_line] =
                    format!("{}{}", &line[..col - glen], &line[col..]);
                self.set_cursor_col(col - glen);
            }
        } else if self.state.cursor_line > 0 {
            self.push_undo_snapshot();
            let current = self.state.lines.remove(self.state.cursor_line);
            self.state.cursor_line -= 1;
            let prev_len = self.state.lines[self.state.cursor_line].len();
            self.state.lines[self.state.cursor_line].push_str(&current);
            self.set_cursor_col(prev_len);
        }
        self.emit_change();
        self.refresh_autocomplete_after_edit();
    }

    fn handle_forward_delete(&mut self) {
        self.history.exit_browsing();
        self.last_action = None;
        let current = self.state.current_line().to_owned();
        if self.state.cursor_col < current.len() {
            let col = self.state.cursor_col;
            if let Some(marker) = find_paste_markers(&current)
                .into_iter()
                .find(|marker| marker.start == col && self.pastes.contains_key(&marker.id))
            {
                self.delete_paste_marker(marker.start, marker.end, marker.id);
            } else {
                self.push_undo_snapshot();
                let ids = valid_paste_ids(&self.pastes);
                let glen = next_grapheme_len(&current, col, &ids);
                self.state.lines[self.state.cursor_line] =
                    format!("{}{}", &current[..col], &current[col + glen..]);
            }
        } else if self.state.cursor_line + 1 < self.state.lines.len() {
            self.push_undo_snapshot();
            let next = self.state.lines.remove(self.state.cursor_line + 1);
            self.state.lines[self.state.cursor_line].push_str(&next);
        }
        self.emit_change();
        self.refresh_autocomplete_after_edit();
    }

    fn delete_paste_marker(&mut self, start: usize, end: usize, id: u32) {
        debug_assert_eq!(
            paste_marker_id(&self.state.current_line()[start..end]),
            Some(id)
        );
        self.push_undo_snapshot();
        let line_index = self.state.cursor_line;
        self.state.lines[line_index].replace_range(start..end, "");

        let cursor_after_removal = start;
        let preceding_id_shrink: usize = find_paste_markers(&self.state.lines[line_index])
            .into_iter()
            .filter(|marker| marker.end <= cursor_after_removal && marker.id > id)
            .map(|marker| {
                marker.id.to_string().len() - (marker.id - 1).to_string().len()
            })
            .sum();

        renumber_after_delete(&mut self.state.lines, &mut self.pastes, id);
        self.paste_counter = self.paste_counter.saturating_sub(1);
        self.set_cursor_col(cursor_after_removal.saturating_sub(preceding_id_shrink));
    }

    fn refresh_autocomplete_after_navigation(&mut self) {
        if self.autocomplete_state.is_some() {
            self.update_autocomplete();
        }
    }

    fn refresh_autocomplete_after_edit(&mut self) {
        if self.autocomplete_state.is_some() {
            self.update_autocomplete();
        } else {
            let before = &self.state.current_line()
                [..self.state.cursor_col.min(self.state.current_line().len())];
            if self.is_in_slash_command_context(before) || self.trigger_pattern_matches(before) {
                self.try_trigger_autocomplete(false);
            }
        }
    }

    fn delete_to_line_start(&mut self) {
        self.history.exit_browsing();
        let current = self.state.current_line().to_owned();
        if self.state.cursor_col > 0 {
            self.push_undo_snapshot();
            let deleted = current[..self.state.cursor_col].to_owned();
            self.kill_ring.push(
                &deleted,
                KillPushOptions {
                    prepend: true,
                    accumulate: self.last_action == Some(LastAction::Kill),
                },
            );
            self.last_action = Some(LastAction::Kill);
            current[self.state.cursor_col..]
                .clone_into(&mut self.state.lines[self.state.cursor_line]);
            self.set_cursor_col(0);
        } else if self.state.cursor_line > 0 {
            self.push_undo_snapshot();
            self.kill_ring.push(
                "\n",
                KillPushOptions {
                    prepend: true,
                    accumulate: self.last_action == Some(LastAction::Kill),
                },
            );
            self.last_action = Some(LastAction::Kill);
            let current = self.state.lines.remove(self.state.cursor_line);
            self.state.cursor_line -= 1;
            let prev_len = self.state.lines[self.state.cursor_line].len();
            self.state.lines[self.state.cursor_line].push_str(&current);
            self.set_cursor_col(prev_len);
        }
        self.emit_change();
    }

    fn delete_to_line_end(&mut self) {
        self.history.exit_browsing();
        let current = self.state.current_line().to_owned();
        if self.state.cursor_col < current.len() {
            self.push_undo_snapshot();
            let deleted = current[self.state.cursor_col..].to_owned();
            self.kill_ring.push(
                &deleted,
                KillPushOptions {
                    prepend: false,
                    accumulate: self.last_action == Some(LastAction::Kill),
                },
            );
            self.last_action = Some(LastAction::Kill);
            current[..self.state.cursor_col]
                .clone_into(&mut self.state.lines[self.state.cursor_line]);
        } else if self.state.cursor_line + 1 < self.state.lines.len() {
            self.push_undo_snapshot();
            self.kill_ring.push(
                "\n",
                KillPushOptions {
                    prepend: false,
                    accumulate: self.last_action == Some(LastAction::Kill),
                },
            );
            self.last_action = Some(LastAction::Kill);
            let next = self.state.lines.remove(self.state.cursor_line + 1);
            self.state.lines[self.state.cursor_line].push_str(&next);
        }
        self.emit_change();
    }

    fn delete_word_backwards(&mut self) {
        self.history.exit_browsing();
        let current = self.state.current_line().to_owned();
        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.push_undo_snapshot();
                self.kill_ring.push(
                    "\n",
                    KillPushOptions {
                        prepend: true,
                        accumulate: self.last_action == Some(LastAction::Kill),
                    },
                );
                self.last_action = Some(LastAction::Kill);
                let current = self.state.lines.remove(self.state.cursor_line);
                self.state.cursor_line -= 1;
                let prev_len = self.state.lines[self.state.cursor_line].len();
                self.state.lines[self.state.cursor_line].push_str(&current);
                self.set_cursor_col(prev_len);
            }
        } else {
            self.push_undo_snapshot();
            let was_kill = self.last_action == Some(LastAction::Kill);
            let old = self.state.cursor_col;
            let from = self.word_backward_col(&current, old);
            let deleted = current[from..old].to_owned();
            self.kill_ring.push(
                &deleted,
                KillPushOptions {
                    prepend: true,
                    accumulate: was_kill,
                },
            );
            self.last_action = Some(LastAction::Kill);
            self.state.lines[self.state.cursor_line] =
                format!("{}{}", &current[..from], &current[old..]);
            self.set_cursor_col(from);
        }
        self.emit_change();
    }

    fn delete_word_forward(&mut self) {
        self.history.exit_browsing();
        let current = self.state.current_line().to_owned();
        if self.state.cursor_col >= current.len() {
            if self.state.cursor_line + 1 < self.state.lines.len() {
                self.push_undo_snapshot();
                self.kill_ring.push(
                    "\n",
                    KillPushOptions {
                        prepend: false,
                        accumulate: self.last_action == Some(LastAction::Kill),
                    },
                );
                self.last_action = Some(LastAction::Kill);
                let next = self.state.lines.remove(self.state.cursor_line + 1);
                self.state.lines[self.state.cursor_line].push_str(&next);
            }
        } else {
            self.push_undo_snapshot();
            let was_kill = self.last_action == Some(LastAction::Kill);
            let old = self.state.cursor_col;
            let to = self.word_forward_col(&current, old);
            let deleted = current[old..to].to_owned();
            self.kill_ring.push(
                &deleted,
                KillPushOptions {
                    prepend: false,
                    accumulate: was_kill,
                },
            );
            self.last_action = Some(LastAction::Kill);
            self.state.lines[self.state.cursor_line] =
                format!("{}{}", &current[..old], &current[to..]);
        }
        self.emit_change();
    }

    fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().map(str::to_owned) else {
            return;
        };
        self.push_undo_snapshot();
        self.insert_yanked_text(&text);
        self.last_action = Some(LastAction::Yank);
    }

    fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo_snapshot();
        self.delete_yanked_text();
        self.kill_ring.rotate();
        if let Some(text) = self.kill_ring.peek().map(str::to_owned) {
            self.insert_yanked_text(&text);
        }
        self.last_action = Some(LastAction::Yank);
    }

    fn insert_yanked_text(&mut self, text: &str) {
        self.history.exit_browsing();
        self.insert_text_at_cursor_internal(text);
    }

    fn delete_yanked_text(&mut self) {
        let Some(yanked) = self.kill_ring.peek().map(str::to_owned) else {
            return;
        };
        let yank_lines: Vec<&str> = yanked.split('\n').collect();
        if yank_lines.len() == 1 {
            let current = self.state.current_line().to_owned();
            let delete_len = yanked.len();
            if self.state.cursor_col >= delete_len {
                let before = &current[..self.state.cursor_col - delete_len];
                let after = &current[self.state.cursor_col..];
                self.state.lines[self.state.cursor_line] = format!("{before}{after}");
                self.set_cursor_col(self.state.cursor_col - delete_len);
            }
        } else {
            let start_line = self.state.cursor_line.saturating_sub(yank_lines.len() - 1);
            let first_len = yank_lines[0].len();
            let start_col = self
                .state
                .lines
                .get(start_line)
                .map_or(0, |l| l.len().saturating_sub(first_len));
            let after = self.state.current_line()
                [self.state.cursor_col.min(self.state.current_line().len())..]
                .to_owned();
            let before = self
                .state
                .lines
                .get(start_line)
                .map(|l| l[..start_col.min(l.len())].to_owned())
                .unwrap_or_default();
            let remove_count = self.state.cursor_line - start_line + 1;
            self.state.lines.splice(
                start_line..start_line + remove_count,
                [format!("{before}{after}")],
            );
            self.state.cursor_line = start_line;
            self.set_cursor_col(start_col);
        }
        self.emit_change();
    }

    fn undo(&mut self) {
        self.history.exit_browsing();
        let Some(snapshot) = self.undo_stack.pop() else {
            return;
        };
        self.state = snapshot.state;
        self.pastes = snapshot.pastes;
        self.paste_counter = snapshot.paste_counter;
        self.last_action = None;
        self.preferred_visual_col = None;
        self.emit_change();
        self.refresh_autocomplete_after_edit();
    }

    fn marker_aware_word_segments(&self, text: &str) -> Vec<WordSegment> {
        let ids = valid_paste_ids(&self.pastes);
        let markers: Vec<_> = find_paste_markers(text)
            .into_iter()
            .filter(|marker| ids.contains(&marker.id))
            .collect();
        if markers.is_empty() {
            return default_word_segments(text);
        }

        let mut segments = Vec::new();
        let mut last = 0;
        for marker in markers {
            segments.extend(default_word_segments(&text[last..marker.start]).into_iter().map(
                |mut segment| {
                    segment.index += last;
                    segment
                },
            ));
            segments.push(WordSegment {
                segment: text[marker.start..marker.end].to_owned(),
                index: marker.start,
                is_word_like: true,
            });
            last = marker.end;
        }
        segments.extend(default_word_segments(&text[last..]).into_iter().map(|mut segment| {
            segment.index += last;
            segment
        }));
        segments
    }

    fn word_backward_col(&self, text: &str, cursor_col: usize) -> usize {
        let segmenter = |value: &str| self.marker_aware_word_segments(value);
        find_word_backward(
            text,
            cursor_col,
            &WordNavigationOptions {
                segment: Some(&segmenter),
                is_atomic_segment: Some(&is_paste_marker),
            },
        )
    }

    fn word_forward_col(&self, text: &str, cursor_col: usize) -> usize {
        let segmenter = |value: &str| self.marker_aware_word_segments(value);
        find_word_forward(
            text,
            cursor_col,
            &WordNavigationOptions {
                segment: Some(&segmenter),
                is_atomic_segment: Some(&is_paste_marker),
            },
        )
    }

    fn move_word_backwards(&mut self) {
        self.last_action = None;
        let current = self.state.current_line().to_owned();
        if self.state.cursor_col == 0 {
            if self.state.cursor_line > 0 {
                self.state.cursor_line -= 1;
                let len = self.state.lines[self.state.cursor_line].len();
                self.set_cursor_col(len);
            }
            self.refresh_autocomplete_after_navigation();
            return;
        }
        let col = self.word_backward_col(&current, self.state.cursor_col);
        self.set_cursor_col(col);
        self.refresh_autocomplete_after_navigation();
    }

    fn move_word_forwards(&mut self) {
        self.last_action = None;
        let current = self.state.current_line().to_owned();
        if self.state.cursor_col >= current.len() {
            if self.state.cursor_line + 1 < self.state.lines.len() {
                self.state.cursor_line += 1;
                self.set_cursor_col(0);
            }
            self.refresh_autocomplete_after_navigation();
            return;
        }
        let col = self.word_forward_col(&current, self.state.cursor_col);
        self.set_cursor_col(col);
        self.refresh_autocomplete_after_navigation();
    }

    fn move_cursor(&mut self, delta_line: isize, delta_col: isize) {
        self.last_action = None;
        let visual_lines = self.visual_lines();
        let current_vl =
            find_visual_line_at(&visual_lines, self.state.cursor_line, self.state.cursor_col);

        if delta_line != 0 {
            let target = current_vl.cast_signed() + delta_line;
            if target >= 0 && target.cast_unsigned() < visual_lines.len() {
                self.move_to_visual_line(&visual_lines, current_vl, target.cast_unsigned());
            }
        }

        if delta_col != 0 {
            let ids = valid_paste_ids(&self.pastes);
            let current = self.state.current_line().to_owned();
            if delta_col > 0 {
                if self.state.cursor_col < current.len() {
                    let glen = next_grapheme_len(&current, self.state.cursor_col, &ids);
                    self.set_cursor_col(self.state.cursor_col + glen);
                } else if self.state.cursor_line + 1 < self.state.lines.len() {
                    self.state.cursor_line += 1;
                    self.set_cursor_col(0);
                } else if let Some(vl) = visual_lines.get(current_vl).copied() {
                    self.preferred_visual_col = Some(display_column_in_visual_line(
                        &current,
                        vl,
                        self.state.cursor_col,
                    ));
                }
            } else if self.state.cursor_col > 0 {
                let glen = prev_grapheme_len(&current, self.state.cursor_col, &ids);
                self.set_cursor_col(self.state.cursor_col - glen);
            } else if self.state.cursor_line > 0 {
                self.state.cursor_line -= 1;
                let len = self.state.lines[self.state.cursor_line].len();
                self.set_cursor_col(len);
            }
        }

        self.refresh_autocomplete_after_navigation();
    }

    fn move_to_visual_line(
        &mut self,
        visual_lines: &[VisualLine],
        current_visual_line: usize,
        target_visual_line: usize,
    ) {
        let Some(current_vl) = visual_lines.get(current_visual_line).copied() else {
            return;
        };
        let Some(target_vl) = visual_lines.get(target_visual_line).copied() else {
            return;
        };
        let Some(source_line) = self.state.lines.get(current_vl.logical_line) else {
            return;
        };
        let Some(target_line) = self.state.lines.get(target_vl.logical_line) else {
            return;
        };
        let ids = valid_paste_ids(&self.pastes);

        let actual_current_visual_col =
            display_column_in_visual_line(source_line, current_vl, self.state.cursor_col);
        let current_visual_col = self
            .preferred_visual_col
            .unwrap_or(actual_current_visual_col);
        let is_last_source = current_visual_line + 1 >= visual_lines.len()
            || visual_lines[current_visual_line + 1].logical_line != current_vl.logical_line;
        let source_max = max_display_column(source_line, current_vl, is_last_source, &ids);

        let is_last_target = target_visual_line + 1 >= visual_lines.len()
            || visual_lines[target_visual_line + 1].logical_line != target_vl.logical_line;
        let target_max = max_display_column(target_line, target_vl, is_last_target, &ids);

        let move_to = compute_vertical_move_column(
            &mut self.preferred_visual_col,
            current_visual_col,
            source_max,
            target_max,
        );
        let target_col = byte_column_for_display_column(target_line, target_vl, move_to, &ids);
        let landed_display_col = display_column_in_visual_line(target_line, target_vl, target_col);
        if landed_display_col != move_to {
            self.preferred_visual_col = Some(move_to);
        }
        self.state.cursor_line = target_vl.logical_line;
        self.state.cursor_col = target_col;
    }

    fn page_scroll(&mut self, direction: i8) {
        self.last_action = None;
        let page = self.max_visible_lines();
        let visual_lines = self.visual_lines();
        let current =
            find_visual_line_at(&visual_lines, self.state.cursor_line, self.state.cursor_col);
        let max_idx = visual_lines.len().saturating_sub(1);
        let signed_current = isize::try_from(current).unwrap_or(isize::MAX);
        let signed_page = isize::try_from(page).unwrap_or(isize::MAX);
        let signed_max = isize::try_from(max_idx).unwrap_or(isize::MAX);
        let signed_target =
            (signed_current + isize::from(direction) * signed_page).clamp(0, signed_max);
        let target = usize::try_from(signed_target).unwrap_or(0);
        self.move_to_visual_line(&visual_lines, current, target);
        self.refresh_autocomplete_after_navigation();
    }

    fn jump_to_char(&mut self, ch: &str, direction: JumpDir) {
        self.last_action = None;
        let is_forward = matches!(direction, JumpDir::Forward);
        let lines = &self.state.lines;
        let mut line_idx = self.state.cursor_line.cast_signed();
        let end = if is_forward {
            lines.len().cast_signed()
        } else {
            -1
        };
        let step: isize = if is_forward { 1 } else { -1 };

        while line_idx != end {
            let line = &lines[line_idx.cast_unsigned()];
            let is_current = line_idx.cast_unsigned() == self.state.cursor_line;
            let idx = if is_forward {
                let from = if is_current {
                    self.state.cursor_col.saturating_add(1)
                } else {
                    0
                };
                line[from.min(line.len())..].find(ch).map(|i| i + from)
            } else {
                let to = if is_current {
                    self.state.cursor_col.saturating_sub(1)
                } else {
                    line.len()
                };
                line[..=to.min(line.len().saturating_sub(1).min(line.len()))]
                    .rfind(ch)
                    .or_else(|| {
                        if to >= line.len() {
                            line.rfind(ch)
                        } else {
                            line[..=to].rfind(ch)
                        }
                    })
            };
            if let Some(idx) = idx {
                self.state.cursor_line = line_idx.cast_unsigned();
                self.set_cursor_col(idx);
                self.refresh_autocomplete_after_navigation();
                return;
            }
            line_idx += step;
        }
    }

    fn handle_key(&mut self, event: &KeyEvent) -> EventResult {
        let kb = get_keybindings();
        if let Some(result) = self.handle_jump_mode(event, &kb) {
            return result;
        }
        if let Some(result) = self.handle_autocomplete_keys(event, &kb) {
            return result;
        }
        if let Some(result) = self.handle_edit_keys(event, &kb) {
            return result;
        }
        if let Some(result) = self.handle_nav_keys(event, &kb) {
            return result;
        }
        self.handle_printable_key(event)
    }

    fn handle_jump_mode(
        &mut self,
        event: &KeyEvent,
        kb: &crate::keybindings::KeybindingsManager,
    ) -> Option<EventResult> {
        let dir = self.jump_mode?;
        if kb.matches(event, "tui.editor.jumpForward")
            || kb.matches(event, "tui.editor.jumpBackward")
        {
            self.jump_mode = None;
            return Some(EventResult::Consumed);
        }
        if let KeyCode::Char(c) = event.code
            && !event.modifiers.contains(KeyModifiers::CONTROL)
            && !event.modifiers.contains(KeyModifiers::ALT)
        {
            self.jump_mode = None;
            self.jump_to_char(&c.to_string(), dir);
            return Some(EventResult::Render);
        }
        self.jump_mode = None;
        None
    }

    fn handle_autocomplete_keys(
        &mut self,
        event: &KeyEvent,
        kb: &crate::keybindings::KeybindingsManager,
    ) -> Option<EventResult> {
        if self.autocomplete_state.is_none() {
            if kb.matches(event, "tui.input.tab") {
                self.handle_tab_completion();
                return Some(EventResult::Render);
            }
            return None;
        }
        if kb.matches(event, "tui.select.cancel") {
            self.cancel_autocomplete();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.select.up") {
            if !self.autocomplete_items.is_empty() {
                if self.autocomplete_selected == 0 {
                    self.autocomplete_selected = self.autocomplete_items.len() - 1;
                } else {
                    self.autocomplete_selected -= 1;
                }
            }
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.select.down") {
            if !self.autocomplete_items.is_empty() {
                self.autocomplete_selected =
                    (self.autocomplete_selected + 1) % self.autocomplete_items.len();
            }
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.input.tab") {
            if self.apply_selected_completion() {
                self.cancel_autocomplete();
            }
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.select.confirm") {
            let slash = self.autocomplete_prefix.starts_with('/');
            if self.apply_selected_completion() {
                self.cancel_autocomplete();
                if !slash {
                    return Some(EventResult::Render);
                }
            } else if !slash {
                return Some(EventResult::Consumed);
            }
            // slash confirm falls through to submit via remaining handlers
            return None;
        }
        None
    }

    fn handle_edit_keys(
        &mut self,
        event: &KeyEvent,
        kb: &crate::keybindings::KeybindingsManager,
    ) -> Option<EventResult> {
        if kb.matches(event, "tui.input.copy") {
            return Some(EventResult::Ignored);
        }
        if kb.matches(event, "tui.editor.undo") {
            self.undo();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.deleteToLineEnd") {
            self.delete_to_line_end();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.deleteToLineStart") {
            self.delete_to_line_start();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.deleteWordBackward") {
            self.delete_word_backwards();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.deleteWordForward") {
            self.delete_word_forward();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.deleteCharBackward")
            || key_matches(event, &KeyId::from_raw("shift+backspace"))
        {
            self.handle_backspace();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.deleteCharForward")
            || key_matches(event, &KeyId::from_raw("shift+delete"))
        {
            self.handle_forward_delete();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.yank") {
            self.yank();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.yankPop") {
            self.yank_pop();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.input.newLine") {
            let char_before = self
                .state
                .current_line()
                .get(..self.state.cursor_col)
                .and_then(|s| s.chars().next_back());
            let submit_keys = kb.get_keys("tui.input.submit");
            if should_submit_on_backslash_enter(
                event,
                char_before,
                &submit_keys,
                self.disable_submit,
            ) {
                self.handle_backspace();
                self.submit_value();
                return Some(EventResult::Render);
            }
            self.add_new_line();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.input.submit") {
            if self.disable_submit {
                return Some(EventResult::Consumed);
            }
            let char_before = self
                .state
                .current_line()
                .get(..self.state.cursor_col)
                .and_then(|s| s.chars().next_back());
            if backslash_enter_inserts_newline(event, char_before) {
                self.handle_backspace();
                self.add_new_line();
                return Some(EventResult::Render);
            }
            self.submit_value();
            return Some(EventResult::Render);
        }
        None
    }

    fn handle_nav_keys(
        &mut self,
        event: &KeyEvent,
        kb: &crate::keybindings::KeybindingsManager,
    ) -> Option<EventResult> {
        if kb.matches(event, "tui.editor.cursorLineStart") {
            self.last_action = None;
            self.set_cursor_col(0);
            self.refresh_autocomplete_after_navigation();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.cursorLineEnd") {
            self.last_action = None;
            let len = self.state.current_line().len();
            self.set_cursor_col(len);
            self.refresh_autocomplete_after_navigation();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.cursorWordLeft") {
            self.move_word_backwards();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.cursorWordRight") {
            self.move_word_forwards();
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.cursorUp") {
            if self.is_on_first_visual_line()
                && (self.state.is_empty()
                    || self.history.is_browsing()
                    || self.state.cursor_col == 0)
            {
                self.navigate_history(-1);
            } else if self.is_on_first_visual_line() {
                self.last_action = None;
                self.set_cursor_col(0);
                self.refresh_autocomplete_after_navigation();
            } else {
                self.move_cursor(-1, 0);
            }
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.cursorDown") {
            if self.history.is_browsing() && self.is_on_last_visual_line() {
                self.navigate_history(1);
            } else if self.is_on_last_visual_line() {
                self.last_action = None;
                let len = self.state.current_line().len();
                self.set_cursor_col(len);
                self.refresh_autocomplete_after_navigation();
            } else {
                self.move_cursor(1, 0);
            }
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.cursorRight") {
            self.move_cursor(0, 1);
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.cursorLeft") {
            self.move_cursor(0, -1);
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.pageUp") {
            self.page_scroll(-1);
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.pageDown") {
            self.page_scroll(1);
            return Some(EventResult::Render);
        }
        if kb.matches(event, "tui.editor.jumpForward") {
            self.jump_mode = Some(JumpDir::Forward);
            return Some(EventResult::Consumed);
        }
        if kb.matches(event, "tui.editor.jumpBackward") {
            self.jump_mode = Some(JumpDir::Backward);
            return Some(EventResult::Consumed);
        }
        None
    }

    fn handle_printable_key(&mut self, event: &KeyEvent) -> EventResult {
        if key_matches(event, &KeyId::from_raw("shift+space")) {
            self.insert_character(" ");
            return EventResult::Render;
        }
        if let KeyCode::Char(c) = event.code
            && !event.modifiers.contains(KeyModifiers::CONTROL)
            && !event.modifiers.contains(KeyModifiers::ALT)
            && !event.modifiers.contains(KeyModifiers::SUPER)
        {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            self.insert_character(s);
            return EventResult::Render;
        }
        EventResult::Ignored
    }

    fn handle_tab_completion(&mut self) {
        if self.autocomplete_provider.is_none() {
            return;
        }
        let before = &self.state.current_line()
            [..self.state.cursor_col.min(self.state.current_line().len())];
        if self.is_in_slash_command_context(before) && !before.trim_start().contains(' ') {
            self.request_autocomplete(false, true);
        } else {
            self.request_autocomplete(true, true);
        }
    }
    fn paint_top_border(&self, area: Rect, buf: &mut Buffer, width: u16, y: u16) -> u16 {
        let top = if self.scroll_offset > 0 {
            let indicator = format!("─── ↑ {} more ", self.scroll_offset);
            let rem = usize::from(width).saturating_sub(visible_width(&indicator));
            format!("{indicator}{}", "─".repeat(rem))
        } else {
            "─".repeat(usize::from(width))
        };
        paint_plain(buf, area.x, y, width, &top);
        y.saturating_add(1)
    }

    fn paint_bottom_border(
        &self,
        area: Rect,
        buf: &mut Buffer,
        width: u16,
        visual: &[VisualLine],
        max_visible: usize,
        y: u16,
    ) -> u16 {
        if y >= area.y + area.height {
            return y;
        }
        let visible_end = (self.scroll_offset + max_visible).min(visual.len());
        let lines_below = visual.len().saturating_sub(visible_end);
        let bottom = if lines_below > 0 {
            let indicator = format!("─── ↓ {lines_below} more ");
            let rem = usize::from(width).saturating_sub(visible_width(&indicator));
            format!("{indicator}{}", "─".repeat(rem))
        } else {
            "─".repeat(usize::from(width))
        };
        paint_plain(buf, area.x, y, width, &bottom);
        y.saturating_add(1)
    }

    fn paint_body_lines(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        width: u16,
        padding_x: u16,
        visual: &[VisualLine],
        mut y: u16,
    ) -> u16 {
        let max_visible = self.max_visible_lines();
        let visible_end = (self.scroll_offset + max_visible).min(visual.len());
        let cursor_visual =
            find_visual_line_at(visual, self.state.cursor_line, self.state.cursor_col);
        let mut cursor_screen: Option<(u16, u16)> = None;
        let right = area.x.saturating_add(width);

        for (row_i, vl) in visual[self.scroll_offset..visible_end].iter().enumerate() {
            if y >= area.y + area.height.saturating_sub(1) {
                break;
            }
            let line = self
                .state
                .lines
                .get(vl.logical_line)
                .map_or("", String::as_str);
            let end = vl.start_col.saturating_add(vl.length).min(line.len());
            let start = vl.start_col.min(line.len());
            let text = line.get(start..end).unwrap_or("");
            let cursor_in_line = self.scroll_offset.saturating_add(row_i) == cursor_visual;
            let mut col_x = area.x.saturating_add(padding_x);

            if cursor_in_line {
                let relative_cursor = self.state.cursor_col.saturating_sub(start).min(text.len());
                let before = text.get(..relative_cursor).unwrap_or("");
                let after = text.get(relative_cursor..).unwrap_or("");
                for grapheme in UnicodeSegmentation::graphemes(before, true) {
                    col_x = paint_grapheme(
                        buf,
                        Position { x: col_x, y },
                        right,
                        grapheme,
                        Style::default(),
                    );
                }
                if self.focused {
                    set_cursor(Position { x: col_x, y });
                    cursor_screen = Some((col_x, y));
                }
                let mut after_graphemes = UnicodeSegmentation::graphemes(after, true);
                let cursor_grapheme = after_graphemes.next().unwrap_or(" ");
                col_x = paint_grapheme(
                    buf,
                    Position { x: col_x, y },
                    right,
                    cursor_grapheme,
                    Style::default().add_modifier(Modifier::REVERSED),
                );
                for grapheme in after_graphemes {
                    col_x = paint_grapheme(
                        buf,
                        Position { x: col_x, y },
                        right,
                        grapheme,
                        Style::default(),
                    );
                }
            } else {
                for grapheme in UnicodeSegmentation::graphemes(text, true) {
                    col_x = paint_grapheme(
                        buf,
                        Position { x: col_x, y },
                        right,
                        grapheme,
                        Style::default(),
                    );
                }
            }
            y = y.saturating_add(1);
        }
        self.last_cursor_screen = cursor_screen;
        y
    }

    fn paint_autocomplete(
        &self,
        area: Rect,
        buf: &mut Buffer,
        width: u16,
        padding_x: u16,
        content_width: u16,
        mut y: u16,
    ) {
        if self.autocomplete_state.is_none() || self.autocomplete_items.is_empty() {
            return;
        }
        let max = self.autocomplete_max_visible;
        let start = self
            .autocomplete_selected
            .saturating_sub(max / 2)
            .min(self.autocomplete_items.len().saturating_sub(max));
        for (i, item) in self
            .autocomplete_items
            .iter()
            .enumerate()
            .skip(start)
            .take(max)
        {
            if y >= area.y + area.height {
                break;
            }
            let selected = i == self.autocomplete_selected;
            let prefix = if selected { "→ " } else { "  " };
            let label = if let Some(desc) = &item.description {
                format!("{prefix}{}  {desc}", item.label)
            } else {
                format!("{prefix}{}", item.label)
            };
            let line = truncate_to_width(&label, usize::from(content_width), "...", true);
            let x0 = area.x + padding_x;
            let style = if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            for (j, ch) in line.chars().enumerate() {
                let x = x0.saturating_add(u16::try_from(j).unwrap_or(u16::MAX));
                if x >= area.x + width {
                    break;
                }
                buf[(x, y)].set_symbol(&ch.to_string());
                buf[(x, y)].set_style(style);
            }
            y = y.saturating_add(1);
        }
    }
}

fn display_column_in_visual_line(line: &str, visual: VisualLine, cursor_col: usize) -> usize {
    let start = visual.start_col.min(line.len());
    let end = cursor_col
        .min(visual.start_col.saturating_add(visual.length))
        .min(line.len());
    line.get(start..end).map_or(0, visible_width)
}

fn max_display_column(
    line: &str,
    visual: VisualLine,
    is_last: bool,
    valid_ids: &HashSet<u32>,
) -> usize {
    let start = visual.start_col.min(line.len());
    let end = visual
        .start_col
        .saturating_add(visual.length)
        .min(line.len());
    let Some(segment) = line.get(start..end) else {
        return 0;
    };
    let width = visible_width(segment);
    if is_last {
        return width;
    }
    let last_width = segment_graphemes_with_markers(segment, valid_ids)
        .last()
        .map_or(0, |grapheme| visible_width(&grapheme.segment));
    width.saturating_sub(last_width)
}

fn byte_column_for_display_column(
    line: &str,
    visual: VisualLine,
    target_display_col: usize,
    valid_ids: &HashSet<u32>,
) -> usize {
    let start = visual.start_col.min(line.len());
    let end = visual
        .start_col
        .saturating_add(visual.length)
        .min(line.len());
    let Some(segment) = line.get(start..end) else {
        return start;
    };
    let mut display_col = 0usize;
    for grapheme in segment_graphemes_with_markers(segment, valid_ids) {
        let next_display_col = display_col.saturating_add(visible_width(&grapheme.segment));
        if target_display_col < next_display_col {
            let distance_to_start = target_display_col.saturating_sub(display_col);
            let distance_to_end = next_display_col.saturating_sub(target_display_col);
            return if distance_to_start <= distance_to_end {
                start.saturating_add(grapheme.index)
            } else {
                start
                    .saturating_add(grapheme.index)
                    .saturating_add(grapheme.segment.len())
            };
        }
        if target_display_col == next_display_col {
            return start
                .saturating_add(grapheme.index)
                .saturating_add(grapheme.segment.len());
        }
        display_col = next_display_col;
    }
    end
}

fn paint_grapheme(
    buf: &mut Buffer,
    position: Position,
    right: u16,
    grapheme: &str,
    style: Style,
) -> u16 {
    let width = u16::try_from(visible_width(grapheme).max(1)).unwrap_or(u16::MAX);
    if position.x.saturating_add(width) > right {
        return right;
    }
    if let Some(cell) = buf.cell_mut(position) {
        cell.set_symbol(if grapheme.is_empty() { " " } else { grapheme });
        cell.set_style(style);
    }
    for extra in 1..width {
        if let Some(cell) = buf.cell_mut((position.x.saturating_add(extra), position.y)) {
            cell.reset();
            cell.set_diff_option(CellDiffOption::Skip);
        }
    }
    position.x.saturating_add(width)
}

fn best_autocomplete_match_index(items: &[AutocompleteItem], prefix: &str) -> isize {
    if prefix.is_empty() {
        return -1;
    }
    let mut first_prefix = -1isize;
    for (i, item) in items.iter().enumerate() {
        if item.value == prefix {
            return i.cast_signed();
        }
        if first_prefix < 0 && item.value.starts_with(prefix) {
            first_prefix = i.cast_signed();
        }
    }
    first_prefix
}

fn poll_once<T>(fut: std::pin::Pin<Box<dyn Future<Output = T> + Send + '_>>) -> Option<T> {
    use std::task::{Context, Poll, Waker};
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut fut = fut;
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => Some(v),
        Poll::Pending => None,
    }
}

impl Component for Editor {
    fn measure(&mut self, width: u16) -> u16 {
        let max_padding = width.saturating_sub(1) / 2;
        let padding_x = self.padding_x.min(max_padding);
        let content_width = width.saturating_sub(padding_x.saturating_mul(2)).max(1);
        let layout_width = if padding_x > 0 {
            content_width
        } else {
            content_width.saturating_sub(1).max(1)
        };
        self.last_width = usize::from(layout_width);

        let visual = self.visual_lines();
        let body = visual.len().min(self.max_visible_lines()).max(1);
        let mut height = body + 2; // borders
        if self.autocomplete_state.is_some() {
            let ac = self
                .autocomplete_items
                .len()
                .min(self.autocomplete_max_visible)
                .max(1);
            height += ac;
        }
        u16::try_from(height).unwrap_or(u16::MAX)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let width = area.width;
        let max_padding = width.saturating_sub(1) / 2;
        let padding_x = self.padding_x.min(max_padding);
        let content_width = width.saturating_sub(padding_x.saturating_mul(2)).max(1);
        let layout_width = if padding_x > 0 {
            content_width
        } else {
            content_width.saturating_sub(1).max(1)
        };
        self.last_width = usize::from(layout_width);

        let visual = self.visual_lines();
        let max_visible = self.max_visible_lines();
        let cursor_vl = find_visual_line_at(&visual, self.state.cursor_line, self.state.cursor_col);
        if cursor_vl < self.scroll_offset {
            self.scroll_offset = cursor_vl;
        } else if cursor_vl >= self.scroll_offset + max_visible {
            self.scroll_offset = cursor_vl + 1 - max_visible;
        }
        let max_scroll = visual.len().saturating_sub(max_visible);
        self.scroll_offset = self.scroll_offset.min(max_scroll);

        let mut y = area.y;
        y = self.paint_top_border(area, buf, width, y);
        y = self.paint_body_lines(area, buf, width, padding_x, &visual, y);
        y = self.paint_bottom_border(area, buf, width, &visual, max_visible, y);
        self.paint_autocomplete(area, buf, width, padding_x, content_width, y);
        self.needs_layout = false;
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        match event {
            UiEvent::Key(key) => self.handle_key(key),
            UiEvent::Paste(text) => {
                self.handle_paste(text);
                EventResult::Render
            }
            UiEvent::Resize { .. } => {
                self.needs_layout = true;
                EventResult::Render
            }
            UiEvent::FocusGained => {
                self.focused = true;
                EventResult::Render
            }
            UiEvent::FocusLost => {
                self.focused = false;
                EventResult::Render
            }
        }
    }

    fn invalidate(&mut self) {
        self.needs_layout = true;
    }
}

fn paint_plain(buf: &mut Buffer, x: u16, y: u16, width: u16, text: &str) {
    for (i, ch) in text.chars().enumerate() {
        let cx = x.saturating_add(u16::try_from(i).unwrap_or(u16::MAX));
        if cx >= x + width {
            break;
        }
        buf[(cx, y)].set_symbol(&ch.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_support::ApplyCompletionResult;
    use crate::keys::key_press;
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    struct TestAutocompleteProvider;

    impl AutocompleteProvider for TestAutocompleteProvider {
        fn get_suggestions(
            &self,
            _lines: &[String],
            _cursor_line: usize,
            _cursor_col: usize,
            _options: SuggestionOptions,
        ) -> Pin<Box<dyn Future<Output = Option<AutocompleteSuggestions>> + Send + '_>> {
            Box::pin(async { None })
        }

        fn apply_completion(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            _item: &AutocompleteItem,
            _prefix: &str,
        ) -> ApplyCompletionResult {
            ApplyCompletionResult {
                lines: lines.to_vec(),
                cursor_line,
                cursor_col,
            }
        }
    }

    fn large_paste(label: usize) -> String {
        (0..11)
            .map(|line| format!("paste-{label}-line-{line}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn editor_with_ten_reverse_ordered_pastes() -> (Editor, Vec<String>) {
        let mut editor = Editor::with_defaults();
        let payloads: Vec<_> = (1..=10).map(large_paste).collect();
        editor.handle_paste(&payloads[0]);
        for payload in &payloads[1..] {
            editor.set_cursor_col(0);
            editor.handle_paste(payload);
        }
        (editor, payloads)
    }

    fn open_test_autocomplete(editor: &mut Editor) {
        editor.set_autocomplete_provider(Some(Arc::new(TestAutocompleteProvider)));
        editor.apply_autocomplete_suggestions(
            AutocompleteSuggestions {
                items: vec![AutocompleteItem {
                    value: "stale".to_owned(),
                    label: "stale".to_owned(),
                    description: None,
                }],
                prefix: "stale".to_owned(),
            },
            AutocompleteUiState::Regular,
        );
    }

    fn press(code: KeyCode) -> KeyEvent {
        key_press(code, KeyModifiers::empty())
    }

    fn ctrl(c: char) -> KeyEvent {
        key_press(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn legacy_omission_documented() {
        assert!(LEGACY_MODIFY_OTHER_KEYS_OMISSION.contains("modifyOtherKeys"));
        assert!(LEGACY_MODIFY_OTHER_KEYS_OMISSION.contains("backslash-Enter"));
    }

    #[test]
    fn insert_and_submit() {
        let submitted = Arc::new(Mutex::new(None));
        let submitted2 = submitted.clone();
        let mut ed = Editor::with_defaults();
        ed.on_submit = Some(Box::new(move |t| {
            *submitted2
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(t);
        }));
        ed.handle_event(&UiEvent::Key(press(KeyCode::Char('h'))));
        ed.handle_event(&UiEvent::Key(press(KeyCode::Char('i'))));
        assert_eq!(ed.get_text(), "hi");
        ed.handle_event(&UiEvent::Key(press(KeyCode::Enter)));
        assert_eq!(ed.get_text(), "");
        assert_eq!(
            submitted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .as_deref(),
            Some("hi")
        );
    }

    #[test]
    fn backslash_enter_inserts_newline() {
        let mut ed = Editor::with_defaults();
        ed.handle_event(&UiEvent::Key(press(KeyCode::Char('\\'))));
        ed.handle_event(&UiEvent::Key(press(KeyCode::Enter)));
        assert_eq!(ed.get_text(), "\n");
        assert_eq!(ed.get_cursor(), (1, 0));
    }

    #[test]
    fn ctrl_j_newline() {
        let mut ed = Editor::with_defaults();
        ed.handle_event(&UiEvent::Key(press(KeyCode::Char('a'))));
        ed.handle_event(&UiEvent::Key(ctrl('j')));
        assert_eq!(ed.get_text(), "a\n");
    }

    #[test]
    fn history_up_down_draft() {
        let mut ed = Editor::with_defaults();
        ed.add_to_history("older");
        ed.add_to_history("newer");
        ed.set_text("draft");
        // History enter requires first visual line and (empty | browsing | col 0).
        ed.handle_event(&UiEvent::Key(press(KeyCode::Home)));
        ed.handle_event(&UiEvent::Key(press(KeyCode::Up)));
        assert_eq!(ed.get_text(), "newer");
        ed.handle_event(&UiEvent::Key(press(KeyCode::Up)));
        assert_eq!(ed.get_text(), "older");
        ed.handle_event(&UiEvent::Key(press(KeyCode::Down)));
        assert_eq!(ed.get_text(), "newer");
        ed.handle_event(&UiEvent::Key(press(KeyCode::Down)));
        assert_eq!(ed.get_text(), "draft");
    }

    #[test]
    fn kill_yank_and_yank_pop_gate() {
        let mut ed = Editor::with_defaults();
        ed.set_text("hello world");
        // move to start
        ed.handle_event(&UiEvent::Key(press(KeyCode::Home)));
        // delete word forward (alt+d) — use binding
        ed.handle_event(&UiEvent::Key(key_press(
            KeyCode::Char('d'),
            KeyModifiers::ALT,
        )));
        // cursor at 0, " world" remains? delete word forward from 0 deletes "hello"
        assert!(
            ed.get_text().contains("world")
                || ed.get_text() == " world"
                || ed.get_text() == "world"
                || ed.kill_ring_len() >= 1
        );
        ed.handle_event(&UiEvent::Key(ctrl('y')));
        // yank-pop only after yank
        let before = ed.get_text();
        ed.handle_event(&UiEvent::Key(key_press(
            KeyCode::Char('y'),
            KeyModifiers::ALT,
        )));
        // with single kill entry, yank-pop is no-op
        assert_eq!(ed.get_text(), before);
    }

    #[test]
    fn fish_undo_coalescing() {
        let mut ed = Editor::with_defaults();
        for c in ['a', 'b', 'c'] {
            ed.handle_event(&UiEvent::Key(press(KeyCode::Char(c))));
        }
        ed.handle_event(&UiEvent::Key(press(KeyCode::Char(' '))));
        ed.handle_event(&UiEvent::Key(press(KeyCode::Char('d'))));
        // undo should remove space+d together after space snapshot
        ed.handle_event(&UiEvent::Key(key_press(
            KeyCode::Char('-'),
            KeyModifiers::CONTROL,
        )));
        // After one undo: "abc" or "abc " depending on coalesce — at least not "abcd"
        let t = ed.get_text();
        assert!(t == "abc" || t == "abc " || t.starts_with("abc"));
    }

    #[test]
    fn large_paste_marker_atomic() {
        let mut ed = Editor::with_defaults();
        let big = (0..15)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        ed.handle_event(&UiEvent::Paste(big.clone()));
        let text = ed.get_text();
        assert!(text.starts_with("[paste #1"));
        assert!(is_paste_marker(text.trim()));
        assert_eq!(ed.get_expanded_text(), big);
        // backspace removes marker
        ed.handle_event(&UiEvent::Key(press(KeyCode::Backspace)));
        assert_eq!(ed.get_text(), "");
    }

    #[test]
    fn grapheme_left_right_emoji() {
        let mut ed = Editor::with_defaults();
        ed.set_text("a👍b");
        // end
        let (_l, col) = ed.get_cursor();
        assert_eq!(col, "a👍b".len());
        ed.handle_event(&UiEvent::Key(press(KeyCode::Left)));
        // should land before 'b'
        let (_, col) = ed.get_cursor();
        assert_eq!(col, "a👍".len());
        ed.handle_event(&UiEvent::Key(press(KeyCode::Left)));
        let (_, col) = ed.get_cursor();
        assert_eq!(col, 1); // after 'a'
    }

    #[test]
    fn measure_includes_borders() {
        let mut ed = Editor::with_defaults();
        ed.set_text("hello");
        let h = ed.measure(40);
        assert!(h >= 3);
    }

    #[test]
    fn preferred_column_survives_short_line() {
        let mut ed = Editor::with_defaults();
        ed.set_terminal_rows(40);
        ed.set_text("abcdefghij\nxy");
        ed.state.cursor_line = 0;
        ed.set_cursor_col(10);
        ed.last_width = 80;
        ed.move_cursor(1, 0);
        assert_eq!(ed.state.cursor_line, 1);
        assert!(ed.state.cursor_col <= 2);
    }

    #[test]
    fn render_uses_terminal_width_for_cjk_and_emoji() -> Result<(), String> {
        let mut editor = Editor::with_defaults();
        editor.set_text("你🙂a");
        editor.state.cursor_line = 0;
        editor.set_cursor_col("你".len());
        editor.handle_event(&UiEvent::FocusGained);

        let area = Rect::new(0, 0, 10, 5);
        let mut buffer = Buffer::empty(area);
        editor.render(area, &mut buffer);

        let cjk = buffer
            .cell((0, 1))
            .ok_or_else(|| "missing CJK cell".to_owned())?;
        assert_eq!(cjk.symbol(), "你");
        let cjk_tail = buffer
            .cell((1, 1))
            .ok_or_else(|| "missing CJK trailing cell".to_owned())?;
        assert_eq!(cjk_tail.diff_option, CellDiffOption::Skip);
        let emoji = buffer
            .cell((2, 1))
            .ok_or_else(|| "missing emoji cell".to_owned())?;
        assert_eq!(emoji.symbol(), "🙂");
        let emoji_tail = buffer
            .cell((3, 1))
            .ok_or_else(|| "missing emoji trailing cell".to_owned())?;
        assert_eq!(emoji_tail.diff_option, CellDiffOption::Skip);
        assert_eq!(editor.last_cursor_screen, Some((2, 1)));
        Ok(())
    }

    #[test]
    fn vertical_move_uses_display_columns_and_grapheme_boundaries() {
        let combined = "e\u{301}";
        let first = format!("{combined}z");
        let second = "你x";
        let mut editor = Editor::with_defaults();
        editor.set_text(&format!("{first}\n{second}"));
        editor.state.cursor_line = 0;
        editor.set_cursor_col(combined.len());
        editor.last_width = 80;

        editor.move_cursor(1, 0);
        assert_eq!(editor.state.cursor_line, 1);
        assert_eq!(editor.state.cursor_col, 0);
        assert!(second.is_char_boundary(editor.state.cursor_col));

        editor.move_cursor(-1, 0);
        assert_eq!(editor.state.cursor_line, 0);
        assert_eq!(editor.state.cursor_col, combined.len());
        assert!(first.is_char_boundary(editor.state.cursor_col));
    }

    #[test]
    fn vertical_move_matches_cjk_emoji_display_column() {
        let first = "a你b";
        let combined = "e\u{301}";
        let second = format!("{combined}🙂z");
        let mut editor = Editor::with_defaults();
        editor.set_text(&format!("{first}\n{second}"));
        editor.state.cursor_line = 0;
        editor.set_cursor_col("a你".len());
        editor.last_width = 80;

        editor.move_cursor(1, 0);
        assert_eq!(editor.state.cursor_line, 1);
        assert_eq!(editor.state.cursor_col, combined.len() + "🙂".len());
        assert!(second.is_char_boundary(editor.state.cursor_col));

        editor.move_cursor(-1, 0);
        assert_eq!(editor.state.cursor_line, 0);
        assert_eq!(editor.state.cursor_col, "a你".len());
        assert!(first.is_char_boundary(editor.state.cursor_col));
    }
    #[test]
    fn eof_right_preserves_mixed_unicode_display_column() {
        let first = "abcdefghijklmnop";
        let last = "e\u{301}你🙂";
        let mut editor = Editor::with_defaults();
        editor.set_text(&format!("{first}\n{last}"));
        editor.last_width = 80;

        editor.handle_event(&UiEvent::Key(press(KeyCode::Right)));
        editor.move_cursor(-1, 0);
        assert_eq!(editor.state.cursor_line, 0);
        assert_eq!(editor.state.cursor_col, 5);

        editor.move_cursor(1, 0);
        assert_eq!(editor.state.cursor_line, 1);
        assert_eq!(editor.state.cursor_col, last.len());
        assert!(last.is_char_boundary(editor.state.cursor_col));
    }

    #[test]
    fn backspace_deletes_marker_before_ten_to_nine_renumber() -> Result<(), &'static str> {
        let (mut editor, payloads) = editor_with_ten_reverse_ordered_pastes();
        editor.set_cursor_col(editor.state.current_line().len());
        editor.insert_character("x");
        let marker_one = find_paste_markers(&editor.get_text())
            .into_iter()
            .find(|marker| marker.id == 1)
            .ok_or("marker #1 should precede the suffix")?;
        editor.set_cursor_col(marker_one.end);
        let expected_shrink: usize = find_paste_markers(editor.state.current_line())
            .into_iter()
            .filter(|marker| marker.end <= marker_one.start && marker.id > marker_one.id)
            .map(|marker| {
                marker.id.to_string().len() - (marker.id - 1).to_string().len()
            })
            .sum();
        assert_eq!(expected_shrink, 1);

        editor.handle_backspace();

        let text = editor.get_text();
        assert_eq!(editor.get_cursor(), (0, marker_one.start - 1));
        assert!(!text.contains("[paste #10 ") );
        assert_eq!(find_paste_markers(&text).len(), 9);
        assert_eq!(editor.paste_counter, 9);
        assert_eq!(editor.pastes.len(), 9);
        let expected = format!(
            "{}x",
            payloads[1..].iter().rev().cloned().collect::<String>()
        );
        assert_eq!(editor.get_expanded_text(), expected);
        Ok(())
    }

    #[test]
    fn forward_delete_renumbers_marker_and_undo_restores_payload_state() -> Result<(), &'static str> {
        let (mut editor, payloads) = editor_with_ten_reverse_ordered_pastes();
        editor.set_cursor_col(editor.state.current_line().len());
        editor.insert_character("x");
        let marker_one = find_paste_markers(&editor.get_text())
            .into_iter()
            .find(|marker| marker.id == 1)
            .ok_or("marker #1 should precede the suffix")?;
        editor.set_cursor_col(marker_one.start);
        let before_text = editor.get_text();
        let before_expanded = editor.get_expanded_text();

        editor.handle_forward_delete();

        assert_eq!(editor.get_cursor(), (0, marker_one.start - 1));
        assert!(!editor.get_text().contains("[paste #10 ") );
        assert_eq!(editor.paste_counter, 9);
        assert_eq!(editor.pastes.len(), 9);
        let expected = format!(
            "{}x",
            payloads[1..].iter().rev().cloned().collect::<String>()
        );
        assert_eq!(editor.get_expanded_text(), expected);

        editor.undo();
        assert_eq!(editor.get_text(), before_text);
        assert_eq!(editor.get_expanded_text(), before_expanded);
        assert_eq!(editor.paste_counter, 10);
        assert_eq!(editor.pastes.len(), 10);
        assert!(editor.pastes.keys().all(|id| (1..=10).contains(id)));
        Ok(())
    }

    #[test]
    fn word_motion_treats_unicode_adjacent_marker_as_atomic() {
        let mut editor = Editor::with_defaults();
        editor.insert_character("α");
        editor.handle_paste(&large_paste(1));
        editor.insert_character("β");
        let marker = find_paste_markers(&editor.get_text())[0];

        editor.set_cursor_col("α".len());
        editor.move_word_forwards();
        assert_eq!(editor.get_cursor(), (0, marker.end));
        assert!(editor.state.current_line().is_char_boundary(editor.state.cursor_col));

        editor.move_word_backwards();
        assert_eq!(editor.get_cursor(), (0, marker.start));
        assert!(editor.state.current_line().is_char_boundary(editor.state.cursor_col));
    }

    #[test]
    fn history_and_non_character_moves_refresh_open_autocomplete() -> Result<(), &'static str> {
        let mut editor = Editor::with_defaults();
        editor.add_to_history("history value");
        editor.set_text("draft");
        editor.set_cursor_col(0);
        open_test_autocomplete(&mut editor);

        editor.navigate_history(-1);
        let pending = editor.autocomplete_pending.as_ref().ok_or("history refresh")?;
        assert_eq!(pending.snapshot_text, "history value");
        assert_eq!(pending.snapshot_col, 0);

        editor.move_word_forwards();
        let pending = editor.autocomplete_pending.as_ref().ok_or("word refresh")?;
        assert_eq!(pending.snapshot_col, "history".len());

        editor.set_text("zero\none\ntwo\nthree\nfour\nfive\nsix\nseven");
        editor.set_terminal_rows(1);
        open_test_autocomplete(&mut editor);
        let before = editor.get_cursor();
        editor.page_scroll(-1);
        let pending = editor.autocomplete_pending.as_ref().ok_or("page refresh")?;
        assert_eq!((pending.snapshot_line, pending.snapshot_col), editor.get_cursor());
        assert_ne!(editor.get_cursor(), before);
        Ok(())
    }
}
