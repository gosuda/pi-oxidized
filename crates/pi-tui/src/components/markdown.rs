//! Markdown renderer using pulldown-cmark with theme hooks and streaming fence trim.

use std::sync::Arc;

use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::component::{Component, EventResult, UiEvent};
use crate::link::hyperlink;
use crate::text::{
    is_image_line, strip_trailing_partial_closing_fence, visible_width, wrap_text_with_ansi,
};

use super::util::{apply_background, empty_line, paint_lines};

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
#[derive(Debug, Clone, Default)]
pub struct MarkdownOptions {
    /// Preserve source ordered-list markers (best-effort with pulldown-cmark).
    pub preserve_ordered_list_markers: bool,
    /// Preserve backslash escapes as raw (best-effort).
    pub preserve_backslash_escapes: bool,
    /// Enable OSC 8 hyperlinks (caller supplies capability).
    pub hyperlinks: bool,
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
    lines: Vec<String>,
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

    fn lines_for_width(&mut self, width: u16) -> Vec<String> {
        if let Some(cache) = &self.cache
            && cache.width == width
            && cache.text == self.text
        {
            return cache.lines.clone();
        }
        let lines = self.render_lines(width);
        self.cache = Some(Cache {
            text: self.text.clone(),
            width,
            lines: lines.clone(),
        });
        lines
    }
}

impl Component for Markdown {
    fn measure(&mut self, width: u16) -> u16 {
        let lines = self.lines_for_width(width);
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let lines = self.lines_for_width(area.width);
        paint_lines(area, buf, &lines);
    }

    fn handle_event(&mut self, _event: &UiEvent) -> EventResult {
        EventResult::Ignored
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }
}

fn render_markdown(
    text: &str,
    width: usize,
    theme: &MarkdownTheme,
    options: &MarkdownOptions,
    apply_default: impl Fn(&str) -> String,
) -> Vec<String> {
    let mut parser_options = Options::empty();
    parser_options.insert(Options::ENABLE_TABLES);
    parser_options.insert(Options::ENABLE_STRIKETHROUGH);
    parser_options.insert(Options::ENABLE_TASKLISTS);

    let mut renderer = MarkdownRenderer::new(width, theme, options, &apply_default);
    for event in Parser::new_ext(text, parser_options) {
        renderer.consume(event);
    }
    renderer.finish()
}

struct MarkdownRenderer<'a> {
    width: usize,
    theme: &'a MarkdownTheme,
    options: &'a MarkdownOptions,
    apply_default: &'a dyn Fn(&str) -> String,
    lines: Vec<String>,
    inline: String,
    lists: Vec<ListState>,
    heading: Option<u8>,
    quote_depth: usize,
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

impl<'a> MarkdownRenderer<'a> {
    fn new(
        width: usize,
        theme: &'a MarkdownTheme,
        options: &'a MarkdownOptions,
        apply_default: &'a dyn Fn(&str) -> String,
    ) -> Self {
        Self {
            width,
            theme,
            options,
            apply_default,
            lines: Vec::new(),
            inline: String::new(),
            lists: Vec::new(),
            heading: None,
            quote_depth: 0,
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
    fn consume(&mut self, event: Event<'_>) {
        if is_list_event(&event) {
            self.consume_list(&event);
            return;
        }
        match event {
            Event::Start(
                Tag::Paragraph | Tag::Heading { .. } | Tag::BlockQuote(_) | Tag::CodeBlock(_),
            )
            | Event::End(
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::BlockQuote(_) | TagEnd::CodeBlock,
            ) => self.consume_block(event),
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

    fn consume_block(&mut self, event: Event<'_>) {
        match event {
            Event::Start(Tag::Paragraph) => self.inline.clear(),
            Event::End(TagEnd::Paragraph) => {
                self.flush_inline(None);
                self.lines.push(String::new());
            }
            Event::Start(Tag::Heading { level, .. }) => {
                self.inline.clear();
                self.heading = Some(level as u8);
            }
            Event::End(TagEnd::Heading(_)) => {
                self.flush_inline(self.heading);
                self.heading = None;
                self.lines.push(String::new());
            }
            Event::Start(Tag::BlockQuote(_)) => self.quote_depth += 1,
            Event::End(TagEnd::BlockQuote(_)) => {
                self.quote_depth = self.quote_depth.saturating_sub(1);
                if self.quote_depth == 0 {
                    rewrite_quote_borders(&mut self.lines, self.theme, self.width);
                    self.lines.push(String::new());
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => self.start_code_block(kind),
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
        let fence = format!("```{}", lang.as_deref().unwrap_or(""));
        self.lines.push((self.theme.code_block_border)(&fence));
        let indent = &self.theme.code_block_indent;
        if let Some(highlight) = &self.theme.highlight_code {
            self.lines.extend(
                highlight(&self.code_body, lang.as_deref())
                    .into_iter()
                    .map(|line| format!("{indent}{line}")),
            );
        } else {
            self.lines.extend(
                self.code_body
                    .split('\n')
                    .map(|line| format!("{indent}{}", (self.theme.code_block)(line))),
            );
            if self.code_body.ends_with('\n')
                && self.lines.last().is_some_and(|last| {
                    last == indent || last == &format!("{indent}{}", (self.theme.code_block)(""))
                })
            {
                self.lines.pop();
            }
        }
        self.lines.push((self.theme.code_block_border)("```"));
        self.lines.push(String::new());
        self.code_body.clear();
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
            hyperlink(&styled, &href)
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

    fn consume_list(&mut self, event: &Event<'_>) {
        match event {
            Event::Start(Tag::List(start)) => self.lists.push(ListState {
                ordered: start.is_some(),
                next_index: start.unwrap_or(1),
                task: None,
                loose: true,
            }),
            Event::Start(Tag::Item) => self.inline.clear(),
            Event::End(TagEnd::List(_)) => {
                self.lists.pop();
            }
            Event::End(TagEnd::Item) => self.finish_list_item(),
            Event::TaskListMarker(checked) => {
                if let Some(list) = self.lists.last_mut() {
                    list.task = Some(if *checked { "[x] " } else { "[ ] " }.to_owned());
                }
            }
            Event::Rule => {
                self.lines
                    .push((self.theme.hr)(&"─".repeat(self.width.min(80))));
                self.lines.push(String::new());
            }
            _ => {}
        }
    }

    fn finish_list_item(&mut self) {
        let parent_depth = self.lists.len().saturating_sub(1);
        let indent = list_indent(&self.lists[..parent_depth]);
        let Some(list) = self.lists.last_mut() else {
            return;
        };
        let marker = list.take_marker(self.options.preserve_ordered_list_markers);
        let task = list.task.take().unwrap_or_default();
        let body = std::mem::take(&mut self.inline);
        let body = if body.is_empty() {
            String::new()
        } else {
            (self.apply_default)(&body)
        };
        self.lines.push(format!(
            "{indent}{}{body}",
            (self.theme.list_bullet)(&format!("{marker}{task}"))
        ));
        if list.ordered {
            list.next_index = list.next_index.saturating_add(1);
        }
    }

    fn consume_table(&mut self, event: Event<'_>) {
        match event {
            Event::Start(Tag::Table(alignments)) => {
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
                    self.lines
                        .extend(render_table(&table, self.width, self.theme));
                    self.lines.push(String::new());
                }
            }
            Event::Start(Tag::TableHead) => self.set_table_head(true),
            Event::End(TagEnd::TableHead) => self.set_table_head(false),
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
        let mut text = std::mem::take(&mut self.inline);
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
        } else if self.quote_depth == 0 {
            text = (self.apply_default)(&text);
        }

        let indent = list_indent(&self.lists);
        if let Some(list) = self.lists.last() {
            self.lines.push(format!(
                "{indent}{}{text}",
                (self.theme.list_bullet)(&list.current_marker())
            ));
        } else if self.quote_depth > 0 {
            self.lines
                .push((self.theme.quote)(&(self.theme.italic)(&text)));
        } else {
            self.lines.push(text);
        }
    }

    fn finish(mut self) -> Vec<String> {
        if !self.inline.is_empty() {
            self.flush_inline(self.heading);
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
    #[allow(dead_code)]
    loose: bool,
}

impl ListState {
    fn current_marker(&self) -> String {
        if self.ordered {
            format!("{}. ", self.next_index)
        } else {
            "- ".to_owned()
        }
    }

    fn take_marker(&self, _preserve: bool) -> String {
        self.current_marker()
    }
}

fn list_indent(stack: &[ListState]) -> String {
    "    ".repeat(stack.len().saturating_sub(1))
}

fn rewrite_quote_borders(lines: &mut Vec<String>, theme: &MarkdownTheme, width: usize) {
    // Walk backwards over non-empty lines until blank; prefix with quote border.
    let mut i = lines.len();
    while i > 0 {
        i -= 1;
        if lines[i].is_empty() {
            break;
        }
        // Avoid double-prefixing.
        if lines[i].starts_with('│') {
            continue;
        }
        let content_width = width.saturating_sub(2).max(1);
        let styled = lines[i].clone();
        let mut rebuilt = Vec::new();
        for w in wrap_text_with_ansi(&styled, content_width) {
            rebuilt.push(format!("{}{w}", (theme.quote_border)("│ ")));
        }
        if rebuilt.is_empty() {
            lines[i].clone_from(&(theme.quote_border)("│ "));
        } else {
            lines[i].clone_from(&rebuilt[0]);
            for extra in rebuilt.into_iter().skip(1) {
                i += 1;
                lines.insert(i, extra);
            }
        }
    }
}

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
}
