//! Header view-model (logo + key hints).
//!
//! Ports the `builtInHeader` `ExpandableText` from `interactive-mode.ts`
//! (lines ~724-782): collapsed shows the logo + compact hints + onboarding;
//! expanded shows the full keybinding list.

use pi_tui::component::Component;
use pi_tui::components::{Markdown, Spacer, Text};

use super::messages::CONTENT_INDENT;
use super::state::HeaderData;
use super::theme::{self, MarkdownTheme, ResolvedTheme, ThemeColor, user_markdown_options};

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
            user_markdown_options(),
        )));
    } else {
        let onboarding = data.onboarding.clone().unwrap_or_default();
        let compact = if onboarding.is_empty() {
            format!("{logo}  •  type a message to begin")
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
///
/// Every chord resolves from the process-global keybinding registry so rebound
/// users see their own keys; slash-command and bash-syntax rows stay raw.
#[must_use]
pub fn expanded_hints(th: &ResolvedTheme) -> String {
    let _ = th;
    use pi_tui::keybindings::key_display_text;

    let cycle_models = [
        key_display_text("app.model.cycleForward"),
        key_display_text("app.model.cycleBackward"),
    ]
    .join(" / ");
    [
        "**Interrupt / exit**",
        &format!("- `{}` — interrupt streaming / abort bash", key_display_text("app.interrupt")),
        &format!("- `{}` — clear editor (press twice to exit)", key_display_text("app.clear")),
        &format!("- `{}` — exit (when editor is empty)", key_display_text("app.exit")),
        &format!("- `{}` — suspend", key_display_text("app.suspend")),
        "",
        "**Editing**",
        &format!("- `{}` — cycle thinking level", key_display_text("app.thinking.cycle")),
        &format!("- `{}` — model selector", key_display_text("app.model.select")),
        &format!("- `{}` — cycle models", cycle_models),
        &format!("- `{}` — toggle tool output", key_display_text("app.tools.expand")),
        &format!("- `{}` — toggle thinking", key_display_text("app.thinking.toggle")),
        &format!("- `{}` — external editor", key_display_text("app.editor.external")),
        &format!("- `{}` — copy last assistant", key_display_text("app.message.copy")),
        &format!("- `{}` — paste image / text", key_display_text("app.clipboard.pasteImage")),
        "",
        "**Queues & commands**",
        &format!("- `{}` — queue follow-up", key_display_text("app.message.followUp")),
        &format!("- `{}` — restore queued follow-up", key_display_text("app.message.dequeue")),
        "- `!cmd` / `!!cmd` — bash (excluded from context with `!!`)",
        "- `/help` — slash commands",
    ]
    .join("\n")
}
