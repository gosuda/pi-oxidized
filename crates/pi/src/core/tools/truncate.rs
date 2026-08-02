//! Shared truncation utilities for tool outputs.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/tools/truncate.ts`.
//! Truncation is based on two independent limits — whichever is hit first
//! wins: a line limit (default [`DEFAULT_MAX_LINES`]) and a byte limit
//! (default [`DEFAULT_MAX_BYTES`]). Never returns partial lines, except the
//! bash tail-truncation edge case where the final line alone exceeds the byte
//! limit.

use serde::Deserialize;
use serde::Serialize;

/// Default maximum number of lines kept by [`truncate_head`] /
/// [`truncate_tail`] (TypeScript `DEFAULT_MAX_LINES`).
pub const DEFAULT_MAX_LINES: usize = 2000;

/// Default maximum number of UTF-8 bytes kept by [`truncate_head`] /
/// [`truncate_tail`] (TypeScript `DEFAULT_MAX_BYTES`, 50 KiB).
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// Maximum characters per grep match line before [`truncate_line`] appends
/// the `[truncated]` suffix (TypeScript `GREP_MAX_LINE_LENGTH`).
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// Which truncation limit was hit (TypeScript `"lines" | "bytes"`).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TruncatedBy {
    /// The line limit was hit.
    Lines,
    /// The byte limit was hit.
    Bytes,
}

/// Result of [`truncate_head`] / [`truncate_tail`] (TypeScript
/// `TruncationResult`). Field order and camelCase wire names match the
/// TypeScript object exactly so tool `details.truncation` payloads stay
/// wire-compatible.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationResult {
    /// The truncated content.
    pub content: String,
    /// Whether truncation occurred.
    pub truncated: bool,
    /// Which limit was hit, or `None` (JSON `null`) when not truncated.
    pub truncated_by: Option<TruncatedBy>,
    /// Total number of lines in the original content.
    pub total_lines: usize,
    /// Total number of UTF-8 bytes in the original content.
    pub total_bytes: usize,
    /// Number of complete lines in the truncated output.
    pub output_lines: usize,
    /// Number of UTF-8 bytes in the truncated output.
    pub output_bytes: usize,
    /// Whether the last line was partially truncated (only for the tail
    /// truncation edge case).
    pub last_line_partial: bool,
    /// Whether the first line exceeded the byte limit (for head truncation).
    pub first_line_exceeds_limit: bool,
    /// The max lines limit that was applied.
    pub max_lines: usize,
    /// The max bytes limit that was applied.
    pub max_bytes: usize,
}

/// Optional limits for [`truncate_head`] / [`truncate_tail`] (TypeScript
/// `TruncationOptions`). `None` falls back to [`DEFAULT_MAX_LINES`] /
/// [`DEFAULT_MAX_BYTES`].
#[derive(Clone, Copy, Debug, Default)]
pub struct TruncationOptions {
    /// Maximum number of lines (default: [`DEFAULT_MAX_LINES`]).
    pub max_lines: Option<usize>,
    /// Maximum number of UTF-8 bytes (default: [`DEFAULT_MAX_BYTES`]).
    pub max_bytes: Option<usize>,
}

impl TruncationOptions {
    fn limits(self) -> (usize, usize) {
        (
            self.max_lines.unwrap_or(DEFAULT_MAX_LINES),
            self.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
        )
    }
}

/// Split for line counting: `content.split("\n")` with the trailing empty
/// entry popped when the content ends with a newline. Empty content yields no
/// lines at all (TypeScript `splitLinesForCounting`).
fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Format bytes as a human-readable size (TypeScript `formatSize`).
///
/// Mirrors `Number.prototype.toFixed(1)` rounding, which breaks exact ties
/// toward the larger value (e.g. 1280 bytes is `"1.3KB"`, not `"1.2KB"`), by
/// computing the rounded tenths with integer arithmetic.
#[must_use]
pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format_scaled(bytes, 1024, "KB")
    } else {
        format_scaled(bytes, 1024 * 1024, "MB")
    }
}

/// Round `bytes / unit` to one decimal place with ties rounding up, matching
/// `toFixed(1)` without floating-point intermediate error.
fn format_scaled(bytes: u64, unit: u64, suffix: &str) -> String {
    let tenths = bytes.saturating_mul(10).saturating_add(unit / 2) / unit;
    format!("{}.{}{}", tenths / 10, tenths % 10, suffix)
}

/// Truncate content from the head, keeping the first N lines/bytes (file-read
/// style, TypeScript `truncateHead`).
///
/// Never returns partial lines. When the first line alone exceeds the byte
/// limit, returns empty content with `first_line_exceeds_limit` set.
#[must_use]
pub fn truncate_head(content: &str, options: TruncationOptions) -> TruncationResult {
    let (max_lines, max_bytes) = options.limits();

    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_owned(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    let first_line_bytes = lines.first().map_or(0, |line| line.len());
    if first_line_bytes > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    // Collect complete lines that fit. Each line after the first carries one
    // extra byte for the joining newline.
    let mut output_lines: Vec<&str> = Vec::new();
    let mut output_bytes = 0_usize;
    let mut truncated_by = TruncatedBy::Lines;

    for (index, line) in lines.iter().enumerate().take(max_lines) {
        let line_bytes = line.len() + usize::from(index > 0);
        if output_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        output_lines.push(line);
        output_bytes += line_bytes;
    }

    // Exiting by hitting the line limit (not a byte break) reports "lines".
    if output_lines.len() >= max_lines && output_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let content = output_lines.join("\n");
    let final_output_bytes = content.len();

    TruncationResult {
        output_lines: output_lines.len(),
        content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_bytes: final_output_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate content from the tail, keeping the last N lines/bytes (bash
/// style, TypeScript `truncateTail`).
///
/// May return a partial first line when the last line of the original content
/// alone exceeds the byte limit; the suffix is cut on a UTF-8 character
/// boundary and flagged via `last_line_partial`.
#[must_use]
pub fn truncate_tail(content: &str, options: TruncationOptions) -> TruncationResult {
    let (max_lines, max_bytes) = options.limits();

    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_owned(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    // Walk backwards from the end, prepending lines that fit. Kept lines are
    // pushed in reverse order and flipped before joining.
    let mut output_lines: Vec<String> = Vec::new();
    let mut output_bytes = 0_usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;

    for line in lines.iter().rev() {
        if output_lines.len() >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(!output_lines.is_empty());
        if output_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            // Edge case: no line kept yet and this (last) line alone exceeds
            // maxBytes — take a UTF-8-safe suffix of it.
            if output_lines.is_empty() {
                let truncated_line = truncate_string_to_bytes_from_end(line, max_bytes);
                output_bytes = truncated_line.len();
                output_lines.push(truncated_line.to_owned());
                last_line_partial = true;
            }
            break;
        }
        output_lines.push((*line).to_owned());
        output_bytes += line_bytes;
    }

    // Exiting by hitting the line limit (not a byte break) reports "lines".
    if output_lines.len() >= max_lines && output_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    output_lines.reverse();
    let content = output_lines.join("\n");
    let final_output_bytes = content.len();

    TruncationResult {
        output_lines: output_lines.len(),
        content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_bytes: final_output_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate a string to fit within a byte limit, keeping the end
/// (TypeScript `truncateStringToBytesFromEnd`). Multi-byte UTF-8 characters
/// are never split: continuation bytes at the cut point are skipped forward
/// to the next character boundary.
fn truncate_string_to_bytes_from_end(content: &str, max_bytes: usize) -> &str {
    let bytes = content.as_bytes();
    if bytes.len() <= max_bytes {
        return content;
    }

    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
        start += 1;
    }
    // `start` sits on a character boundary by construction; `get` keeps this
    // panic-free even if the input were not well-formed.
    content.get(start..).unwrap_or(content)
}

/// Outcome of [`truncate_line`] / [`truncate_line_with`] (TypeScript
/// `{ text, wasTruncated }`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TruncatedLine {
    /// The possibly truncated line text.
    pub text: String,
    /// Whether the line exceeded the limit and was shortened.
    pub was_truncated: bool,
}

/// Truncate a single line to [`GREP_MAX_LINE_LENGTH`] characters, adding a
/// `... [truncated]` suffix (TypeScript `truncateLine` with its default
/// limit). Used for grep match lines.
#[must_use]
pub fn truncate_line(line: &str) -> TruncatedLine {
    truncate_line_with(line, GREP_MAX_LINE_LENGTH)
}

/// Truncate a single line to `max_chars` characters.
///
/// The limit is measured in UTF-16 code units to match JavaScript
/// `String.length` / `String.slice`. The cut itself lands on a whole
/// character: for BMP text this is byte-for-byte identical to the TypeScript
/// behavior, and for an astral character straddling the limit the partial
/// character is dropped instead of emitting the lone surrogate JavaScript
/// would produce (a Rust `String` cannot hold lone surrogates).
#[must_use]
pub fn truncate_line_with(line: &str, max_chars: usize) -> TruncatedLine {
    if line.encode_utf16().count() <= max_chars {
        return TruncatedLine {
            text: line.to_owned(),
            was_truncated: false,
        };
    }

    let mut units = 0_usize;
    let mut end = 0_usize;
    for (index, ch) in line.char_indices() {
        let width = ch.len_utf16();
        if units + width > max_chars {
            break;
        }
        units += width;
        end = index + ch.len_utf8();
    }

    TruncatedLine {
        text: format!("{}... [truncated]", &line[..end]),
        was_truncated: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn head(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
        truncate_head(
            content,
            TruncationOptions {
                max_lines: Some(max_lines),
                max_bytes: Some(max_bytes),
            },
        )
    }

    fn tail(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
        truncate_tail(
            content,
            TruncationOptions {
                max_lines: Some(max_lines),
                max_bytes: Some(max_bytes),
            },
        )
    }

    proptest! {
        #[test]
        fn head_truncation_respects_byte_and_line_limits(
            lines in prop::collection::vec("(?:[a-z]|é|界){1,12}", 0..16),
            max_lines in 0_usize..16,
            max_bytes in 1_usize..96,
        ) {
            let content = lines.join("\n");
            let result = truncate_head(&content, TruncationOptions {
                max_lines: Some(max_lines),
                max_bytes: Some(max_bytes),
            });

            prop_assert_eq!(result.output_bytes, result.content.len());
            prop_assert_eq!(result.output_lines, split_lines_for_counting(&result.content).len());
            prop_assert!(result.output_bytes <= max_bytes);
            prop_assert!(result.output_lines <= max_lines);
            prop_assert!(content.starts_with(&result.content));
        }

        #[test]
        fn tail_truncation_respects_byte_and_line_limits(
            lines in prop::collection::vec("(?:[a-z]|é|界){1,12}", 0..16),
            max_lines in 0_usize..16,
            max_bytes in 1_usize..96,
        ) {
            let content = lines.join("\n");
            let result = truncate_tail(&content, TruncationOptions {
                max_lines: Some(max_lines),
                max_bytes: Some(max_bytes),
            });

            prop_assert_eq!(result.output_bytes, result.content.len());
            prop_assert!(result.output_bytes <= max_bytes);
            prop_assert!(result.output_lines <= max_lines);
            prop_assert!(content.ends_with(&result.content));
        }
    }

    #[test]
    fn format_size_matches_js_to_fixed() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(1023), "1023B");
        assert_eq!(format_size(1024), "1.0KB");
        // Exact .05 tie: JS toFixed(1) picks the larger candidate.
        assert_eq!(format_size(1280), "1.3KB");
        assert_eq!(format_size(1536), "1.5KB");
        assert_eq!(format_size(1024 * 1024 - 1), "1024.0KB");
        assert_eq!(format_size(1024 * 1024), "1.0MB");
        assert_eq!(format_size(1024 * 1024 * 3 / 2), "1.5MB");
    }

    #[test]
    fn head_returns_content_unchanged_when_within_limits() {
        let result = truncate_head("hello\nworld\n", TruncationOptions::default());
        assert!(!result.truncated);
        assert_eq!(result.truncated_by, None);
        assert_eq!(result.content, "hello\nworld\n");
        assert_eq!(result.total_lines, 2);
        assert_eq!(result.total_bytes, 12);
        assert_eq!(result.output_lines, 2);
        assert_eq!(result.output_bytes, 12);
        assert!(!result.last_line_partial);
        assert!(!result.first_line_exceeds_limit);
        assert_eq!(result.max_lines, DEFAULT_MAX_LINES);
        assert_eq!(result.max_bytes, DEFAULT_MAX_BYTES);
    }

    #[test]
    fn empty_content_is_not_truncated() {
        let result = truncate_head("", TruncationOptions::default());
        assert!(!result.truncated);
        assert_eq!(result.total_lines, 0);
        assert_eq!(result.output_lines, 0);
        assert_eq!(result.content, "");
    }

    #[test]
    fn head_hits_exact_default_line_limit() {
        let content = (0..=2000)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_head(&content, TruncationOptions::default());
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(result.total_lines, 2001);
        assert_eq!(result.output_lines, 2000);
        assert_eq!(
            result.content,
            (0..2000)
                .map(|i| format!("line{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(!result.content.ends_with('\n'));
    }

    #[test]
    fn head_byte_limit_boundary_is_exact() {
        // 7 bytes total: fits exactly at maxBytes=7.
        let fits = head("abc\ndef", 10, 7);
        assert!(!fits.truncated);
        assert_eq!(fits.content, "abc\ndef");

        // 6 bytes: "def" plus its joining newline (4 bytes) no longer fits.
        let cut = head("abc\ndef", 10, 6);
        assert!(cut.truncated);
        assert_eq!(cut.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(cut.content, "abc");
        assert_eq!(cut.output_lines, 1);
        assert_eq!(cut.output_bytes, 3);
        assert_eq!(cut.total_lines, 2);
        assert_eq!(cut.total_bytes, 7);
    }

    #[test]
    fn head_includes_line_landing_exactly_on_byte_limit() {
        let result = head("ab\ncd\nef", 10, 5);
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(result.content, "ab\ncd");
        assert_eq!(result.output_bytes, 5);
        assert_eq!(result.output_lines, 2);
    }

    #[test]
    fn head_first_line_exceeding_byte_limit_returns_empty() {
        let result = head("abcdefgh\nx", 10, 4);
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert!(result.first_line_exceeds_limit);
        assert_eq!(result.content, "");
        assert_eq!(result.output_lines, 0);
        assert_eq!(result.output_bytes, 0);
        assert_eq!(result.total_lines, 2);
    }

    #[test]
    fn head_never_returns_partial_multibyte_line() {
        // "éé" is 4 bytes and fits; the next line cannot, so the output must
        // end on the whole character rather than a split sequence.
        let result = head("éé\nyyyy", 10, 4);
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(result.content, "éé");
        assert_eq!(result.output_bytes, 4);
    }

    #[test]
    fn head_reports_lines_when_line_limit_breaks_first() {
        let result = head("a\nb\nc\nd", 2, 1024);
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(result.content, "a\nb");
        assert_eq!(result.output_lines, 2);
    }

    #[test]
    fn tail_keeps_last_lines() {
        let result = tail("l0\nl1\nl2", 2, 1024);
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(result.content, "l1\nl2");
        assert_eq!(result.total_lines, 3);
        assert_eq!(result.output_lines, 2);
        assert!(!result.last_line_partial);
    }

    #[test]
    fn tail_byte_limit_walks_backwards() {
        let result = tail("aaaa\nbbbb\ncccc", 10, 5);
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(result.content, "cccc");
        assert_eq!(result.output_bytes, 4);
        assert!(!result.last_line_partial);
    }

    #[test]
    fn tail_includes_line_landing_exactly_on_byte_limit() {
        let result = tail("ab\ncd\nef", 10, 5);
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(result.content, "cd\nef");
        assert_eq!(result.output_bytes, 5);
        assert!(!result.last_line_partial);
    }

    #[test]
    fn tail_partial_line_is_utf8_safe_suffix() {
        // "aéé" is 5 bytes; keeping 3 lands on a continuation byte of the
        // first é, which must be skipped to the next character boundary.
        let result = tail("aéé", 10, 3);
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
        assert!(result.last_line_partial);
        assert_eq!(result.content, "é");
        assert_eq!(result.output_bytes, 2);
        assert_eq!(result.output_lines, 1);
        assert_eq!(result.total_bytes, 5);
    }

    #[test]
    fn tail_partial_line_on_boundary_keeps_full_characters() {
        // 4 bytes lands exactly on a character boundary: "éé" survives whole.
        let result = tail("aéé", 10, 4);
        assert!(result.truncated);
        assert!(result.last_line_partial);
        assert_eq!(result.content, "éé");
        assert_eq!(result.output_bytes, 4);
    }

    #[test]
    fn tail_line_limit_break_reports_lines() {
        let result = tail("a\nb\nc", 1, 1024);
        assert!(result.truncated);
        assert_eq!(result.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(result.content, "c");
        assert!(!result.last_line_partial);
    }

    #[test]
    fn trailing_newline_does_not_count_extra_line() {
        let result = truncate_tail("a\n\n", TruncationOptions::default());
        assert!(!result.truncated);
        assert_eq!(result.total_lines, 2);
    }

    #[test]
    fn truncate_line_keeps_short_lines() {
        let short = "x".repeat(GREP_MAX_LINE_LENGTH);
        let result = truncate_line(&short);
        assert!(!result.was_truncated);
        assert_eq!(result.text, short);
    }

    #[test]
    fn truncate_line_appends_suffix_over_limit() {
        let long = "x".repeat(GREP_MAX_LINE_LENGTH + 1);
        let result = truncate_line(&long);
        assert!(result.was_truncated);
        assert_eq!(
            result.text,
            format!("{}... [truncated]", "x".repeat(GREP_MAX_LINE_LENGTH))
        );
    }

    #[test]
    fn truncate_line_counts_bmp_characters() {
        let long = "é".repeat(GREP_MAX_LINE_LENGTH + 1);
        let result = truncate_line(&long);
        assert!(result.was_truncated);
        assert_eq!(
            result.text,
            format!("{}... [truncated]", "é".repeat(GREP_MAX_LINE_LENGTH))
        );
    }

    #[test]
    fn truncate_line_counts_utf16_units_without_splitting_astral() {
        // 499 BMP chars + two astral chars (2 UTF-16 units each) = 503 units.
        let mut line = "a".repeat(499);
        line.push('🦀');
        line.push('🦀');
        let result = truncate_line(&line);
        assert!(result.was_truncated);
        // The first crab would cross the 500-unit limit; the cut lands before
        // it, keeping the string valid UTF-8.
        assert_eq!(result.text, format!("{}... [truncated]", "a".repeat(499)));
    }

    #[test]
    fn truncation_result_serializes_with_camel_case_and_null()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let result = head("abc\ndef", 10, 6);
        let value = serde_json::to_value(&result)?;
        let object = value.as_object().ok_or_else(|| {
            std::io::Error::other("truncation result should serialize as an object")
        })?;
        assert!(object.contains_key("truncatedBy"));
        assert!(object.contains_key("totalLines"));
        assert!(object.contains_key("outputBytes"));
        assert!(object.contains_key("lastLinePartial"));
        assert!(object.contains_key("firstLineExceedsLimit"));
        assert!(object.contains_key("maxLines"));
        assert_eq!(
            object.get("truncatedBy"),
            Some(&serde_json::Value::String("bytes".to_owned()))
        );

        let clean = truncate_head("ok", TruncationOptions::default());
        let value = serde_json::to_value(&clean)?;
        assert_eq!(value.get("truncatedBy"), Some(&serde_json::Value::Null));
        Ok(())
    }
}
