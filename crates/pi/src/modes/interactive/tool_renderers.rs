//! Built-in tool renderers: typed one-line call signatures plus collapsed
//! result bodies, replacing the raw-JSON fallback for the seven core tools.
//!
//! Registry keys are `AgentTool::name()` verbatim (`"read"`, `"bash"`,
//! `"edit"`, `"write"`, `"grep"`, `"find"`, `"ls"`). Header fields are read
//! defensively from `call.raw_args`; a missing or wrong-typed field falls
//! back to the bare verb — renderers never panic and never print JSON.
//!
//! The theme is resolved via [`theme::current`] because composition runs
//! inside [`theme::with_theme`], and the static renderer table must not own
//! a theme snapshot (user theme switches must repaint correctly).

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde_json::Value;

use super::messages::TOOL_PREVIEW_LINES;
use super::theme::{self, ResolvedTheme, ThemeColor};
use super::tool_renderer::{
    CustomToolRenderer, ToolCallView, ToolResultView, default_result_lines,
};

/// Lazily built static renderer table, shared across every compose.
static BUILTIN_RENDERERS: LazyLock<BTreeMap<String, Box<dyn CustomToolRenderer>>> =
    LazyLock::new(|| {
        let mut map: BTreeMap<String, Box<dyn CustomToolRenderer>> = BTreeMap::new();
        map.insert("read".to_owned(), Box::new(ReadRenderer));
        map.insert("bash".to_owned(), Box::new(BashRenderer));
        map.insert("edit".to_owned(), Box::new(EditRenderer));
        map.insert("write".to_owned(), Box::new(WriteRenderer));
        map.insert("grep".to_owned(), Box::new(GrepRenderer));
        map.insert("find".to_owned(), Box::new(FindRenderer));
        map.insert("ls".to_owned(), Box::new(LsRenderer));
        map
    });

/// The shared static renderer table, keyed by `AgentTool::name()`.
#[must_use]
pub fn builtin_tool_renderers() -> &'static BTreeMap<String, Box<dyn CustomToolRenderer>> {
    &BUILTIN_RENDERERS
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Collapse to [`TOOL_PREVIEW_LINES`] unless expanded, appending a hint row.
fn collapse(lines: Vec<String>, expanded: bool, theme: &ResolvedTheme) -> Vec<String> {
    if expanded || lines.len() <= TOOL_PREVIEW_LINES {
        return lines;
    }
    let hidden = lines.len() - TOOL_PREVIEW_LINES;
    let mut out: Vec<String> = lines.into_iter().take(TOOL_PREVIEW_LINES).collect();
    out.push(theme.fg(ThemeColor::Dim, &format!("… {hidden} more lines · ctrl+o")));
    out
}

/// Colourize unified-diff lines by leading marker.
///
/// Marker-based colourization is deliberate: it works whether the edit result
/// text is a unified patch, a numbered diff, or plain prose, so it cannot
/// mis-render.
fn diff_lines(text: &str, theme: &ResolvedTheme) -> Vec<String> {
    text.lines()
        .map(|line| {
            let color = match line.chars().next() {
                Some('+') => ThemeColor::ToolDiffAdded,
                Some('-') => ThemeColor::ToolDiffRemoved,
                Some('@') if line.starts_with("@@") => ThemeColor::Muted,
                _ => ThemeColor::ToolDiffContext,
            };
            theme.fg(color, line)
        })
        .collect()
}

/// Result body for the default-collapsed tools: verbatim lines plus
/// spill/error tails, capped at [`TOOL_PREVIEW_LINES`] with a hint.
fn default_collapsed(result: &ToolResultView, expanded: bool) -> Vec<String> {
    collapse(default_result_lines(result), expanded, &theme::current())
}

/// Read a string argument; absent or wrong-typed yields `None`.
fn arg_str<'a>(call: &'a ToolCallView, key: &str) -> Option<&'a str> {
    call.raw_args.get(key).and_then(Value::as_str)
}

/// Read an integer argument; absent or wrong-typed yields `None`.
fn arg_u64(call: &ToolCallView, key: &str) -> Option<u64> {
    call.raw_args.get(key).and_then(Value::as_u64)
}

/// Collapse newlines and other control characters in `s` to single spaces.
///
/// Tails are tool-supplied argument text (paths, patterns). An embedded `\n`
/// or `\r` would otherwise forge extra terminal rows and break the one-line
/// signature contract every header promises; tab is preserved (it never starts
/// a new row). The result is always a single physical line.
pub(super) fn sanitize_single_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() && c != '\t' { ' ' } else { c })
        .collect()
}

/// Verb (`ToolTitle`) + optional argument tail (`ToolOutput`) as one header line.
///
/// The tail is sanitized here — the single chokepoint every renderer flows
/// through — so no tool argument can forge extra rows.
fn header_line(verb: &str, tail: Option<String>) -> Vec<String> {
    let theme = theme::current();
    let mut line = theme.fg(ThemeColor::ToolTitle, verb);
    if let Some(tail) = tail {
        let safe = sanitize_single_line(&tail);
        line.push_str(&theme.fg(ThemeColor::ToolOutput, &format!(" {safe}")));
    }
    vec![line]
}

// ---------------------------------------------------------------------------
// Per-tool renderers
// ---------------------------------------------------------------------------

/// `read {path}`, and — when `offset` and `limit` are both numbers with a
/// non-zero `limit` — `read {path}:{offset}-{offset+limit-1}` (inclusive end,
/// computed with saturating arithmetic so oversized args never panic).
struct ReadRenderer;

impl CustomToolRenderer for ReadRenderer {
    fn render_call_lines(&self, call: &ToolCallView, _expanded: bool) -> Option<Vec<String>> {
        let tail = arg_str(call, "path").map(|path| {
            // `offset + limit` is the exclusive end; the signature shows the
            // inclusive last line, `offset + limit - 1`. A zero-line read
            // (limit 0) has no meaningful end, so it falls back to path-only.
            // Saturating arithmetic keeps adversarial oversized args from
            // panicking in debug builds (CodeRabbit overflow finding).
            match (arg_u64(call, "offset"), arg_u64(call, "limit")) {
                (Some(offset), Some(limit)) if limit >= 1 => {
                    let end = offset.saturating_add(limit).saturating_sub(1);
                    format!("{path}:{offset}-{end}")
                }
                _ => path.to_owned(),
            }
        });
        Some(header_line("read", tail))
    }

    fn render_result_lines(&self, result: &ToolResultView, expanded: bool) -> Vec<String> {
        default_collapsed(result, expanded)
    }
}

/// `$ {command}`, first line only, styled `BashMode`.
struct BashRenderer;

impl CustomToolRenderer for BashRenderer {
    fn render_call_lines(&self, call: &ToolCallView, _expanded: bool) -> Option<Vec<String>> {
        let theme = theme::current();
        let line = match arg_str(call, "command") {
            Some(command) => {
                let first = command.lines().next().unwrap_or("");
                theme.fg(ThemeColor::BashMode, &format!("$ {first}"))
            }
            None => theme.fg(ThemeColor::ToolTitle, "bash"),
        };
        Some(vec![line])
    }

    fn render_result_lines(&self, result: &ToolResultView, expanded: bool) -> Vec<String> {
        default_collapsed(result, expanded)
    }
}

/// `edit {path}`; result body diff-colourized and collapsed.
struct EditRenderer;

impl CustomToolRenderer for EditRenderer {
    fn render_call_lines(&self, call: &ToolCallView, _expanded: bool) -> Option<Vec<String>> {
        Some(header_line(
            "edit",
            arg_str(call, "path").map(str::to_owned),
        ))
    }

    fn render_result_lines(&self, result: &ToolResultView, expanded: bool) -> Vec<String> {
        let theme = theme::current();
        if result.text.is_empty() {
            return default_collapsed(result, expanded);
        }
        let mut lines = diff_lines(&result.text, &theme);
        if result.truncated
            && let Some(path) = result.full_output_path.as_deref()
        {
            lines.push(theme.fg(
                ThemeColor::ToolOutput,
                &format!("[Output truncated. Full output: {path}]"),
            ));
        }
        if let Some(err) = result.error.as_deref() {
            lines.push(theme.fg(ThemeColor::ToolOutput, &format!("Error: {err}")));
        }
        collapse(lines, expanded, &theme)
    }
}

/// `write {path}`.
struct WriteRenderer;

impl CustomToolRenderer for WriteRenderer {
    fn render_call_lines(&self, call: &ToolCallView, _expanded: bool) -> Option<Vec<String>> {
        Some(header_line(
            "write",
            arg_str(call, "path").map(str::to_owned),
        ))
    }

    fn render_result_lines(&self, result: &ToolResultView, expanded: bool) -> Vec<String> {
        default_collapsed(result, expanded)
    }
}

/// `grep {pattern}` plus ` in {path}` when `path` is a non-empty string.
struct GrepRenderer;

impl CustomToolRenderer for GrepRenderer {
    fn render_call_lines(&self, call: &ToolCallView, _expanded: bool) -> Option<Vec<String>> {
        Some(header_line("grep", search_tail(call)))
    }

    fn render_result_lines(&self, result: &ToolResultView, expanded: bool) -> Vec<String> {
        default_collapsed(result, expanded)
    }
}

/// `find {pattern}` plus ` in {path}` when `path` is a non-empty string.
struct FindRenderer;

impl CustomToolRenderer for FindRenderer {
    fn render_call_lines(&self, call: &ToolCallView, _expanded: bool) -> Option<Vec<String>> {
        Some(header_line("find", search_tail(call)))
    }

    fn render_result_lines(&self, result: &ToolResultView, expanded: bool) -> Vec<String> {
        default_collapsed(result, expanded)
    }
}

/// `ls {path}`, falling back to `ls .` when `path` is absent.
struct LsRenderer;

impl CustomToolRenderer for LsRenderer {
    fn render_call_lines(&self, call: &ToolCallView, _expanded: bool) -> Option<Vec<String>> {
        let path = arg_str(call, "path").unwrap_or(".");
        Some(header_line("ls", Some(path.to_owned())))
    }

    fn render_result_lines(&self, result: &ToolResultView, expanded: bool) -> Vec<String> {
        default_collapsed(result, expanded)
    }
}

/// Shared `grep`/`find` argument tail: the pattern plus an ` in {path}`
/// qualifier when `path` is a non-empty string.
fn search_tail(call: &ToolCallView) -> Option<String> {
    let pattern = arg_str(call, "pattern")?;
    match arg_str(call, "path") {
        Some(path) if !path.is_empty() => Some(format!("{pattern} in {path}")),
        _ => Some(pattern.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{ReadRenderer, header_line, sanitize_single_line};
    use crate::modes::interactive::tool_renderer::{CustomToolRenderer, ToolCallView, line_plain};
    use serde_json::{Map, Value, json};

    /// Build a `read` call view with optional `offset`/`limit`.
    fn read_call(path: &str, offset: Option<u64>, limit: Option<u64>) -> ToolCallView {
        let mut raw = Map::new();
        raw.insert("path".to_owned(), json!(path));
        if let Some(o) = offset {
            raw.insert("offset".to_owned(), json!(o));
        }
        if let Some(l) = limit {
            raw.insert("limit".to_owned(), json!(l));
        }
        ToolCallView {
            name: "read".to_owned(),
            id: "call_1".to_owned(),
            args_summary: String::new(),
            raw_args: Value::Object(raw),
        }
    }

    // --- Item 1: inclusive read range -------------------------------------

    #[test]
    fn read_range_uses_inclusive_end() -> Result<(), String> {
        // offset=5, limit=3 reads lines 5, 6, 7 -> signature `x.rs:5-7`,
        // not the off-by-one `x.rs:5-8`.
        let call = read_call("x.rs", Some(5), Some(3));
        let lines = ReadRenderer
            .render_call_lines(&call, false)
            .ok_or("read always renders")?;
        assert_eq!(lines.len(), 1);
        let plain = line_plain(&lines[0]);
        assert!(
            plain.contains("read x.rs:5-7"),
            "inclusive end (offset+limit-1) expected, got: {plain:?}"
        );
        Ok(())
    }

    #[test]
    fn read_range_zero_limit_falls_back_to_path_only() -> Result<(), String> {
        // A zero-line read has no meaningful range; show just the path.
        let call = read_call("x.rs", Some(5), Some(0));
        let lines = ReadRenderer
            .render_call_lines(&call, false)
            .ok_or("read always renders")?;
        let plain = line_plain(&lines[0]);
        assert!(
            plain.contains("read x.rs"),
            "path-only fallback expected, got: {plain:?}"
        );
        assert!(
            !plain.contains(':'),
            "a zero-line read must not fabricate a range, got: {plain:?}"
        );
        Ok(())
    }

    #[test]
    fn read_range_oversized_args_do_not_panic() -> Result<(), String> {
        // Adversarial JSON: offset+limit overflows u64. Saturating math must
        // keep this panic-free (was a debug-build panic per CodeRabbit).
        let call = read_call("big.rs", Some(u64::MAX), Some(u64::MAX));
        let lines = ReadRenderer
            .render_call_lines(&call, false)
            .ok_or("read always renders")?;
        assert_eq!(lines.len(), 1);
        assert!(
            line_plain(&lines[0]).contains("big.rs"),
            "path still rendered under saturating end"
        );
        Ok(())
    }

    // --- Item 2: newline-forged rows --------------------------------------

    #[test]
    fn header_collapses_newline_in_tail_to_one_line() {
        // A tool path carrying `\n` must render as one line with the newline
        // turned into a space, never as two forged terminal rows.
        let lines = header_line("read", Some("a\nb".to_owned()));
        assert_eq!(lines.len(), 1, "tail with newline must stay one line");
        let raw = &lines[0];
        assert!(
            !raw.contains('\n') && !raw.contains('\r'),
            "no raw newlines may reach the terminal: {raw:?}"
        );
        let plain = line_plain(raw);
        assert!(
            plain.contains("a b"),
            "newline collapsed to a space expected, got: {plain:?}"
        );
    }

    #[test]
    fn sanitize_replaces_control_chars_preserves_tab() {
        assert_eq!(sanitize_single_line("a\nb"), "a b");
        assert_eq!(sanitize_single_line("a\rb"), "a b");
        assert_eq!(sanitize_single_line("a\x00b"), "a b");
        // Tab never starts a new row, so it is preserved verbatim.
        assert_eq!(sanitize_single_line("a\tb"), "a\tb");
    }
}
