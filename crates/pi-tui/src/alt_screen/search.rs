//! Literal Unicode transcript search and its focusable overlay component.

use std::any::Any;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_segmentation::UnicodeSegmentation;

use crate::component::{Component, EventResult, UiEvent};
use crate::components::Input;
use crate::focus::{FocusId, Focusable};
use crate::text::{extract_ansi_code, grapheme_width, visible_width};

/// One visible row/cell segment belonging to a search match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchSegment {
    /// Zero-based document row.
    pub row: usize,
    /// Inclusive starting display-cell column.
    pub start_col: usize,
    /// Exclusive ending display-cell column.
    pub end_col: usize,
}

/// One literal search match, possibly spanning wrapped rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    /// Non-overlapping row/cell segments in document order.
    pub segments: Vec<SearchSegment>,
}

/// Result of an index search. The match slice borrows the index's current set.
pub struct SearchResult<'a> {
    /// Current match set.
    pub matches: &'a [SearchMatch],
    /// Whether the corpus or normalized query changed.
    pub changed: bool,
}

/// Navigation direction for the overlay controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDirection {
    /// Select the previous match.
    Previous,
    /// Select the next match.
    Next,
}

#[derive(Debug, Clone)]
struct SearchSourceSpan {
    text_start: usize,
    text_end: usize,
    row: usize,
    start_col: usize,
    end_col: usize,
}

#[derive(Debug, Default, Clone)]
struct SearchCorpus {
    text: String,
    spans: Vec<SearchSourceSpan>,
}

/// Cached literal-search corpus and matches.
///
/// Callers with a document generation should use [`Self::search_generation`]
/// so an unchanged projection is not compared or copied. [`Self::search`]
/// remains convenient for standalone callers and caches its source rows.
pub struct SearchIndex {
    source_generation: Option<u64>,
    source_lines: Option<Vec<String>>,
    corpus: Option<SearchCorpus>,
    normalized_query: Option<String>,
    matches: Vec<SearchMatch>,
}

impl Default for SearchIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchIndex {
    /// Create an empty search index.
    #[must_use]
    pub fn new() -> Self {
        Self {
            source_generation: None,
            source_lines: None,
            corpus: None,
            normalized_query: None,
            matches: Vec::new(),
        }
    }

    /// Search rows, detecting source changes by value.
    pub fn search<'a>(&'a mut self, lines: &[String], query: &str) -> SearchResult<'a> {
        let source_changed = match self.source_lines.as_ref() {
            Some(previous) if previous.len() == lines.len() => previous
                .iter()
                .zip(lines)
                .any(|(previous, current)| previous != current),
            Some(_) | None => true,
        };
        let corpus_changed = source_changed || self.corpus.is_none();
        if corpus_changed {
            self.source_lines = Some(lines.to_vec());
            self.source_generation = None;
            self.corpus = Some(build_search_corpus(lines));
        }
        self.finish_search(query, corpus_changed)
    }

    /// Search rows whose retained document generation is known to the caller.
    pub fn search_generation<'a>(
        &'a mut self,
        generation: u64,
        lines: &[String],
        query: &str,
    ) -> SearchResult<'a> {
        let source_changed = self.source_generation != Some(generation) || self.corpus.is_none();
        if source_changed {
            self.source_lines = None;
            self.source_generation = Some(generation);
            self.corpus = Some(build_search_corpus(lines));
        }
        self.finish_search(query, source_changed)
    }

    /// Borrow the currently indexed matches without changing the query.
    #[must_use]
    pub fn matches(&self) -> &[SearchMatch] {
        &self.matches
    }

    /// Normalized, case-folded query currently indexed.
    #[must_use]
    pub fn normalized_query(&self) -> Option<&str> {
        self.normalized_query.as_deref()
    }

    fn finish_search<'a>(&'a mut self, query: &str, source_changed: bool) -> SearchResult<'a> {
        let normalized_query = normalize_query(query);
        let query_changed = self.normalized_query.as_deref() != Some(normalized_query.as_str());
        if source_changed || query_changed {
            self.normalized_query = Some(normalized_query.clone());
            self.matches = self
                .corpus
                .as_ref()
                .map_or_else(Vec::new, |corpus| find_corpus_matches(corpus, &normalized_query));
        }
        SearchResult {
            matches: &self.matches,
            changed: source_changed || query_changed,
        }
    }
}

/// Find literal Unicode case-insensitive matches in rows.
#[must_use]
pub fn find_search_matches(lines: &[String], query: &str) -> Vec<SearchMatch> {
    let normalized_query = normalize_query(query);
    if normalized_query.is_empty() {
        return Vec::new();
    }
    find_corpus_matches(&build_search_corpus(lines), &normalized_query)
}

/// C-compatible alias for callers that used the original name.
pub type AltScreenSearchMatch = SearchMatch;
/// C-compatible alias for callers that used the original name.
pub type AltScreenSearchSegment = SearchSegment;
/// C-compatible alias for callers that used the original name.
pub type AltScreenSearchIndex = SearchIndex;

/// Stable key for retaining a selected match through source regeneration.
#[must_use]
pub fn search_match_key(search_match: &SearchMatch) -> String {
    let Some(first) = search_match.segments.first() else {
        return String::new();
    };
    let Some(last) = search_match.segments.last() else {
        return String::new();
    };
    format!(
        "{}:{}:{}:{}",
        first.row, first.start_col, last.row, last.end_col
    )
}

/// Alias for the original C-port helper name.
#[must_use]
pub fn get_alt_screen_search_match_key(search_match: &SearchMatch) -> String {
    search_match_key(search_match)
}

/// Compute the clipped top-right search overlay rectangle.
#[must_use]
pub fn transcript_search_rect(area: Rect) -> Rect {
    if area.width == 0 || area.height == 0 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    let margin: u16 = u16::from(area.width > 2);
    let available_width = area.width.saturating_sub(margin.saturating_mul(2));
    let forty_percent = u16::try_from(u32::from(area.width).saturating_mul(40) / 100)
        .unwrap_or(u16::MAX);
    let requested_width = forty_percent.max(32);
    let width = requested_width.min(available_width);
    let x = area
        .x
        .saturating_add(area.width.saturating_sub(margin).saturating_sub(width));
    let y = area.y.saturating_add(margin.min(area.height));
    let height = area.height.saturating_sub(margin).min(3);
    Rect::new(x, y, width, height)
}

type QueryChangeCallback = Box<dyn FnMut(&str) + Send>;

/// Focusable transcript search input plus result/navigation controls.
pub struct TranscriptSearch {
    input: Input,
    focus_id: FocusId,
    focused: bool,
    result_index: Option<usize>,
    result_count: usize,
    previous_hint: String,
    next_hint: String,
    hovered: Option<SearchDirection>,
    previous_start: Option<u16>,
    previous_end: Option<u16>,
    next_start: Option<u16>,
    next_end: Option<u16>,
    on_query_change: Option<QueryChangeCallback>,
}

impl Default for TranscriptSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptSearch {
    /// Create an empty search overlay.
    #[must_use]
    pub fn new() -> Self {
        Self {
            input: Input::new(),
            focus_id: FocusId::new(),
            focused: false,
            result_index: None,
            result_count: 0,
            previous_hint: "Shift+Enter".to_owned(),
            next_hint: "Enter".to_owned(),
            hovered: None,
            previous_start: None,
            previous_end: None,
            next_start: None,
            next_end: None,
            on_query_change: None,
        }
    }

    /// Current query text.
    #[must_use]
    pub fn query(&self) -> &str {
        self.input.value()
    }

    /// Replace the query and reset the input cursor safely.
    pub fn set_query(&mut self, query: impl Into<String>) {
        self.input.set_value(query);
    }

    /// Set a callback invoked after the query changes through input events.
    pub fn set_query_callback(&mut self, callback: Option<QueryChangeCallback>) {
        self.on_query_change = callback;
    }

    /// Update result count and current index.
    pub fn set_result(&mut self, index: Option<usize>, count: usize) {
        self.result_index = index.filter(|value| *value < count);
        self.result_count = count;
    }

    /// Current selected result index, if one exists.
    #[must_use]
    pub const fn selected_index(&self) -> Option<usize> {
        self.result_index
    }

    /// Current result count.
    #[must_use]
    pub const fn result_count(&self) -> usize {
        self.result_count
    }

    /// Set the configured navigation key hints shown in the footer row.
    pub fn set_navigation_hints(
        &mut self,
        previous: impl Into<String>,
        next: impl Into<String>,
    ) {
        self.previous_hint = previous.into();
        self.next_hint = next.into();
    }

    /// Return the navigation button under an overlay-local coordinate.
    #[must_use]
    pub fn navigation_direction_at(&self, row: u16, column: u16) -> Option<SearchDirection> {
        if row != 2 {
            return None;
        }
        if self
            .previous_start
            .zip(self.previous_end)
            .is_some_and(|(start, end)| column >= start && column < end)
        {
            return Some(SearchDirection::Previous);
        }
        if self
            .next_start
            .zip(self.next_end)
            .is_some_and(|(start, end)| column >= start && column < end)
        {
            return Some(SearchDirection::Next);
        }
        None
    }

    /// Set the hovered navigation control; returns whether rendering changed.
    pub fn set_hovered_navigation_direction(&mut self, direction: Option<SearchDirection>) -> bool {
        if direction == self.hovered {
            return false;
        }
        self.hovered = direction;
        true
    }

    /// Render the overlay into a clipped rectangle.
    pub fn render_into(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            self.previous_start = None;
            self.previous_end = None;
            self.next_start = None;
            self.next_end = None;
            return;
        }
        let max_x = area.x.saturating_add(area.width);
        let max_y = area.y.saturating_add(area.height);
        for y in area.y..max_y {
            for x in area.x..max_x {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.reset();
                }
            }
        }
        if area.height >= 1 {
            draw_border_row(buf, area.x, area.y, area.width, '┌', '─', '┐');
        }
        if area.height >= 2 {
            draw_border_row(buf, area.x, area.y.saturating_add(1), area.width, '│', ' ', '│');
            if area.width > 2 {
                let inner = Rect::new(area.x.saturating_add(1), area.y.saturating_add(1), area.width - 2, 1);
                self.input.render(inner, buf);
                self.paint_result(inner, buf);
            }
        }
        if area.height >= 3 {
            self.paint_controls(buf, area);
        } else {
            self.previous_start = None;
            self.previous_end = None;
            self.next_start = None;
            self.next_end = None;
        }
    }

    fn paint_result(&self, inner: Rect, buf: &mut Buffer) {
        let result = if self.query().is_empty() {
            String::new()
        } else if self.result_count == 0 {
            "No matches".to_owned()
        } else {
            format!(
                "{}/{}",
                self.result_index.map_or(0, |index| index.saturating_add(1)),
                self.result_count
            )
        };
        let result_width = visible_width(&result).min(usize::from(inner.width));
        let result_width = u16::try_from(result_width).unwrap_or(inner.width);
        let start = inner
            .x
            .saturating_add(inner.width.saturating_sub(result_width));
        let mut x = start;
        for grapheme in result.graphemes(true) {
            let width = u16::try_from(grapheme_width(grapheme)).unwrap_or(u16::MAX);
            if width == 0 || x >= inner.right() {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, inner.y)) {
                cell.set_symbol(grapheme);
                cell.set_style(Style::default().add_modifier(Modifier::DIM));
            }
            x = x.saturating_add(width).min(inner.right());
        }
    }

    fn paint_controls(&mut self, buf: &mut Buffer, area: Rect) {
        let y = area.y.saturating_add(2);
        draw_border_row(buf, area.x, y, area.width, '└', '─', '┘');
        self.previous_start = None;
        self.previous_end = None;
        self.next_start = None;
        self.next_end = None;
        if area.width <= 2 {
            return;
        }
        let inner_start = area.x.saturating_add(1);
        let inner_end = area.right().saturating_sub(1);
        let previous = format!("↑ {}", self.previous_hint);
        let next = format!("↓ {}", self.next_hint);
        let separator = " · ";
        let controls_width = visible_width(&previous)
            .saturating_add(visible_width(separator))
            .saturating_add(visible_width(&next));
        let available = usize::from(inner_end.saturating_sub(inner_start));
        if controls_width > available {
            if available >= 1 {
                self.paint_button(buf, y, inner_start, "↑", SearchDirection::Previous);
            }
            if available >= 3 {
                self.paint_button(
                    buf,
                    y,
                    inner_start.saturating_add(2),
                    "↓",
                    SearchDirection::Next,
                );
            }
            return;
        }
        let start = usize::from(inner_start)
            .saturating_add(available.saturating_sub(controls_width));
        let start = u16::try_from(start).unwrap_or(inner_start);
        let previous_width = u16::try_from(visible_width(&previous)).unwrap_or(u16::MAX);
        self.paint_button(buf, y, start, &previous, SearchDirection::Previous);
        let next_start = start
            .saturating_add(previous_width)
            .saturating_add(u16::try_from(visible_width(separator)).unwrap_or(u16::MAX));
        self.paint_button(buf, y, next_start, &next, SearchDirection::Next);
    }

    fn paint_button(&mut self, buf: &mut Buffer, y: u16, start: u16, text: &str, direction: SearchDirection) {
        let width = u16::try_from(visible_width(text)).unwrap_or(u16::MAX);
        match direction {
            SearchDirection::Previous => {
                self.previous_start = Some(start);
                self.previous_end = Some(start.saturating_add(width));
            }
            SearchDirection::Next => {
                self.next_start = Some(start);
                self.next_end = Some(start.saturating_add(width));
            }
        }
        let style = if self.hovered == Some(direction) {
            Style::default().add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
        };
        let mut x = start;
        for grapheme in text.graphemes(true) {
            let width = u16::try_from(grapheme_width(grapheme)).unwrap_or(u16::MAX);
            if width == 0 {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol(grapheme);
                cell.set_style(style);
            }
            x = x.saturating_add(width);
        }
    }
}

impl Component for TranscriptSearch {
    fn measure(&mut self, _width: u16) -> u16 {
        3
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.render_into(area, buf);
    }

    fn handle_event(&mut self, event: &UiEvent) -> EventResult {
        let previous = self.input.value().to_owned();
        let result = self.input.handle_event(event);
        if self.input.value() != previous
            && let Some(callback) = self.on_query_change.as_mut()
        {
            callback(self.input.value());
        }
        result
    }

    fn invalidate(&mut self) {
        self.input.invalidate();
    }
}

impl Focusable for TranscriptSearch {
    fn focus_id(&self) -> FocusId {
        self.focus_id
    }

    fn is_focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.set_focused(focused);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn normalize_query(query: &str) -> String {
    let mut normalized = String::new();
    let mut pending_space = false;
    for grapheme in query.graphemes(true) {
        if grapheme.chars().all(char::is_whitespace) {
            pending_space = !normalized.is_empty();
            continue;
        }
        if pending_space {
            normalized.push(' ');
            pending_space = false;
        }
        normalized.extend(grapheme.chars().flat_map(char::to_lowercase));
    }
    normalized.trim().to_owned()
}

fn build_search_corpus(lines: &[String]) -> SearchCorpus {
    let mut corpus = SearchCorpus::default();
    let mut pending_separator = false;
    for (row, line) in lines.iter().enumerate() {
        let mut byte = 0usize;
        let mut column = 0usize;
        while byte < line.len() {
            if let Some(ansi) = extract_ansi_code(line, byte) {
                byte = byte.saturating_add(ansi.len);
                continue;
            }
            let mut end = byte;
            while end < line.len() && extract_ansi_code(line, end).is_none() {
                let Some(character) = line.get(end..).and_then(|rest| rest.chars().next()) else {
                    break;
                };
                end = end.saturating_add(character.len_utf8());
            }
            let Some(plain) = line.get(byte..end) else {
                break;
            };
            for grapheme in plain.graphemes(true) {
                let width = grapheme_width(grapheme);
                if grapheme.chars().all(char::is_whitespace) {
                    if !corpus.text.is_empty() {
                        pending_separator = true;
                    }
                    column = column.saturating_add(width);
                    continue;
                }
                if pending_separator {
                    corpus.text.push(' ');
                    pending_separator = false;
                }
                let folded = grapheme.chars().flat_map(char::to_lowercase).collect::<String>();
                let text_start = corpus.text.len();
                corpus.text.push_str(&folded);
                let text_end = corpus.text.len();
                corpus.spans.push(SearchSourceSpan {
                    text_start,
                    text_end,
                    row,
                    start_col: column,
                    end_col: column.saturating_add(width),
                });
                column = column.saturating_add(width);
            }
            byte = end;
        }
        if !corpus.text.is_empty() {
            pending_separator = true;
        }
    }
    corpus
}

fn find_corpus_matches(corpus: &SearchCorpus, normalized_query: &str) -> Vec<SearchMatch> {
    if normalized_query.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    let mut search_from = 0usize;
    while search_from < corpus.text.len() {
        let Some(remainder) = corpus.text.get(search_from..) else {
            break;
        };
        let Some(relative_start) = remainder.find(normalized_query) else {
            break;
        };
        let start = search_from.saturating_add(relative_start);
        let Some(end) = start.checked_add(normalized_query.len()) else {
            break;
        };
        let mut segments: Vec<SearchSegment> = Vec::new();
        for span in &corpus.spans {
            if span.text_end <= start {
                continue;
            }
            if span.text_start >= end {
                break;
            }
            if let Some(previous) = segments.last_mut()
                && previous.row == span.row
                && span.start_col <= previous.end_col
            {
                previous.end_col = previous.end_col.max(span.end_col);
            } else {
                segments.push(SearchSegment {
                    row: span.row,
                    start_col: span.start_col,
                    end_col: span.end_col,
                });
            }
        }
        if !segments.is_empty() {
            matches.push(SearchMatch { segments });
        }
        search_from = end.max(search_from.saturating_add(1));
    }
    matches
}

fn draw_border_row(buf: &mut Buffer, x: u16, y: u16, width: u16, left: char, middle: char, right: char) {
    if width == 0 {
        return;
    }
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(&left.to_string());
    }
    if width > 1 {
        let last = x.saturating_add(width.saturating_sub(1));
        for offset in 1..width.saturating_sub(1) {
            if let Some(cell) = buf.cell_mut((x.saturating_add(offset), y)) {
                cell.set_symbol(&middle.to_string());
            }
        }
        if let Some(cell) = buf.cell_mut((last, y)) {
            cell.set_symbol(&right.to_string());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, reason = "test code")]
mod tests {
    use super::*;

    fn rows(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn search_matches_literal_unicode_across_rows() {
        let lines = rows(&["café", " au lait"]);
        let matches = find_search_matches(&lines, "CAFÉ   AU");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].segments.len(), 2);
        assert_eq!(matches[0].segments[0].row, 0);
        assert_eq!(matches[0].segments[1].row, 1);
    }

    #[test]
    fn regex_metacharacters_are_literal() {
        let lines = rows(&["a.*b", "aXXb"]);
        let matches = find_search_matches(&lines, "a.*b");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].segments[0].start_col, 0);
        assert_eq!(matches[0].segments[0].end_col, 4);
    }

    #[test]
    fn overlay_geometry_clips_small_terminal() {
        for area in [
            Rect::new(0, 0, 0, 2),
            Rect::new(0, 0, 2, 0),
            Rect::new(4, 5, 1, 1),
            Rect::new(4, 5, 2, 2),
            Rect::new(4, 5, 20, 2),
        ] {
            let rect = transcript_search_rect(area);
            assert!(rect.x >= area.x && rect.y >= area.y);
            assert!(
                u32::from(rect.x) + u32::from(rect.width)
                    <= u32::from(area.x) + u32::from(area.width)
            );
            assert!(
                u32::from(rect.y) + u32::from(rect.height)
                    <= u32::from(area.y) + u32::from(area.height)
            );
        }
    }
}
