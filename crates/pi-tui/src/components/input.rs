//! Single-line input with horizontal scroll, paste newline stripping, and grapheme cursor.

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;

use crate::component::{Component, EventResult, UiEvent};
use crate::frame::set_cursor;
use crate::keybindings::get_keybindings;
use crate::text::{is_whitespace_char, slice_by_column, visible_width};

/// Snapshot for undo.
#[derive(Debug, Clone)]
struct InputState {
    value: String,
    cursor: usize,
}

/// Simple kill ring (single-slot accumulate for Input subset).
#[derive(Debug, Default)]
struct KillRing {
    entries: Vec<String>,
    index: usize,
}

impl KillRing {
    fn push(&mut self, text: String, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }
        if accumulate && let Some(last) = self.entries.last_mut() {
            if prepend {
                *last = format!("{text}{last}");
            } else {
                last.push_str(&text);
            }
            self.index = self.entries.len() - 1;
            return;
        }
        self.entries.push(text);
        self.index = self.entries.len() - 1;
    }

    fn peek(&self) -> Option<&str> {
        self.entries.get(self.index).map(String::as_str)
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn rotate(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        if self.index == 0 {
            self.index = self.entries.len() - 1;
        } else {
            self.index -= 1;
        }
    }
}

#[derive(Debug, Default)]
struct UndoStack {
    stack: Vec<InputState>,
}

impl UndoStack {
    fn push(&mut self, state: InputState) {
        self.stack.push(state);
    }

    fn pop(&mut self) -> Option<InputState> {
        self.stack.pop()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    Kill,
    Yank,
    TypeWord,
}

/// Submit callback type.
type InputSubmitCallback = Box<dyn FnMut(&str) + Send>;
/// Escape callback type.
type InputEscapeCallback = Box<dyn FnMut() + Send>;

/// Single-line text input with horizontal scrolling and fake reverse-video cursor.
pub struct Input {
    value: String,
    /// Cursor as UTF-8 byte index on a grapheme boundary.
    cursor: usize,
    focused: bool,
    kill_ring: KillRing,
    undo: UndoStack,
    last_action: Option<LastAction>,
    /// Called on submit.
    pub on_submit: Option<InputSubmitCallback>,
    /// Called on escape/cancel.
    pub on_escape: Option<InputEscapeCallback>,
}

impl Input {
    /// Create an empty input.
    #[must_use]
    pub fn new() -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            focused: false,
            kill_ring: KillRing::default(),
            undo: UndoStack::default(),
            last_action: None,
            on_submit: None,
            on_escape: None,
        }
    }

    /// Current value.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Set the value; clamps cursor.
    pub fn set_value(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = self.cursor.min(self.value.len());
        self.snap_cursor();
    }

    /// Cursor byte index.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Focus flag (affects hardware cursor annotation).
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Whether focused.
    #[must_use]
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// Insert paste text with newlines stripped and tabs → 4 spaces.
    pub fn paste(&mut self, pasted: &str) {
        self.last_action = None;
        self.push_undo();
        let clean = pasted.replace(['\r', '\n'], "").replace('\t', "    ");
        self.value = format!(
            "{}{}{}",
            &self.value[..self.cursor],
            clean,
            &self.value[self.cursor..]
        );
        self.cursor += clean.len();
    }

    fn snap_cursor(&mut self) {
        if self.cursor > self.value.len() {
            self.cursor = self.value.len();
            return;
        }
        // Ensure cursor is on a char boundary.
        if !self.value.is_char_boundary(self.cursor) {
            self.cursor = self
                .value
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|&i| i <= self.cursor)
                .last()
                .unwrap_or(0);
        }
    }

    fn push_undo(&mut self) {
        self.undo.push(InputState {
            value: self.value.clone(),
            cursor: self.cursor,
        });
    }

    fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let is_ws = text
            .chars()
            .next()
            .is_some_and(|c| is_whitespace_char(c.encode_utf8(&mut [0; 4])));
        if is_ws || self.last_action != Some(LastAction::TypeWord) {
            self.push_undo();
        }
        self.last_action = Some(LastAction::TypeWord);
        self.value = format!(
            "{}{}{}",
            &self.value[..self.cursor],
            text,
            &self.value[self.cursor..]
        );
        self.cursor += text.len();
    }

    fn grapheme_before_cursor(&self) -> Option<&str> {
        let before = &self.value[..self.cursor];
        UnicodeSegmentation::graphemes(before, true).next_back()
    }

    fn grapheme_at_cursor(&self) -> Option<&str> {
        let after = &self.value[self.cursor..];
        UnicodeSegmentation::graphemes(after, true).next()
    }

    fn handle_backspace(&mut self) {
        self.last_action = None;
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let len = self.grapheme_before_cursor().map_or(1, str::len);
        let start = self.cursor - len;
        self.value = format!("{}{}", &self.value[..start], &self.value[self.cursor..]);
        self.cursor = start;
    }

    fn handle_forward_delete(&mut self) {
        self.last_action = None;
        if self.cursor >= self.value.len() {
            return;
        }
        self.push_undo();
        let len = self.grapheme_at_cursor().map_or(1, str::len);
        self.value = format!(
            "{}{}",
            &self.value[..self.cursor],
            &self.value[self.cursor + len..]
        );
    }

    fn delete_to_line_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.push_undo();
        let deleted = self.value[..self.cursor].to_owned();
        self.kill_ring
            .push(deleted, true, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
        self.value = self.value[self.cursor..].to_owned();
        self.cursor = 0;
    }

    fn delete_to_line_end(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        self.push_undo();
        let deleted = self.value[self.cursor..].to_owned();
        self.kill_ring
            .push(deleted, false, self.last_action == Some(LastAction::Kill));
        self.last_action = Some(LastAction::Kill);
        self.value = self.value[..self.cursor].to_owned();
    }

    fn find_word_backward(text: &str, cursor: usize) -> usize {
        if cursor == 0 {
            return 0;
        }
        let bytes = text.as_bytes();
        let mut i = cursor;
        // Skip whitespace left.
        while i > 0 {
            let prev = {
                let mut p = i - 1;
                while p > 0 && !text.is_char_boundary(p) {
                    p -= 1;
                }
                p
            };
            let ch = text[prev..i].chars().next().unwrap_or(' ');
            if !ch.is_whitespace() {
                break;
            }
            i = prev;
        }
        // Skip word chars left.
        while i > 0 {
            let prev = {
                let mut p = i - 1;
                while p > 0 && !text.is_char_boundary(p) {
                    p -= 1;
                }
                p
            };
            let ch = text[prev..i].chars().next().unwrap_or(' ');
            if ch.is_whitespace() {
                break;
            }
            i = prev;
            let _ = bytes;
        }
        i
    }

    fn find_word_forward(text: &str, cursor: usize) -> usize {
        let len = text.len();
        if cursor >= len {
            return len;
        }
        let mut i = cursor;
        let at_ws = text[i..].chars().next().is_some_and(char::is_whitespace);
        if at_ws {
            while i < len {
                let ch = text[i..].chars().next().unwrap_or(' ');
                if !ch.is_whitespace() {
                    break;
                }
                i += ch.len_utf8();
            }
        } else {
            while i < len {
                let ch = text[i..].chars().next().unwrap_or(' ');
                if ch.is_whitespace() {
                    break;
                }
                i += ch.len_utf8();
            }
            while i < len {
                let ch = text[i..].chars().next().unwrap_or(' ');
                if !ch.is_whitespace() {
                    break;
                }
                i += ch.len_utf8();
            }
        }
        i
    }

    fn delete_word_backwards(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let was_kill = self.last_action == Some(LastAction::Kill);
        self.push_undo();
        let from = Self::find_word_backward(&self.value, self.cursor);
        let deleted = self.value[from..self.cursor].to_owned();
        self.kill_ring.push(deleted, true, was_kill);
        self.last_action = Some(LastAction::Kill);
        self.value = format!("{}{}", &self.value[..from], &self.value[self.cursor..]);
        self.cursor = from;
    }

    fn delete_word_forward(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        let was_kill = self.last_action == Some(LastAction::Kill);
        self.push_undo();
        let to = Self::find_word_forward(&self.value, self.cursor);
        let deleted = self.value[self.cursor..to].to_owned();
        self.kill_ring.push(deleted, false, was_kill);
        self.last_action = Some(LastAction::Kill);
        self.value = format!("{}{}", &self.value[..self.cursor], &self.value[to..]);
    }

    fn yank(&mut self) {
        let Some(text) = self.kill_ring.peek().map(str::to_owned) else {
            return;
        };
        self.push_undo();
        self.value = format!(
            "{}{}{}",
            &self.value[..self.cursor],
            text,
            &self.value[self.cursor..]
        );
        self.cursor += text.len();
        self.last_action = Some(LastAction::Yank);
    }

    fn yank_pop(&mut self) {
        if self.last_action != Some(LastAction::Yank) || self.kill_ring.len() <= 1 {
            return;
        }
        self.push_undo();
        let prev = self.kill_ring.peek().unwrap_or("").to_owned();
        if self.cursor >= prev.len() {
            self.value = format!(
                "{}{}",
                &self.value[..self.cursor - prev.len()],
                &self.value[self.cursor..]
            );
            self.cursor -= prev.len();
        }
        self.kill_ring.rotate();
        let text = self.kill_ring.peek().unwrap_or("").to_owned();
        self.value = format!(
            "{}{}{}",
            &self.value[..self.cursor],
            text,
            &self.value[self.cursor..]
        );
        self.cursor += text.len();
        self.last_action = Some(LastAction::Yank);
    }

    fn undo(&mut self) {
        if let Some(snap) = self.undo.pop() {
            self.value = snap.value;
            self.cursor = snap.cursor;
            self.last_action = None;
        }
    }

    fn move_left(&mut self) {
        self.last_action = None;
        if let Some(g) = self.grapheme_before_cursor() {
            self.cursor -= g.len();
        }
    }

    fn move_right(&mut self) {
        self.last_action = None;
        if let Some(g) = self.grapheme_at_cursor() {
            self.cursor += g.len();
        }
    }

    fn render_line(&self, width: u16) -> (String, usize) {
        let prompt = "> ";
        let prompt_width = visible_width(prompt);
        let available = usize::from(width).saturating_sub(prompt_width);
        if available == 0 {
            return (prompt.to_owned(), 0);
        }

        let total_width = visible_width(&self.value);
        let (visible_text, cursor_byte_in_visible) = if total_width < available {
            (self.value.clone(), self.cursor)
        } else {
            let scroll_width = if self.cursor == self.value.len() {
                available.saturating_sub(1)
            } else {
                available
            };
            let cursor_col = visible_width(&self.value[..self.cursor]);
            if scroll_width == 0 {
                (String::new(), 0)
            } else {
                let half = scroll_width / 2;
                let start_col = if cursor_col < half {
                    0
                } else if cursor_col > total_width.saturating_sub(half) {
                    total_width.saturating_sub(scroll_width)
                } else {
                    cursor_col.saturating_sub(half)
                };
                let visible = slice_by_column(&self.value, start_col, scroll_width, true);
                let before = slice_by_column(
                    &self.value,
                    start_col,
                    cursor_col.saturating_sub(start_col),
                    true,
                );
                (visible, before.len())
            }
        };

        (
            format!("{prompt}{visible_text}"),
            cursor_byte_in_visible + prompt.len(),
        )
    }
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Input {
    fn measure(&mut self, _width: u16) -> u16 {
        1
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        // Direct cell writer: claim the row span so damage scoping
        // accounts for it (PERF-T11 Design B).
        crate::frame::claim_opaque_span(Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        });
        let (line, cursor_byte) = self.render_line(area.width);
        // Paint base text without reverse cursor, then overlay reverse on cursor grapheme.
        let prompt = "> ";
        let prompt_len = prompt.len();
        let y = area.y;
        let mut col = 0usize;
        // prompt
        for (i, ch) in prompt.chars().enumerate() {
            let s = ch.to_string();
            if let Some(cell) = buf.cell_mut((
                area.x.saturating_add(u16::try_from(i).unwrap_or(u16::MAX)),
                y,
            )) {
                cell.set_symbol(&s);
            }
            col += 1;
        }
        let _ = prompt_len;
        let visible = line.get(prompt.len()..).unwrap_or("");
        let mut byte_i = 0usize;
        let max_w = usize::from(area.width);
        for g in UnicodeSegmentation::graphemes(visible, true) {
            let gw = visible_width(g).max(1);
            if col + gw > max_w {
                break;
            }
            let abs_byte = prompt.len() + byte_i;
            let is_cursor = abs_byte == cursor_byte;
            let symbol = if g.is_empty() { " " } else { g };
            let style = if is_cursor {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            if let Some(cell) = buf.cell_mut((
                area.x
                    .saturating_add(u16::try_from(col).unwrap_or(u16::MAX)),
                y,
            )) {
                cell.set_symbol(if is_cursor && cursor_byte >= line.len() {
                    " "
                } else {
                    symbol
                });
                cell.set_style(style);
            }
            for extra in 1..gw {
                if let Some(cell) = buf.cell_mut((
                    area.x
                        .saturating_add(u16::try_from(col + extra).unwrap_or(u16::MAX)),
                    y,
                )) {
                    cell.reset();
                    cell.set_diff_option(CellDiffOption::Skip);
                }
            }
            col += gw;
            byte_i += g.len();
        }
        // Cursor past end of visible text.
        if cursor_byte >= line.len()
            && col < max_w
            && let Some(cell) = buf.cell_mut((
                area.x
                    .saturating_add(u16::try_from(col).unwrap_or(u16::MAX)),
                y,
            ))
        {
            cell.set_symbol(" ");
            cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
        }
        if self.focused {
            // Approximate hardware cursor at end of prompt + visible cursor col.
            let cursor_col = visible_width(&line[..cursor_byte.min(line.len())]);
            set_cursor(ratatui::layout::Position {
                x: area
                    .x
                    .saturating_add(u16::try_from(cursor_col).unwrap_or(u16::MAX)),
                y: area.y,
            });
        }
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        match event {
            UiEvent::Paste(text) => {
                self.paste(text);
                EventResult::Render
            }
            UiEvent::Key(key) => {
                let kb = get_keybindings();
                if kb.matches(key, "tui.select.cancel") {
                    if let Some(cb) = self.on_escape.as_mut() {
                        cb();
                    }
                    return EventResult::Consumed;
                }
                if kb.matches(key, "tui.editor.undo") {
                    self.undo();
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.input.submit") {
                    if let Some(cb) = self.on_submit.as_mut() {
                        cb(&self.value);
                    }
                    return EventResult::Consumed;
                }
                if kb.matches(key, "tui.editor.deleteCharBackward") {
                    self.handle_backspace();
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.editor.deleteCharForward") {
                    self.handle_forward_delete();
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.editor.deleteWordBackward") {
                    self.delete_word_backwards();
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.editor.deleteWordForward") {
                    self.delete_word_forward();
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.editor.deleteToLineStart") {
                    self.delete_to_line_start();
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.editor.deleteToLineEnd") {
                    self.delete_to_line_end();
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.editor.yank") {
                    self.yank();
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.editor.yankPop") {
                    self.yank_pop();
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.editor.cursorLeft") {
                    self.move_left();
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.editor.cursorRight") {
                    self.move_right();
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.editor.cursorLineStart") {
                    self.last_action = None;
                    self.cursor = 0;
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.editor.cursorLineEnd") {
                    self.last_action = None;
                    self.cursor = self.value.len();
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.editor.cursorWordLeft") {
                    self.last_action = None;
                    self.cursor = Self::find_word_backward(&self.value, self.cursor);
                    return EventResult::Render;
                }
                if kb.matches(key, "tui.editor.cursorWordRight") {
                    self.last_action = None;
                    self.cursor = Self::find_word_forward(&self.value, self.cursor);
                    return EventResult::Render;
                }

                // Printable character from KeyCode::Char
                if let crossterm::event::KeyCode::Char(c) = key.code {
                    // Reject control via modifiers that aren't shift/alt alone for unicode.
                    if key.modifiers.intersects(
                        crossterm::event::KeyModifiers::CONTROL
                            | crossterm::event::KeyModifiers::SUPER,
                    ) {
                        return EventResult::Ignored;
                    }
                    let mut buf = [0u8; 4];
                    let s = c.encode_utf8(&mut buf);
                    self.insert_text(s);
                    return EventResult::Render;
                }
                EventResult::Ignored
            }
            _ => EventResult::Ignored,
        }
    }

    fn invalidate(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::util::render_snapshot;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn paste_strips_newlines() {
        let mut input = Input::new();
        input.paste("a\nb\r\nc\td");
        assert_eq!(input.value(), "abc    d");
    }

    #[test]
    fn grapheme_backspace() {
        let mut input = Input::new();
        input.set_value("a😀b");
        input.cursor = input.value.len();
        input.handle_backspace();
        assert_eq!(input.value(), "a😀");
        input.handle_backspace();
        assert_eq!(input.value(), "a");
    }

    #[test]
    fn horizontal_scroll_measure_one() {
        let mut input = Input::new();
        input.set_value("x".repeat(200));
        assert_eq!(input.measure(40), 1);
        let snap = render_snapshot(&mut input, 40);
        assert_eq!(snap.len(), 1);
        assert!(snap[0].starts_with('>') || snap[0].contains('>'));
    }

    #[test]
    fn char_insert_and_submit() {
        let mut input = Input::new();
        input.on_submit = Some(Box::new(move |_| {}));
        let key = UiEvent::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(input.handle_event(&key), EventResult::Render);
        assert_eq!(input.value(), "h");
    }

    #[test]
    fn paste_event() {
        let mut input = Input::new();
        assert_eq!(
            input.handle_event(&UiEvent::Paste("hello\nworld".into())),
            EventResult::Render
        );
        assert_eq!(input.value(), "helloworld");
    }
}
