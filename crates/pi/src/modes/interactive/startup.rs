//! Startup surfaces: loaded resources summary, diagnostics, first-time setup,
//! shortcut overlay, and release-notes/changelog.
//!
//! Ports `renderLoadedResources`, `IdleStatus`, `FirstTimeSetupComponent`,
//! the `/hotkeys` overlay, and the `/changelog` markdown box. Pure data →
//! pi-tui components.

use pi_tui::component::Component;
use pi_tui::components::{Markdown, Spacer, Text};

use super::state::{LoadedResource, ShortcutHint, StartupDiagnostics};
use super::theme::{self, MarkdownOptions, MarkdownTheme, ResolvedTheme, ThemeColor};

// ---------------------------------------------------------------------------
// Loaded resources
// ---------------------------------------------------------------------------

/// Build the loaded-resources summary (one line per skill/prompt/theme/context).
#[must_use]
pub fn build_resources(resources: &[LoadedResource], th: &ResolvedTheme) -> Box<dyn Component> {
    if resources.is_empty() {
        return Box::new(Spacer::new(0));
    }
    let mut stack = super::messages::ColumnStack::new();
    for r in resources {
        let line = format!(
            "{} {}",
            th.fg(ThemeColor::Accent, &format!("[{}]", r.kind)),
            th.fg(ThemeColor::Muted, &r.label),
        );
        stack.push(Box::new(Text::with_padding(line, 1, 0)));
    }
    Box::new(stack)
}

// ---------------------------------------------------------------------------
// Startup diagnostics
// ---------------------------------------------------------------------------

/// Build the startup diagnostics block (warnings/errors from resource load).
#[must_use]
pub fn build_diagnostics(
    diagnostics: &StartupDiagnostics,
    th: &ResolvedTheme,
) -> Box<dyn Component> {
    if diagnostics.entries.is_empty() {
        return Box::new(Spacer::new(0));
    }
    let mut stack = super::messages::ColumnStack::new();
    for d in &diagnostics.entries {
        let (color, glyph) = match d.severity {
            super::state::DiagnosticSeverity::Warning => (ThemeColor::Warning, "⚠"),
            super::state::DiagnosticSeverity::Error => (ThemeColor::Error, "✗"),
        };
        let line = format!(
            "{} {}{}",
            th.fg(color, glyph),
            th.fg(ThemeColor::Dim, &format!("{}: ", d.source)),
            d.message,
        );
        stack.push(Box::new(Text::with_padding(line, 1, 0)));
    }
    Box::new(stack)
}

// ---------------------------------------------------------------------------
// First-time setup
// ---------------------------------------------------------------------------

/// Build the first-time-setup wizard component (theme + analytics steps).
#[must_use]
pub fn build_first_time_setup(
    step: usize,
    md_theme: MarkdownTheme,
    th: &ResolvedTheme,
) -> Box<dyn Component> {
    let mut stack = super::messages::ColumnStack::new();
    stack.push(Box::new(Text::with_padding(
        theme::bold(&th.fg(ThemeColor::Accent, "Welcome to pi")),
        1,
        0,
    )));
    let body = match step {
        0 => "Choose a theme: **dark** or **light**?\nUse `/theme` later to change.",
        1 => "Enable anonymous usage analytics?\n(you can change this in settings)",
        _ => "Setup complete. Type a message to begin.",
    };
    stack.push(Box::new(Markdown::new(
        body,
        1,
        0,
        md_theme,
        theme::default_text_style(),
        MarkdownOptions::default(),
    )));
    Box::new(stack)
}

// ---------------------------------------------------------------------------
// Shortcut overlay
// ---------------------------------------------------------------------------

/// Build the shortcut/help overlay component from hint rows.
#[must_use]
pub fn build_shortcut_overlay(hints: &[ShortcutHint], th: &ResolvedTheme) -> Box<dyn Component> {
    let mut stack = super::messages::ColumnStack::new();
    stack.push(Box::new(Text::with_padding(
        theme::bold(&th.fg(ThemeColor::Accent, "Keyboard shortcuts")),
        1,
        0,
    )));
    for h in hints {
        let line = format!(
            "  {}  {}",
            th.fg(ThemeColor::Accent, &h.key),
            th.fg(ThemeColor::Muted, &h.action),
        );
        stack.push(Box::new(Text::with_padding(line, 0, 0)));
    }
    Box::new(stack)
}

/// Default shortcut hint rows (ports the reference keybinding table).
#[must_use]
pub fn default_shortcut_hints() -> Vec<ShortcutHint> {
    use super::state::ShortcutHint;
    [
        ("Esc", "Interrupt / abort"),
        ("Ctrl+C", "Clear editor (×2 to exit)"),
        ("Ctrl+D", "Exit"),
        ("Shift+Tab", "Cycle thinking"),
        ("Ctrl+L", "Model selector"),
        ("Ctrl+P", "Cycle models"),
        ("Ctrl+O", "Toggle tool output"),
        ("Ctrl+T", "Toggle thinking"),
        ("Ctrl+G", "External editor"),
        ("Ctrl+X", "Copy last assistant"),
        ("Alt+Enter", "Queue follow-up"),
        ("Alt+Up", "Restore follow-up"),
        ("Ctrl+V", "Paste image/text"),
        ("/help", "Slash commands"),
        ("? , /hotkeys", "This overlay"),
    ]
    .into_iter()
    .map(|(key, action)| ShortcutHint {
        key: key.to_owned(),
        action: action.to_owned(),
    })
    .collect()
}

// ---------------------------------------------------------------------------
// Release notes / changelog
// ---------------------------------------------------------------------------

/// Build the changelog/release-notes component from markdown source.
#[must_use]
pub fn build_changelog(
    markdown: &str,
    md_theme: MarkdownTheme,
    _th: &ResolvedTheme,
) -> Box<dyn Component> {
    let mut stack = super::messages::ColumnStack::new();
    stack.push(Box::new(Markdown::new(
        markdown,
        1,
        0,
        md_theme,
        theme::default_text_style(),
        MarkdownOptions::default(),
    )));
    Box::new(stack)
}
