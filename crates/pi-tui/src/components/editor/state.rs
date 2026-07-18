//! Editor buffer state and grapheme/paste-marker segmentation.

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;

use unicode_segmentation::UnicodeSegmentation;

use super::paste::find_marker_spans;
use crate::editor_support::GraphemeSeg;

/// Logical editor buffer and cursor (byte indices over UTF-8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorState {
    /// Logical lines (always at least one, possibly empty).
    pub lines: Vec<String>,
    /// Cursor line index.
    pub cursor_line: usize,
    /// Cursor byte column within the current line.
    pub cursor_col: usize,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
        }
    }
}

impl EditorState {
    /// Create an empty editor state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Join lines with `\n`.
    #[must_use]
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// True when the buffer is a single empty line.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// Current line text.
    #[must_use]
    pub fn current_line(&self) -> &str {
        self.lines.get(self.cursor_line).map_or("", String::as_str)
    }

    /// Clamp cursor into valid ranges.
    pub fn clamp_cursor(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        if self.cursor_line >= self.lines.len() {
            self.cursor_line = self.lines.len() - 1;
        }
        let line_len = self.lines[self.cursor_line].len();
        if self.cursor_col > line_len {
            self.cursor_col = line_len;
        }
        while self.cursor_col > 0 && !self.lines[self.cursor_line].is_char_boundary(self.cursor_col)
        {
            self.cursor_col -= 1;
        }
    }
}

/// Last editor action category (kill accumulation / undo coalescing / yank-pop gate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastAction {
    /// Kill/delete-to-kill-ring sequence.
    Kill,
    /// Yank or yank-pop.
    Yank,
    /// Consecutive non-whitespace typing (fish undo coalesce).
    TypeWord,
}

/// Segment `text` into graphemes, merging valid paste markers into atomic units.
#[must_use]
pub fn segment_graphemes_with_markers<S: BuildHasher>(
    text: &str,
    valid_ids: &HashSet<u32, S>,
) -> Vec<GraphemeSeg> {
    if valid_ids.is_empty() || !text.contains("[paste #") {
        return default_grapheme_segs(text);
    }
    let markers = find_marker_spans(text, valid_ids);
    if markers.is_empty() {
        return default_grapheme_segs(text);
    }

    let base: Vec<(usize, &str)> = text.grapheme_indices(true).collect();
    let mut result = Vec::new();
    let mut marker_idx = 0usize;

    for (index, segment) in base {
        while marker_idx < markers.len() && markers[marker_idx].1 <= index {
            marker_idx += 1;
        }
        let marker = markers.get(marker_idx).copied();
        if let Some((m_start, m_end)) = marker
            && index >= m_start
            && index < m_end
        {
            if index == m_start {
                result.push(GraphemeSeg {
                    segment: text[m_start..m_end].to_owned(),
                    index: m_start,
                });
            }
            continue;
        }
        result.push(GraphemeSeg {
            segment: segment.to_owned(),
            index,
        });
    }
    result
}

/// Default grapheme segmentation.
#[must_use]
pub fn default_grapheme_segs(text: &str) -> Vec<GraphemeSeg> {
    text.grapheme_indices(true)
        .map(|(index, segment)| GraphemeSeg {
            segment: segment.to_owned(),
            index,
        })
        .collect()
}

/// Valid paste ids from the paste store.
#[must_use]
pub fn valid_paste_ids<S: BuildHasher, V>(pastes: &HashMap<u32, V, S>) -> HashSet<u32> {
    pastes.keys().copied().collect()
}

/// Length in bytes of the grapheme before `col` (0 if at start).
#[must_use]
pub fn prev_grapheme_len<S: BuildHasher>(
    line: &str,
    col: usize,
    valid_ids: &HashSet<u32, S>,
) -> usize {
    if col == 0 {
        return 0;
    }
    let segs = segment_graphemes_with_markers(&line[..col.min(line.len())], valid_ids);
    segs.last().map_or(1, |s| s.segment.len())
}

/// Length in bytes of the grapheme at `col`.
#[must_use]
pub fn next_grapheme_len<S: BuildHasher>(
    line: &str,
    col: usize,
    valid_ids: &HashSet<u32, S>,
) -> usize {
    if col >= line.len() {
        return 0;
    }
    let segs = segment_graphemes_with_markers(&line[col..], valid_ids);
    segs.first().map_or(1, |s| s.segment.len())
}

/// Sticky-column decision table from TS `computeVerticalMoveColumn`.
#[must_use]
pub fn compute_vertical_move_column(
    preferred: &mut Option<usize>,
    current_visual_col: usize,
    source_max_visual_col: usize,
    target_max_visual_col: usize,
) -> usize {
    let has_preferred = preferred.is_some();
    let cursor_in_middle = current_visual_col < source_max_visual_col;
    let target_too_short = target_max_visual_col < current_visual_col;

    if !has_preferred || cursor_in_middle {
        if target_too_short {
            *preferred = Some(current_visual_col);
            return target_max_visual_col;
        }
        *preferred = None;
        return current_visual_col;
    }

    let preferred_col = preferred.unwrap_or(current_visual_col);
    let target_cant_fit_preferred = target_max_visual_col < preferred_col;
    if target_too_short || target_cant_fit_preferred {
        return target_max_visual_col;
    }

    *preferred = None;
    preferred_col
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_state() {
        let s = EditorState::new();
        assert!(s.is_empty());
        assert_eq!(s.text(), "");
    }

    #[test]
    fn sticky_column_table() {
        let mut pref = None;
        assert_eq!(compute_vertical_move_column(&mut pref, 5, 10, 3), 3);
        assert_eq!(pref, Some(5));
        assert_eq!(compute_vertical_move_column(&mut pref, 3, 3, 8), 5);
        assert_eq!(pref, None);
    }

    #[test]
    fn paste_marker_atomic_segment() -> Result<(), &'static str> {
        let mut pastes: HashMap<u32, String> = HashMap::new();
        pastes.insert(1, "big".into());
        let ids = valid_paste_ids(&pastes);
        let line = "hi [paste #1 3 chars] there";
        let segs = segment_graphemes_with_markers(line, &ids);
        let marker = segs
            .iter()
            .find(|s| s.segment.starts_with("[paste #"))
            .ok_or("missing paste marker segment")?;
        assert_eq!(marker.segment, "[paste #1 3 chars]");
        Ok(())
    }
}
