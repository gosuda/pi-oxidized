//! Startup surfaces: loaded resources summary, diagnostics, first-time setup,
//! shortcut overlay, and release-notes/changelog.
//!
//! Ports `renderLoadedResources`, `IdleStatus`, `FirstTimeSetupComponent`,
//! the `/hotkeys` overlay, and the `/changelog` markdown box. Pure data →
//! pi-tui components.

use pi_tui::component::Component;
use pi_tui::components::{Markdown, Spacer, Text};

use super::messages::CONTENT_INDENT;
use super::state::{LoadedResource, ShortcutHint, StartupDiagnostics};
use super::theme::{self, MarkdownTheme, ResolvedTheme, ThemeColor, user_markdown_options};
use crate::core::settings::ThemeMode;

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
        stack.push(Box::new(Text::with_padding(line, CONTENT_INDENT, 0)));
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
        stack.push(Box::new(Text::with_padding(line, CONTENT_INDENT, 0)));
    }
    Box::new(stack)
}

// ---------------------------------------------------------------------------
// First-time setup
// ---------------------------------------------------------------------------

/// First-run wizard steps: family → mode → analytics.
pub const FIRST_RUN_STEP_FAMILY: usize = 0;
/// Theme-mode step.
pub const FIRST_RUN_STEP_MODE: usize = 1;
/// Analytics opt-in step.
pub const FIRST_RUN_STEP_ANALYTICS: usize = 2;

/// Built-in family options for the first-run family step.
#[must_use]
pub fn first_run_family_options() -> Vec<&'static str> {
    super::theme::BUILT_IN_THEME_FAMILIES.to_vec()
}

/// Mode options for the first-run mode step (`value`, label).
#[must_use]
pub fn first_run_mode_options() -> Vec<(ThemeMode, &'static str)> {
    vec![
        (ThemeMode::Auto, "Auto"),
        (ThemeMode::Dark, "Dark"),
        (ThemeMode::Light, "Light"),
    ]
}

/// Analytics options for the final first-run step (`value`, label).
#[must_use]
pub fn first_run_analytics_options() -> Vec<(bool, &'static str)> {
    vec![(true, "Share anonymous usage data"), (false, "Don't share")]
}

/// Build the first-time-setup wizard component (family + mode + analytics).
#[must_use]
pub fn build_first_time_setup(
    step: usize,
    md_theme: MarkdownTheme,
    th: &ResolvedTheme,
) -> Box<dyn Component> {
    build_first_time_setup_with_selection(step, 0, None, None, md_theme, th)
}

/// Build the first-time-setup wizard with highlighted option index and retained
/// family/mode for the live-preview path.
#[must_use]
pub fn build_first_time_setup_with_selection(
    step: usize,
    selected_index: usize,
    family: Option<&str>,
    mode: Option<ThemeMode>,
    md_theme: MarkdownTheme,
    th: &ResolvedTheme,
) -> Box<dyn Component> {
    let mut stack = super::messages::ColumnStack::new();
    stack.push(Box::new(Text::with_padding(
        theme::bold(&th.fg(
            ThemeColor::Accent,
            "Welcome to pi, the minimal coding agent.",
        )),
        CONTENT_INDENT,
        0,
    )));
    let body = match step {
        FIRST_RUN_STEP_FAMILY => "Choose a theme family.\nHighlight previews live; Enter confirms.",
        FIRST_RUN_STEP_MODE => "Choose a theme mode.\nAuto matches the terminal background.",
        FIRST_RUN_STEP_ANALYTICS => {
            "Opt-in to anonymous usage data sharing?\nOpting in stores a tracking identifier in settings.json and enables anonymous\nusage analytics. This helps us to better debug, reproduce, and resolve issues\nand bugs within Pi. You can observe what is shared using /privacy and make\nchanges anytime in settings.json."
        }
        _ => "Setup complete. Type a message to begin.",
    };
    stack.push(Box::new(Markdown::new(
        body,
        CONTENT_INDENT,
        0,
        md_theme,
        theme::default_text_style(),
        user_markdown_options(),
    )));

    let options: Vec<String> = match step {
        FIRST_RUN_STEP_FAMILY => first_run_family_options()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        FIRST_RUN_STEP_MODE => first_run_mode_options()
            .into_iter()
            .map(|(_, label)| label.to_owned())
            .collect(),
        FIRST_RUN_STEP_ANALYTICS => first_run_analytics_options()
            .into_iter()
            .map(|(_, label)| label.to_owned())
            .collect(),
        _ => Vec::new(),
    };
    for (idx, label) in options.iter().enumerate() {
        let marker = if idx == selected_index { "→ " } else { "  " };
        let line = format!("{marker}{label}");
        let color = if idx == selected_index {
            ThemeColor::Accent
        } else {
            ThemeColor::Muted
        };
        stack.push(Box::new(Text::with_padding(
            th.fg(color, &line),
            CONTENT_INDENT,
            0,
        )));
    }

    if let Some(family) = family {
        let mode_label = mode.map_or("—", ThemeMode::as_str);
        stack.push(Box::new(Text::with_padding(
            th.fg(
                ThemeColor::Dim,
                &format!("Selected family: {family} · mode: {mode_label}"),
            ),
            CONTENT_INDENT,
            0,
        )));
    }
    Box::new(stack)
}

// ---------------------------------------------------------------------------
// Shortcut overlay
// ---------------------------------------------------------------------------

/// Build the shortcut/help overlay component from hint rows.
#[must_use]
pub fn build_shortcut_overlay(
    hints: &[ShortcutHint],
    extension_hints: &[ShortcutHint],
    th: &ResolvedTheme,
) -> Box<dyn Component> {
    let mut stack = super::messages::ColumnStack::new();
    stack.push(Box::new(Text::with_padding(
        theme::bold(&th.fg(ThemeColor::Accent, "Keyboard shortcuts")),
        CONTENT_INDENT,
        0,
    )));
    push_shortcut_hints(&mut stack, hints, th);
    if !extension_hints.is_empty() {
        stack.push(Box::new(Spacer::new(1)));
        stack.push(Box::new(Text::with_padding(
            theme::bold(&th.fg(ThemeColor::Accent, "Extensions")),
            CONTENT_INDENT,
            0,
        )));
        // Extension rows carry raw key strings; render them display-formatted
        // like the reference extension hotkeys table (interactive-mode.ts:6347).
        let display_hints: Vec<ShortcutHint> = extension_hints
            .iter()
            .map(|hint| ShortcutHint {
                key: pi_tui::keybindings::format_key_text(&hint.key, true),
                action: hint.action.clone(),
            })
            .collect();
        push_shortcut_hints(&mut stack, &display_hints, th);
    }
    Box::new(stack)
}

fn push_shortcut_hints(
    stack: &mut super::messages::ColumnStack,
    hints: &[ShortcutHint],
    th: &ResolvedTheme,
) {
    for hint in hints {
        let line = format!(
            "  {}  {}",
            th.fg(ThemeColor::Accent, &hint.key),
            th.fg(ThemeColor::Muted, &hint.action),
        );
        stack.push(Box::new(Text::with_padding(line, 0, 0)));
    }
}

/// Default shortcut hint rows (ports the reference keybinding table).
///
/// Every key column resolves from the process-global keybinding registry so
/// rebound users see their own chords; slash-command rows stay raw.
#[must_use]
pub fn default_shortcut_hints() -> Vec<ShortcutHint> {
    use pi_tui::keybindings::key_display_text;

    let cycle_models = [
        key_display_text("app.model.cycleForward"),
        key_display_text("app.model.cycleBackward"),
    ]
    .join(" / ");
    [
        (key_display_text("app.interrupt"), "Interrupt / abort"),
        (key_display_text("app.clear"), "Clear editor (×2 to exit)"),
        (key_display_text("app.exit"), "Exit"),
        (key_display_text("app.thinking.cycle"), "Cycle thinking"),
        (key_display_text("app.model.select"), "Model selector"),
        (cycle_models, "Cycle models"),
        (key_display_text("app.tools.expand"), "Toggle tool output"),
        (key_display_text("app.thinking.toggle"), "Toggle thinking"),
        (key_display_text("app.editor.external"), "External editor"),
        (key_display_text("app.message.copy"), "Copy last assistant"),
        (key_display_text("app.message.followUp"), "Queue follow-up"),
        (key_display_text("app.message.dequeue"), "Restore follow-up"),
        (
            key_display_text("app.clipboard.pasteImage"),
            "Paste image/text",
        ),
        ("/help".to_owned(), "Slash commands"),
        ("/hotkeys".to_owned(), "This overlay"),
    ]
    .into_iter()
    .map(|(key, action)| ShortcutHint {
        key,
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
        CONTENT_INDENT,
        0,
        md_theme,
        theme::default_text_style(),
        user_markdown_options(),
    )));
    Box::new(stack)
}
