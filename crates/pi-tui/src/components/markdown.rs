//! Markdown renderer using pulldown-cmark with theme hooks and streaming fence trim.

use std::ops::Range;
use std::sync::Arc;

use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::component::{Component, EventResult, UiEvent};
use crate::link::hyperlink_capped;
use crate::text::{
    is_image_line, render_latex, strip_trailing_partial_closing_fence, visible_width,
    wrap_text_with_ansi,
};

use super::util::{KeyedLine, apply_background, empty_line, paint_lines_keyed};

/// Default text styling applied to body content (not backgrounds).
#[derive(Clone, Default)]
pub struct DefaultTextStyle {
    /// Foreground wrapper.
    pub color: Option<fn(&str) -> String>,
    /// Background applied at the full-line padding stage.
    pub bg_color: Option<fn(&str) -> String>,
    /// bit0 bold, bit1 italic, bit2 strike, bit3 underline
    style_flags: u8,
}

impl DefaultTextStyle {
    const BOLD: u8 = 1;
    const ITALIC: u8 = 1 << 1;
    const STRIKE: u8 = 1 << 2;
    const UNDERLINE: u8 = 1 << 3;

    /// Create with explicit decoration flags.
    #[must_use]
    pub fn with_style_flags(style_flags: u8) -> Self {
        Self {
            color: None,
            bg_color: None,
            style_flags,
        }
    }

    /// Build flags from a decoration mask bits tuple `(bold, italic, strike, underline)`.
    #[must_use]
    pub fn flags_from(bits: [bool; 4]) -> u8 {
        let mut style_flags = 0u8;
        if bits[0] {
            style_flags |= Self::BOLD;
        }
        if bits[1] {
            style_flags |= Self::ITALIC;
        }
        if bits[2] {
            style_flags |= Self::STRIKE;
        }
        if bits[3] {
            style_flags |= Self::UNDERLINE;
        }
        style_flags
    }

    /// Bold decoration enabled.
    #[must_use]
    pub fn bold(&self) -> bool {
        self.style_flags & Self::BOLD != 0
    }
    /// Italic decoration enabled.
    #[must_use]
    pub fn italic(&self) -> bool {
        self.style_flags & Self::ITALIC != 0
    }
    /// Strikethrough decoration enabled.
    #[must_use]
    pub fn strikethrough(&self) -> bool {
        self.style_flags & Self::STRIKE != 0
    }
    /// Underline decoration enabled.
    #[must_use]
    pub fn underline(&self) -> bool {
        self.style_flags & Self::UNDERLINE != 0
    }
}

type BackgroundFn = Box<dyn Fn(&str) -> String>;

/// Syntax highlight hook.
pub type HighlightCodeFn = Arc<dyn Fn(&str, Option<&str>) -> Vec<String> + Send + Sync>;

/// Theme functions for markdown elements (ANSI wrappers).
#[derive(Clone)]
pub struct MarkdownTheme {
    /// Heading text.
    pub heading: fn(&str) -> String,
    /// Link text.
    pub link: fn(&str) -> String,
    /// Parenthetical URL when hyperlinks are off.
    pub link_url: fn(&str) -> String,
    /// Inline code.
    pub code: fn(&str) -> String,
    /// Code block body line.
    pub code_block: fn(&str) -> String,
    /// Code block fence border.
    pub code_block_border: fn(&str) -> String,
    /// Blockquote body.
    pub quote: fn(&str) -> String,
    /// Blockquote border (`│ `).
    pub quote_border: fn(&str) -> String,
    /// Horizontal rule.
    pub hr: fn(&str) -> String,
    /// List bullet / marker.
    pub list_bullet: fn(&str) -> String,
    /// Bold span.
    pub bold: fn(&str) -> String,
    /// Italic span.
    pub italic: fn(&str) -> String,
    /// Strikethrough span.
    pub strikethrough: fn(&str) -> String,
    /// Underline span.
    pub underline: fn(&str) -> String,
    /// Optional syntax highlighter: `(code, lang) -> lines`.
    pub highlight_code: Option<HighlightCodeFn>,
    /// Indent prefix for code block body lines (default `"  "`).
    pub code_block_indent: String,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        fn id(s: &str) -> String {
            s.to_owned()
        }
        Self {
            heading: id,
            link: id,
            link_url: id,
            code: id,
            code_block: id,
            code_block_border: id,
            quote: id,
            quote_border: id,
            hr: id,
            list_bullet: id,
            bold: id,
            italic: id,
            strikethrough: id,
            underline: id,
            highlight_code: None,
            code_block_indent: "  ".to_owned(),
        }
    }
}

/// Markdown parse options.
#[expect(
    clippy::struct_excessive_bools,
    reason = "four independent feature toggles; collapsing into enums would widen the public API without clarifying intent"
)]
pub struct MarkdownOptions {
    /// Preserve source ordered-list markers (best-effort with pulldown-cmark).
    pub preserve_ordered_list_markers: bool,
    /// Preserve backslash escapes as raw (best-effort).
    pub preserve_backslash_escapes: bool,
    /// Enable OSC 8 hyperlinks (caller supplies capability).
    pub hyperlinks: bool,
    /// Render supported LaTeX math expressions as Unicode text (default: true).
    pub render_latex: bool,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            preserve_ordered_list_markers: false,
            preserve_backslash_escapes: false,
            hyperlinks: false,
            render_latex: true,
        }
    }
}

/// Markdown component with width-keyed render cache.
pub struct Markdown {
    text: String,
    padding_x: u16,
    padding_y: u16,
    theme: MarkdownTheme,
    default_style: DefaultTextStyle,
    options: MarkdownOptions,
    cache: Option<Cache>,
}

struct Cache {
    text: String,
    width: u16,
    lines: Vec<KeyedLine>,
}

impl Markdown {
    /// Create a markdown component.
    #[must_use]
    pub fn new(
        text: impl Into<String>,
        padding_x: u16,
        padding_y: u16,
        theme: MarkdownTheme,
        default_style: DefaultTextStyle,
        options: MarkdownOptions,
    ) -> Self {
        Self {
            text: text.into(),
            padding_x,
            padding_y,
            theme,
            default_style,
            options,
            cache: None,
        }
    }

    /// Replace markdown source.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cache = None;
    }

    /// Borrow source text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Enable/disable hyperlink encoding.
    pub fn set_hyperlinks(&mut self, enabled: bool) {
        self.options.hyperlinks = enabled;
        self.cache = None;
    }

    fn apply_default_style(&self, text: &str) -> String {
        let mut styled = text.to_owned();
        if let Some(color) = self.default_style.color {
            styled = color(&styled);
        }
        if self.default_style.bold() {
            styled = (self.theme.bold)(&styled);
        }
        if self.default_style.italic() {
            styled = (self.theme.italic)(&styled);
        }
        if self.default_style.strikethrough() {
            styled = (self.theme.strikethrough)(&styled);
        }
        if self.default_style.underline() {
            styled = (self.theme.underline)(&styled);
        }
        styled
    }

    fn render_lines(&self, width: u16) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        if self.text.trim().is_empty() {
            return Vec::new();
        }

        let content_width =
            usize::from(width.saturating_sub(self.padding_x.saturating_mul(2))).max(1);
        let normalized = self.text.replace('\t', "   ");
        let trimmed = strip_trailing_partial_closing_fence(&normalized);

        let rendered = render_markdown(&trimmed, content_width, &self.theme, &self.options, |t| {
            self.apply_default_style(t)
        });

        let mut wrapped: Vec<String> = Vec::new();
        for line in rendered {
            if is_image_line(&line) {
                wrapped.push(line);
            } else {
                for w in wrap_text_with_ansi(&line, content_width) {
                    wrapped.push(w);
                }
            }
        }

        let left = " ".repeat(usize::from(self.padding_x));
        let right = left.clone();
        let bg: Option<BackgroundFn> = self
            .default_style
            .bg_color
            .map(|f| Box::new(move |s: &str| f(s)) as BackgroundFn);
        let bg_ref = bg.as_deref();

        let mut content_lines = Vec::with_capacity(wrapped.len());
        for line in wrapped {
            if is_image_line(&line) {
                content_lines.push(line);
                continue;
            }
            let with_margins = format!("{left}{line}{right}");
            content_lines.push(apply_background(&with_margins, usize::from(width), bg_ref));
        }

        let empty = match bg_ref {
            Some(f) => f(&empty_line(usize::from(width))),
            None => empty_line(usize::from(width)),
        };
        let mut result = Vec::new();
        for _ in 0..self.padding_y {
            result.push(empty.clone());
        }
        result.extend(content_lines);
        for _ in 0..self.padding_y {
            result.push(empty.clone());
        }
        if result.is_empty() {
            vec![String::new()]
        } else {
            result
        }
    }

    fn lines_for_width(&mut self, width: u16) -> &[KeyedLine] {
        let cache_hit = match &self.cache {
            Some(cache) => cache.width == width && cache.text == self.text,
            None => false,
        };
        if !cache_hit {
            // Key each line once at cache fill (Design E): the paint walk
            // then reuses the key every frame instead of re-hashing.
            let lines = self
                .render_lines(width)
                .into_iter()
                .map(|line| KeyedLine::new(line, width))
                .collect();
            self.cache = Some(Cache {
                text: self.text.clone(),
                width,
                lines,
            });
        }
        self.cache
            .as_ref()
            .map(|cache| cache.lines.as_slice())
            .unwrap_or_default()
    }
}

impl Component for Markdown {
    fn measure(&mut self, width: u16) -> u16 {
        let lines = self.lines_for_width(width);
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let lines = self.lines_for_width(area.width);
        paint_lines_keyed(area, buf, lines);
    }

    fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
        EventResult::Ignored
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }
}

/// Preprocess LaTeX math delimiters in markdown source, rendering supported
/// expressions to Unicode text. Implements the upstream marked-extension
/// delimiter contract: block `$$…$$` and `\[…]` first, then inline `$$…$$`,
/// `\(...\)`, `\[…]`, and single `$…$` with four rejection rules.
///
/// Code fences and inline code spans are excluded — math delimiters inside
/// them stay literal. Escaped `\$` is consumed as a markdown escape before
/// the math path sees it.
/// A `CommonMark` fence line: up to three leading spaces then `` ``` `` or `~~~`
/// (at least three markers, optionally followed by an info string).
fn is_fence_line(line: &str) -> bool {
    let stripped = line.strip_prefix("   ").or_else(|| line.strip_prefix("  "));
    let rest = stripped.unwrap_or(line.trim_start_matches(' '));
    rest.starts_with("```") || rest.starts_with("~~~")
}

fn preprocess_math(source: &str) -> String {
    // Fast path: no math delimiters at all
    if !source.contains('$') && !source.contains("\\[") && !source.contains("\\(") {
        return source.to_owned();
    }

    let mut result = String::with_capacity(source.len());
    let lines: Vec<&str> = source.split('\n').collect();
    let mut i = 0;
    let mut in_fence = false;
    while i < lines.len() {
        // Fenced code blocks pass through verbatim: math delimiters inside
        // them stay literal (the code-fence exclusion contract).
        if is_fence_line(lines[i]) {
            in_fence = !in_fence;
            result.push_str(lines[i]);
            if i + 1 < lines.len() {
                result.push('\n');
            }
            i += 1;
            continue;
        }
        if in_fence {
            result.push_str(lines[i]);
            if i + 1 < lines.len() {
                result.push('\n');
            }
            i += 1;
            continue;
        }
        // Check for block math: ^ {0,3}$$...$$ or ^ {0,3}\[...\]
        if let Some((rendered, consumed)) = try_block_math(&lines[i..]) {
            for line in rendered.split('\n') {
                result.push_str(line);
                result.push('\n');
            }
            i += consumed;
            continue;
        }

        // Process inline math within the line
        result.push_str(&process_inline_math(lines[i]));
        if i + 1 < lines.len() {
            result.push('\n');
        }
        i += 1;
    }
    result
}

/// Try to match block math starting at the given lines.
/// Returns (`rendered_text`, `lines_consumed`) if matched, None otherwise.
fn try_block_math(lines: &[&str]) -> Option<(String, usize)> {
    let first = lines.first()?;

    // $$...$$ block
    if let Some(rest) = strip_leading_spaces(first, "$$") {
        let after_open = rest.trim_start();

        // Check if opener line has content after $$
        if let Some(end_idx) = find_block_closer_dollar(after_open) {
            let body = after_open[..end_idx].trim();
            let rendered = render_latex(body, true).unwrap_or_else(|| format!("$${body}$$"));
            return Some((rendered, 1));
        }

        // Multi-line: collect until closing $$
        let mut search_lines = vec![after_open.to_owned()];
        for (idx, line) in lines.iter().enumerate().skip(1) {
            if let Some(pos) = find_block_closer_dollar(line) {
                let before = &line[..pos];
                if !before.trim().is_empty() {
                    search_lines.push(before.to_owned());
                }
                let body = search_lines.join("\n").trim().to_owned();
                let rendered = render_latex(&body, true).unwrap_or_else(|| format!("$${body}$$"));
                let after = &line[pos + 2..];
                let mut result = rendered;
                if !after.trim().is_empty() {
                    result.push('\n');
                    result.push_str(after.trim());
                }
                return Some((result, idx + 1));
            }
            search_lines.push(line.to_string());
        }
        // Pending (unclosed) $$ block — check if body looks like math
        if looks_like_pending_dollar_math(&search_lines.join("\n")) {
            return Some((format!("$${}", search_lines.join("\n")), lines.len()));
        }
        return None;
    }

    // \[...\] block
    if let Some(rest) = strip_leading_spaces(first, "\\[") {
        let after_open = rest.trim_start();
        // Check if opener line has content after \[
        if let Some(end_idx) = find_block_closer_bracket(after_open) {
            let body = after_open[..end_idx].trim();
            let rendered = render_latex(body, true).unwrap_or_else(|| format!("\\[{body}\\]"));
            return Some((rendered, 1));
        }

        // Multi-line: collect until closing \]
        let mut search_lines = vec![after_open.to_owned()];
        for (idx, line) in lines.iter().enumerate().skip(1) {
            if let Some(pos) = line.find("\\]") {
                let before = &line[..pos];
                if !before.trim().is_empty() {
                    search_lines.push(before.to_owned());
                }
                let body = search_lines.join("\n").trim().to_owned();
                let rendered = render_latex(&body, true).unwrap_or_else(|| format!("\\[{body}\\]"));
                let after = &line[pos + 2..];
                let mut result = rendered;
                if !after.trim().is_empty() {
                    result.push('\n');
                    result.push_str(after.trim());
                }
                return Some((result, idx + 1));
            }
            search_lines.push(line.to_string());
        }
        // Pending \[ block — always pending
        let mut result = String::from("\\[\n");
        result.push_str(&search_lines.join("\n"));
        return Some((result, lines.len()));
    }

    None
}

/// Strip up to 3 leading spaces then match the prefix. Returns the rest after prefix.
fn strip_leading_spaces<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let mut chars = line.chars();
    let mut spaces = 0;
    while spaces < 3 {
        match chars.clone().next() {
            Some(' ') => {
                chars.next();
                spaces += 1;
            }
            _ => break,
        }
    }
    let rest: &str = chars.as_str();
    rest.starts_with(prefix).then(|| &rest[prefix.len()..])
}

/// Find the closing $$ in a string, respecting backslash escapes.
fn find_block_closer_dollar(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'$' && bytes[i + 1] == b'$' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Find the closing \] in a string.
fn find_block_closer_bracket(s: &str) -> Option<usize> {
    s.find("\\]")
}

/// Check if pending dollar math body looks like math.
fn looks_like_pending_dollar_math(source: &str) -> bool {
    source.contains('\\')
        || source.contains('_')
        || source.contains('^')
        || source.contains('=')
        || source.contains('+')
        || source.contains('*')
        || source.contains('/')
        || source.contains('<')
        || source.contains('>')
        || source.contains('(')
        || source.contains(')')
        || source.contains('[')
        || source.contains(']')
        || source.contains('|')
        || source.contains("±")
        || source.contains("≤")
        || source.contains("≥")
        || source.contains("≠")
        || source.contains("≈")
        || source.contains("∈")
        || source.contains("→")
        || source.contains("⇒")
        || source.contains("∞")
        || source.contains("∫")
        || source.contains("∑")
        || source.contains("√")
        || source.contains('-')
}

/// Process inline math delimiters within a single line.
fn process_inline_math(line: &str) -> String {
    if !line.contains('$') && !line.contains("\\(") && !line.contains("\\[") {
        return line.to_owned();
    }

    let mut result = String::with_capacity(line.len());
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Skip inline code spans (backtick-delimited)
        if chars[i] == '`' {
            // Find matching backtick
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '`' {
                j += 1;
            }
            if j < chars.len() {
                // Include the code span verbatim
                let span: String = chars[i..=j].iter().collect();
                result.push_str(&span);
                i = j + 1;
                continue;
            }
        }

        // Check for $$ inline
        if i + 1 < chars.len()
            && chars[i] == '$'
            && chars[i + 1] == '$'
            && let Some((rendered, end)) = match_inline_dollar_dollar(&chars, i)
        {
            result.push_str(&rendered);
            i = end;
            continue;
        }

        // Check for \( inline
        if i + 1 < chars.len()
            && chars[i] == '\\'
            && chars[i + 1] == '('
            && let Some((rendered, end)) = match_inline_paren(&chars, i)
        {
            result.push_str(&rendered);
            i = end;
            continue;
        }

        // Check for \[ inline (single-line only)
        if i + 1 < chars.len()
            && chars[i] == '\\'
            && chars[i + 1] == '['
            && let Some((rendered, end)) = match_inline_bracket(&chars, i)
        {
            result.push_str(&rendered);
            i = end;
            continue;
        }

        // Check for single $ inline
        if chars[i] == '$' {
            // Reject if followed by whitespace
            if i + 1 < chars.len() && (chars[i + 1] == ' ' || chars[i + 1] == '\t') {
                result.push(chars[i]);
                i += 1;
                continue;
            }
            if i + 1 >= chars.len() {
                result.push(chars[i]);
                i += 1;
                continue;
            }
            if let Some((rendered, end)) = match_single_dollar(&chars, i) {
                result.push_str(&rendered);
                i = end;
                continue;
            }
        }

        // Escaped \$ — pass through as-is (pulldown-cmark will handle the escape)
        if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '$' {
            result.push(chars[i]);
            result.push(chars[i + 1]);
            i += 2;
            continue;
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Match $$...$$ inline math. Returns (rendered, `end_index`).
fn match_inline_dollar_dollar(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut i = start + 2;
    while i + 1 < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == '$' && chars[i + 1] == '$' {
            let body: String = chars[start + 2..i].iter().collect();
            if body.is_empty() || body.contains('\n') {
                return None;
            }
            let rendered = render_latex(&body, false).unwrap_or_else(|| format!("$${body}$$"));
            return Some((rendered, i + 2));
        }
        i += 1;
    }
    // Pending
    let body: String = chars[start + 2..].iter().collect();
    if looks_like_pending_dollar_math(&body) {
        return Some((format!("$${body}"), chars.len()));
    }
    None
}

/// Match \(...\) inline math. Returns (rendered, `end_index`).
fn match_inline_paren(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut i = start + 2;
    while i + 1 < chars.len() {
        // Check for closing \) before generic backslash skip
        if chars[i] == '\\' && chars[i + 1] == ')' {
            let body: String = chars[start + 2..i].iter().collect();
            if body.is_empty() || body.contains('\n') {
                return None;
            }
            let rendered = render_latex(&body, false).unwrap_or_else(|| format!("\\({body}\\)"));
            return Some((rendered, i + 2));
        }
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        i += 1;
    }
    // Pending — \(\ is always pending
    let body: String = chars[start + 2..].iter().collect();
    Some((format!("\\({body}"), chars.len()))
}

/// Match \[...\] inline math (single-line). Returns (rendered, `end_index`).
fn match_inline_bracket(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut i = start + 2;
    while i + 1 < chars.len() {
        // Check for closing \] before generic backslash skip
        if chars[i] == '\\' && chars[i + 1] == ']' {
            let body: String = chars[start + 2..i].iter().collect();
            if body.is_empty() || body.contains('\n') {
                return None;
            }
            let rendered = render_latex(&body, false).unwrap_or_else(|| format!("\\[{body}\\]"));
            return Some((rendered, i + 2));
        }
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        i += 1;
    }
    // Pending
    let body: String = chars[start + 2..].iter().collect();
    Some((format!("\\[{body}"), chars.len()))
}

/// Match single $...$ inline math with four rejection rules.
/// Returns (rendered, `end_index`).
fn match_single_dollar(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut i = start + 1;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == '$' {
            let body: String = chars[start + 1..i].iter().collect();
            if body.is_empty() || body.contains('\n') {
                return None;
            }

            // Rejection rule 1: inner text ends with whitespace
            if body.ends_with(' ') || body.ends_with('\t') {
                return None;
            }
            // Rejection rule 2: char after closing $ is a digit (currency)
            if i + 1 < chars.len() && chars[i + 1].is_ascii_digit() {
                return None;
            }
            // Rejection rule 3: ALL-CAPS identifier followed by identifier start (shell vars)
            if is_all_caps_identifier(&body)
                && i + 1 < chars.len()
                && (chars[i + 1].is_alphabetic() || chars[i + 1] == '_')
            {
                return None;
            }
            // Rejection rule 4: inner text contains backtick (code span)
            if body.contains('`') {
                return None;
            }

            let rendered = render_latex(&body, false).unwrap_or_else(|| format!("${body}$"));
            return Some((rendered, i + 1));
        }
        i += 1;
    }
    // Pending — only if body looks like math
    let body: String = chars[start + 1..].iter().collect();
    if looks_like_pending_dollar_math(&body) {
        return Some((format!("${body}"), chars.len()));
    }
    None
}

/// Check if a string is an ALL-CAPS identifier (optionally one trailing punctuation)
/// per the upstream regex: /^[A-Z_][A-Z0-9_]*(?:[^A-Za-z0-9_\s])?$/
fn is_all_caps_identifier(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return false;
    }
    // First char must be [A-Z_]
    if !chars[0].is_ascii_uppercase() && chars[0] != '_' {
        return false;
    }
    // Rest must be [A-Z0-9_]* optionally followed by one non-alphanumeric-non-space
    let mut i = 1;
    while i < chars.len()
        && (chars[i].is_ascii_uppercase() || chars[i].is_ascii_digit() || chars[i] == '_')
    {
        i += 1;
    }
    if i == chars.len() {
        return true;
    }
    // One trailing non-alphanumeric-non-whitespace
    if i + 1 == chars.len() && !chars[i].is_alphanumeric() && !chars[i].is_whitespace() {
        return true;
    }
    false
}

fn render_markdown(
    text: &str,
    width: usize,
    theme: &MarkdownTheme,
    options: &MarkdownOptions,
    apply_default: impl Fn(&str) -> String,
) -> Vec<String> {
    let transformed = if options.render_latex {
        preprocess_math(text)
    } else {
        text.to_owned()
    };
    let mut parser_options = Options::empty();
    parser_options.insert(Options::ENABLE_TABLES);
    parser_options.insert(Options::ENABLE_STRIKETHROUGH);
    parser_options.insert(Options::ENABLE_TASKLISTS);

    let mut renderer = MarkdownRenderer::new(&transformed, width, theme, options, &apply_default);
    for (event, range) in Parser::new_ext(&transformed, parser_options).into_offset_iter() {
        renderer.consume(event, range);
    }
    renderer.finish()
}

struct MarkdownRenderer<'a> {
    source: &'a str,
    width: usize,
    theme: &'a MarkdownTheme,
    options: &'a MarkdownOptions,
    apply_default: &'a dyn Fn(&str) -> String,
    lines: Vec<String>,
    inline: String,
    lists: Vec<ListState>,
    heading: Option<u8>,
    quote_depth: usize,
    /// One segment buffer per open blockquote depth.
    quote_stack: Vec<Vec<ItemSegment>>,
    pending_paragraph_end: Option<usize>,
    code_lang: Option<String>,
    code_body: String,
    in_code: bool,
    link_href: Option<String>,
    link_text: String,
    table: Option<TableBuilder>,
    strong: usize,
    emphasis: usize,
    strike: usize,
}

/// Absolute ceiling on blockquote nesting levels that still receive a border
/// and a wrap at their own content width. The effective cap used at runtime
/// is derived from the available width: each bordered level spends
/// `border_w` columns on the border, so once `content_width` would saturate
/// at 1 the bordered transform only re-splits accumulated lines without
/// adding any real content columns — the line count then grows
/// multiplicatively with depth. Folding raw below the width-derived cap
/// keeps output size linear in input size at every terminal width.
const MAX_BORDERED_QUOTE_DEPTH: usize = 16;

impl<'a> MarkdownRenderer<'a> {
    fn new(
        source: &'a str,
        width: usize,
        theme: &'a MarkdownTheme,
        options: &'a MarkdownOptions,
        apply_default: &'a dyn Fn(&str) -> String,
    ) -> Self {
        Self {
            source,
            width,
            theme,
            options,
            apply_default,
            lines: Vec::new(),
            inline: String::new(),
            lists: Vec::new(),
            heading: None,
            quote_depth: 0,
            quote_stack: Vec::new(),
            pending_paragraph_end: None,
            code_lang: None,
            code_body: String::new(),
            in_code: false,
            link_href: None,
            link_text: String::new(),
            table: None,
            strong: 0,
            emphasis: 0,
            strike: 0,
        }
    }

    fn consume(&mut self, event: Event<'_>, range: Range<usize>) {
        if is_list_event(&event) {
            self.consume_list(&event, range);
            return;
        }
        match event {
            Event::Start(
                Tag::Paragraph | Tag::Heading { .. } | Tag::BlockQuote(_) | Tag::CodeBlock(_),
            )
            | Event::End(
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::BlockQuote(_) | TagEnd::CodeBlock,
            ) => self.consume_block(event, range),
            Event::Start(Tag::Table(_) | Tag::TableHead | Tag::TableRow | Tag::TableCell)
            | Event::End(
                TagEnd::Table | TagEnd::TableHead | TagEnd::TableRow | TagEnd::TableCell,
            ) => self.consume_table(event),
            Event::Text(_)
            | Event::Code(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::Start(Tag::Strong | Tag::Emphasis | Tag::Strikethrough | Tag::Link { .. })
            | Event::End(
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link,
            )
            | Event::Html(_)
            | Event::InlineHtml(_) => self.consume_inline(event),
            Event::FootnoteReference(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::Start(_)
            | Event::End(_)
            | Event::TaskListMarker(_)
            | Event::Rule => {}
        }
    }

    fn consume_block(&mut self, event: Event<'_>, range: Range<usize>) {
        if matches!(&event, Event::Start(_)) {
            self.pending_paragraph_end = None;
        }
        match event {
            Event::Start(Tag::Paragraph) => {
                if let Some(list) = self.lists.last_mut() {
                    list.loose = true;
                }
                self.inline.clear();
            }
            Event::End(TagEnd::Paragraph) => {
                let quote_is_inside_item = self.quote_depth
                    > self
                        .lists
                        .last()
                        .map_or(0, |list| list.quote_depth_at_start);
                if quote_is_inside_item {
                    self.flush_inline_to_quote();
                } else if self.lists.is_empty() {
                    self.flush_inline(None);
                    self.lines.push(String::new());
                    self.pending_paragraph_end = Some(range.end);
                } else {
                    self.flush_inline_to_segment();
                }
            }
            Event::Start(Tag::Heading { level, .. }) => {
                if !self.lists.is_empty() && !self.inline.is_empty() {
                    self.flush_inline_to_segment();
                }
                self.inline.clear();
                self.heading = Some(level as u8);
            }
            Event::End(TagEnd::Heading(_)) => {
                if self.quote_depth > 0 || !self.lists.is_empty() {
                    let heading = self.heading.take();
                    self.flush_heading_to_segment(heading);
                } else {
                    self.flush_inline(self.heading);
                    self.heading = None;
                    self.lines.push(String::new());
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                if !self.lists.is_empty() && !self.inline.is_empty() {
                    self.flush_inline_to_segment();
                }
                self.quote_depth += 1;
                self.quote_stack.push(Vec::new());
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                let segments = self.quote_stack.pop().unwrap_or_default();
                self.quote_depth = self.quote_stack.len();
                let quote_belongs_to_item = self
                    .lists
                    .last()
                    .is_some_and(|list| list.quote_depth_at_start == self.quote_depth);
                if quote_belongs_to_item {
                    self.push_list_segment(ItemSegment::Quote(segments));
                } else if self.quote_depth > 0 {
                    self.push_quote_segment(ItemSegment::Quote(segments));
                } else {
                    self.lines
                        .extend(self.render_quoted_segments(segments, self.width));
                    self.lines.push(String::new());
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                if !self.lists.is_empty() && !self.inline.is_empty() {
                    self.flush_inline_to_segment();
                }
                self.start_code_block(kind);
            }
            Event::End(TagEnd::CodeBlock) => self.finish_code_block(),
            _ => {}
        }
    }

    fn start_code_block(&mut self, kind: CodeBlockKind<'_>) {
        self.in_code = true;
        self.code_body.clear();
        self.code_lang = match kind {
            CodeBlockKind::Fenced(lang) if !lang.trim().is_empty() => Some(lang.trim().to_owned()),
            CodeBlockKind::Fenced(_) | CodeBlockKind::Indented => None,
        };
    }

    fn finish_code_block(&mut self) {
        self.in_code = false;
        let lang = self.code_lang.take();
        let body = std::mem::take(&mut self.code_body);
        if self.quote_depth > 0 || !self.lists.is_empty() {
            self.push_list_segment(ItemSegment::Code { lang, body });
            return;
        }
        self.lines
            .extend(self.code_block_logical_lines(lang.as_deref(), &body));
        self.lines.push(String::new());
    }

    fn consume_inline(&mut self, event: Event<'_>) {
        match event {
            Event::Text(text) => self.consume_text(&text),
            Event::Code(text) => {
                let styled = (self.theme.code)(&text);
                if self.link_href.is_some() {
                    self.link_text.push_str(&styled);
                } else {
                    self.inline.push_str(&styled);
                }
            }
            Event::SoftBreak | Event::HardBreak => self.inline.push('\n'),
            Event::Start(Tag::Strong) => self.strong += 1,
            Event::End(TagEnd::Strong) => self.strong = self.strong.saturating_sub(1),
            Event::Start(Tag::Emphasis) => self.emphasis += 1,
            Event::End(TagEnd::Emphasis) => self.emphasis = self.emphasis.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => self.strike += 1,
            Event::End(TagEnd::Strikethrough) => self.strike = self.strike.saturating_sub(1),
            Event::Start(Tag::Link { dest_url, .. }) => {
                self.link_href = Some(dest_url.into_string());
                self.link_text.clear();
            }
            Event::End(TagEnd::Link) => self.finish_link(),
            Event::Html(html) | Event::InlineHtml(html) => self.inline.push_str(&html),
            _ => {}
        }
    }

    fn consume_text(&mut self, text: &str) {
        if self.in_code {
            self.code_body.push_str(text);
            return;
        }
        if self.link_href.is_some() {
            self.link_text.push_str(text);
            return;
        }
        let mut chunk = text.to_owned();
        if self.strong > 0 {
            chunk = (self.theme.bold)(&chunk);
        }
        if self.emphasis > 0 {
            chunk = (self.theme.italic)(&chunk);
        }
        if self.strike > 0 {
            chunk = (self.theme.strikethrough)(&chunk);
        }
        self.inline.push_str(&chunk);
    }

    fn finish_link(&mut self) {
        let Some(href) = self.link_href.take() else {
            return;
        };
        let styled = (self.theme.link)(&(self.theme.underline)(&self.link_text));
        let rendered = if self.options.hyperlinks {
            hyperlink_capped(&styled, &href, None)
        } else {
            let comparable = href.strip_prefix("mailto:").unwrap_or(&href);
            if self.link_text == href || self.link_text == comparable {
                styled
            } else {
                format!("{styled}{}", (self.theme.link_url)(&format!(" ({href})")))
            }
        };
        self.inline.push_str(&rendered);
        self.link_text.clear();
    }

    fn consume_list(&mut self, event: &Event<'_>, range: Range<usize>) {
        match event {
            Event::Start(Tag::List(start)) => {
                if self.lists.is_empty()
                    && self.quote_depth == 0
                    && self.pending_paragraph_end.take().is_some()
                    && self.lines.last().is_some_and(String::is_empty)
                {
                    let newline_count = self.source[..range.start.min(self.source.len())]
                        .chars()
                        .rev()
                        .take_while(|ch| ch.is_whitespace())
                        .filter(|ch| *ch == '\n')
                        .count();
                    if newline_count < 2 {
                        self.lines.pop();
                    }
                } else {
                    self.pending_paragraph_end = None;
                }
                // Flush pending inline into parent item before nesting.
                if !self.lists.is_empty() && !self.inline.is_empty() {
                    self.flush_inline_to_segment();
                }
                let ordered = start.is_some();
                self.lists.push(ListState {
                    ordered,
                    next_index: start.unwrap_or(1),
                    task: None,
                    loose: false,
                    source_marker: None,
                    quote_depth_at_start: self.quote_depth,
                    segments: Vec::new(),
                    finished_items: Vec::new(),
                });
            }
            Event::Start(Tag::Item) => {
                if let Some(list) = self.lists.last_mut() {
                    list.segments.clear();
                    list.task = None;
                    let item_src = &self.source[range.start..range.end.min(self.source.len())];
                    list.source_marker = extract_source_marker(item_src, list.ordered);
                }
                self.inline.clear();
            }
            Event::End(TagEnd::List(_)) => self.finish_list(range),
            Event::End(TagEnd::Item) => self.finish_list_item(),
            Event::TaskListMarker(checked) => {
                if let Some(list) = self.lists.last_mut() {
                    list.task = Some(if *checked { "[x] " } else { "[ ] " }.to_owned());
                }
            }
            Event::Rule => {
                self.pending_paragraph_end = None;
                let line = (self.theme.hr)(&"─".repeat(self.width.min(80)));
                if self.quote_depth > 0 {
                    self.push_list_segment(ItemSegment::Lines(vec![line]));
                } else if self.lists.is_empty() {
                    self.lines.push(line);
                    self.lines.push(String::new());
                } else {
                    self.push_list_segment(ItemSegment::Lines(vec![line]));
                }
            }
            _ => {}
        }
    }

    fn finish_list(&mut self, range: Range<usize>) {
        let is_top_level = self.lists.len() == 1;
        let finished = {
            let Some(list) = self.lists.last_mut() else {
                // End(List) without matching Start(List): defensive no-op.
                return;
            };
            let items = std::mem::take(&mut list.finished_items);
            let loose = list.loose;
            let quote_depth_at_start = list.quote_depth_at_start;
            let mut out: Vec<String> = Vec::new();
            let count = items.len();
            for (i, item_lines) in items.into_iter().enumerate() {
                out.extend(item_lines);
                if loose && i + 1 < count {
                    out.push(String::new());
                }
            }
            (out, loose, quote_depth_at_start)
        };

        self.lists.pop();

        if is_top_level && self.quote_depth > 0 {
            self.push_quote_segment(ItemSegment::Nested(finished.0));
        } else if is_top_level {
            self.lines.extend(finished.0);
            // Space-between-lists parity: if the list source range ends with `\n\n`,
            // push a blank line.
            if range.end <= self.source.len() {
                let tail = &self.source[range.start..range.end];
                if tail.ends_with("\n\n") {
                    self.lines.push(String::new());
                }
            }
        } else if self
            .lists
            .last()
            .is_some_and(|parent| finished.2 > parent.quote_depth_at_start)
        {
            self.push_quote_segment(ItemSegment::Nested(finished.0));
        } else if let Some(parent) = self.lists.last_mut() {
            // Nested list end: flatten child items into the parent item.
            parent.segments.push(ItemSegment::Nested(finished.0));
        }
    }

    #[allow(clippy::too_many_lines)]
    fn finish_list_item(&mut self) {
        // Flush any pending inline as a final Text segment for tight items.
        self.flush_inline_to_segment();

        let depth = self.lists.len().saturating_sub(1);
        let indent = "    ".repeat(depth);

        let (first_prefix, continuation_prefix, item_width, segments) = {
            let Some(list) = self.lists.last_mut() else {
                return;
            };

            let bullet = take_marker(
                list.ordered,
                list.next_index,
                list.source_marker.as_deref(),
                self.options.preserve_ordered_list_markers,
            );
            let task = list.task.take().unwrap_or_default();
            let marker = format!("{bullet}{task}");
            let first_prefix = format!("{indent}{}", (self.theme.list_bullet)(&marker));
            let continuation_prefix = format!("{indent}{}", " ".repeat(visible_width(&marker)));
            let item_width = self
                .width
                .saturating_sub(visible_width(&first_prefix))
                .max(1);

            let segments = std::mem::take(&mut list.segments);
            list.source_marker = None;
            if list.ordered {
                list.next_index = list.next_index.saturating_add(1);
            }
            (first_prefix, continuation_prefix, item_width, segments)
        };

        let mut composed: Vec<String> = Vec::new();
        let mut rendered_any = false;

        for seg in segments {
            match seg {
                ItemSegment::Text(text) => {
                    let styled = if text.is_empty() {
                        text
                    } else {
                        (self.apply_default)(&text)
                    };
                    emit_wrapped(
                        &mut composed,
                        &mut rendered_any,
                        &first_prefix,
                        &continuation_prefix,
                        &styled,
                        item_width,
                    );
                }
                ItemSegment::Blank => {
                    if rendered_any {
                        composed.push(continuation_prefix.clone());
                    }
                }
                ItemSegment::Nested(lines) => {
                    for line in lines {
                        composed.push(line);
                        rendered_any = true;
                    }
                }
                ItemSegment::Quote(segments) => {
                    for line in self.render_quoted_segments(segments, item_width) {
                        let prefix = if rendered_any {
                            &continuation_prefix
                        } else {
                            &first_prefix
                        };
                        composed.push(format!("{prefix}{line}"));
                        rendered_any = true;
                    }
                }
                ItemSegment::Code { lang, body } => {
                    for line in self.code_block_logical_lines(lang.as_deref(), &body) {
                        emit_wrapped(
                            &mut composed,
                            &mut rendered_any,
                            &first_prefix,
                            &continuation_prefix,
                            &line,
                            item_width,
                        );
                    }
                }
                ItemSegment::Lines(lines) => {
                    for line in lines {
                        emit_wrapped(
                            &mut composed,
                            &mut rendered_any,
                            &first_prefix,
                            &continuation_prefix,
                            &line,
                            item_width,
                        );
                    }
                }
                ItemSegment::Table(table) => {
                    for line in render_table(&table, item_width, self.theme) {
                        emit_wrapped(
                            &mut composed,
                            &mut rendered_any,
                            &first_prefix,
                            &continuation_prefix,
                            &line,
                            item_width,
                        );
                    }
                }
            }
        }

        if !rendered_any {
            composed.push(first_prefix);
        }

        if let Some(list) = self.lists.last_mut() {
            list.finished_items.push(composed);
        }
    }

    /// Flush pending `inline` into a Text segment on the current list item.
    /// If there are already segments and inline is non-empty, insert a Blank
    /// separator to mark loose multi-paragraph content.
    fn flush_inline_to_segment(&mut self) {
        let Some(list) = self.lists.last_mut() else {
            return;
        };
        if self.inline.is_empty() {
            return;
        }
        if !list.segments.is_empty() {
            list.loose = true;
            list.segments.push(ItemSegment::Blank);
        }
        let text = std::mem::take(&mut self.inline);
        list.segments.push(ItemSegment::Text(text));
    }

    fn flush_inline_to_quote(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.inline);
        self.push_quote_segment(ItemSegment::Text(text));
    }

    fn push_quote_segment(&mut self, segment: ItemSegment) {
        let Some(quote) = self.quote_stack.last_mut() else {
            return;
        };
        let omit_separator = quote.is_empty()
            || matches!(quote.last(), Some(ItemSegment::Blank))
            || matches!(
                (quote.last(), &segment),
                (Some(ItemSegment::Text(_)), ItemSegment::Nested(_))
            );
        if !omit_separator {
            quote.push(ItemSegment::Blank);
        }
        quote.push(segment);
    }

    fn render_quoted_segments(&self, segments: Vec<ItemSegment>, width: usize) -> Vec<String> {
        struct Frame {
            pending: std::vec::IntoIter<ItemSegment>,
            content_width: usize,
            depth: usize,
            raw: Vec<String>,
        }
        let border = (self.theme.quote_border)("│ ");
        let border_w = visible_width("│ ");
        let quote_style = |text: &str| (self.theme.quote)(&(self.theme.italic)(text));
        let styled_sentinel = quote_style("\0");
        let style_prefix = styled_sentinel
            .find('\0')
            .map_or("", |index| &styled_sentinel[..index]);
        let reset_with_style =
            (!style_prefix.is_empty()).then(|| format!("\u{1b}[0m{style_prefix}"));

        // Iterative depth-safe traversal: each stack frame carries its own
        // `content_width` (shrunk by one border per nesting level), its nesting
        // `depth`, and the raw lines collected so far. When a `Quote` segment
        // is encountered we suspend the current frame and push a child frame;
        // when a frame is exhausted we apply its border/style transform and
        // fold the bordered lines into the parent (frames past the
        // width-derived cap fold raw instead), so deeply nested
        // blockquotes neither recurse on the call stack nor blow up the line
        // count.
        let initial_width = width.saturating_sub(border_w).max(1);
        // Derive the effective depth cap from the available width: once
        // `content_width` would saturate at 1, a bordered level only
        // re-splits accumulated lines without adding real content columns,
        // so the line count grows multiplicatively. The cap is the largest
        // depth at which `content_width` is still at least 1 before the
        // `.max(1)` floor — i.e. `width - border_w * depth >= 1`.
        let effective_cap = MAX_BORDERED_QUOTE_DEPTH.min(width.saturating_sub(1) / border_w);
        let mut stack: Vec<Frame> = vec![Frame {
            pending: segments.into_iter(),
            content_width: initial_width,
            depth: 1,
            raw: Vec::new(),
        }];

        while let Some(frame) = stack.last_mut() {
            let Some(segment) = frame.pending.next() else {
                let depth = frame.depth;
                let content_width = frame.content_width;
                let raw = std::mem::take(&mut frame.raw);
                stack.pop();
                // Past the depth cap, fold raw lines into the parent without
                // the border/wrap transform: each bordered level shrinks the
                // parent's content width by one border while lengthening every
                // accumulated line by one, so wrapping re-splits those lines
                // and the line count grows multiplicatively with depth.
                // Folding raw keeps output size linear in input size.
                if depth > effective_cap {
                    match stack.last_mut() {
                        Some(parent) => parent.raw.extend(raw),
                        None => return raw,
                    }
                    continue;
                }
                // Frame exhausted below the cap: style, wrap, and border every
                // raw line at this frame's content width, then fold into the
                // parent.
                let reset = reset_with_style.clone();
                let bordered: Vec<String> = raw
                    .into_iter()
                    .flat_map(|line| {
                        let line = match &reset {
                            Some(reset) => line.replace("\u{1b}[0m", reset),
                            None => line,
                        };
                        let styled = quote_style(&line);
                        wrap_text_with_ansi(&styled, content_width)
                            .into_iter()
                            .map(|wrapped| format!("{border}{wrapped}"))
                    })
                    .collect();
                match stack.last_mut() {
                    Some(parent) => parent.raw.extend(bordered),
                    None => return bordered,
                }
                continue;
            };

            match segment {
                ItemSegment::Text(text) => frame.raw.push(text),
                ItemSegment::Blank => frame.raw.push(String::new()),
                ItemSegment::Nested(nested) | ItemSegment::Lines(nested) => {
                    frame.raw.extend(nested);
                }
                ItemSegment::Quote(nested) => {
                    let child_width = frame.content_width.saturating_sub(border_w).max(1);
                    let child_depth = frame.depth + 1;
                    stack.push(Frame {
                        pending: nested.into_iter(),
                        content_width: child_width,
                        depth: child_depth,
                        raw: Vec::new(),
                    });
                }
                ItemSegment::Code { lang, body } => {
                    frame
                        .raw
                        .extend(self.code_block_logical_lines(lang.as_deref(), &body));
                }
                ItemSegment::Table(table) => {
                    frame
                        .raw
                        .extend(render_table(&table, frame.content_width, self.theme));
                }
            }
        }

        // Unreachable: the initial frame always either returns from the
        // exhausted branch or is popped with a parent to fold into.
        Vec::new()
    }

    fn flush_heading_to_segment(&mut self, heading: Option<u8>) {
        if self.inline.is_empty() && heading.is_none() {
            return;
        }
        let raw = std::mem::take(&mut self.inline);
        let text = self.style_heading_text(raw, heading);
        self.push_list_segment(ItemSegment::Lines(vec![text]));
    }

    fn push_list_segment(&mut self, segment: ItemSegment) {
        let quote_is_inside_item = self.quote_depth
            > self
                .lists
                .last()
                .map_or(0, |list| list.quote_depth_at_start);
        if quote_is_inside_item {
            self.push_quote_segment(segment);
            return;
        }
        let Some(list) = self.lists.last_mut() else {
            return;
        };
        if !list.segments.is_empty() {
            list.loose = true;
            list.segments.push(ItemSegment::Blank);
        }
        list.segments.push(segment);
    }

    fn style_heading_text(&self, mut text: String, heading: Option<u8>) -> String {
        if let Some(level) = heading {
            let style = |value: &str| {
                if level == 1 {
                    (self.theme.heading)(&(self.theme.bold)(&(self.theme.underline)(value)))
                } else {
                    (self.theme.heading)(&(self.theme.bold)(value))
                }
            };
            if level >= 3 {
                text = format!(
                    "{}{}",
                    style(&format!("{} ", "#".repeat(level.into()))),
                    text
                );
            } else {
                text = style(&text);
            }
        }
        text
    }

    fn code_block_logical_lines(&self, lang: Option<&str>, body: &str) -> Vec<String> {
        let mut logical = Vec::new();
        let fence = format!("```{}", lang.unwrap_or(""));
        logical.push((self.theme.code_block_border)(&fence));
        let indent = &self.theme.code_block_indent;
        if let Some(highlight) = &self.theme.highlight_code {
            logical.extend(
                highlight(body, lang)
                    .into_iter()
                    .map(|line| format!("{indent}{line}")),
            );
        } else {
            logical.extend(
                body.split('\n')
                    .map(|line| format!("{indent}{}", (self.theme.code_block)(line))),
            );
            if body.ends_with('\n')
                && logical.last().is_some_and(|last| {
                    last == indent || last == &format!("{indent}{}", (self.theme.code_block)(""))
                })
            {
                logical.pop();
            }
        }
        logical.push((self.theme.code_block_border)("```"));
        logical
    }

    fn consume_table(&mut self, event: Event<'_>) {
        match event {
            Event::Start(Tag::Table(alignments)) => {
                self.pending_paragraph_end = None;
                if !self.lists.is_empty() && !self.inline.is_empty() {
                    self.flush_inline_to_segment();
                }
                self.table = Some(TableBuilder {
                    alignments: alignments.len(),
                    headers: Vec::new(),
                    rows: Vec::new(),
                    current_row: Vec::new(),
                    in_head: true,
                });
            }
            Event::End(TagEnd::Table) => {
                if let Some(table) = self.table.take() {
                    if self.quote_depth > 0 || !self.lists.is_empty() {
                        self.push_list_segment(ItemSegment::Table(table));
                    } else {
                        self.lines
                            .extend(render_table(&table, self.width, self.theme));
                        self.lines.push(String::new());
                    }
                }
            }
            Event::Start(Tag::TableHead) => self.set_table_head(true),
            Event::End(TagEnd::TableHead) => {
                // pulldown-cmark does not wrap the header row in TableRow
                // events, so finalize the accumulated cells here before
                // clearing the in-head flag; otherwise Start(TableRow)
                // discards the header data.
                self.finish_table_row();
                self.set_table_head(false);
            }
            Event::Start(Tag::TableRow) => {
                if let Some(table) = self.table.as_mut() {
                    table.current_row.clear();
                }
            }
            Event::End(TagEnd::TableRow) => self.finish_table_row(),
            Event::Start(Tag::TableCell) => self.inline.clear(),
            Event::End(TagEnd::TableCell) => {
                if let Some(table) = self.table.as_mut() {
                    table.current_row.push(std::mem::take(&mut self.inline));
                }
            }
            _ => {}
        }
    }

    fn set_table_head(&mut self, in_head: bool) {
        if let Some(table) = self.table.as_mut() {
            table.in_head = in_head;
        }
    }

    fn finish_table_row(&mut self) {
        let Some(table) = self.table.as_mut() else {
            return;
        };
        let row = std::mem::take(&mut table.current_row);
        if table.in_head && table.headers.is_empty() {
            table.headers = row;
        } else {
            table.rows.push(row);
        }
    }

    fn flush_inline(&mut self, heading: Option<u8>) {
        if self.inline.is_empty() && heading.is_none() {
            return;
        }
        let text = std::mem::take(&mut self.inline);
        let text = if heading.is_some() {
            self.style_heading_text(text, heading)
        } else if self.quote_depth == 0 {
            (self.apply_default)(&text)
        } else {
            text
        };

        if self.quote_depth > 0 {
            self.lines
                .push((self.theme.quote)(&(self.theme.italic)(&text)));
        } else {
            self.lines.push(text);
        }
    }

    fn finish(mut self) -> Vec<String> {
        if !self.inline.is_empty() {
            if self.lists.is_empty() {
                self.flush_inline(self.heading);
            } else {
                self.flush_inline_to_segment();
            }
        }
        while self.lines.last().is_some_and(String::is_empty) {
            self.lines.pop();
        }
        let _ = CowStr::from("");
        self.lines
    }
}

fn is_list_event(event: &Event<'_>) -> bool {
    matches!(
        event,
        Event::Start(Tag::List(_) | Tag::Item)
            | Event::End(TagEnd::List(_) | TagEnd::Item)
            | Event::TaskListMarker(_)
            | Event::Rule
    )
}

#[derive(Clone)]
struct ListState {
    ordered: bool,
    next_index: u64,
    task: Option<String>,
    loose: bool,
    quote_depth_at_start: usize,
    source_marker: Option<String>,
    segments: Vec<ItemSegment>,
    finished_items: Vec<Vec<String>>,
}

/// A segment of content within a list item.
#[derive(Clone)]
enum ItemSegment {
    Text(String),
    Blank,
    Nested(Vec<String>),
    /// Blockquote content deferred for item-width wrapping and border.
    Quote(Vec<ItemSegment>),
    /// Fenced/indented code deferred for item-width wrapping.
    Code {
        lang: Option<String>,
        body: String,
    },
    /// Pre-styled block lines (headings/tables) wrapped at item width.
    Lines(Vec<String>),
    /// Table deferred for item-width rendering.
    Table(TableBuilder),
}

/// Compose the bullet marker for a list item.
///
/// When `preserve` is true, use the source-extracted marker if available;
/// otherwise normalize to `N. ` for ordered lists and `- ` for unordered.
fn take_marker(
    ordered: bool,
    next_index: u64,
    source_marker: Option<&str>,
    preserve: bool,
) -> String {
    if preserve && let Some(marker) = source_marker {
        return marker.to_owned();
    }
    if ordered {
        format!("{next_index}. ")
    } else {
        "- ".to_owned()
    }
}

/// Extract the source list marker from the beginning of an item's source slice.
///
/// Ordered: `^(?: {0,3})(\d{1,9}[.)])[ \t]+` → `"{capture} "`
/// Unordered: `^(?: {0,3})([-+*])(?:[ \t]+|(?=\r?\n|$))` → `"{capture} "`
///
/// Returns `None` when the source does not exact-match so callers can fall back
/// to normalized `N. `/`- ` markers via [`take_marker`].
fn extract_source_marker(item_src: &str, ordered: bool) -> Option<String> {
    let bytes = item_src.as_bytes();
    let mut pos = 0;
    // Skip up to 3 leading spaces.
    while pos < bytes.len() && bytes[pos] == b' ' && pos < 3 {
        pos += 1;
    }
    if ordered {
        // \d{1,9}
        let digits_start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() && pos - digits_start < 9 {
            pos += 1;
        }
        let num_digits = pos - digits_start;
        if num_digits == 0 || num_digits > 9 {
            return None;
        }
        // [.)]
        if pos >= bytes.len() || (bytes[pos] != b'.' && bytes[pos] != b')') {
            return None;
        }
        let punct = bytes[pos];
        pos += 1;
        // [ \t]+
        if pos >= bytes.len() || (bytes[pos] != b' ' && bytes[pos] != b'\t') {
            return None;
        }
        // `pos` points at the first whitespace after digits+punct.
        let capture = &item_src[digits_start..pos - 1];
        Some(format!("{capture}{} ", punct as char))
    } else {
        // [-+*]
        if pos >= bytes.len() {
            return None;
        }
        let ch = bytes[pos];
        if ch != b'-' && ch != b'+' && ch != b'*' {
            return None;
        }
        pos += 1;
        // (?:[ \t]+|(?=\r?\n|$)) — bare CR is invalid; CRLF is ok.
        if pos < bytes.len() && (bytes[pos] == b' ' || bytes[pos] == b'\t') {
            // marker followed by spaces/tabs
        } else if pos < bytes.len() && bytes[pos] == b'\n' {
            // LF
        } else if pos + 1 < bytes.len() && bytes[pos] == b'\r' && bytes[pos + 1] == b'\n' {
            // CRLF
        } else if pos == bytes.len() {
            // EOF
        } else {
            return None;
        }
        Some(format!("{} ", ch as char))
    }
}

fn emit_wrapped(
    composed: &mut Vec<String>,
    rendered_any: &mut bool,
    first_prefix: &str,
    continuation_prefix: &str,
    text: &str,
    item_width: usize,
) {
    for wrapped in wrap_text_with_ansi(text, item_width) {
        let prefix = if *rendered_any {
            continuation_prefix
        } else {
            first_prefix
        };
        composed.push(format!("{prefix}{wrapped}"));
        *rendered_any = true;
    }
}

#[derive(Clone)]
struct TableBuilder {
    #[allow(dead_code)]
    alignments: usize,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    in_head: bool,
}

fn longest_word_width(text: &str, max: usize) -> usize {
    text.split_whitespace()
        .map(visible_width)
        .max()
        .unwrap_or(0)
        .min(max)
        .max(1)
}

fn render_table(
    table: &TableBuilder,
    available_width: usize,
    theme: &MarkdownTheme,
) -> Vec<String> {
    let num_cols = table
        .headers
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    if num_cols == 0 {
        return Vec::new();
    }

    let border_overhead = 3 * num_cols + 1;
    let available_for_cells = available_width.saturating_sub(border_overhead);
    if available_for_cells < num_cols {
        return render_narrow_table(table, available_width);
    }
    let column_widths = table_column_widths(table, num_cols, available_width, available_for_cells);
    render_table_grid(table, &column_widths, theme)
}

fn render_narrow_table(table: &TableBuilder, available_width: usize) -> Vec<String> {
    let mut raw = String::new();
    raw.push_str(&table.headers.join(" | "));
    raw.push('\n');
    for row in &table.rows {
        raw.push_str(&row.join(" | "));
        raw.push('\n');
    }
    wrap_text_with_ansi(raw.trim_end(), available_width.max(1))
}

fn table_column_widths(
    table: &TableBuilder,
    num_cols: usize,
    available_width: usize,
    available_for_cells: usize,
) -> Vec<usize> {
    let mut natural = vec![0usize; num_cols];
    let mut min_word = vec![1usize; num_cols];
    update_column_requirements(&table.headers, &mut natural, &mut min_word);
    for row in &table.rows {
        update_column_requirements(row, &mut natural, &mut min_word);
    }

    let mut minimums = minimum_column_widths(&min_word, available_for_cells);
    let minimum_total: usize = minimums.iter().sum();
    let border_overhead = 3 * num_cols + 1;
    let natural_total: usize = natural.iter().sum::<usize>() + border_overhead;
    if natural_total <= available_width {
        return natural
            .iter()
            .zip(minimums.iter())
            .map(|(natural, minimum)| (*natural).max(*minimum))
            .collect();
    }

    let growth_capacity: usize = natural
        .iter()
        .zip(minimums.iter())
        .map(|(natural, minimum)| natural.saturating_sub(*minimum))
        .sum();
    let extra = available_for_cells.saturating_sub(minimum_total);
    for (index, minimum) in minimums.iter_mut().enumerate() {
        let delta = natural[index].saturating_sub(*minimum);
        let growth = delta
            .checked_mul(extra)
            .and_then(|value| value.checked_div(growth_capacity))
            .unwrap_or(0);
        *minimum += growth;
    }
    distribute_remaining_width(&mut minimums, &natural, available_for_cells);
    minimums
}

fn update_column_requirements(row: &[String], natural: &mut [usize], minimum: &mut [usize]) {
    for (index, cell) in row.iter().enumerate().take(natural.len()) {
        natural[index] = natural[index].max(visible_width(cell));
        minimum[index] = minimum[index].max(longest_word_width(cell, 30));
    }
}

fn minimum_column_widths(minimum: &[usize], available: usize) -> Vec<usize> {
    if minimum.iter().sum::<usize>() <= available {
        return minimum.to_vec();
    }
    let mut widths = vec![1usize; minimum.len()];
    let remaining = available.saturating_sub(minimum.len());
    if remaining == 0 {
        return widths;
    }
    let total_weight: usize = minimum.iter().map(|width| width.saturating_sub(1)).sum();
    let mut growth: Vec<usize> = minimum
        .iter()
        .map(|width| {
            width
                .saturating_sub(1)
                .checked_mul(remaining)
                .and_then(|value| value.checked_div(total_weight))
                .unwrap_or(0)
        })
        .collect();
    let mut leftover = remaining.saturating_sub(growth.iter().sum());
    for item in &mut growth {
        *item += usize::from(leftover > 0);
        leftover = leftover.saturating_sub(1);
    }
    for (width, extra) in widths.iter_mut().zip(growth) {
        *width += extra;
    }
    widths
}

fn distribute_remaining_width(widths: &mut [usize], natural: &[usize], available: usize) {
    let mut remaining = available.saturating_sub(widths.iter().sum());
    while remaining > 0 {
        let mut grew = false;
        for (width, target) in widths.iter_mut().zip(natural) {
            if remaining == 0 {
                break;
            }
            if *width < *target {
                *width += 1;
                remaining -= 1;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
}

fn render_table_grid(
    table: &TableBuilder,
    column_widths: &[usize],
    theme: &MarkdownTheme,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(table_border(column_widths, "┌─", "─┬─", "─┐"));
    let header_cells = wrap_table_row(&table.headers, column_widths);
    append_table_row(&mut lines, &header_cells, column_widths, Some(theme.bold));

    let separator = table_border(column_widths, "├─", "─┼─", "─┤");
    lines.push(separator.clone());
    for (index, row) in table.rows.iter().enumerate() {
        let cells = wrap_table_row(row, column_widths);
        append_table_row(&mut lines, &cells, column_widths, None);
        if index + 1 < table.rows.len() {
            lines.push(separator.clone());
        }
    }
    lines.push(table_border(column_widths, "└─", "─┴─", "─┘"));
    lines
}

fn wrap_table_row(row: &[String], widths: &[usize]) -> Vec<Vec<String>> {
    widths
        .iter()
        .enumerate()
        .map(|(index, width)| {
            wrap_text_with_ansi(row.get(index).map_or("", String::as_str), (*width).max(1))
        })
        .collect()
}

fn append_table_row(
    lines: &mut Vec<String>,
    cells: &[Vec<String>],
    widths: &[usize],
    style: Option<fn(&str) -> String>,
) {
    let height = cells.iter().map(Vec::len).max().unwrap_or(1);
    for row in 0..height {
        let parts: Vec<String> = widths
            .iter()
            .enumerate()
            .map(|(column, width)| {
                let text = cells
                    .get(column)
                    .and_then(|cell| cell.get(row))
                    .cloned()
                    .unwrap_or_default();
                let padded = format!(
                    "{text}{}",
                    " ".repeat(width.saturating_sub(visible_width(&text)))
                );
                style.map_or(padded.clone(), |apply| apply(&padded))
            })
            .collect();
        lines.push(format!("│ {} │", parts.join(" │ ")));
    }
}

fn table_border(widths: &[usize], left: &str, separator: &str, right: &str) -> String {
    let body = widths
        .iter()
        .map(|width| "─".repeat(*width))
        .collect::<Vec<_>>()
        .join(separator);
    format!("{left}{body}{right}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::util::{render_snapshot, strip_ansi};

    fn plain(text: &str, width: u16, opts: MarkdownOptions) -> Vec<String> {
        let mut m = Markdown::new(
            text,
            0,
            0,
            MarkdownTheme::default(),
            DefaultTextStyle::default(),
            opts,
        );
        render_snapshot(&mut m, width)
            .into_iter()
            .map(|l| strip_ansi(&l).trim_end().to_string())
            .collect()
    }

    fn plain_default(text: &str, width: u16) -> Vec<String> {
        plain(text, width, MarkdownOptions::default())
    }

    fn plain_preserve(text: &str, width: u16) -> Vec<String> {
        plain(
            text,
            width,
            MarkdownOptions {
                preserve_ordered_list_markers: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn tight_task_list() {
        let lines = plain_default("- [ ] beep\n- [x] boop", 80);
        assert_eq!(lines, vec!["- [ ] beep", "- [x] boop"]);
    }

    #[test]
    fn loose_task_list() {
        let lines = plain_default("- [ ] loose a\n\n- [x] loose b", 80);
        assert_eq!(lines, vec!["- [ ] loose a", "", "- [x] loose b"]);
    }

    #[test]
    fn loose_ordered_paragraphs() {
        let src = "1. Lorem ipsum dolor sit amet.\n\n   Ut enim ad minim veniam.\n\n2. Duis aute irure dolor.\n\n   Excepteur sint occaecat cupidatat.\n\n3. Beep boop";
        let lines = plain_default(src, 80);
        assert_eq!(
            lines,
            vec![
                "1. Lorem ipsum dolor sit amet.",
                "",
                "   Ut enim ad minim veniam.",
                "",
                "2. Duis aute irure dolor.",
                "",
                "   Excepteur sint occaecat cupidatat.",
                "",
                "3. Beep boop",
            ]
        );
    }

    #[test]
    fn nested_list() {
        let src = "- Item 1\n  - Nested 1.1\n  - Nested 1.2\n- Item 2";
        let lines = plain_default(src, 80);
        assert_eq!(
            lines,
            vec![
                "- Item 1",
                "    - Nested 1.1",
                "    - Nested 1.2",
                "- Item 2",
            ]
        );
    }

    #[test]
    fn deeply_nested_list() {
        let src = "- Level 1\n  - Level 2\n    - Level 3\n      - Level 4";
        let lines = plain_default(src, 80);
        assert_eq!(
            lines,
            vec![
                "- Level 1",
                "    - Level 2",
                "        - Level 3",
                "            - Level 4",
            ]
        );
    }

    #[test]
    fn ordered_nested_list() {
        let src = "1. First\n   1. Nested first\n   2. Nested second\n2. Second";
        let lines = plain_default(src, 80);
        assert_eq!(
            lines,
            vec![
                "1. First",
                "    1. Nested first",
                "    2. Nested second",
                "2. Second",
            ]
        );
    }

    #[test]
    fn mixed_nested_list() {
        let src = "1. Ordered item\n   - Unordered nested\n   - Another nested\n2. Second ordered\n   - More nested";
        let lines = plain_default(src, 80);
        assert_eq!(
            lines,
            vec![
                "1. Ordered item",
                "    - Unordered nested",
                "    - Another nested",
                "2. Second ordered",
                "    - More nested",
            ]
        );
    }

    #[test]
    fn narrow_wrap_unordered() {
        let lines = plain_default("- alpha beta gamma delta epsilon", 20);
        assert_eq!(lines, vec!["- alpha beta gamma", "  delta epsilon"]);
    }

    #[test]
    fn narrow_wrap_ordered() {
        let lines = plain_default("1. alpha beta gamma delta epsilon", 20);
        assert_eq!(lines, vec!["1. alpha beta gamma", "   delta epsilon"]);
    }

    #[test]
    fn narrow_wrap_multidigit() {
        let lines = plain_default("10. alpha beta gamma delta epsilon", 21);
        assert_eq!(lines, vec!["10. alpha beta gamma", "    delta epsilon"]);
    }

    #[test]
    fn narrow_wrap_nested_unordered_parent() {
        let src = "- parent\n  - alpha beta gamma delta epsilon";
        let lines = plain_default(src, 24);
        assert_eq!(
            lines,
            vec!["- parent", "    - alpha beta gamma", "      delta epsilon"]
        );
    }

    #[test]
    fn narrow_wrap_nested_ordered_parent() {
        let src = "1. parent\n   - alpha beta gamma delta epsilon";
        let lines = plain_default(src, 24);
        assert_eq!(
            lines,
            vec!["1. parent", "    - alpha beta gamma", "      delta epsilon"]
        );
    }

    #[test]
    fn preserve_source_markers() {
        let src = "  4. forth\n  3. third\n\n10) ten\n7) seven\n\n+ plus\n* star\n- minus\n+";
        let lines = plain_preserve(src, 80);
        assert_eq!(
            lines,
            vec![
                "4. forth", "3. third", "", "10) ten", "7) seven", "", "+ plus", "* star",
                "- minus", "+",
            ]
        );
    }

    #[test]
    fn disabled_preservation_normalizes() {
        let lines = plain_default("1. alpha\n1. beta\n1. gamma", 80);
        assert_eq!(lines, vec!["1. alpha", "2. beta", "3. gamma"]);
    }

    #[test]
    fn task_markers_with_preserved_bullets() {
        let src = "+ [ ] beep\n* [x] boop";
        let lines = plain_preserve(src, 80);
        assert_eq!(lines, vec!["+ [ ] beep", "* [x] boop"]);
    }

    #[test]
    fn preserve_nested_source_markers() {
        // Non-default markers at both parent (+) and nested (4)/7)) depths.
        // Blank line after the parent paragraph is required for CommonMark to
        // accept an ordered nested list under an unordered item.
        let src = "+ parent\n\n  4) nested four\n  7) nested seven\n* sibling";
        let lines = plain_preserve(src, 80);
        assert_eq!(
            lines,
            vec![
                "+ parent",
                "    4) nested four",
                "    7) nested seven",
                "* sibling",
            ]
        );
    }

    #[test]
    fn list_blockquote_wrap() {
        let lines = plain_default("- > alpha beta gamma delta epsilon zeta", 24);
        assert_eq!(
            lines,
            vec!["- │ alpha beta gamma", "  │ delta epsilon zeta"]
        );
    }

    #[test]
    fn list_blockquote_keeps_nested_blocks_quoted() {
        let src = "- > before\n  >\n  > - nested";
        let lines = plain_default(src, 40);
        assert!(
            lines
                .iter()
                .filter(|line| !line.trim().is_empty())
                .all(|line| line.contains('│')),
            "{lines:?}"
        );
        assert!(lines.iter().any(|line| line.contains("nested")));
    }

    #[test]
    fn blockquote_keeps_block_children_quoted() {
        let src = "> before\n>\n> - nested\n>\n> ---\n>\n> ```ts\n> code\n> ```";
        let lines = plain_default(src, 40);
        assert!(
            lines
                .iter()
                .filter(|line| !line.trim().is_empty())
                .all(|line| line.contains('│')),
            "{lines:?}"
        );
        assert!(lines.iter().any(|line| line.contains("nested")));
        assert!(lines.iter().any(|line| line.contains('─')));
        assert!(lines.iter().any(|line| line.contains("code")));
    }

    #[test]
    fn blockquote_list_keeps_loose_item_text_with_its_marker() {
        let lines = plain_default("> - a\n>\n> - b", 40);
        let visible = lines
            .iter()
            .map(|line| line.trim_end())
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(visible, ["│ - a", "│", "│ - b"]);
    }

    #[test]
    fn blockquote_separates_paragraph_blocks() {
        let lines = plain_default("> a\n>\n> b", 40);
        assert!(
            lines.windows(3).any(|window| {
                window[0].trim_end() == "│ a"
                    && window[1].trim_end() == "│"
                    && window[2].trim_end() == "│ b"
            }),
            "{lines:?}"
        );
    }

    #[test]
    fn nested_blockquote_keeps_inner_border() {
        let lines = plain_default("> outer\n> > inner", 40);
        assert!(
            lines.iter().any(|line| line.contains("│ │ inner")),
            "{lines:?}"
        );
    }

    #[test]
    fn quote_inside_quoted_list_item_stays_with_marker() {
        let lines = plain_default("> - > quoted", 40);
        assert!(
            lines.iter().any(|line| line.contains("- │ quoted")),
            "{lines:?}"
        );
    }

    #[test]
    fn interrupting_list_has_no_spurious_paragraph_gap() {
        let lines = plain_default("para\n- item", 40);
        assert!(
            lines
                .windows(2)
                .any(|window| window[0] == "para" && window[1] == "- item"),
            "{lines:?}"
        );
    }

    #[test]
    fn list_after_source_blank_keeps_paragraph_gap() {
        let lines = plain_default("para\n\n- item", 40);
        assert!(
            lines.windows(3).any(|window| {
                window[0] == "para" && window[1].is_empty() && window[2] == "- item"
            }),
            "{lines:?}"
        );
    }

    #[test]
    fn blockquote_styles_every_block_line_and_reapplies_after_reset() {
        fn quote_style(text: &str) -> String {
            format!("\u{1b}[3m{text}\u{1b}[0m")
        }
        let theme = MarkdownTheme {
            quote: quote_style,
            ..MarkdownTheme::default()
        };
        let options = MarkdownOptions::default();
        let apply_default = |text: &str| text.to_owned();
        let renderer = MarkdownRenderer::new("", 40, &theme, &options, &apply_default);
        let lines = renderer.render_quoted_segments(
            vec![ItemSegment::Nested(vec![
                "nested \u{1b}[0mchild".to_owned(),
            ])],
            40,
        );
        assert_eq!(
            lines,
            vec!["│ \u{1b}[3mnested \u{1b}[0m\u{1b}[3mchild\u{1b}[0m"]
        );
        let wrapped = renderer.render_quoted_segments(
            vec![ItemSegment::Nested(vec![
                "alpha beta gamma delta".to_owned(),
            ])],
            12,
        );
        assert!(wrapped.len() > 1, "{wrapped:?}");
        assert!(
            wrapped.iter().all(|line| line.starts_with("│ \u{1b}[3m")),
            "{wrapped:?}"
        );
        assert!(
            wrapped
                .last()
                .is_some_and(|line| line.ends_with("\u{1b}[0m")),
            "{wrapped:?}"
        );
    }

    #[test]
    fn list_rule_stays_inside_item() {
        let src = "- before\n\n  ---\n\n  after";
        let lines = plain_default(src, 40);
        let mut saw_before = false;
        let mut saw_rule = false;
        for line in &lines {
            saw_before |= line.contains("before");
            if line.contains('─') {
                assert!(saw_before, "{lines:?}");
                assert!(line.starts_with("  "), "{lines:?}");
                saw_rule = true;
            }
        }
        assert!(saw_before, "{lines:?}");
        assert!(saw_rule, "{lines:?}");
    }

    #[test]
    fn top_level_rule_is_rendered() {
        let lines = plain_default("---", 40);
        assert_eq!(lines, vec!["─".repeat(40)]);
    }

    #[test]
    fn list_code_block_wrap() {
        let src = "- ```ts\n  alpha beta gamma delta epsilon zeta\n  ```";
        let lines = plain_default(src, 24);
        assert_eq!(
            lines,
            vec![
                "- ```ts",
                "    alpha beta gamma",
                "  delta epsilon zeta",
                "  ```",
            ]
        );
    }

    #[test]
    fn extract_source_marker_crlf_and_fallback() {
        assert_eq!(extract_source_marker("+\r\n", false).as_deref(), Some("+ "));
        assert_eq!(extract_source_marker("+\n", false).as_deref(), Some("+ "));
        assert_eq!(extract_source_marker("+", false).as_deref(), Some("+ "));
        // Bare CR is not a valid unordered marker terminator.
        assert_eq!(extract_source_marker("+\r", false), None);
        // Malformed ordered markers fall back via None (take_marker normalizes).
        assert_eq!(extract_source_marker("abc", true), None);
        assert_eq!(extract_source_marker("4.x", true), None);
        assert_eq!(extract_source_marker("4. ok", true).as_deref(), Some("4. "));
        assert_eq!(
            extract_source_marker("10) ok", true).as_deref(),
            Some("10) ")
        );
    }

    fn md(text: &str) -> Markdown {
        Markdown::new(
            text,
            0,
            0,
            MarkdownTheme::default(),
            DefaultTextStyle::default(),
            MarkdownOptions::default(),
        )
    }

    #[test]
    fn headings_lists_hr() {
        let mut m = md("# Title\n\n## Sub\n\n- a\n- b\n\n---\n");
        for w in [24_u16, 60, 80, 120] {
            let snap = render_snapshot(&mut m, w);
            let joined = snap
                .iter()
                .map(|s| strip_ansi(s))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(joined.contains("Title") || joined.contains("Sub") || !joined.is_empty());
        }
    }

    #[test]
    fn table_and_narrow_fallback() {
        let src = "| A | B |\n| --- | --- |\n| 1 | 2 |\n";
        let mut m = md(src);
        let wide = render_snapshot(&mut m, 80);
        let joined = wide
            .iter()
            .map(|s| strip_ansi(s))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains('┌') || joined.contains('│') || joined.contains('A'));

        m.invalidate();
        let narrow = render_snapshot(&mut m, 8);
        assert!(!narrow.is_empty());
    }

    #[test]
    fn task_list() {
        let mut m = md("- [x] done\n- [ ] todo\n");
        let snap = render_snapshot(&mut m, 60);
        let joined = snap
            .iter()
            .map(|s| strip_ansi(s))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("[x]") || joined.contains("done"));
    }

    #[test]
    fn fence_and_partial_stream() {
        let mut m = md("```rs\nfn main() {}\n```");
        let snap = render_snapshot(&mut m, 60);
        let joined = snap
            .iter()
            .map(|s| strip_ansi(s))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("```"));

        // Partial closing fence should not collapse.
        m.set_text("```rs\nfn main() {}\n``");
        let snap2 = render_snapshot(&mut m, 60);
        assert!(!snap2.is_empty());
    }

    #[test]
    fn blockquote() {
        let mut m = md("> quoted text\n");
        let snap = render_snapshot(&mut m, 60);
        let joined = snap
            .iter()
            .map(|s| strip_ansi(s))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains('│') || joined.contains("quoted"));
    }

    #[test]
    fn link_fallback_suffix() {
        let mut m = Markdown::new(
            "[label](https://example.com)",
            0,
            0,
            MarkdownTheme::default(),
            DefaultTextStyle::default(),
            MarkdownOptions {
                hyperlinks: false,
                ..Default::default()
            },
        );
        let snap = render_snapshot(&mut m, 80);
        let joined = snap
            .iter()
            .map(|s| strip_ansi(s))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("example.com") || joined.contains("label"));
    }

    #[test]
    fn link_hyperlink_enabled_hides_url() {
        let mut m = Markdown::new(
            "[label](https://example.com)",
            0,
            0,
            MarkdownTheme::default(),
            DefaultTextStyle::default(),
            MarkdownOptions {
                hyperlinks: true,
                ..Default::default()
            },
        );
        let snap = render_snapshot(&mut m, 80);
        let plain_text: String = snap
            .iter()
            .map(|s| strip_ansi(s))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain_text.contains("label"));
        assert!(!plain_text.contains("(https://example.com)"));
    }

    #[test]
    fn link_hyperlink_disabled_shows_url_in_parens() {
        let mut m = Markdown::new(
            "[label](https://example.com)",
            0,
            0,
            MarkdownTheme::default(),
            DefaultTextStyle::default(),
            MarkdownOptions {
                hyperlinks: false,
                ..Default::default()
            },
        );
        let snap = render_snapshot(&mut m, 80);
        let plain_text: String = snap
            .iter()
            .map(|s| strip_ansi(s))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain_text.contains("label"));
        assert!(plain_text.contains("(https://example.com)"));
    }

    #[test]
    fn link_hyperlink_capped_falls_back_on_oversized_uri() {
        let long_uri = format!("https://example.com/{}", "x".repeat(2048));
        let mut m = Markdown::new(
            format!("[label]({long_uri})"),
            0,
            0,
            MarkdownTheme::default(),
            DefaultTextStyle::default(),
            MarkdownOptions {
                hyperlinks: true,
                ..Default::default()
            },
        );
        let snap = render_snapshot(&mut m, 80);
        let plain_text: String = snap
            .iter()
            .map(|s| strip_ansi(s))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain_text.contains("label"));
        assert!(!plain_text.contains(&long_uri));
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "test: 'label' is 5 bytes, well within u16"
    )]
    #[test]
    fn link_hyperlink_emits_osc8_raw_region_on_wire_channel() {
        use crate::frame::{FrameAnnotations, with_annotations};
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use std::cell::RefCell;

        let mut m = Markdown::new(
            "see [label](https://example.com) done",
            0,
            0,
            MarkdownTheme::default(),
            DefaultTextStyle::default(),
            MarkdownOptions {
                hyperlinks: true,
                ..Default::default()
            },
        );
        let height = m.measure(80).max(1);
        let annotations = RefCell::new(FrameAnnotations::new());
        with_annotations(&annotations, || {
            let mut buf = Buffer::empty(Rect::new(0, 0, 80, height));
            m.render(Rect::new(0, 0, 80, height), &mut buf);
        });
        let regions = annotations.into_inner().into_parts().1;
        assert_eq!(regions.len(), 1, "exactly one region per rendered link");
        let bytes = String::from_utf8_lossy(&regions[0].bytes).into_owned();
        assert!(bytes.contains("\u{1b}]8;;https://example.com\u{1b}\\"));
        assert!(bytes.contains("label"));
        assert!(bytes.ends_with("\u{1b}]8;;\u{1b}\\\u{1b}[0m"));
        assert_eq!(regions[0].area.height, 1);
        assert_eq!(regions[0].area.width, "label".len() as u16);
    }

    #[test]
    fn link_hyperlink_disabled_emits_no_region() {
        use crate::frame::{FrameAnnotations, with_annotations};
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use std::cell::RefCell;

        let mut m = Markdown::new(
            "see [label](https://example.com) done",
            0,
            0,
            MarkdownTheme::default(),
            DefaultTextStyle::default(),
            MarkdownOptions {
                hyperlinks: false,
                ..Default::default()
            },
        );
        let height = m.measure(80).max(1);
        let annotations = RefCell::new(FrameAnnotations::new());
        with_annotations(&annotations, || {
            let mut buf = Buffer::empty(Rect::new(0, 0, 80, height));
            m.render(Rect::new(0, 0, 80, height), &mut buf);
        });
        let regions = annotations.into_inner().into_parts().1;
        assert!(
            regions.is_empty(),
            "fallback path never touches the wire channel"
        );
    }

    #[test]
    fn empty_measures_zero() {
        let mut m = md("   ");
        assert_eq!(m.measure(80), 0);
    }

    #[test]
    fn cache_invalidation() {
        let mut m = md("hello");
        let h1 = m.measure(40);
        m.set_text("# Hello\n\nworld");
        let h2 = m.measure(40);
        assert!(h2 >= h1);
    }

    #[test]
    fn list_table_task_marker_item_width() {
        // Task markers are wider than the old depth-based approximation (`width - 2`);
        // at width 14 the true item width (8) selects narrow table fallback, not grid.
        // The header row (`A | B`) is populated from the table head and must appear
        // in the narrow fallback output alongside the data row.
        let src = "- [ ] task\n\n  | A | B |\n  | --- | --- |\n  | 1 | 2 |\n";
        let lines = plain_default(src, 14);
        assert_eq!(lines, vec!["- [ ] task", "", "      A | B", "      1 | 2"]);
    }

    #[test]
    fn deeply_nested_blockquote_does_not_exhaust_stack() {
        // A genuinely 200-level-deep CommonMark blockquote: every `> ` marker
        // opens one nesting level, so the source is `> > > … > x`. The earlier
        // fixture `"> x".repeat(200)` was a single flat one-level line and
        // proved nothing about nested rendering.
        //
        // The iterative `render_quoted_segments` walks nesting on an explicit
        // heap stack instead of the call stack, and nesting past the
        // width-derived cap folds raw lines into the parent without the
        // border/wrap transform, so output size stays linear in input size.
        // At width 80 the cap is `MAX_BORDERED_QUOTE_DEPTH` (16), so this
        // fixture exercises the same path as before. The single-token "x"
        // never wraps, so it only proves the stack is bounded — the
        // multiplicative saturation path needs a narrow width and multi-token
        // text (see `deeply_nested_blockquote_narrow_width_stays_bounded`).
        let src = format!("{}x", "> ".repeat(200));
        let lines = plain_default(&src, 80);
        // The single text segment "x" is one raw line; each of the
        // `MAX_BORDERED_QUOTE_DEPTH` bordered levels wraps it at a content
        // width that still fits the accumulated line, so the quote renders as
        // exactly one bordered line plus the trailing blank the quote closer
        // appends. Anything multiplicative in the 200 nesting levels breaks
        // this bound.
        assert!(lines.len() <= 2, "expected bounded output, got {lines:?}");
        // Nesting-depth proof: border columns are present up to the cap. A
        // flat one-level fixture yields exactly 1 border column; genuine
        // nesting yields one per bordered level.
        let border_columns = lines.first().map_or(0, |line| line.matches('│').count());
        assert_eq!(
            border_columns, MAX_BORDERED_QUOTE_DEPTH,
            "expected one border column per bordered level in {lines:?}"
        );
        // The text survives the deep fold.
        assert!(
            lines.first().is_some_and(|line| line.contains('x')),
            "content lost in {lines:?}"
        );
    }

    #[test]
    fn deeply_nested_blockquote_narrow_width_stays_bounded()
    -> Result<(), Box<dyn std::error::Error>> {
        // At width 20 the width-derived cap is `(20 - 1) / 2 = 9`, so only
        // depths 1..=9 receive borders (content_width 18, 16, …, 2). Depths
        // 10..=200 fold raw. With the old fixed cap of 16, depths 10..=16
        // would still border at content_width 1, splitting every accumulated
        // line into single characters and re-splitting at each parent level —
        // multiplicative growth. Multi-token text forces wrapping at the
        // deepest bordered level so the saturation path is actually reached.
        let src = format!("{}alpha beta gamma delta", "> ".repeat(200));
        // Bound the render with an explicit deadline so a hang (e.g. from a
        // reverted fixed cap) reports as a test failure, not a CI timeout.
        // The render thread is detached (not joined): joining it would block
        // the watchdog on return, defeating the deadline entirely.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(plain_default(&src, 20));
        });
        let lines = match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(lines) => lines,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return Err(
                    "rendering 200 nested quotes at width 20 did not complete in 10s".into(),
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("render thread panicked for 200-level quote at width 20".into());
            }
        };
        // The deepest bordered level (depth 9, content_width 2) wraps the
        // 22-character text into at most 11 lines; each parent level adds a
        // border but never re-splits (parent content_width = child
        // content_width + border_w). So the total is bounded by the deepest
        // level's wrap count plus the trailing blank.
        assert!(
            lines.len() <= 15,
            "expected bounded output at width 20, got {} lines",
            lines.len()
        );
        // The text survives the deep fold — at content_width 2 it is split
        // into 2-char fragments, so check for the first fragment rather than
        // the full word.
        assert!(
            lines.iter().any(|line| line.contains("al")),
            "content lost in {lines:?}"
        );
        Ok(())
    }

    #[test]
    fn finish_list_without_start_does_not_panic() {
        // Internal state where End(List) arrives with no matching Start(List)
        // must return gracefully, not panic.
        let theme = MarkdownTheme::default();
        let options = MarkdownOptions::default();
        let apply_default = |text: &str| text.to_owned();
        let mut renderer = MarkdownRenderer::new("", 40, &theme, &options, &apply_default);
        // Directly invoke finish_list with an empty list stack.
        renderer.finish_list(0..0);
        // Should have returned without panicking; nothing to assert on output.
    }

    // ----- LaTeX math integration tests -----

    #[test]
    fn inline_dollar_math_renders() {
        let lines = plain_default("A map $\\mathbb{C}^3 \\to \\mathbb{C}^3$.", 80);
        assert_eq!(lines, vec!["A map ℂ³ → ℂ³."]);
    }

    #[test]
    fn inline_paren_math_renders() {
        let lines = plain_default("Limit \\(s \\to \\infty\\).", 80);
        assert_eq!(lines, vec!["Limit s → ∞."]);
    }

    #[test]
    fn inline_dollar_dollar_math_renders() {
        let lines = plain_default("Vector $$\\vec{x}$$ end.", 80);
        assert_eq!(lines, vec!["Vector x⃗ end."]);
    }

    #[test]
    fn block_dollar_math_renders() {
        let lines = plain_default("$$\nx^2 + y^2 = r^2\n$$", 80);
        assert_eq!(lines, vec!["x² + y² = r²"]);
    }

    #[test]
    fn block_bracket_math_renders() {
        let lines = plain_default("\\[\n\\frac{a}{b}\n\\]", 80);
        // display mode: stacked fraction with bar width = max(numerator, denominator)
        assert_eq!(lines, vec!["a", "─", "b"]);
    }

    #[test]
    fn dollar_sign_currency_not_math() {
        let lines = plain_default("Price: $100 total.", 80);
        assert_eq!(lines, vec!["Price: $100 total."]);
    }

    #[test]
    fn dollar_followed_by_space_not_math() {
        let lines = plain_default("Cost: $ and benefit.", 80);
        assert_eq!(lines, vec!["Cost: $ and benefit."]);
    }

    #[test]
    fn all_caps_shell_var_not_math() {
        let lines = plain_default("Use $HOME variable.", 80);
        assert_eq!(lines, vec!["Use $HOME variable."]);
    }

    #[test]
    fn unsupported_math_falls_back_to_raw() {
        let lines = plain_default("Bad: $\\unknown{thing}$.", 80);
        assert_eq!(lines, vec!["Bad: $\\unknown{thing}$."]);
    }

    #[test]
    fn math_inside_code_span_not_rendered() {
        let lines = plain_default("Code: `$x^2$` here.", 80);
        // pulldown-cmark renders inline code without backticks
        assert_eq!(lines, vec!["Code: $x^2$ here."]);
    }

    #[test]
    fn math_inside_fenced_code_block_not_rendered() {
        let lines = plain_default(
            "Example:\n```\n$$\nx^2 + y^2 = r^2\n$$\ninline $x^2$ too\n```\nDone $z^2$.",
            80,
        );
        assert!(
            lines.iter().any(|l| l.contains("$$")),
            "fenced block math must stay literal in {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("x^2 + y^2")),
            "fenced LaTeX must stay literal in {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("inline $x^2$ too")),
            "inline math inside a fence must stay literal in {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("z²")),
            "math after the fence must still render in {lines:?}"
        );
    }

    #[test]
    fn multiple_inline_math_in_one_line() {
        let lines = plain_default("$x^2$ and $y^2$ and $z^2$", 80);
        assert_eq!(lines, vec!["x² and y² and z²"]);
    }

    #[test]
    fn math_with_text_in_paragraph() {
        let lines = plain_default("The formula $\\frac{1}{2}$ is simple.", 80);
        assert_eq!(lines, vec!["The formula 1/2 is simple."]);
    }

    #[test]
    fn escaped_dollar_not_math() {
        let lines = plain_default("Price \\$5 dollars.", 80);
        // pulldown-cmark processes \$ as escape → renders as $
        assert_eq!(lines, vec!["Price $5 dollars."]);
    }

    #[test]
    fn render_latex_disabled_passes_raw() {
        let lines = plain(
            "Math $x^2$ here.",
            80,
            MarkdownOptions {
                render_latex: false,
                ..Default::default()
            },
        );
        assert_eq!(lines, vec!["Math $x^2$ here."]);
    }

    #[test]
    fn block_math_with_surrounding_text() {
        let lines = plain_default("Before\n$$\n\\sum_{i=1}^n x_i\n$$\nAfter", 80);
        assert!(
            lines.iter().any(|l| l.contains("∑") || l.contains("xᵢ")),
            "expected math output in {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("Before")),
            "expected 'Before' in {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("After")),
            "expected 'After' in {lines:?}"
        );
    }

    #[test]
    fn inline_math_with_fraction_and_subscript() {
        let lines = plain_default("Value $\\frac{1}{x_0}$ end.", 80);
        // format_fraction wraps non-simple denominator (x₀ is not is_simple_num)
        assert_eq!(lines, vec!["Value 1/(x₀) end."]);
    }
    #[test]
    fn display_math_fraction_stacked() {
        let lines = plain_default("$$\n\\frac{x+1}{x-1}\n$$", 80);
        // display mode: bar width = max(numerator, denominator) = 3
        assert_eq!(lines, vec!["x+1", "───", "x-1"]);
    }
}
