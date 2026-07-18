//! ANSI-aware word wrapping with style/hyperlink carry.

use unicode_segmentation::UnicodeSegmentation;

use super::ansi::{AnsiCodeTracker, extract_ansi_code, update_tracker_from_text};
use super::width::{cjk_break_grapheme, visible_width};

/// Wrap `text` to `width` visible columns, preserving ANSI/OSC 8 across breaks.
///
/// Only word-wraps — no padding, no background application.
#[must_use]
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    if width == 0 {
        return vec![String::new()];
    }

    // Split on LF / CRLF / CR while tracking style across logical newlines.
    let mut result: Vec<String> = Vec::new();
    let mut tracker = AnsiCodeTracker::new();

    for input_line in split_lines(text) {
        let prefix = if result.is_empty() {
            String::new()
        } else {
            tracker.get_active_codes()
        };
        let mut combined = prefix;
        combined.push_str(input_line);
        let wrapped = wrap_single_line(&combined, width);
        result.extend(wrapped);
        update_tracker_from_text(input_line, &mut tracker);
    }

    if result.is_empty() {
        vec![String::new()]
    } else {
        result
    }
}

fn split_lines(text: &str) -> Vec<&str> {
    // Match TS: text.split(/\r\n|\r|\n/)
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            lines.push(&text[start..i]);
            if bytes.get(i + 1) == Some(&b'\n') {
                i += 2;
            } else {
                i += 1;
            }
            start = i;
            continue;
        }
        if bytes[i] == b'\n' {
            lines.push(&text[start..i]);
            i += 1;
            start = i;
            continue;
        }
        i += 1;
    }
    lines.push(&text[start..]);
    lines
}

fn wrap_single_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let visible_length = visible_width(line);
    if visible_length <= width {
        return vec![line.to_owned()];
    }

    let mut wrapped: Vec<String> = Vec::new();
    let mut tracker = AnsiCodeTracker::new();
    let tokens = split_into_tokens_with_ansi(line);

    let mut current_line = String::new();
    let mut current_visible_length = 0usize;

    for token in tokens {
        let token_visible_length = visible_width(&token);
        let is_whitespace = token.trim().is_empty();

        if token_visible_length > width && !is_whitespace {
            if !current_line.is_empty() {
                let line_end_reset = tracker.get_line_end_reset();
                if !line_end_reset.is_empty() {
                    current_line.push_str(&line_end_reset);
                }
                wrapped.push(std::mem::take(&mut current_line));
                current_visible_length = 0;
            }
            let broken = break_long_word(&token, width, &mut tracker);
            if let Some((last, rest)) = broken.split_last() {
                for part in rest {
                    wrapped.push(part.clone());
                }
                current_line.clone_from(last);
                current_visible_length = visible_width(&current_line);
            }
            continue;
        }

        let total_needed = current_visible_length.saturating_add(token_visible_length);
        if total_needed > width && current_visible_length > 0 {
            let mut line_to_wrap = trim_end_preserve_escapes(&current_line);
            let line_end_reset = tracker.get_line_end_reset();
            if !line_end_reset.is_empty() {
                line_to_wrap.push_str(&line_end_reset);
            }
            wrapped.push(line_to_wrap);
            if is_whitespace {
                current_line = tracker.get_active_codes();
                current_visible_length = 0;
            } else {
                current_line = tracker.get_active_codes();
                current_line.push_str(&token);
                current_visible_length = token_visible_length;
            }
        } else {
            current_line.push_str(&token);
            current_visible_length = current_visible_length.saturating_add(token_visible_length);
        }

        update_tracker_from_text(&token, &mut tracker);
    }

    if !current_line.is_empty() {
        wrapped.push(current_line);
    }

    if wrapped.is_empty() {
        vec![String::new()]
    } else {
        wrapped
            .into_iter()
            .map(|line| trim_end_preserve_escapes(&line))
            .collect()
    }
}

/// Trim trailing whitespace from the *visible* end of a line, leaving escapes.
fn trim_end_preserve_escapes(line: &str) -> String {
    // TS uses String.trimEnd() which only trims JS whitespace, including spaces
    // after ANSI. Mirror that: strip trailing Unicode whitespace chars while
    // keeping any trailing escape sequences that follow content.
    line.trim_end().to_owned()
}

fn split_into_tokens_with_ansi(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut pending_ansi = String::new();
    let mut current_kind: Option<bool> = None; // true = space, false = word
    let mut i = 0usize;

    let flush_current =
        |tokens: &mut Vec<String>, current: &mut String, current_kind: &mut Option<bool>| {
            if !current.is_empty() {
                tokens.push(std::mem::take(current));
                *current_kind = None;
            }
        };

    while i < text.len() {
        if let Some(ansi) = extract_ansi_code(text, i) {
            pending_ansi.push_str(ansi.code);
            i += ansi.len;
            continue;
        }

        let mut end = i;
        while end < text.len() && extract_ansi_code(text, end).is_none() {
            end += text[end..].chars().next().map_or(1, char::len_utf8);
        }

        let portion = &text[i..end];
        for segment in portion.graphemes(true) {
            let segment_is_space = segment == " ";
            if !segment_is_space && cjk_break_grapheme(segment) {
                flush_current(&mut tokens, &mut current, &mut current_kind);
                let mut token = std::mem::take(&mut pending_ansi);
                token.push_str(segment);
                tokens.push(token);
                continue;
            }

            let segment_kind = segment_is_space;
            if !current.is_empty() && current_kind != Some(segment_kind) {
                flush_current(&mut tokens, &mut current, &mut current_kind);
            }
            if !pending_ansi.is_empty() {
                current.push_str(&pending_ansi);
                pending_ansi.clear();
            }
            current_kind = Some(segment_kind);
            current.push_str(segment);
        }
        i = end;
    }

    if !pending_ansi.is_empty() {
        if !current.is_empty() {
            current.push_str(&pending_ansi);
        } else if let Some(last) = tokens.last_mut() {
            last.push_str(&pending_ansi);
        } else {
            current = pending_ansi;
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

enum WordSegment<'a> {
    Ansi(&'a str),
    Grapheme(&'a str),
}

fn break_long_word(word: &str, width: usize, tracker: &mut AnsiCodeTracker) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current_line = tracker.get_active_codes();
    let mut current_width = 0usize;

    let mut segments: Vec<WordSegment<'_>> = Vec::new();
    let mut i = 0usize;
    while i < word.len() {
        if let Some(ansi) = extract_ansi_code(word, i) {
            segments.push(WordSegment::Ansi(ansi.code));
            i += ansi.len;
        } else {
            let mut end = i;
            while end < word.len() && extract_ansi_code(word, end).is_none() {
                end += word[end..].chars().next().map_or(1, char::len_utf8);
            }
            for g in word[i..end].graphemes(true) {
                segments.push(WordSegment::Grapheme(g));
            }
            i = end;
        }
    }

    for seg in segments {
        match seg {
            WordSegment::Ansi(code) => {
                current_line.push_str(code);
                tracker.process(code);
            }
            WordSegment::Grapheme(grapheme) => {
                if grapheme.is_empty() {
                    continue;
                }
                let gw = visible_width(grapheme);
                if current_width.saturating_add(gw) > width {
                    let line_end_reset = tracker.get_line_end_reset();
                    if !line_end_reset.is_empty() {
                        current_line.push_str(&line_end_reset);
                    }
                    lines.push(std::mem::take(&mut current_line));
                    current_line = tracker.get_active_codes();
                    current_width = 0;
                }
                current_line.push_str(grapheme);
                current_width = current_width.saturating_add(gw);
            }
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

/// Detect a trailing partial markdown closing fence line (streaming).
///
/// A line consisting solely of 1..=(marker_len-1) backticks or tildes matching
/// the opening fence character is a partial close.
#[must_use]
pub fn is_partial_closing_fence_line(line: &str, open_marker: &str) -> bool {
    let Some(ch) = open_marker.chars().next() else {
        return false;
    };
    if open_marker.len() < 3 {
        return false;
    }
    if line.is_empty() || line.len() >= open_marker.len() {
        return false;
    }
    line.chars().all(|c| c == ch) && line.chars().count() == line.len()
}

/// Strip a trailing partial closing fence from raw markdown text if present.
///
/// Used by streaming markdown so incomplete ` `` ` tails do not collapse a fence.
#[must_use]
pub fn strip_trailing_partial_closing_fence(raw: &str) -> String {
    let Some(first_line) = raw.lines().next() else {
        return raw.to_owned();
    };
    let marker = {
        let bytes = first_line.as_bytes();
        if bytes.len() >= 3 {
            let ch = bytes[0];
            if (ch == b'`' || ch == b'~') && bytes.iter().take_while(|&&b| b == ch).count() >= 3 {
                let n = bytes.iter().take_while(|&&b| b == ch).count();
                Some(first_line[..n].to_owned())
            } else {
                None
            }
        } else {
            None
        }
    };
    let Some(marker) = marker else {
        return raw.to_owned();
    };
    let Some(last_line) = raw.split('\n').next_back() else {
        return raw.to_owned();
    };
    if is_partial_closing_fence_line(last_line, &marker) {
        // Drop the trailing partial fence (and its preceding newline if any).
        if let Some(idx) = raw.rfind('\n') {
            return raw[..idx].to_owned();
        }
        return String::new();
    }
    raw.to_owned()
}
