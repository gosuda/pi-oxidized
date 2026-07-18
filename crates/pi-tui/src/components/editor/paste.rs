//! Large-paste markers and paste normalization.
//!
//! Ports paste handling from `.references/pi/packages/tui/src/components/editor.ts`.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::hash::BuildHasher;

/// Threshold: more than this many lines → large paste marker.
pub const LARGE_PASTE_LINE_THRESHOLD: usize = 10;
/// Threshold: more than this many characters → large paste marker.
pub const LARGE_PASTE_CHAR_THRESHOLD: usize = 1000;

const MARKER_PREFIX: &str = "[paste #";

/// A matched paste-marker span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkerMatch {
    /// Byte start.
    pub start: usize,
    /// Byte end (exclusive).
    pub end: usize,
    /// Numeric id.
    pub id: u32,
    /// Suffix start within the full match (after id).
    pub suffix_start: usize,
    /// Suffix end within the full match (before `]`).
    pub suffix_end: usize,
}

/// Scan `text` for paste markers.
#[must_use]
pub fn find_paste_markers(text: &str) -> Vec<MarkerMatch> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(rel) = text[i..].find(MARKER_PREFIX) else {
            break;
        };
        let start = i + rel;
        let after_prefix = start + MARKER_PREFIX.len();
        let rest = &text[after_prefix..];
        let mut id_end = 0usize;
        for (idx, ch) in rest.char_indices() {
            if ch.is_ascii_digit() {
                id_end = idx + ch.len_utf8();
            } else {
                break;
            }
        }
        if id_end == 0 {
            i = after_prefix;
            continue;
        }
        let Ok(id) = rest[..id_end].parse::<u32>() else {
            i = after_prefix;
            continue;
        };
        let after_id = after_prefix + id_end;
        let tail = &text[after_id..];

        if let Some(stripped) = tail.strip_prefix(' ') {
            if let Some(plus) = stripped.strip_prefix('+') {
                let mut digits = 0usize;
                for (idx, ch) in plus.char_indices() {
                    if ch.is_ascii_digit() {
                        digits = idx + 1;
                    } else {
                        break;
                    }
                }
                if digits > 0 && plus[digits..].starts_with(" lines]") {
                    let end = after_id + 1 + 1 + digits + " lines".len() + 1;
                    out.push(MarkerMatch {
                        start,
                        end,
                        id,
                        suffix_start: after_id,
                        suffix_end: end - 1,
                    });
                    i = end;
                    continue;
                }
            } else {
                let mut digits = 0usize;
                for (idx, ch) in stripped.char_indices() {
                    if ch.is_ascii_digit() {
                        digits = idx + 1;
                    } else {
                        break;
                    }
                }
                if digits > 0 && stripped[digits..].starts_with(" chars]") {
                    let end = after_id + 1 + digits + " chars".len() + 1;
                    out.push(MarkerMatch {
                        start,
                        end,
                        id,
                        suffix_start: after_id,
                        suffix_end: end - 1,
                    });
                    i = end;
                    continue;
                }
            }
        }

        if tail.starts_with(']') {
            let end = after_id + 1;
            out.push(MarkerMatch {
                start,
                end,
                id,
                suffix_start: after_id,
                suffix_end: after_id,
            });
            i = end;
            continue;
        }
        i = after_prefix;
    }
    out
}

/// True when `segment` is a complete paste marker token.
#[must_use]
pub fn is_paste_marker(segment: &str) -> bool {
    if segment.len() < 10 {
        return false;
    }
    let matches = find_paste_markers(segment);
    matches.len() == 1 && matches[0].start == 0 && matches[0].end == segment.len()
}

/// Parse the numeric id from a paste marker segment.
#[must_use]
pub fn paste_marker_id(segment: &str) -> Option<u32> {
    let matches = find_paste_markers(segment);
    if matches.len() == 1 && matches[0].start == 0 && matches[0].end == segment.len() {
        Some(matches[0].id)
    } else {
        None
    }
}

/// Format a large-paste marker for the given id and content.
#[must_use]
pub fn format_paste_marker(id: u32, text: &str) -> String {
    let line_count = if text.is_empty() {
        1
    } else {
        text.split('\n').count()
    };
    let total_chars = text.len();
    if line_count > LARGE_PASTE_LINE_THRESHOLD {
        format!("[paste #{id} +{line_count} lines]")
    } else {
        format!("[paste #{id} {total_chars} chars]")
    }
}

/// True when paste content should be collapsed to a marker.
#[must_use]
pub fn is_large_paste(text: &str) -> bool {
    let lines = if text.is_empty() {
        1
    } else {
        text.split('\n').count()
    };
    lines > LARGE_PASTE_LINE_THRESHOLD || text.len() > LARGE_PASTE_CHAR_THRESHOLD
}

/// Normalize paste/editor text: CRLF/CR → LF, tabs → 4 spaces.
#[must_use]
pub fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
}

/// Decode CSI-u Ctrl sequences that some terminals embed inside bracketed paste.
#[must_use]
pub fn decode_csi_u_ctrl(pasted: &str) -> String {
    let mut out = String::with_capacity(pasted.len());
    let bytes = pasted.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            let dig_start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if dig_start < j
                && bytes.get(j) == Some(&b';')
                && bytes.get(j + 1) == Some(&b'5')
                && bytes.get(j + 2) == Some(&b'u')
                && let Ok(code) = std::str::from_utf8(&bytes[dig_start..j])
                && let Ok(cp) = code.parse::<u32>()
            {
                if (97..=122).contains(&cp)
                    && let Some(c) = char::from_u32(cp - 96)
                {
                    out.push(c);
                    i = j + 3;
                    continue;
                }
                if (65..=90).contains(&cp)
                    && let Some(c) = char::from_u32(cp - 64)
                {
                    out.push(c);
                    i = j + 3;
                    continue;
                }
            }
        }
        let Some(ch) = pasted[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Filter non-printable characters except newlines.
#[must_use]
pub fn filter_printable_keep_newlines(text: &str) -> String {
    text.chars()
        .filter(|c| *c == '\n' || (*c as u32) >= 32)
        .collect()
}

/// Path-paste heuristic: if paste starts with `/`, `~`, or `.` and the char
/// before the cursor is a word character, prepend a space.
#[must_use]
pub fn maybe_prepend_path_space(paste: &str, char_before_cursor: Option<char>) -> String {
    let starts_path = paste
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '/' | '~' | '.'));
    if !starts_path {
        return paste.to_owned();
    }
    if char_before_cursor.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
        format!(" {paste}")
    } else {
        paste.to_owned()
    }
}

/// Expand all stored paste markers in `text`.
#[must_use]
pub fn expand_paste_markers<S: BuildHasher, V: AsRef<str>>(
    text: &str,
    pastes: &HashMap<u32, V, S>,
) -> String {
    let markers = find_paste_markers(text);
    if markers.is_empty() {
        return text.to_owned();
    }
    let mut result = String::with_capacity(text.len());
    let mut last = 0usize;
    for m in markers {
        result.push_str(&text[last..m.start]);
        if let Some(content) = pastes.get(&m.id) {
            result.push_str(content.as_ref());
        } else {
            result.push_str(&text[m.start..m.end]);
        }
        last = m.end;
    }
    result.push_str(&text[last..]);
    result
}

/// After deleting paste `target_id`, renumber higher markers and remap the store.
pub fn renumber_after_delete<V>(
    lines: &mut [String],
    pastes: &mut HashMap<u32, V>,
    target_id: u32,
) {
    pastes.remove(&target_id);

    for line in lines.iter_mut() {
        let markers = find_paste_markers(line);
        if markers.is_empty() {
            continue;
        }
        let mut new_line = String::with_capacity(line.len());
        let mut last = 0usize;
        for m in markers {
            new_line.push_str(&line[last..m.start]);
            if m.id <= target_id {
                new_line.push_str(&line[m.start..m.end]);
            } else {
                let suffix = &line[m.suffix_start..m.suffix_end];
                let _ = write!(new_line, "[paste #{}{}]", m.id - 1, suffix);
            }
            last = m.end;
        }
        new_line.push_str(&line[last..]);
        *line = new_line;
    }

    let mut rebuilt = HashMap::with_capacity(pastes.len());
    for (id, content) in pastes.drain() {
        let new_id = if id > target_id { id - 1 } else { id };
        rebuilt.insert(new_id, content);
    }
    *pastes = rebuilt;
}

/// Find all valid paste-marker spans in `text` whose ids are in `valid_ids`.
#[must_use]
pub fn find_marker_spans<S: std::hash::BuildHasher>(
    text: &str,
    valid_ids: &HashSet<u32, S>,
) -> Vec<(usize, usize)> {
    find_paste_markers(text)
        .into_iter()
        .filter(|m| valid_ids.contains(&m.id))
        .map(|m| (m.start, m.end))
        .collect()
}

/// Full paste pipeline used by the editor before insert.
#[must_use]
pub fn prepare_paste_text(raw: &str, char_before_cursor: Option<char>) -> String {
    let decoded = decode_csi_u_ctrl(raw);
    let normalized = normalize_text(&decoded);
    let filtered = filter_printable_keep_newlines(&normalized);
    maybe_prepend_path_space(&filtered, char_before_cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_paste_detection_and_marker() {
        let many_lines = (0..12)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(is_large_paste(&many_lines));
        let marker = format_paste_marker(1, &many_lines);
        assert!(marker.starts_with("[paste #1 +"));
        assert!(is_paste_marker(&marker));
        assert_eq!(paste_marker_id(&marker), Some(1));

        let long = "x".repeat(1001);
        assert!(is_large_paste(&long));
        let m = format_paste_marker(2, &long);
        assert!(m.contains("chars"));
        assert!(is_paste_marker(&m));
    }

    #[test]
    fn normalize_and_filter() {
        assert_eq!(normalize_text("a\r\nb\rc\td"), "a\nb\nc    d");
        assert_eq!(filter_printable_keep_newlines("a\x01b\nc"), "ab\nc");
    }

    #[test]
    fn path_space_heuristic() {
        assert_eq!(maybe_prepend_path_space("/tmp", Some('x')), " /tmp");
        assert_eq!(maybe_prepend_path_space("/tmp", Some(' ')), "/tmp");
        assert_eq!(maybe_prepend_path_space("hello", Some('x')), "hello");
    }

    #[test]
    fn expand_markers() {
        let mut pastes = HashMap::new();
        pastes.insert(1, "hello\nworld".to_owned());
        let text = "before [paste #1 +2 lines] after";
        assert_eq!(
            expand_paste_markers(text, &pastes),
            "before hello\nworld after"
        );
    }

    #[test]
    fn renumber_on_delete() {
        let mut pastes = HashMap::new();
        pastes.insert(1, "a".into());
        pastes.insert(2, "b".into());
        pastes.insert(3, "c".into());
        let mut lines = vec!["[paste #1 1 chars] [paste #2 1 chars] [paste #3 1 chars]".into()];
        renumber_after_delete(&mut lines, &mut pastes, 1);
        assert!(!pastes.contains_key(&3));
        assert_eq!(pastes.get(&1), Some(&"b".to_owned()));
        assert_eq!(pastes.get(&2), Some(&"c".to_owned()));
        assert!(lines[0].contains("[paste #1 1 chars]"));
        assert!(lines[0].contains("[paste #2 1 chars]"));
        assert!(!lines[0].contains("[paste #3"));
    }

    #[test]
    fn decode_csi_u() {
        let raw = "hello\u{1b}[106;5uworld";
        let decoded = decode_csi_u_ctrl(raw);
        assert_eq!(decoded, "hello\nworld");
    }
}
