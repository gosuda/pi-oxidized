//! View composition: exact-order stack + buffer rendering + golden snapshot helpers.
//!
//! [`compose`] builds the ordered component stack for one frame:
//!
//! ```text
//! header → resources → chat → pending → status
//!        → widgets above → editor → widgets below → footer
//! ```
//!
//! [`render_view`] measures each section, allocates vertical rects, and renders
//! into one Ratatui [`Buffer`]. Everything runs inside
//! [`super::theme::with_theme`] so the `fn`-pointer component themes resolve
//! against the view's theme. No terminal, no stdout — fully snapshot-testable.

use std::collections::BTreeMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
#[cfg(test)]
use ratatui::style::{Color, Modifier};

use pi_ext::adapters::{SlotComponent, tui_overlay_spec};
use pi_tui::component::Component;
use pi_tui::components::Text;
use pi_tui::focus::Focusable;

use super::footer;
use super::header;
use super::messages::{self, MessageView};
use super::progress;
use super::startup;
use super::state::{FocusArea, OverlayKind, ViewState, WidgetSlot};
use super::status;
use super::theme::{self, MarkdownTheme, ResolvedTheme, markdown_theme};

/// One composed section: a label + the boxed component.
pub struct ComposedSection {
    /// Section label (for golden-snapshot headers / debugging).
    pub label: &'static str,
    /// The component.
    pub component: Box<dyn Component>,
}

/// The full composed view: ordered sections + optional overlay.
pub struct ComposedView {
    /// Ordered sections (header → resources → chat → pending → status →
    /// widgets above → editor → widgets below → footer).
    pub sections: Vec<ComposedSection>,
    /// Optional overlay rendered on top (shortcut help / changelog / login / …).
    pub overlay: Option<Box<dyn Component>>,
    /// Extension overlay layout specification, when the overlay is host-owned.
    pub overlay_spec: Option<pi_tui::layout::OverlaySpec>,
}

/// Compose the full view-model into ordered sections for `state`.
///
/// The caller renders via [`render_view`] or walks sections directly. The
/// theme is installed thread-locally for the duration of composition.
#[must_use]
pub fn compose(state: &ViewState) -> ComposedView {
    theme::with_theme(state.theme.clone(), || compose_inner(state))
}

fn compose_inner(state: &ViewState) -> ComposedView {
    let md_theme = markdown_theme();
    let mut sections: Vec<ComposedSection> = Vec::new();

    // 1. Header
    if !state.quiet {
        sections.push(ComposedSection {
            label: "header",
            component: header::build_header(&state.header, md_theme.clone(), &state.theme),
        });
    }

    // 2. Loaded resources
    sections.push(ComposedSection {
        label: "resources",
        component: startup::build_resources(&state.resources, &state.theme),
    });

    // 3. Startup diagnostics (rendered with resources, above chat)
    sections.push(ComposedSection {
        label: "diagnostics",
        component: startup::build_diagnostics(&state.diagnostics, &state.theme),
    });

    // 4. Chat messages
    sections.push(ComposedSection {
        label: "chat",
        component: build_chat(state, &md_theme),
    });

    // 5. Pending queue
    sections.push(ComposedSection {
        label: "pending",
        component: progress::build_pending(&state.pending, &state.theme),
    });

    // 6. Status indicator
    sections.push(ComposedSection {
        label: "status",
        component: build_status_section(state),
    });

    // 7. Widgets above editor
    sections.push(ComposedSection {
        label: "widgets-above",
        component: build_widget_stack(&state.widgets_above, &state.theme),
    });

    // 8. Editor (or active selector / overlay replacement)
    sections.push(ComposedSection {
        label: "editor",
        component: build_editor_section(state),
    });

    // 9. Widgets below editor
    sections.push(ComposedSection {
        label: "widgets-below",
        component: build_widget_stack(&state.widgets_below, &state.theme),
    });

    // 10. Footer
    sections.push(ComposedSection {
        label: "footer",
        component: footer::build_footer(&state.footer, &state.theme, state.width),
    });

    let overlay = build_overlay(state, &md_theme);
    let overlay_spec = state
        .extension_overlay_slot
        .as_ref()
        .and_then(|slot| slot.overlay_options.as_ref())
        .map(tui_overlay_spec);

    ComposedView {
        sections,
        overlay,
        overlay_spec,
    }
}

/// Build the chat container from all message view-models.
fn build_chat(state: &ViewState, md_theme: &MarkdownTheme) -> Box<dyn Component> {
    let renderers = super::tool_renderers::builtin_tool_renderers();
    let mut stack = messages::ColumnStack::new();
    for msg in &state.messages {
        let comps = build_message(msg, renderers, md_theme, &state.theme);
        for c in comps {
            stack.push(c);
        }
    }
    if state.messages.is_empty() && !state.streaming {
        // Empty-state hint: discoverability beats a bare sentence (C7).
        stack.push(Box::new(Text::with_padding(
            state.theme.fg(
                super::theme::ThemeColor::Dim,
                "Type a message, or / for commands.",
            ),
            messages::CONTENT_INDENT,
            0,
        )));
        stack.push(Box::new(Text::with_padding(
            state.theme.fg(
                super::theme::ThemeColor::Dim,
                "? shortcuts · ctrl+o expand tools · shift+tab thinking",
            ),
            messages::CONTENT_INDENT,
            0,
        )));
    }
    Box::new(stack)
}

/// Build the component stack for one message view-model.
fn build_message(
    msg: &MessageView,
    renderers: &BTreeMap<String, Box<dyn super::tool_renderer::CustomToolRenderer>>,
    md_theme: &MarkdownTheme,
    th: &ResolvedTheme,
) -> Vec<Box<dyn Component>> {
    match msg {
        MessageView::User(v) => vec![messages::build_user(v, md_theme, th)],
        MessageView::Assistant(v) => messages::build_assistant(v, md_theme, th),
        MessageView::Tool(v) => messages::build_tool(v, renderers, th),
        MessageView::Bash(v) => vec![messages::build_bash(v, th)],
        MessageView::Custom(v) => vec![messages::build_custom(v, md_theme, th)],
        MessageView::Compaction(v) => vec![messages::build_compaction(v, md_theme, th)],
        MessageView::Branch(v) => vec![messages::build_branch(v, md_theme, th)],
        MessageView::Skill(v) => vec![messages::build_skill(v, md_theme, th)],
    }
}

/// Build the status section (active indicator or idle).
fn build_status_section(state: &ViewState) -> Box<dyn Component> {
    if let Some(status) = state.status.as_ref() {
        status::build_status(status, &state.theme)
    } else {
        status::build_idle(state.width)
    }
}

/// Build the editor area (input, or a selector replacing it, or a progress block).
fn build_editor_section(state: &ViewState) -> Box<dyn Component> {
    // Progress overlays (compaction/retry/auth/bash) replace the editor.
    if state.focus == FocusArea::Selector {
        // Selectors are rendered as overlays in `build_overlay`; here we keep
        // a blank row so the editor slot keeps its height contract (C11).
        return Box::new(pi_tui::components::Spacer::new(1));
    }
    let editor = &state.editor;
    let display = if editor.text.is_empty() {
        state
            .theme
            .fg(super::theme::ThemeColor::Dim, &editor.placeholder)
    } else {
        editor.text.clone()
    };
    let marker = if editor.text.starts_with('!') {
        state.theme.fg(super::theme::ThemeColor::BashMode, "$ ")
    } else {
        state.theme.fg(super::theme::ThemeColor::Accent, "❯ ")
    };
    let paste_marker = editor.paste_marker.as_deref().unwrap_or("");
    Box::new(Text::with_padding(
        format!("{marker}{display}{paste_marker}"),
        1,
        0,
    ))
}

/// Build a vertical widget stack from pre-rendered slot lines.
fn build_widget_stack(slots: &[WidgetSlot], _th: &ResolvedTheme) -> Box<dyn Component> {
    let mut stack = messages::ColumnStack::new();
    for widget in slots {
        let mut component = SlotComponent::new(widget.slot.clone());
        component.set_focused(widget.focused);
        stack.push(Box::new(component));
    }
    if stack.is_empty() {
        stack.push(Box::new(pi_tui::components::Spacer::new(0)));
    }
    Box::new(stack)
}

/// Build the overlay component (selectors render here in the reference's
/// editor-replace model; help/changelog/login render as overlays).
fn build_overlay(state: &ViewState, md_theme: &MarkdownTheme) -> Option<Box<dyn Component>> {
    let overlay = state.overlay.as_ref()?;
    let comp: Box<dyn Component> = match overlay.kind {
        OverlayKind::ShortcutHelp => startup::build_shortcut_overlay(
            &startup::default_shortcut_hints(),
            &state.extension_shortcuts,
            &state.theme,
        ),
        OverlayKind::Changelog => {
            startup::build_changelog(&overlay.lines.join("\n"), md_theme.clone(), &state.theme)
        }
        OverlayKind::FirstTimeSetup => startup::build_first_time_setup_with_selection(
            state
                .first_run_step
                .unwrap_or(startup::FIRST_RUN_STEP_FAMILY),
            state.first_run_selected,
            state.first_run_family.as_deref(),
            state.first_run_mode,
            md_theme.clone(),
            &state.theme,
        ),
        OverlayKind::Login => {
            let mut stack = messages::ColumnStack::new();
            for line in &overlay.lines {
                stack.push(Box::new(Text::with_padding(line.clone(), 1, 0)));
            }
            Box::new(stack)
        }
        OverlayKind::Extension => {
            let slot = state.extension_overlay_slot.as_ref()?;
            let mut component = SlotComponent::new(slot.clone());
            component.set_focused(
                state.focus == FocusArea::Overlay
                    && !slot
                        .overlay_options
                        .as_ref()
                        .is_some_and(|options| options.non_capturing),
            );
            Box::new(component)
        }
    };
    Some(comp)
}

// ---------------------------------------------------------------------------
// Buffer rendering
// ---------------------------------------------------------------------------

/// Render the composed view into a fresh buffer of `width` × `height`.
///
/// Sections are stacked top-to-bottom; each is measured at `width` and
/// allocated a vertical rect. The overlay (if any) is rendered last at the top.
/// Sections that would overflow `height` are truncated (later sections drop).
#[must_use]
pub fn render_view(state: &ViewState, width: u16, height: u16) -> Buffer {
    render_view_with_height(state, width, height)
}

/// Render into a buffer sized to exactly the measured content height (no fixed
/// height cap). Useful for golden snapshots that want the full content.
#[must_use]
pub fn render_view_with_height(state: &ViewState, width: u16, height: u16) -> Buffer {
    let composed = compose(state);
    let area = Rect::new(0, 0, width.max(1), height.max(1));
    let mut buf = Buffer::empty(area);
    let mut y = 0u16;
    // Consume sections so each boxed component can be rendered by value.
    for mut section in composed.sections {
        let mut h = section.component.measure(width.max(1));
        if h == 0 {
            continue;
        }
        if y.saturating_add(h) > height {
            h = height.saturating_sub(y);
            if h == 0 {
                break;
            }
        }
        let rect = Rect::new(0, y, width.max(1), h);
        let mut comp = section.component;
        comp.render(rect, &mut buf);
        y = y.saturating_add(h);
        if y >= height {
            break;
        }
    }
    if let Some(mut overlay) = composed.overlay {
        let measured = overlay.measure(width.max(1)).min(height);
        let rect = composed.overlay_spec.as_ref().map_or_else(
            || Rect::new(0, 0, width.max(1), measured),
            |spec| {
                let layout = pi_tui::layout::resolve_overlay_layout(
                    spec,
                    measured,
                    width.max(1),
                    height.max(1),
                );
                let overlay_height = layout
                    .max_height
                    .map_or(measured, |max_height| measured.min(max_height))
                    .min(height.saturating_sub(layout.row));
                Rect::new(layout.col, layout.row, layout.width, overlay_height)
            },
        );
        if rect.height > 0 {
            overlay.render(rect, &mut buf);
        }
    }
    buf
}

/// Render a single component into a buffer at `width`, measuring its height.
///
/// Test helper for golden snapshots of individual sections.
#[cfg(test)]
#[must_use]
pub fn render_component(comp: &mut dyn Component, width: u16) -> Buffer {
    let h = comp.measure(width.max(1)).max(1);
    let area = Rect::new(0, 0, width.max(1), h);
    let mut buf = Buffer::empty(area);
    comp.render(area, &mut buf);
    buf
}

/// Snapshot the visible cell symbols of a buffer region (plain text, no ANSI).
///
/// One `String` per row; wide-cell skips and trailing spaces preserved.
#[cfg(test)]
#[must_use]
pub fn snapshot_buffer_plain(buf: &Buffer, width: u16, height: u16) -> Vec<String> {
    use ratatui::buffer::CellDiffOption;
    let mut out = Vec::with_capacity(usize::from(height));
    for row in 0..height {
        let mut line = String::new();
        for x in 0..width {
            if let Some(cell) = buf.cell((x, row)) {
                if cell.diff_option == CellDiffOption::Skip {
                    continue;
                }
                line.push_str(cell.symbol());
            } else {
                line.push(' ');
            }
        }
        out.push(line);
    }
    out
}

/// Snapshot a buffer region to ANSI-styled text (SGR codes re-emitted per cell).
///
/// Produces one string per row with truecolor/256 SGR sequences reconstructed
/// from the cell styles — the "ANSI snapshot" required by the golden suite.
#[cfg(test)]
#[must_use]
pub fn snapshot_buffer_ansi(
    buf: &Buffer,
    width: u16,
    height: u16,
    mode: super::theme::ColorMode,
) -> Vec<String> {
    use ratatui::style::{Color, Modifier};
    let mut out = Vec::with_capacity(usize::from(height));
    for row in 0..height {
        let mut line = String::new();
        let mut previous_foreground: Option<Color> = None;
        let mut previous_background: Option<Color> = None;
        let mut previous_style_modifiers = Modifier::empty();
        let mut previous_style_set = false;
        for x in 0..width {
            if let Some(cell) = buf.cell((x, row)) {
                let style = cell.style();
                let foreground = style.fg;
                let background = style.bg;
                let style_modifiers = style.add_modifier;
                if !previous_style_set
                    || foreground != previous_foreground
                    || background != previous_background
                    || style_modifiers != previous_style_modifiers
                {
                    line.push_str("\x1b[0m");
                    if let Some(c) = foreground {
                        push_color(&mut line, c, true, mode);
                    }
                    if let Some(c) = background {
                        push_color(&mut line, c, false, mode);
                    }
                    push_style_modifiers(&mut line, style_modifiers);
                    previous_foreground = foreground;
                    previous_background = background;
                    previous_style_modifiers = style_modifiers;
                    previous_style_set = true;
                }
                line.push_str(cell.symbol());
            } else {
                line.push(' ');
            }
        }
        if previous_style_set {
            line.push_str("\x1b[0m");
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
fn push_color(
    out: &mut String,
    color: ratatui::style::Color,
    fg: bool,
    mode: super::theme::ColorMode,
) {
    use std::fmt::Write as _;
    let prefix = if fg { 38 } else { 48 };
    match color {
        Color::Rgb(r, g, b) => match mode {
            super::theme::ColorMode::Truecolor => {
                let _ = write!(out, "\x1b[{prefix};2;{r};{g};{b}m");
            }
            super::theme::ColorMode::Palette256 => {
                let idx = super::theme::rgb_to_256(super::theme::Rgb(r, g, b));
                let _ = write!(out, "\x1b[{prefix};5;{idx}m");
            }
        },
        Color::Indexed(i) => {
            let _ = write!(out, "\x1b[{prefix};5;{i}m");
        }
        c => {
            let idx = basic_color_index(c);
            if idx < 16 {
                let _ = write!(out, "\x1b[{prefix};5;{idx}m");
            }
        }
    }
}

#[cfg(test)]
fn basic_color_index(c: Color) -> u8 {
    match c {
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        _ => 255,
    }
}

#[cfg(test)]
fn push_style_modifiers(out: &mut String, style_modifiers: Modifier) {
    use ratatui::style::Modifier;
    if style_modifiers.contains(Modifier::BOLD) {
        out.push_str("\x1b[1m");
    }
    if style_modifiers.contains(Modifier::DIM) {
        out.push_str("\x1b[2m");
    }
    if style_modifiers.contains(Modifier::ITALIC) {
        out.push_str("\x1b[3m");
    }
    if style_modifiers.contains(Modifier::UNDERLINED) {
        out.push_str("\x1b[4m");
    }
    if style_modifiers.contains(Modifier::REVERSED) {
        out.push_str("\x1b[7m");
    }
    if style_modifiers.contains(Modifier::CROSSED_OUT) {
        out.push_str("\x1b[9m");
    }
}
