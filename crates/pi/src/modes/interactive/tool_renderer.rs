//! Custom tool renderer adapter.
//!
//! The reference `tool-execution.ts` looks up each tool's call/result renderer
//! from its tool definition, falling back to a JSON dump. Built-in tools ship
//! their own compact renderers (bash preview, find/ls/grep truncation, read/
//! write counts, edit diff). Extensions may register custom renderers via the
//! host. This module defines the adapter trait + view-model types the
//! presentation layer uses, decoupled from the live tool registry.
//!
//! Renderers return pre-styled `Vec<String>` lines (ANSI already applied via
//! [`super::theme`]) so the caller wraps them in a plain [`pi_tui::components::Text`]
//! rather than a boxed component — this keeps the hot path allocation-light and
//! snapshot-friendly.

use pi_tui::components::util::strip_ansi;

/// Pure view-model for a tool call.
#[derive(Clone, Debug)]
pub struct ToolCallView {
    /// Tool name.
    pub name: String,
    /// Tool-call id.
    pub id: String,
    /// Pretty-printed arguments (already rendered by the tool def).
    pub args_summary: String,
    /// Raw arguments preserved for the JSON fallback renderer.
    pub raw_args: serde_json::Value,
}

/// Pure view-model for a tool result.
#[derive(Clone, Debug)]
pub struct ToolResultView {
    /// Text output lines (possibly truncated; spill path in [`Self::truncated`]).
    pub text: String,
    /// Whether `text` is a truncated preview.
    pub truncated: bool,
    /// Spill-file path for the full output, when truncated.
    pub full_output_path: Option<String>,
    /// Inline image references, when any (rendered by caller via Image).
    pub images: Vec<ImageRef>,
    /// Error message when the tool failed.
    pub error: Option<String>,
}

/// Reference to an inline image in a tool result.
#[derive(Clone, Debug)]
pub struct ImageRef {
    /// Base64-encoded image bytes.
    pub base64: String,
    /// MIME type.
    pub mime: String,
    /// Optional filename.
    pub filename: Option<String>,
}

/// Aggregate tool execution state for one tool-call id.
#[derive(Clone, Debug)]
pub struct ToolState {
    /// The call.
    pub call: ToolCallView,
    /// Pending/started/finished result, when present.
    pub result: Option<ToolResultView>,
    /// Whether output is expanded.
    pub expanded: bool,
    /// Background state: pending / success / error.
    pub phase: ToolPhase,
}

/// Tool execution phase (selects the background color).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolPhase {
    /// Call emitted, result pending.
    Pending,
    /// Result received successfully.
    Success,
    /// Result is an error.
    Error,
}

/// Errors a custom tool renderer can report.
#[derive(Debug, thiserror::Error)]
pub enum ToolRenderError {
    /// The renderer returned no usable lines.
    #[error("tool renderer produced no output for `{0}`")]
    Empty(String),
}

/// Adapter trait for tool-specific call/result renderers.
///
/// Built-in tools implement this directly; extension renderers are bridged
/// through the host and adapt to this trait. Renderers return pre-styled lines
/// (ANSI applied) so the caller wraps them in a plain `Text` component.
pub trait CustomToolRenderer: Send + Sync {
    /// Render the tool *call* header lines. `None` hides the shell entirely.
    fn render_call_lines(&self, call: &ToolCallView, expanded: bool) -> Option<Vec<String>>;

    /// Render the tool *result* body lines.
    ///
    /// The default implementation renders the result text verbatim (with the
    /// truncation spill note and error tail).
    fn render_result_lines(&self, result: &ToolResultView, expanded: bool) -> Vec<String> {
        let _ = expanded;
        default_result_lines(result)
    }

    /// Pretty-print the call arguments for the header (one line).
    fn summarize_args(&self, call: &ToolCallView) -> String {
        call.args_summary.clone()
    }

    /// Human-readable tool name for display.
    fn display_name(&self) -> &'static str {
        ""
    }
}

/// Default result lines: the (possibly truncated) text plus spill/error tails.
#[must_use]
pub fn default_result_lines(result: &ToolResultView) -> Vec<String> {
    if result.text.is_empty() && result.error.is_none() {
        return vec!["(no output)".to_owned()];
    }
    let mut lines: Vec<String> = if result.text.is_empty() {
        Vec::new()
    } else {
        result.text.lines().map(str::to_owned).collect()
    };
    if result.truncated
        && let Some(path) = result.full_output_path.as_deref()
    {
        lines.push(format!("[Output truncated. Full output: {path}]"));
    }
    if let Some(err) = result.error.as_deref() {
        lines.push(format!("Error: {err}"));
    }
    if lines.is_empty() {
        lines.push("(no output)".to_owned());
    }
    lines
}

/// Visible (ANSI-stripped) width of a styled line.
#[must_use]
pub fn line_visible_width(line: &str) -> usize {
    pi_tui::text::visible_width(line)
}

/// Strip ANSI from a styled line (for golden snapshots).
#[must_use]
pub fn line_plain(line: &str) -> String {
    strip_ansi(line)
}
