//! Pure edit matching and diff helpers for the edit tool.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/tools/edit-diff.ts`.
//! Matching runs against the original LF-normalized file; multi-edits apply in
//! reverse offset order. Fuzzy matches preserve untouched original line bytes.

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

/// Line ending detected from file content (first occurrence wins).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineEnding {
    /// Unix LF.
    Lf,
    /// Windows CRLF.
    Crlf,
}

impl LineEnding {
    /// Returns the ending bytes as a string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }
}

/// One exact-text replacement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Edit {
    /// Text to find in the original file.
    pub old_text: String,
    /// Replacement text.
    pub new_text: String,
}

/// Result of applying edits to LF-normalized content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedEditsResult {
    /// Content used as the left-hand side of display/unified diffs (original LF).
    pub base_content: String,
    /// Content after all replacements (still LF-normalized).
    pub new_content: String,
}

/// Display-oriented diff result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffStringResult {
    /// Numbered context diff text.
    pub diff: String,
    /// First changed line number in the new file (1-based), if any.
    pub first_changed_line: Option<usize>,
}

/// Detect line ending from the first newline occurrence.
#[must_use]
pub fn detect_line_ending(content: &str) -> LineEnding {
    let crlf_idx = content.find("\r\n");
    let lf_idx = content.find('\n');
    match (crlf_idx, lf_idx) {
        (None, None | Some(_)) => LineEnding::Lf,
        (Some(_), None) => LineEnding::Crlf,
        (Some(crlf), Some(lf)) => {
            if crlf < lf {
                LineEnding::Crlf
            } else {
                LineEnding::Lf
            }
        }
    }
}

/// Normalize `\r\n` and bare `\r` to `\n`.
#[must_use]
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Restore LF content to the original line ending style.
#[must_use]
pub fn restore_line_endings(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => text.to_owned(),
        LineEnding::Crlf => text.replace('\n', "\r\n"),
    }
}

/// Strip a leading UTF-8 BOM, returning `(bom, text_without_bom)`.
///
/// `bom` is `"\u{FEFF}"` when present, otherwise empty.
#[must_use]
pub fn strip_bom(content: &str) -> (String, String) {
    if let Some(rest) = content.strip_prefix('\u{FEFF}') {
        ("\u{FEFF}".to_owned(), rest.to_owned())
    } else {
        (String::new(), content.to_owned())
    }
}

/// Normalize text for fuzzy matching (NFKC, trimEnd, quotes, dashes, spaces).
#[must_use]
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let trimmed: String = nfkc
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        match ch {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => out.push('\''),
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => out.push('"'),
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => out.push('-'),
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

#[derive(Clone, Debug)]
struct FuzzyMatchResult {
    found: bool,
    index: usize,
    match_length: usize,
    used_fuzzy_match: bool,
}

fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatchResult {
    if let Some(exact_index) = content.find(old_text) {
        return FuzzyMatchResult {
            found: true,
            index: exact_index,
            match_length: old_text.len(),
            used_fuzzy_match: false,
        };
    }

    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old = normalize_for_fuzzy_match(old_text);
    if let Some(fuzzy_index) = fuzzy_content.find(&fuzzy_old) {
        return FuzzyMatchResult {
            found: true,
            index: fuzzy_index,
            match_length: fuzzy_old.len(),
            used_fuzzy_match: true,
        };
    }

    FuzzyMatchResult {
        found: false,
        index: 0,
        match_length: 0,
        used_fuzzy_match: false,
    }
}

/// Count non-overlapping occurrences after fuzzy normalization (JS `split.length - 1`).
fn count_occurrences(content: &str, old_text: &str) -> usize {
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old = normalize_for_fuzzy_match(old_text);
    if fuzzy_old.is_empty() {
        return 0;
    }
    fuzzy_content
        .split(fuzzy_old.as_str())
        .count()
        .saturating_sub(1)
}

fn empty_old_text_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!("oldText must not be empty in {path}.")
    } else {
        format!("edits[{edit_index}].oldText must not be empty in {path}.")
    }
}

fn not_found_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        )
    } else {
        format!(
            "Could not find edits[{edit_index}] in {path}. The oldText must match exactly including all whitespace and newlines."
        )
    }
}

fn duplicate_error(
    path: &str,
    edit_index: usize,
    total_edits: usize,
    occurrences: usize,
) -> String {
    if total_edits == 1 {
        format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        )
    } else {
        format!(
            "Found {occurrences} occurrences of edits[{edit_index}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
        )
    }
}

fn no_change_error(path: &str, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        )
    } else {
        format!("No changes made to {path}. The replacements produced identical content.")
    }
}

#[derive(Clone, Debug)]
struct MatchedEdit {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

#[derive(Clone, Copy, Debug)]
struct LineSpan {
    start: usize,
    end: usize,
}

fn split_lines_with_endings(content: &str) -> Vec<String> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0usize;
    let bytes = content.as_bytes();
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(content[start..=idx].to_owned());
            start = idx + 1;
        }
    }
    if start < content.len() {
        lines.push(content[start..].to_owned());
    }
    lines
}

fn get_line_spans(content: &str) -> Vec<LineSpan> {
    let mut offset = 0usize;
    split_lines_with_endings(content)
        .into_iter()
        .map(|line| {
            let span = LineSpan {
                start: offset,
                end: offset + line.len(),
            };
            offset = span.end;
            span
        })
        .collect()
}

fn get_replacement_line_range(
    lines: &[LineSpan],
    match_index: usize,
    match_length: usize,
) -> Result<(usize, usize), String> {
    let replacement_start = match_index;
    let replacement_end = match_index + match_length;

    let mut start_line = None;
    for (i, line) in lines.iter().enumerate() {
        if replacement_start >= line.start && replacement_start < line.end {
            start_line = Some(i);
            break;
        }
    }
    let start_line =
        start_line.ok_or_else(|| "Replacement range is outside the base content.".to_owned())?;

    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].end < replacement_end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        return Err("Replacement range is outside the base content.".to_owned());
    }
    Ok((start_line, end_line + 1))
}

fn apply_replacements(content: &str, replacements: &[MatchedEdit], offset: usize) -> String {
    let mut result = content.to_owned();
    for replacement in replacements.iter().rev() {
        let match_index = replacement.match_index.saturating_sub(offset);
        let end = match_index + replacement.match_length;
        if match_index > result.len() || end > result.len() {
            continue;
        }
        result.replace_range(match_index..end, &replacement.new_text);
    }
    result
}

#[derive(Clone, Debug)]
struct ReplacementGroup {
    start_line: usize,
    end_line: usize,
    replacements: Vec<MatchedEdit>,
}

/// Apply replacements matched against `base_content` onto `original_content`,
/// copying untouched original line blocks verbatim.
fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[MatchedEdit],
) -> Result<String, String> {
    let original_lines = split_lines_with_endings(original_content);
    let base_lines = get_line_spans(base_content);
    if original_lines.len() != base_lines.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .to_owned(),
        );
    }

    let mut groups: Vec<ReplacementGroup> = Vec::new();
    let mut sorted = replacements.to_vec();
    sorted.sort_by_key(|item| item.match_index);
    for replacement in sorted {
        let (start_line, end_line) = get_replacement_line_range(
            &base_lines,
            replacement.match_index,
            replacement.match_length,
        )?;
        if let Some(current) = groups.last_mut()
            && start_line < current.end_line
        {
            current.end_line = current.end_line.max(end_line);
            current.replacements.push(replacement);
            continue;
        }
        groups.push(ReplacementGroup {
            start_line,
            end_line,
            replacements: vec![replacement],
        });
    }

    let mut original_line_index = 0usize;
    let mut result = String::new();
    for group in groups {
        for line in &original_lines[original_line_index..group.start_line] {
            result.push_str(line);
        }
        let group_start_offset = base_lines[group.start_line].start;
        let group_end_offset = base_lines[group.end_line - 1].end;
        let slice = &base_content[group_start_offset..group_end_offset];
        result.push_str(&apply_replacements(
            slice,
            &group.replacements,
            group_start_offset,
        ));
        original_line_index = group.end_line;
    }
    for line in &original_lines[original_line_index..] {
        result.push_str(line);
    }
    Ok(result)
}

/// Apply one or more exact-text replacements to LF-normalized content.
///
/// # Errors
///
/// Returns an error string matching TypeScript edit-diff messages for empty
/// oldText, not found, duplicate, overlap, and no-op cases.
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEditsResult, String> {
    let normalized_edits: Vec<Edit> = edits
        .iter()
        .map(|edit| Edit {
            old_text: normalize_to_lf(&edit.old_text),
            new_text: normalize_to_lf(&edit.new_text),
        })
        .collect();

    let total = normalized_edits.len();
    for (i, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(empty_old_text_error(path, i, total));
        }
    }

    let initial_matches: Vec<FuzzyMatchResult> = normalized_edits
        .iter()
        .map(|edit| fuzzy_find_text(normalized_content, &edit.old_text))
        .collect();
    let used_fuzzy_match = initial_matches.iter().any(|m| m.used_fuzzy_match);
    let replacement_base_content = if used_fuzzy_match {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_owned()
    };

    let mut matched_edits: Vec<MatchedEdit> = Vec::with_capacity(total);
    for (i, edit) in normalized_edits.iter().enumerate() {
        let match_result = fuzzy_find_text(&replacement_base_content, &edit.old_text);
        if !match_result.found {
            return Err(not_found_error(path, i, total));
        }
        let occurrences = count_occurrences(&replacement_base_content, &edit.old_text);
        if occurrences > 1 {
            return Err(duplicate_error(path, i, total, occurrences));
        }
        matched_edits.push(MatchedEdit {
            edit_index: i,
            match_index: match_result.index,
            match_length: match_result.match_length,
            new_text: edit.new_text.clone(),
        });
    }

    matched_edits.sort_by_key(|item| item.match_index);
    for i in 1..matched_edits.len() {
        let previous = &matched_edits[i - 1];
        let current = &matched_edits[i];
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                previous.edit_index, current.edit_index
            ));
        }
    }

    let base_content = normalized_content.to_owned();
    let new_content = if used_fuzzy_match {
        apply_replacements_preserving_unchanged_lines(
            normalized_content,
            &replacement_base_content,
            &matched_edits,
        )?
    } else {
        apply_replacements(&replacement_base_content, &matched_edits, 0)
    };

    if base_content == new_content {
        return Err(no_change_error(path, total));
    }

    Ok(AppliedEditsResult {
        base_content,
        new_content,
    })
}

fn split_lines_keep_trailing_empty(content: &str) -> Vec<&str> {
    content.split('\n').collect()
}

fn split_patch_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        Vec::new()
    } else {
        content.split_terminator('\n').collect()
    }
}

#[derive(Clone, Debug)]
struct PatchLine {
    prefix: char,
    text: String,
}

impl PatchLine {
    const fn is_context(&self) -> bool {
        self.prefix == ' '
    }

    fn old_increment(&self) -> usize {
        usize::from(self.prefix != '+')
    }

    fn new_increment(&self) -> usize {
        usize::from(self.prefix != '-')
    }
}

fn patch_lines(old_content: &str, new_content: &str) -> Vec<PatchLine> {
    let old_lines = split_patch_lines(old_content);
    let new_lines = split_patch_lines(new_content);
    let lcs = longest_common_subsequence(&old_lines, &new_lines);
    let mut ops = Vec::new();
    let mut old_index = 0;
    let mut new_index = 0;
    let mut common_index = 0;

    while old_index < old_lines.len() || new_index < new_lines.len() {
        if common_index < lcs.len()
            && old_index == lcs[common_index].0
            && new_index == lcs[common_index].1
        {
            ops.push(Op::Equal(old_lines[old_index].to_owned()));
            old_index += 1;
            new_index += 1;
            common_index += 1;
        } else if new_index < new_lines.len()
            && (common_index >= lcs.len() || new_index < lcs[common_index].1)
        {
            ops.push(Op::Added(new_lines[new_index].to_owned()));
            new_index += 1;
        } else if old_index < old_lines.len() {
            ops.push(Op::Removed(old_lines[old_index].to_owned()));
            old_index += 1;
        }
    }

    let mut lines = Vec::new();
    for part in collapse_ops(&ops) {
        match part {
            DiffPart::Equal(equal) => lines.extend(
                equal
                    .into_iter()
                    .map(|text| PatchLine { prefix: ' ', text }),
            ),
            DiffPart::Change { removed, added } => {
                lines.extend(
                    removed
                        .into_iter()
                        .map(|text| PatchLine { prefix: '-', text }),
                );
                lines.extend(
                    added
                        .into_iter()
                        .map(|text| PatchLine { prefix: '+', text }),
                );
            }
        }
    }
    lines
}

fn context_boundary(
    lines: &[PatchLine],
    from: usize,
    context_lines: usize,
    reverse: bool,
) -> usize {
    let mut index = from;
    let mut context = 0;
    while if reverse {
        index > 0
    } else {
        index < lines.len()
    } {
        let candidate = if reverse { index - 1 } else { index };
        if lines[candidate].is_context() {
            if context == context_lines {
                break;
            }
            context += 1;
        }
        index = if reverse { candidate } else { candidate + 1 };
    }
    index
}

/// Generate a standard contextual unified patch.
#[must_use]
pub fn generate_unified_patch(
    path: &str,
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> String {
    let mut out = format!("--- {path}\n+++ {path}\n");
    if old_content == new_content {
        return out;
    }

    let lines = patch_lines(old_content, new_content);
    let changes: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (!line.is_context()).then_some(index))
        .collect();
    let mut groups = Vec::<(usize, usize)>::new();
    let mut first = changes[0];
    let mut last = first;
    for &change in &changes[1..] {
        let unchanged_between = lines[last + 1..change]
            .iter()
            .filter(|line| line.is_context())
            .count();
        if unchanged_between > context_lines.saturating_mul(2) {
            groups.push((first, last));
            first = change;
        }
        last = change;
    }
    groups.push((first, last));

    for (first_change, last_change) in groups {
        let start = context_boundary(&lines, first_change, context_lines, true);
        let end = context_boundary(&lines, last_change + 1, context_lines, false);
        let old_before: usize = lines[..start].iter().map(PatchLine::old_increment).sum();
        let new_before: usize = lines[..start].iter().map(PatchLine::new_increment).sum();
        let old_count: usize = lines[start..end].iter().map(PatchLine::old_increment).sum();
        let new_count: usize = lines[start..end].iter().map(PatchLine::new_increment).sum();
        let old_start = if old_count == 0 {
            old_before
        } else {
            old_before + 1
        };
        let new_start = if new_count == 0 {
            new_before
        } else {
            new_before + 1
        };
        out.push_str("@@ -");
        out.push_str(&old_start.to_string());
        out.push(',');
        out.push_str(&old_count.to_string());
        out.push_str(" +");
        out.push_str(&new_start.to_string());
        out.push(',');
        out.push_str(&new_count.to_string());
        out.push_str(" @@\n");
        for line in &lines[start..end] {
            out.push(line.prefix);
            out.push_str(&line.text);
            out.push('\n');
        }
    }
    out
}

/// Generate a display-oriented numbered diff with context collapse.
#[must_use]
pub fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> DiffStringResult {
    let old_lines = split_lines_keep_trailing_empty(old_content);
    let new_lines = split_lines_keep_trailing_empty(new_content);
    let line_num_width = old_lines
        .len()
        .max(new_lines.len())
        .max(1)
        .to_string()
        .len();
    let parts = collapse_ops(&diff_ops(&old_lines, &new_lines));
    render_diff_parts(&parts, context_lines, line_num_width)
}

fn diff_ops(old_lines: &[&str], new_lines: &[&str]) -> Vec<Op> {
    let lcs = longest_common_subsequence(old_lines, new_lines);
    let mut ops = Vec::new();
    let mut old_index = 0;
    let mut new_index = 0;
    let mut common_index = 0;
    while old_index < old_lines.len() || new_index < new_lines.len() {
        if common_index < lcs.len()
            && old_index < old_lines.len()
            && new_index < new_lines.len()
            && old_index == lcs[common_index].0
            && new_index == lcs[common_index].1
        {
            ops.push(Op::Equal(old_lines[old_index].to_owned()));
            old_index += 1;
            new_index += 1;
            common_index += 1;
        } else if new_index < new_lines.len()
            && (common_index >= lcs.len() || new_index < lcs[common_index].1)
        {
            ops.push(Op::Added(new_lines[new_index].to_owned()));
            new_index += 1;
        } else if old_index < old_lines.len()
            && (common_index >= lcs.len() || old_index < lcs[common_index].0)
        {
            ops.push(Op::Removed(old_lines[old_index].to_owned()));
            old_index += 1;
        } else {
            break;
        }
    }
    ops
}

struct DiffRenderState {
    output: Vec<String>,
    old_line_num: usize,
    new_line_num: usize,
    last_was_change: bool,
    first_changed_line: Option<usize>,
}

impl DiffRenderState {
    fn new() -> Self {
        Self {
            output: Vec::new(),
            old_line_num: 1,
            new_line_num: 1,
            last_was_change: false,
            first_changed_line: None,
        }
    }

    fn push_numbered(&mut self, prefix: char, line: &str, width: usize) {
        let line_num = if prefix == '+' {
            self.new_line_num
        } else {
            self.old_line_num
        };
        self.output
            .push(format!("{prefix}{line_num:>width$} {line}"));
    }

    fn advance_both(&mut self, count: usize) {
        self.old_line_num += count;
        self.new_line_num += count;
    }

    fn push_gap(&mut self, width: usize) {
        self.output.push(format!(" {:>width$} ...", ""));
    }
}

fn render_diff_parts(
    parts: &[DiffPart],
    context_lines: usize,
    line_num_width: usize,
) -> DiffStringResult {
    let mut state = DiffRenderState::new();
    for (part_index, part) in parts.iter().enumerate() {
        match part {
            DiffPart::Change { added, removed } => {
                state.first_changed_line.get_or_insert(state.new_line_num);
                for line in removed {
                    state.push_numbered('-', line, line_num_width);
                    state.old_line_num += 1;
                }
                for line in added {
                    state.push_numbered('+', line, line_num_width);
                    state.new_line_num += 1;
                }
                state.last_was_change = true;
            }
            DiffPart::Equal(lines) => {
                let next_is_change = parts
                    .get(part_index + 1)
                    .is_some_and(|part| matches!(part, DiffPart::Change { .. }));
                render_equal_part(
                    &mut state,
                    lines,
                    context_lines,
                    line_num_width,
                    next_is_change,
                );
                state.last_was_change = false;
            }
        }
    }
    DiffStringResult {
        diff: state.output.join("\n"),
        first_changed_line: state.first_changed_line,
    }
}

fn render_equal_part(
    state: &mut DiffRenderState,
    lines: &[String],
    context_lines: usize,
    line_num_width: usize,
    next_is_change: bool,
) {
    match (state.last_was_change, next_is_change) {
        (true, true) if lines.len() <= context_lines * 2 => {
            push_context_lines(state, lines, line_num_width);
        }
        (true, true) => {
            push_context_lines(state, &lines[..context_lines], line_num_width);
            let skipped = lines.len() - context_lines * 2;
            state.push_gap(line_num_width);
            state.advance_both(skipped);
            push_context_lines(state, &lines[lines.len() - context_lines..], line_num_width);
        }
        (true, false) => {
            let shown = lines.len().min(context_lines);
            push_context_lines(state, &lines[..shown], line_num_width);
            let skipped = lines.len() - shown;
            if skipped > 0 {
                state.push_gap(line_num_width);
                state.advance_both(skipped);
            }
        }
        (false, true) => {
            let skipped = lines.len().saturating_sub(context_lines);
            if skipped > 0 {
                state.push_gap(line_num_width);
                state.advance_both(skipped);
            }
            push_context_lines(state, &lines[skipped..], line_num_width);
        }
        (false, false) => state.advance_both(lines.len()),
    }
}

fn push_context_lines(state: &mut DiffRenderState, lines: &[String], line_num_width: usize) {
    for line in lines {
        state.push_numbered(' ', line, line_num_width);
        state.advance_both(1);
    }
}

#[derive(Clone, Debug)]
enum Op {
    Equal(String),
    Added(String),
    Removed(String),
}

fn longest_common_subsequence(seq_a: &[&str], seq_b: &[&str]) -> Vec<(usize, usize)> {
    let len_a = seq_a.len();
    let len_b = seq_b.len();
    let mut dp = vec![vec![0usize; len_b + 1]; len_a + 1];
    for row in 0..len_a {
        for col in 0..len_b {
            if seq_a[row] == seq_b[col] {
                dp[row + 1][col + 1] = dp[row][col] + 1;
            } else {
                dp[row + 1][col + 1] = dp[row + 1][col].max(dp[row][col + 1]);
            }
        }
    }
    let mut out = Vec::new();
    let mut row = len_a;
    let mut col = len_b;
    while row > 0 && col > 0 {
        if seq_a[row - 1] == seq_b[col - 1] {
            out.push((row - 1, col - 1));
            row -= 1;
            col -= 1;
        } else if dp[row - 1][col] >= dp[row][col - 1] {
            row -= 1;
        } else {
            col -= 1;
        }
    }
    out.reverse();
    out
}

#[derive(Clone, Debug)]
enum DiffPart {
    Equal(Vec<String>),
    Change {
        removed: Vec<String>,
        added: Vec<String>,
    },
}

fn collapse_ops(ops: &[Op]) -> Vec<DiffPart> {
    let mut parts: Vec<DiffPart> = Vec::new();
    let mut idx = 0usize;
    while idx < ops.len() {
        match &ops[idx] {
            Op::Equal(s) => {
                let mut equal = vec![s.clone()];
                idx += 1;
                while idx < ops.len() {
                    if let Op::Equal(next) = &ops[idx] {
                        equal.push(next.clone());
                        idx += 1;
                    } else {
                        break;
                    }
                }
                parts.push(DiffPart::Equal(equal));
            }
            Op::Added(_) | Op::Removed(_) => {
                let mut removed = Vec::new();
                let mut added = Vec::new();
                while idx < ops.len() {
                    match &ops[idx] {
                        Op::Removed(s) => {
                            removed.push(s.clone());
                            idx += 1;
                        }
                        Op::Added(s) => {
                            added.push(s.clone());
                            idx += 1;
                        }
                        Op::Equal(_) => break,
                    }
                }
                parts.push(DiffPart::Change { removed, added });
            }
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn multi_edit_order_is_invariant(ids in prop::collection::vec(0_u8..32, 2..8)) {
            let original = ids.iter().fold(String::new(), |mut content, id| {
                content.push('[');
                content.push_str(&id.to_string());
                content.push(']');
                content
            });
            let edits = ids.iter().enumerate().map(|(index, id)| Edit {
                old_text: format!("[{id}]"),
                new_text: format!("[{}]", ids[(index + 1) % ids.len()]),
            }).collect::<Vec<_>>();
            prop_assume!(ids.iter().collect::<std::collections::HashSet<_>>().len() == ids.len());

            let forward = apply_edits_to_normalized_content(&original, &edits, "f.txt").map_err(TestCaseError::fail)?;
            let mut reversed = edits.clone();
            reversed.reverse();
            let backward = apply_edits_to_normalized_content(&original, &reversed, "f.txt").map_err(TestCaseError::fail)?;

            prop_assert_eq!(forward.new_content, backward.new_content);
        }

        #[test]
        fn overlapping_edits_are_rejected(length in 3_usize..24) {
            let original = "abcdefghijklmnopqrstuvw"[..length].to_owned();
            let first = original[..length - 1].to_owned();
            let second = original[1..].to_owned();
            let result = apply_edits_to_normalized_content(&original, &[
                Edit { old_text: first, new_text: "first".into() },
                Edit { old_text: second, new_text: "second".into() },
            ], "f.txt");

            let error = result.as_ref().err().map_or("", String::as_str);
            prop_assert!(result.is_err());
            prop_assert!(error.contains("overlap"));
        }

        #[test]
        fn line_endings_round_trip(lines in prop::collection::vec("[a-z]{0,8}", 2..12), crlf in any::<bool>()) {
            let ending = if crlf { LineEnding::Crlf } else { LineEnding::Lf };
            let content = lines.join(ending.as_str());
            let normalized = normalize_to_lf(&content);

            prop_assert_eq!(detect_line_ending(&content), ending);
            prop_assert_eq!(restore_line_endings(&normalized, ending), content);
        }

        #[test]
        fn fuzzy_matching_accepts_nfkc_and_smart_punctuation(
            word in "[A-Za-z0-9]{1,20}",
            single_quotes in prop::sample::select(vec!['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}']),
            double_quotes in prop::sample::select(vec!['\u{201C}', '\u{201D}', '\u{201E}', '\u{201F}']),
            dash in prop::sample::select(vec!['\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}']),
            space in prop::sample::select(vec!['\u{00A0}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}', '\u{200A}', '\u{202F}', '\u{205F}', '\u{3000}']),
        ) {
            let fullwidth = word.chars().map(|ch| char::from_u32(ch as u32 + 0xFEE0).unwrap_or(ch)).collect::<String>();
            let original = format!("{single_quotes}{fullwidth}{single_quotes} {double_quotes}{fullwidth}{double_quotes}{space}{dash}\n");
            let old_text = format!("'{word}' \"{word}\" -\n");
            let result = apply_edits_to_normalized_content(&original, &[Edit {
                old_text,
                new_text: "updated\n".into(),
            }], "f.txt").map_err(TestCaseError::fail)?;

            prop_assert_eq!(result.new_content, "updated\n");
        }
    }

    #[test]
    fn strip_bom_detects_prefix() {
        let (bom, text) = strip_bom("\u{FEFF}hello");
        assert_eq!(bom, "\u{FEFF}");
        assert_eq!(text, "hello");
        let (bom, text) = strip_bom("hello");
        assert_eq!(bom, "");
        assert_eq!(text, "hello");
    }

    #[test]
    fn detect_line_ending_prefers_first() {
        assert_eq!(detect_line_ending("a\r\nb\n"), LineEnding::Crlf);
        assert_eq!(detect_line_ending("a\nb\r\n"), LineEnding::Lf);
        assert_eq!(detect_line_ending("no newlines"), LineEnding::Lf);
    }

    #[test]
    fn normalize_and_restore_crlf() {
        assert_eq!(normalize_to_lf("a\r\nb\rc"), "a\nb\nc");
        assert_eq!(
            restore_line_endings("a\nb\n", LineEnding::Crlf),
            "a\r\nb\r\n"
        );
    }

    #[test]
    fn fuzzy_normalizes_quotes_dashes_spaces() {
        let input = "\u{2018}hi\u{2019} \u{2013} \u{00A0}x  ";
        let out = normalize_for_fuzzy_match(input);
        assert_eq!(out, "'hi' -  x");
    }

    #[test]
    fn exact_single_edit() -> Result<(), String> {
        let result = apply_edits_to_normalized_content(
            "Hello, world!",
            &[Edit {
                old_text: "world".into(),
                new_text: "testing".into(),
            }],
            "f.txt",
        )?;
        assert_eq!(result.new_content, "Hello, testing!");
        Ok(())
    }

    #[test]
    fn empty_old_text_rejected() -> Result<(), String> {
        let err = apply_edits_to_normalized_content(
            "abc",
            &[Edit {
                old_text: String::new(),
                new_text: "x".into(),
            }],
            "f.txt",
        )
        .err()
        .ok_or_else(|| "empty oldText was accepted".to_owned())?;
        assert_eq!(err, "oldText must not be empty in f.txt.");
        Ok(())
    }

    #[test]
    fn missing_text_errors() -> Result<(), String> {
        let err = apply_edits_to_normalized_content(
            "abc",
            &[Edit {
                old_text: "zzz".into(),
                new_text: "x".into(),
            }],
            "f.txt",
        )
        .err()
        .ok_or_else(|| "missing oldText was accepted".to_owned())?;
        assert!(err.contains("Could not find the exact text"));
        Ok(())
    }

    #[test]
    fn occurrence_count_errors() -> Result<(), String> {
        let err = apply_edits_to_normalized_content(
            "foo foo foo",
            &[Edit {
                old_text: "foo".into(),
                new_text: "bar".into(),
            }],
            "f.txt",
        )
        .err()
        .ok_or_else(|| "duplicate oldText was accepted".to_owned())?;
        assert!(err.contains("Found 3 occurrences"));
        Ok(())
    }

    #[test]
    fn multi_edit_reverse_and_original_coords() -> Result<(), String> {
        let result = apply_edits_to_normalized_content(
            "foo\nbar\nbaz\n",
            &[
                Edit {
                    old_text: "foo\n".into(),
                    new_text: "foo bar\n".into(),
                },
                Edit {
                    old_text: "bar\n".into(),
                    new_text: "BAR\n".into(),
                },
            ],
            "f.txt",
        )?;
        assert_eq!(result.new_content, "foo bar\nBAR\nbaz\n");
        Ok(())
    }

    #[test]
    fn overlap_rejected() -> Result<(), String> {
        let err = apply_edits_to_normalized_content(
            "one\ntwo\nthree\n",
            &[
                Edit {
                    old_text: "one\ntwo\n".into(),
                    new_text: "ONE\nTWO\n".into(),
                },
                Edit {
                    old_text: "two\nthree\n".into(),
                    new_text: "TWO\nTHREE\n".into(),
                },
            ],
            "f.txt",
        )
        .err()
        .ok_or_else(|| "overlapping edits were accepted".to_owned())?;
        assert!(err.contains("overlap"));
        Ok(())
    }

    #[test]
    fn no_op_rejected() -> Result<(), String> {
        let err = apply_edits_to_normalized_content(
            "same",
            &[Edit {
                old_text: "same".into(),
                new_text: "same".into(),
            }],
            "f.txt",
        )
        .err()
        .ok_or_else(|| "unchanged edit was accepted".to_owned())?;
        assert!(err.contains("No changes made"));
        Ok(())
    }

    #[test]
    fn fuzzy_preserves_untouched_trailing_whitespace() -> Result<(), String> {
        let original = "line one   \nline two  \nline three\n";
        let result = apply_edits_to_normalized_content(
            original,
            &[Edit {
                old_text: "line one\nline two\n".into(),
                new_text: "replaced\n".into(),
            }],
            "f.txt",
        )?;
        assert_eq!(result.new_content, "replaced\nline three\n");
        Ok(())
    }

    #[test]
    fn fuzzy_preserves_duplicate_line_bytes() -> Result<(), String> {
        let original = ["replace me   ", "after   ", ""].join("\n");
        let result = apply_edits_to_normalized_content(
            &original,
            &[Edit {
                old_text: "replace me\n".into(),
                new_text: "after\n".into(),
            }],
            "f.txt",
        )?;
        assert_eq!(result.new_content, ["after", "after   ", ""].join("\n"));
        Ok(())
    }

    #[test]
    fn unified_patch_contains_markers() {
        let patch = generate_unified_patch("a.txt", "Hello, world!", "Hello, testing!", 4);
        assert!(patch.contains("--- a.txt"));
        assert!(patch.contains("+++ a.txt"));
        assert!(patch.contains("@@"));
        assert!(patch.contains("-Hello, world!"));
        assert!(patch.contains("+Hello, testing!"));
    }

    #[test]
    fn unified_patch_splits_distant_changes_with_requested_context() {
        let old = (1..=12)
            .map(|number| format!("line {number}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut new_lines = (1..=12)
            .map(|number| format!("line {number}"))
            .collect::<Vec<_>>();
        new_lines[1] = "LINE 2".to_owned();
        new_lines[10] = "LINE 11".to_owned();
        let patch = generate_unified_patch("a.txt", &old, &new_lines.join("\n"), 1);

        assert_eq!(patch.matches("@@ ").count(), 2);
        assert!(patch.contains("@@ -1,3 +1,3 @@"));
        assert!(patch.contains("@@ -10,3 +10,3 @@"));
        assert!(patch.contains(" line 1\n-line 2\n+LINE 2\n line 3\n"));
        assert!(!patch.contains(" line 6\n"));
    }

    #[test]
    fn unified_patch_uses_zero_count_headers_for_insert_and_delete() {
        let insert = generate_unified_patch("a.txt", "one\ntwo\n", "one\nadded\ntwo\n", 0);
        assert!(insert.contains("@@ -1,0 +2,1 @@\n+added\n"));

        let delete = generate_unified_patch("a.txt", "one\nremoved\ntwo\n", "one\ntwo\n", 0);
        assert!(delete.contains("@@ -2,1 +1,0 @@\n-removed\n"));
    }

    #[test]
    fn display_diff_marks_first_changed_line() {
        let result = generate_diff_string("a\nb\nc\n", "a\nB\nc\n", 4);
        assert_eq!(result.first_changed_line, Some(2));
        assert!(result.diff.contains('B'));
    }
}
