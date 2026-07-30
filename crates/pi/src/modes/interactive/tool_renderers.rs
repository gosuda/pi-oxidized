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

/// Verb (`ToolTitle`) + optional argument tail (`ToolOutput`) as one header line.
fn header_line(verb: &str, tail: Option<String>) -> Vec<String> {
    let theme = theme::current();
    let mut line = theme.fg(ThemeColor::ToolTitle, verb);
    if let Some(tail) = tail {
        line.push_str(&theme.fg(ThemeColor::ToolOutput, &format!(" {tail}")));
    }
    vec![line]
}

// ---------------------------------------------------------------------------
// Per-tool renderers
// ---------------------------------------------------------------------------

/// `read {path}` and, when both `offset` and `limit` are numbers,
/// `read {path}:{offset}-{offset+limit}`.
struct ReadRenderer;

impl CustomToolRenderer for ReadRenderer {
    fn render_call_lines(&self, call: &ToolCallView, _expanded: bool) -> Option<Vec<String>> {
        let tail = arg_str(call, "path").map(|path| {
            match (arg_u64(call, "offset"), arg_u64(call, "limit")) {
                (Some(offset), Some(limit)) => format!("{path}:{offset}-{}", offset + limit),
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
