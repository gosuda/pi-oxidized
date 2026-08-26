//! Chat message view-models and component builders.
//!
//! Ports `.references/pi/packages/coding-agent/src/modes/interactive/components/`
//! `assistant-message.ts`, `user-message.ts`, `tool-execution.ts`,
//! `bash-execution.ts`, `custom-message.ts`, `compaction-summary-message.ts`,
//! `branch-summary-message.ts`, and `skill-invocation-message.ts` into pure
//! view-models that build pi-tui components for composition.

use std::collections::BTreeMap;

use pi_ai::{AssistantContent, AssistantMessage, StopReason};
use pi_tui::component::Component;
use pi_tui::components::{Markdown, Rail, Spacer, Text};

use super::theme::{
    self, MarkdownTheme, ResolvedTheme, ThemeColor, user_markdown_options,
};
use super::tool_renderer::{ToolPhase, ToolState};
/// Shared left-edge indent for unrailed content (column 2; D3).
pub const CONTENT_INDENT: u16 = 2;

/// Collapsed preview line count for every tool and bash body.
pub const TOOL_PREVIEW_LINES: usize = 12;

/// Wrap an event block in a one-cell gutter rail (D1/D2/D5).
///
/// The colour is captured by value so the paint closure is `Send + 'static`;
/// `theme.fg` already returns an owned `String`.
fn railed(
    glyph: &str,
    color: ThemeColor,
    theme: &ResolvedTheme,
    child: impl Component + 'static,
) -> Box<dyn Component> {
    let theme_snapshot = theme.clone();
    let mut rail = Rail::with_glyph(glyph, move |s: &str| theme_snapshot.fg(color, s));
    rail.add_child(child);
    Box::new(rail)
}
/// One chat message view-model.
#[derive(Clone, Debug)]
pub enum MessageView {
    /// User-authored message.
    User(UserMessageView),
    /// Assistant message (text + thinking + stop-reason errors).
    Assistant(AssistantMessageView),
    /// Tool execution block.
    Tool(ToolMessageView),
    /// Bash execution (`!`/`!!`) block.
    Bash(BashMessageView),
    /// Extension-injected custom message.
    Custom(CustomMessageView),
    /// Compaction summary.
    Compaction(CompactionSummaryView),
    /// Branch summary.
    Branch(BranchSummaryView),
    /// Skill invocation block.
    Skill(SkillInvocationView),
}

/// User message view-model.
#[derive(Clone, Debug)]
pub struct UserMessageView {
    /// Markdown source.
    pub text: String,
}

/// Assistant message view-model.
#[derive(Clone, Debug)]
pub struct AssistantMessageView {
    /// The full assistant message (content + usage + stop reason).
    pub message: AssistantMessage,
    /// Whether thinking blocks are hidden behind a static label.
    pub hide_thinking: bool,
    /// Label shown for hidden thinking runs.
    pub hidden_thinking_label: String,
    /// Whether this is the live streaming tail.
    pub streaming: bool,
}

/// Tool message view-model (wraps [`ToolState`] plus renderer key).
#[derive(Clone, Debug)]
pub struct ToolMessageView {
    /// Aggregate tool state.
    pub state: ToolState,
    /// Renderer key (tool name) used to look up a [`super::tool_renderer::CustomToolRenderer`].
    pub renderer: String,
}

/// Bash execution view-model.
#[derive(Clone, Debug)]
pub struct BashMessageView {
    /// Shell command.
    pub command: String,
    /// Captured output (possibly truncated).
    pub output: String,
    /// Whether collapsed preview vs expanded.
    pub expanded: bool,
    /// Exit code when finished.
    pub exit_code: Option<i32>,
    /// Whether cancelled.
    pub cancelled: bool,
    /// Whether output is truncated.
    pub truncated: bool,
    /// Spill path when truncated.
    pub full_output_path: Option<String>,
}

/// Extension custom message view-model.
#[derive(Clone, Debug)]
pub struct CustomMessageView {
    /// Custom type label.
    pub custom_type: String,
    /// Text content.
    pub text: String,
}

/// Compaction summary view-model.
#[derive(Clone, Debug)]
pub struct CompactionSummaryView {
    /// Summary text.
    pub summary: String,
    /// Tokens before compaction.
    pub tokens_before: i64,
}

/// Branch summary view-model.
#[derive(Clone, Debug)]
pub struct BranchSummaryView {
    /// Summary text.
    pub summary: String,
    /// Entry id the branch forked from.
    pub from_id: String,
}

/// Skill invocation view-model.
#[derive(Clone, Debug)]
pub struct SkillInvocationView {
    /// Skill name.
    pub name: String,
    /// Invocation text.
    pub text: String,
}

impl MessageView {
    /// Build a streaming assistant tail view-model.
    #[must_use]
    pub fn streaming_assistant(message: AssistantMessage) -> Self {
        Self::Assistant(AssistantMessageView {
            message,
            hide_thinking: false,
            hidden_thinking_label: "Thinking…".to_owned(),
            streaming: true,
        })
    }
}

/// Build the component stack for one assistant message.
///
/// Returns a `Vec<Box<dyn Component>>` so the caller can splice it into the
/// chat container in order. Mirrors `AssistantMessageComponent.updateContent`.
#[must_use]
pub fn build_assistant(
    view: &AssistantMessageView,
    md_theme: &MarkdownTheme,
    theme: &ResolvedTheme,
) -> Vec<Box<dyn Component>> {
    let mut out: Vec<Box<dyn Component>> = Vec::new();
    let message = &view.message;

    if message.content.iter().any(content_is_visible) {
        out.push(Box::new(Spacer::new(1)));
    }

    push_assistant_content_blocks(&mut out, view, md_theme, theme);
    push_assistant_stop_reason(&mut out, message, theme);
    out
}

fn content_is_visible(content: &AssistantContent) -> bool {
    match content {
        AssistantContent::Text(t) => !t.text.trim().is_empty(),
        AssistantContent::Thinking(t) => !t.thinking.trim().is_empty(),
        AssistantContent::ToolCall(_) => false,
    }
}

fn push_assistant_content_blocks(
    out: &mut Vec<Box<dyn Component>>,
    view: &AssistantMessageView,
    md_theme: &MarkdownTheme,
    theme: &ResolvedTheme,
) {
    let message = &view.message;
    let mut iter = message.content.iter().enumerate().peekable();
    while let Some((idx, content)) = iter.next() {
        match content {
            AssistantContent::Text(t) => {
                if !t.text.trim().is_empty() {
                    out.push(Box::new(Markdown::new(
                        t.text.trim(),
                        CONTENT_INDENT,
                        0,
                        md_theme.clone(),
                        theme::default_text_style(),
                        user_markdown_options(),
                    )));
                }
            }
            AssistantContent::Thinking(tc) => {
                // Preserve the first Thinking content in the run (thinking-current-block fix).
                let mut blocks: Vec<String> = Vec::new();
                if !tc.thinking.trim().is_empty() {
                    blocks.push(tc.thinking.trim().to_owned());
                }
                while let Some(&(_, c)) = iter.peek() {
                    if let AssistantContent::Thinking(next) = c {
                        if !next.thinking.trim().is_empty() {
                            blocks.push(next.thinking.trim().to_owned());
                        }
                        iter.next();
                    } else {
                        break;
                    }
                }
                if blocks.is_empty() {
                    continue;
                }
                push_thinking_components(out, view, md_theme, theme, &blocks, idx);
            }
            AssistantContent::ToolCall(_) => {
                // Tool calls render as separate Tool message blocks, not inline.
            }
        }
    }
}

fn push_thinking_components(
    out: &mut Vec<Box<dyn Component>>,
    view: &AssistantMessageView,
    md_theme: &MarkdownTheme,
    theme: &ResolvedTheme,
    blocks: &[String],
    idx: usize,
) {
    let has_after = view
        .message
        .content
        .iter()
        .skip(idx + 1)
        .any(content_is_visible);
    if view.hide_thinking {
        out.push(Box::new(Text::with_padding(
            theme.fg(
                ThemeColor::ThinkingText,
                &theme::italic(&view.hidden_thinking_label),
            ),
            CONTENT_INDENT,
            0,
        )));
    } else {
        out.push(Box::new(Markdown::new(
            blocks.join("\n\n"),
            CONTENT_INDENT,
            0,
            md_theme.clone(),
            thinking_text_style(),
            user_markdown_options(),
        )));
    }
    if has_after {
        out.push(Box::new(Spacer::new(1)));
    }
}

fn push_assistant_stop_reason(
    out: &mut Vec<Box<dyn Component>>,
    message: &AssistantMessage,
    theme: &ResolvedTheme,
) {
    let has_tool_calls = message
        .content
        .iter()
        .any(|c| matches!(c, AssistantContent::ToolCall(_)));
    match message.stop_reason {
        StopReason::Length => {
            out.push(Box::new(Text::with_padding(
                theme.fg(
                    ThemeColor::Error,
                    "Response was truncated before completion.",
                ),
                CONTENT_INDENT,
                0,
            )));
        }
        StopReason::Aborted if !has_tool_calls => {
            let msg = match message.error_message.as_deref() {
                Some(e) if e != "Request was aborted" => e.to_owned(),
                _ => "Operation aborted".to_owned(),
            };
            out.push(Box::new(Text::with_padding(
                theme.fg(ThemeColor::Error, &msg),
                CONTENT_INDENT,
                0,
            )));
        }
        StopReason::Error if !has_tool_calls => {
            let msg = message
                .error_message
                .clone()
                .unwrap_or_else(|| "Unknown error".to_owned());
            out.push(Box::new(Text::with_padding(
                theme.fg(ThemeColor::Error, &format!("Error: {msg}")),
                CONTENT_INDENT,
                0,
            )));
        }
        _ => {}
    }
}

/// Build the component for a user message (railed gutter, no background slab).
#[must_use]
pub fn build_user(
    view: &UserMessageView,
    md_theme: &MarkdownTheme,
    theme: &ResolvedTheme,
) -> Box<dyn Component> {
    let md = Markdown::new(
        view.text.as_str(),
        0,
        0,
        md_theme.clone(),
        user_text_style(),
        user_markdown_options(),
    );
    let mut stack = ColumnStack::new();
    stack.push(Box::new(Spacer::new(1)));
    stack.push(railed("│", ThemeColor::BorderAccent, theme, md));
    Box::new(stack)
}

/// Build the component stack for a tool execution block.
///
/// Looks up `renderers` for the tool name; unknown tools fall back to a
/// one-line name + args summary header.
/// Call/result renderers return pre-styled lines, wrapped here in `Text`.
#[must_use]
pub fn build_tool(
    view: &ToolMessageView,
    renderers: &BTreeMap<String, Box<dyn super::tool_renderer::CustomToolRenderer>>,
    theme: &ResolvedTheme,
) -> Vec<Box<dyn Component>> {
    let mut out: Vec<Box<dyn Component>> = Vec::new();
    out.push(Box::new(Spacer::new(1)));
    let (glyph, rail_color) = match view.state.phase {
        ToolPhase::Pending => ("│", ThemeColor::Muted),
        ToolPhase::Success => ("│", ThemeColor::Success),
        ToolPhase::Error => ("┃", ThemeColor::Error),
    };
    let mut column = ColumnStack::new();
    // Call header (renderer or one-line summary fallback).
    let header_lines: Vec<String> = if let Some(renderer) = renderers.get(&view.renderer) {
        renderer
            .render_call_lines(&view.state.call, view.state.expanded)
            .unwrap_or_default()
    } else {
        let title = theme.fg(
            ThemeColor::ToolTitle,
            &format!("▶ {}", view.state.call.name),
        );
        let args = theme.fg(
            ThemeColor::ToolOutput,
            &super::tool_renderers::sanitize_single_line(&view.state.call.args_summary),
        );
        vec![title, args]
    };
    if !header_lines.is_empty() {
        column.push(Box::new(Text::with_padding(header_lines.join("\n"), 0, 0)));
    }
    // Result body.
    if let Some(result) = view.state.result.as_ref() {
        let body_lines = if let Some(renderer) = renderers.get(&view.renderer) {
            renderer.render_result_lines(result, view.state.expanded)
        } else {
            super::tool_renderer::default_result_lines(result)
        };
        if !body_lines.is_empty() {
            column.push(Box::new(Text::with_padding(
                theme.fg(ThemeColor::ToolOutput, &body_lines.join("\n")),
                0,
                0,
            )));
        }
    }
    out.push(railed(glyph, rail_color, theme, column));
    out
}

/// Build the bash execution component (railed gutter with preview/expand).
#[must_use]
pub fn build_bash(view: &BashMessageView, theme: &ResolvedTheme) -> Box<dyn Component> {
    let mut out: Vec<Box<dyn Component>> = Vec::new();
    let cmd_line = theme.fg(ThemeColor::BashMode, &format!("$ {}", view.command));
    out.push(Box::new(Text::with_padding(cmd_line, 0, 0)));
    let body = if view.expanded {
        view.output.clone()
    } else {
        preview_lines(&view.output, TOOL_PREVIEW_LINES)
    };
    if !body.is_empty() {
        out.push(Box::new(Text::with_padding(
            theme.fg(ThemeColor::ToolOutput, &body),
            0,
            0,
        )));
    }
    if view.truncated
        && !view.expanded
        && let Some(path) = view.full_output_path.as_deref()
    {
        out.push(Box::new(Text::with_padding(
            theme.fg(
                ThemeColor::Dim,
                &format!("[truncated — full output: {path}]"),
            ),
            0,
            0,
        )));
    }
    if view.cancelled {
        out.push(Box::new(Text::with_padding(
            theme.fg(ThemeColor::Warning, "(cancelled)"),
            0,
            0,
        )));
    } else if let Some(code) = view.exit_code.filter(|&code| code != 0) {
        out.push(Box::new(Text::with_padding(
            theme.fg(ThemeColor::Error, &format!("exit {code}")),
            0,
            0,
        )));
    }
    let mut column = ColumnStack::new();
    for c in out {
        column.push(c);
    }
    let mut stack = ColumnStack::new();
    stack.push(Box::new(Spacer::new(1)));
    stack.push(railed("│", ThemeColor::BashMode, theme, column));
    Box::new(stack)
}

/// Build the custom-message component (railed gutter, no background slab).
#[must_use]
pub fn build_custom(
    view: &CustomMessageView,
    md_theme: &MarkdownTheme,
    theme: &ResolvedTheme,
) -> Box<dyn Component> {
    let label = theme.fg(
        ThemeColor::CustomMessageLabel,
        &format!("[{}]", view.custom_type),
    );
    let mut column = ColumnStack::new();
    column.push(Box::new(Text::with_padding(label, 0, 0)));
    column.push(Box::new(Markdown::new(
        view.text.as_str(),
        0,
        0,
        md_theme.clone(),
        custom_text_style(),
        user_markdown_options(),
    )));
    let mut stack = ColumnStack::new();
    stack.push(Box::new(Spacer::new(1)));
    stack.push(railed("│", ThemeColor::CustomMessageLabel, theme, column));
    Box::new(stack)
}

/// Build the compaction-summary component (collapsible).
#[must_use]
pub fn build_compaction(
    view: &CompactionSummaryView,
    md_theme: &MarkdownTheme,
    theme: &ResolvedTheme,
) -> Box<dyn Component> {
    let label = theme.fg(
        ThemeColor::Accent,
        &format!("⌁ Compacted context (was {} tokens)", view.tokens_before),
    );
    let mut column = ColumnStack::new();
    column.push(Box::new(Text::with_padding(label, 0, 0)));
    column.push(Box::new(Markdown::new(
        view.summary.as_str(),
        0,
        0,
        md_theme.clone(),
        theme::default_text_style(),
        user_markdown_options(),
    )));
    let mut stack = ColumnStack::new();
    stack.push(Box::new(Spacer::new(1)));
    stack.push(railed("│", ThemeColor::BorderMuted, theme, column));
    Box::new(stack)
}

/// Build the branch-summary component.
#[must_use]
pub fn build_branch(
    view: &BranchSummaryView,
    md_theme: &MarkdownTheme,
    theme: &ResolvedTheme,
) -> Box<dyn Component> {
    let label = theme.fg(
        ThemeColor::Accent,
        &format!("↶ Branch summary (from {})", view.from_id),
    );
    let mut column = ColumnStack::new();
    column.push(Box::new(Text::with_padding(label, 0, 0)));
    column.push(Box::new(Markdown::new(
        view.summary.as_str(),
        0,
        0,
        md_theme.clone(),
        theme::default_text_style(),
        user_markdown_options(),
    )));
    let mut stack = ColumnStack::new();
    stack.push(Box::new(Spacer::new(1)));
    stack.push(railed("│", ThemeColor::BorderMuted, theme, column));
    Box::new(stack)
}

/// Build the skill-invocation component.
#[must_use]
pub fn build_skill(
    view: &SkillInvocationView,
    md_theme: &MarkdownTheme,
    theme: &ResolvedTheme,
) -> Box<dyn Component> {
    let label = theme.fg(
        ThemeColor::CustomMessageLabel,
        &format!("[skill:{}]", view.name),
    );
    let mut column = ColumnStack::new();
    column.push(Box::new(Text::with_padding(label, 0, 0)));
    column.push(Box::new(Markdown::new(
        view.text.as_str(),
        0,
        0,
        md_theme.clone(),
        custom_text_style(),
        user_markdown_options(),
    )));
    let mut stack = ColumnStack::new();
    stack.push(Box::new(Spacer::new(1)));
    stack.push(railed("│", ThemeColor::Accent, theme, column));
    Box::new(stack)
}

/// Take the first `n` lines of `text` for a collapsed preview.
fn preview_lines(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

/// User-message default text style (color applied via markdown theme hooks).
fn user_text_style() -> pi_tui::components::DefaultTextStyle {
    pi_tui::components::DefaultTextStyle::default()
}

/// Thinking text style (italic + thinkingText color applied via markdown theme).
fn thinking_text_style() -> pi_tui::components::DefaultTextStyle {
    pi_tui::components::DefaultTextStyle::with_style_flags(0)
}

/// Custom-message default text style.
fn custom_text_style() -> pi_tui::components::DefaultTextStyle {
    pi_tui::components::DefaultTextStyle::default()
}

// ---------------------------------------------------------------------------
// ColumnStack: a minimal vertical stack component (no border).
// ---------------------------------------------------------------------------

/// Vertical stack of components; measure = sum of child heights, render stacks
/// top-to-bottom. Used to assemble multi-block message bodies.
pub struct ColumnStack {
    children: Vec<Box<dyn Component>>,
}

impl ColumnStack {
    /// Create an empty stack.
    #[must_use]
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    /// Push a child.
    pub fn push(&mut self, child: Box<dyn Component>) {
        self.children.push(child);
    }

    /// Whether the stack has no children.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// Number of children.
    #[must_use]
    pub fn len(&self) -> usize {
        self.children.len()
    }
}

impl Default for ColumnStack {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for ColumnStack {
    fn measure(&mut self, width: u16) -> u16 {
        self.children.iter_mut().map(|c| c.measure(width)).sum()
    }

    fn render(&mut self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        let mut y = area.y;
        for child in &mut self.children {
            let h = child.measure(area.width);
            if h == 0 {
                continue;
            }
            let row = ratatui::layout::Rect {
                x: area.x,
                y,
                width: area.width,
                height: h,
            };
            child.render(row, buf);
            y = y.saturating_add(h);
            if y >= area.y.saturating_add(area.height) {
                break;
            }
        }
    }

    fn handle_event(
        &mut self,
        event: &pi_tui::component::UiEvent,
    ) -> pi_tui::component::EventResult {
        let _ = event;
        pi_tui::component::EventResult::Ignored
    }

    fn invalidate(&mut self) {
        for c in &mut self.children {
            c.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use pi_tui::component::Component;
    use ratatui::buffer::{Buffer, CellDiffOption};
    use ratatui::layout::Rect;

    use super::{ColumnStack, ToolMessageView, build_tool};
    use crate::modes::interactive::theme;
    use crate::modes::interactive::tool_renderer::{
        CustomToolRenderer, ToolCallView, ToolPhase, ToolState,
    };

    /// Plain-text cell symbols for a buffer region. ANSI is parsed into cell
    /// style (never the symbol), and wide-cell fillers are skipped — so joining
    /// symbols yields the visible text. This mirrors pi-tui's test-only
    /// `snapshot_area`, which is `#[cfg(test)]`-gated and thus unavailable to
    /// dependents.
    fn snapshot_plain(buf: &Buffer, width: u16, height: u16) -> Vec<String> {
        let mut out = Vec::with_capacity(usize::from(height));
        for y in 0..height {
            let mut line = String::new();
            let mut x = 0u16;
            while x < width {
                match buf.cell((x, y)) {
                    Some(cell) if cell.diff_option == CellDiffOption::Skip => {}
                    Some(cell) => line.push_str(cell.symbol()),
                    None => line.push(' '),
                }
                x = x.saturating_add(1);
            }
            out.push(line);
        }
        out
    }

    /// Unknown/extension tools fall back to a `▶ {name} {args}` header built
    /// straight from `args_summary`. A multiline summary must collapse to one
    /// physical row — the same sanitization the built-in headers use — so it
    /// cannot dump a whole prompt or file body across many terminal rows.
    #[test]
    fn unknown_tool_multiline_args_collapse_to_one_row() {
        let view = ToolMessageView {
            renderer: "mcp__ext".to_owned(),
            state: ToolState {
                call: ToolCallView {
                    name: "mcp__ext".to_owned(),
                    id: "call_1".to_owned(),
                    args_summary: "line1\nline2".to_owned(),
                    raw_args: serde_json::json!({"body": "line1\nline2"}),
                },
                result: None,
                expanded: false,
                phase: ToolPhase::Pending,
            },
        };
        let th = theme::dark();
        let renderers: BTreeMap<String, Box<dyn CustomToolRenderer>> = BTreeMap::new();
        let mut stack = ColumnStack::new();
        for component in build_tool(&view, &renderers, &th) {
            stack.push(component);
        }
        // Render into a fresh buffer; once sanitized, the multiline summary
        // must occupy exactly one row.
        let width = 40u16;
        let height = stack.measure(width).max(1);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        stack.render(area, &mut buf);
        let rows = snapshot_plain(&buf, width, height);
        let summary_rows: Vec<&String> = rows.iter().filter(|row| row.contains("line2")).collect();
        assert_eq!(
            summary_rows.len(),
            1,
            "multiline args must collapse to one row: {rows:?}"
        );
        assert!(
            summary_rows[0].contains("line1"),
            "the single summary row must carry the joined text: {rows:?}"
        );
    }
}
