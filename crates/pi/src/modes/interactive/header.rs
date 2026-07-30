//! Header view-model (logo + key hints).
//!
//! Ports the `builtInHeader` `ExpandableText` from `interactive-mode.ts`
//! (lines ~724-782): collapsed shows the logo + compact hints + onboarding;
//! expanded shows the full keybinding list.

use pi_tui::component::Component;
use pi_tui::components::{Markdown, Spacer, Text};

use super::messages::CONTENT_INDENT;
use super::state::HeaderData;
use super::theme::{self, MarkdownOptions, MarkdownTheme, ResolvedTheme, ThemeColor};

/// Build the header component from header data.
#[must_use]
pub fn build_header(
    data: &HeaderData,
    md_theme: MarkdownTheme,
    th: &ResolvedTheme,
) -> Box<dyn Component> {
    if data.app_name.is_empty() {
        return Box::new(Spacer::new(0));
    }
    let mut stack = super::messages::ColumnStack::new();
    let logo = theme::bold(&th.fg(
        ThemeColor::Accent,
        &format!("{} v{}", data.app_name, data.version),
    ));
    if data.expanded {
        stack.push(Box::new(Text::with_padding(logo, CONTENT_INDENT, 0)));
        let hints = expanded_hints(th);
        stack.push(Box::new(Markdown::new(
            hints,
            CONTENT_INDENT,
            0,
            md_theme,
            theme::default_text_style(),
            MarkdownOptions::default(),
        )));
    } else {
        let onboarding = data.onboarding.clone().unwrap_or_default();
        let compact = if onboarding.is_empty() {
            format!("{logo}  •  type a message, `/` for commands, `?` for help")
        } else {
            format!("{logo}  •  {onboarding}")
        };
        stack.push(Box::new(Text::with_padding(
            th.fg(ThemeColor::Dim, &compact),
            CONTENT_INDENT,
            0,
        )));
    }
    Box::new(stack)
}

/// The expanded keybinding hint markdown block (ports the reference hint list).
#[must_use]
pub fn expanded_hints(th: &ResolvedTheme) -> String {
    let _ = th;
    [
        "**Interrupt / exit**",
        "- `Esc` — interrupt streaming / abort bash",
        "- `Ctrl+C` — clear editor (press twice to exit)",
        "- `Ctrl+D` — exit (when editor is empty)",
        "- `Ctrl+Z` — suspend",
        "",
        "**Editing**",
        "- `Shift+Tab` — cycle thinking level",
        "- `Ctrl+L` — model selector",
        "- `Ctrl+P` / `Shift+Ctrl+P` — cycle models",
        "- `Ctrl+O` — toggle tool output",
        "- `Ctrl+T` — toggle thinking",
        "- `Ctrl+G` — external editor",
        "- `Ctrl+X` — copy last assistant",
        "- `Ctrl+V` — paste image / text",
        "",
        "**Queues & commands**",
        "- `Alt+Enter` — queue follow-up",
        "- `Alt+Up` — restore queued follow-up",
        "- `!cmd` / `!!cmd` — bash (excluded from context with `!!`)",
        "- `/help` — slash commands",
    ]
    .join("\n")
}
