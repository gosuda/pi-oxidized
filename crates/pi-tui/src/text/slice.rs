//! Truncation, column slicing, segment surgery, and cursor-marker helpers.

use unicode_segmentation::UnicodeSegmentation;

use super::ansi::{AnsiCodeTracker, extract_ansi_code};
use super::width::{grapheme_width, is_printable_ascii, visible_width};

/// Hardware-cursor APC marker emitted by focused inputs (`ESC _ pi:c BEL`).
pub const CURSOR_MARKER: &str = "\u{1b}_pi:c\u{7}";

/// Line-end reset that closes SGR and OSC 8 (BEL terminator).
pub const SEGMENT_RESET: &str = "\u{1b}[0m\u{1b}]8;;\u{7}";

const KITTY_PREFIX: &str = "\u{1b}_G";
const ITERM2_PREFIX: &str = "\u{1b}]1337;File=";

/// Result of [`slice_with_width`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceWithWidth {
    /// Extracted text (may contain ANSI).
    pub text: String,
    /// Visible width of `text`.
    pub width: usize,
}

/// Result of [`extract_segments`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedSegments {
    /// Content strictly before the hole.
    pub before: String,
    /// Visible width of `before`.
    pub before_width: usize,
    /// Content after the hole, with inherited active styles prepended.
    pub after: String,
    /// Visible width of `after`.
    pub after_width: usize,
}

/// `true` when a line is a Kitty / iTerm2 image sequence (possibly mid-line).
#[must_use]
pub fn is_image_line(line: &str) -> bool {
    if line.starts_with(KITTY_PREFIX) || line.starts_with(ITERM2_PREFIX) {
        return true;
    }
    line.contains(KITTY_PREFIX) || line.contains(ITERM2_PREFIX)
}

fn truncate_fragment_to_width(text: &str, max_width: usize) -> SliceWithWidth {
    if max_width == 0 || text.is_empty() {
        return SliceWithWidth {
            text: String::new(),
            width: 0,
        };
    }
    if is_printable_ascii(text) {
        let clipped = text.chars().take(max_width).collect::<String>();
        // For pure ASCII, char count == width and == byte length for printable.
        let clipped = if text.is_ascii() {
            text.get(..max_width.min(text.len()))
                .unwrap_or("")
                .to_owned()
        } else {
            clipped
        };
        let width = clipped.len();
        return SliceWithWidth {
            text: clipped,
            width,
        };
    }

    let has_ansi = text.contains('\u{1b}');
    let has_tabs = text.contains('\t');
    if !has_ansi && !has_tabs {
        let mut result = String::new();
        let mut width = 0usize;
        for segment in text.graphemes(true) {
            let w = grapheme_width(segment);
            if width.saturating_add(w) > max_width {
                break;
            }
            result.push_str(segment);
            width = width.saturating_add(w);
        }
        return SliceWithWidth {
            text: result,
            width,
        };
    }

    let mut result = String::new();
    let mut width = 0usize;
    let mut i = 0usize;
    let mut pending_ansi = String::new();

    while i < text.len() {
        if let Some(ansi) = extract_ansi_code(text, i) {
            pending_ansi.push_str(ansi.code);
            i += ansi.len;
            continue;
        }
        if text.as_bytes().get(i) == Some(&b'\t') {
            if width.saturating_add(3) > max_width {
                break;
            }
            if !pending_ansi.is_empty() {
                result.push_str(&pending_ansi);
                pending_ansi.clear();
            }
            result.push('\t');
            width = width.saturating_add(3);
            i += 1;
            continue;
        }

        let mut end = i;
        while end < text.len() && text.as_bytes().get(end) != Some(&b'\t') {
            if extract_ansi_code(text, end).is_some() {
                break;
            }
            end += text[end..].chars().next().map_or(1, char::len_utf8);
        }

        for segment in text[i..end].graphemes(true) {
            let w = grapheme_width(segment);
            if width.saturating_add(w) > max_width {
                return SliceWithWidth {
                    text: result,
                    width,
                };
            }
            if !pending_ansi.is_empty() {
                result.push_str(&pending_ansi);
                pending_ansi.clear();
            }
            result.push_str(segment);
            width = width.saturating_add(w);
        }
        i = end;
    }

    SliceWithWidth {
        text: result,
        width,
    }
}

fn finalize_truncated_result(
    prefix: &str,
    prefix_width: usize,
    ellipsis: &str,
    ellipsis_width: usize,
    max_width: usize,
    pad: bool,
) -> String {
    let reset = "\u{1b}[0m";
    let visible = prefix_width.saturating_add(ellipsis_width);
    let mut result = if ellipsis.is_empty() {
        format!("{prefix}{reset}")
    } else {
        format!("{prefix}{reset}{ellipsis}{reset}")
    };
    if pad {
        let pad_n = max_width.saturating_sub(visible);
        result.push_str(&" ".repeat(pad_n));
    }
    result
}

struct TruncationScan {
    prefix: String,
    kept_width: usize,
    visible_width: usize,
    overflowed: bool,
    exhausted: bool,
}

fn scan_truncation(text: &str, target_width: usize, max_width: usize) -> TruncationScan {
    let mut prefix = String::new();
    let mut pending_ansi = String::new();
    let mut visible_width = 0usize;
    let mut kept_width = 0usize;
    let mut keep_prefix = true;
    let mut overflowed = false;

    if !text.contains('\u{1b}') && !text.contains('\t') {
        for segment in text.graphemes(true) {
            let width = grapheme_width(segment);
            if keep_prefix && kept_width.saturating_add(width) <= target_width {
                prefix.push_str(segment);
                kept_width = kept_width.saturating_add(width);
            } else {
                keep_prefix = false;
            }
            visible_width = visible_width.saturating_add(width);
            if visible_width > max_width {
                overflowed = true;
                break;
            }
        }
        return TruncationScan {
            prefix,
            kept_width,
            visible_width,
            overflowed,
            exhausted: !overflowed,
        };
    }

    let mut index = 0usize;
    while index < text.len() {
        if let Some(ansi) = extract_ansi_code(text, index) {
            pending_ansi.push_str(ansi.code);
            index += ansi.len;
            continue;
        }
        if text.as_bytes().get(index) == Some(&b'\t') {
            keep_tab_prefix(
                &mut prefix,
                &mut pending_ansi,
                &mut kept_width,
                &mut keep_prefix,
                target_width,
            );
            visible_width = visible_width.saturating_add(3);
            if visible_width > max_width {
                overflowed = true;
                break;
            }
            index += 1;
            continue;
        }

        let end = next_escape_or_tab(text, index);
        for segment in text[index..end].graphemes(true) {
            let width = grapheme_width(segment);
            keep_grapheme_prefix(
                segment,
                width,
                &mut prefix,
                &mut pending_ansi,
                &mut kept_width,
                &mut keep_prefix,
                target_width,
            );
            visible_width = visible_width.saturating_add(width);
            if visible_width > max_width {
                overflowed = true;
                break;
            }
        }
        if overflowed {
            break;
        }
        index = end;
    }
    TruncationScan {
        prefix,
        kept_width,
        visible_width,
        overflowed,
        exhausted: index >= text.len(),
    }
}

fn next_escape_or_tab(text: &str, start: usize) -> usize {
    let mut end = start;
    while end < text.len() && text.as_bytes().get(end) != Some(&b'\t') {
        if extract_ansi_code(text, end).is_some() {
            break;
        }
        end += text[end..].chars().next().map_or(1, char::len_utf8);
    }
    end
}

fn keep_tab_prefix(
    prefix: &mut String,
    pending_ansi: &mut String,
    kept_width: &mut usize,
    keep_prefix: &mut bool,
    target_width: usize,
) {
    if *keep_prefix && kept_width.saturating_add(3) <= target_width {
        prefix.push_str(pending_ansi);
        pending_ansi.clear();
        prefix.push('\t');
        *kept_width = kept_width.saturating_add(3);
    } else {
        *keep_prefix = false;
        pending_ansi.clear();
    }
}

fn keep_grapheme_prefix(
    grapheme: &str,
    width: usize,
    prefix: &mut String,
    pending_ansi: &mut String,
    kept_width: &mut usize,
    keep_prefix: &mut bool,
    target_width: usize,
) {
    if *keep_prefix && kept_width.saturating_add(width) <= target_width {
        prefix.push_str(pending_ansi);
        pending_ansi.clear();
        prefix.push_str(grapheme);
        *kept_width = kept_width.saturating_add(width);
    } else {
        *keep_prefix = false;
        pending_ansi.clear();
    }
}

/// Truncate `text` to `max_width` visible columns, optionally padding.
///
/// When truncation occurs, active SGR is closed with `\x1b[0m` around the
/// ellipsis so styles never leak into following content.
#[must_use]
pub fn truncate_to_width(text: &str, max_width: usize, ellipsis: &str, pad: bool) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.is_empty() {
        return if pad {
            " ".repeat(max_width)
        } else {
            String::new()
        };
    }

    let ellipsis_width = visible_width(ellipsis);
    if ellipsis_width >= max_width {
        let text_width = visible_width(text);
        if text_width <= max_width {
            return if pad {
                format!("{text}{}", " ".repeat(max_width - text_width))
            } else {
                text.to_owned()
            };
        }
        let clipped = truncate_fragment_to_width(ellipsis, max_width);
        if clipped.width == 0 {
            return if pad {
                " ".repeat(max_width)
            } else {
                String::new()
            };
        }
        return finalize_truncated_result("", 0, &clipped.text, clipped.width, max_width, pad);
    }

    if is_printable_ascii(text) {
        if text.len() <= max_width {
            return if pad {
                format!("{text}{}", " ".repeat(max_width - text.len()))
            } else {
                text.to_owned()
            };
        }
        let target_width = max_width - ellipsis_width;
        let prefix = text.get(..target_width).unwrap_or("").to_owned();
        return finalize_truncated_result(
            &prefix,
            target_width,
            ellipsis,
            ellipsis_width,
            max_width,
            pad,
        );
    }

    let target_width = max_width - ellipsis_width;
    let scan = scan_truncation(text, target_width, max_width);

    if !scan.overflowed && scan.exhausted {
        return if pad {
            format!(
                "{text}{}",
                " ".repeat(max_width.saturating_sub(scan.visible_width))
            )
        } else {
            text.to_owned()
        };
    }

    finalize_truncated_result(
        &scan.prefix,
        scan.kept_width,
        ellipsis,
        ellipsis_width,
        max_width,
        pad,
    )
}

/// Marker used by visible text truncation throughout the TUI.
pub const TRUNCATION_MARKER: &str = "…";

/// Truncate `text` to `max_width` visible columns, marking any omitted cells.
///
/// The marker consumes one cell from the budget only when the input does not
/// fit. Exact-fit text is returned unchanged.
#[must_use]
pub fn truncate_with_marker(text: &str, max_width: usize, pad: bool) -> String {
    truncate_to_width(text, max_width, TRUNCATION_MARKER, pad)
}

/// Extract `length` visible columns starting at `start_col`.
#[must_use]
pub fn slice_by_column(line: &str, start_col: usize, length: usize, strict: bool) -> String {
    slice_with_width(line, start_col, length, strict).text
}

/// Like [`slice_by_column`] but also returns the actual visible width.
#[must_use]
pub fn slice_with_width(
    line: &str,
    start_col: usize,
    length: usize,
    strict: bool,
) -> SliceWithWidth {
    if length == 0 {
        return SliceWithWidth {
            text: String::new(),
            width: 0,
        };
    }
    let end_col = start_col.saturating_add(length);
    let mut result = String::new();
    let mut result_width = 0usize;
    let mut current_col = 0usize;
    let mut i = 0usize;
    let mut pending_ansi = String::new();

    while i < line.len() {
        if let Some(ansi) = extract_ansi_code(line, i) {
            if current_col >= start_col && current_col < end_col {
                result.push_str(ansi.code);
            } else if current_col < start_col {
                pending_ansi.push_str(ansi.code);
            }
            i += ansi.len;
            continue;
        }

        let mut text_end = i;
        while text_end < line.len() && extract_ansi_code(line, text_end).is_none() {
            text_end += line[text_end..].chars().next().map_or(1, char::len_utf8);
        }

        for segment in line[i..text_end].graphemes(true) {
            let w = grapheme_width(segment);
            let in_range = current_col >= start_col && current_col < end_col;
            let fits = !strict || current_col.saturating_add(w) <= end_col;
            if in_range && fits {
                if !pending_ansi.is_empty() {
                    result.push_str(&pending_ansi);
                    pending_ansi.clear();
                }
                result.push_str(segment);
                result_width = result_width.saturating_add(w);
            }
            current_col = current_col.saturating_add(w);
            if current_col >= end_col {
                break;
            }
        }
        i = text_end;
        if current_col >= end_col {
            break;
        }
    }

    SliceWithWidth {
        text: result,
        width: result_width,
    }
}

/// Extract before/after segments for overlay compositing in one pass.
#[must_use]
pub fn extract_segments(
    line: &str,
    before_end: usize,
    after_start: usize,
    after_len: usize,
    strict_after: bool,
) -> ExtractedSegments {
    let mut before = String::new();
    let mut before_width = 0usize;
    let mut after = String::new();
    let mut after_width = 0usize;
    let mut current_col = 0usize;
    let mut i = 0usize;
    let mut pending_ansi_before = String::new();
    let mut after_started = false;
    let after_end = after_start.saturating_add(after_len);
    let mut style_tracker = AnsiCodeTracker::new();

    while i < line.len() {
        if let Some(ansi) = extract_ansi_code(line, i) {
            style_tracker.process(ansi.code);
            if current_col < before_end {
                pending_ansi_before.push_str(ansi.code);
            } else if current_col >= after_start && current_col < after_end && after_started {
                after.push_str(ansi.code);
            }
            i += ansi.len;
            continue;
        }

        let mut text_end = i;
        while text_end < line.len() && extract_ansi_code(line, text_end).is_none() {
            text_end += line[text_end..].chars().next().map_or(1, char::len_utf8);
        }

        for segment in line[i..text_end].graphemes(true) {
            let w = grapheme_width(segment);
            if current_col < before_end && current_col.saturating_add(w) <= before_end {
                if !pending_ansi_before.is_empty() {
                    before.push_str(&pending_ansi_before);
                    pending_ansi_before.clear();
                }
                before.push_str(segment);
                before_width = before_width.saturating_add(w);
            } else if current_col >= after_start && current_col < after_end {
                let fits = !strict_after || current_col.saturating_add(w) <= after_end;
                if fits {
                    if !after_started {
                        after.push_str(&style_tracker.get_active_codes());
                        after_started = true;
                    }
                    after.push_str(segment);
                    after_width = after_width.saturating_add(w);
                }
            }
            current_col = current_col.saturating_add(w);
            if if after_len == 0 {
                current_col >= before_end
            } else {
                current_col >= after_end
            } {
                break;
            }
        }
        i = text_end;
        if if after_len == 0 {
            current_col >= before_end
        } else {
            current_col >= after_end
        } {
            break;
        }
    }

    ExtractedSegments {
        before,
        before_width,
        after,
        after_width,
    }
}

/// Splice overlay content into a base line at `start_col`.
///
/// Ports `TUI.compositeLineAt`. Image base lines are returned unchanged.
#[must_use]
pub fn composite_line_at(
    base_line: &str,
    overlay_line: &str,
    start_col: usize,
    overlay_width: usize,
    total_width: usize,
) -> String {
    if is_image_line(base_line) {
        return base_line.to_owned();
    }

    let after_start = start_col.saturating_add(overlay_width);
    let after_len = total_width.saturating_sub(after_start);
    let base = extract_segments(base_line, start_col, after_start, after_len, true);
    let overlay = slice_with_width(overlay_line, 0, overlay_width, true);

    let before_pad = start_col.saturating_sub(base.before_width);
    let overlay_pad = overlay_width.saturating_sub(overlay.width);
    let actual_before_width = start_col.max(base.before_width);
    let actual_overlay_width = overlay_width.max(overlay.width);
    let after_target = total_width
        .saturating_sub(actual_before_width)
        .saturating_sub(actual_overlay_width);
    let after_pad = after_target.saturating_sub(base.after_width);

    let mut result = String::with_capacity(
        base.before.len()
            + overlay.text.len()
            + base.after.len()
            + before_pad
            + overlay_pad
            + after_pad
            + SEGMENT_RESET.len() * 2,
    );
    result.push_str(&base.before);
    result.push_str(&" ".repeat(before_pad));
    result.push_str(SEGMENT_RESET);
    result.push_str(&overlay.text);
    result.push_str(&" ".repeat(overlay_pad));
    result.push_str(SEGMENT_RESET);
    result.push_str(&base.after);
    result.push_str(&" ".repeat(after_pad));

    let result_width = visible_width(&result);
    if result_width <= total_width {
        result
    } else {
        slice_by_column(&result, 0, total_width, true)
    }
}

/// Locate the first `CURSOR_MARKER` and report its visual column.
///
/// Returns `(byte_index, visual_col)` or `None`.
#[must_use]
pub fn find_cursor_marker(line: &str) -> Option<(usize, usize)> {
    let idx = line.find(CURSOR_MARKER)?;
    let col = visible_width(&line[..idx]);
    Some((idx, col))
}

/// Strip every `CURSOR_MARKER` from `line`.
#[must_use]
pub fn strip_cursor_marker(line: &str) -> String {
    line.replace(CURSOR_MARKER, "")
}

/// Find the cursor marker in the bottom `height` lines, strip it, and return
/// viewport-relative `{row, col}` (row 0 = top of the full `lines` buffer).
///
/// Searches from the bottom of the viewport upward, matching TS behavior.
#[must_use]
pub fn extract_cursor_marker(lines: &mut [String], height: usize) -> Option<(usize, usize)> {
    if lines.is_empty() || height == 0 {
        return None;
    }
    let viewport_top = lines.len().saturating_sub(height);
    for row in (viewport_top..lines.len()).rev() {
        if let Some((idx, col)) = find_cursor_marker(&lines[row]) {
            let line = &lines[row];
            let mut stripped = String::with_capacity(line.len());
            stripped.push_str(&line[..idx]);
            stripped.push_str(&line[idx + CURSOR_MARKER.len()..]);
            lines[row] = stripped;
            return Some((row, col));
        }
    }
    None
}
