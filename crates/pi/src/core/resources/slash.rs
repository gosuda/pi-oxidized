//! Built-in slash commands and slash-command metadata.
//!
//! Port of `.references/pi/packages/coding-agent/src/core/slash-commands.ts`.

use crate::core::config::APP_NAME;
use crate::core::resources::source_info::SourceInfo;

/// Origin of a registered slash command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlashCommandSource {
    /// Extension-registered command.
    Extension,
    /// Prompt template (`/name`).
    Prompt,
    /// Skill (`/skill:name`).
    Skill,
}

impl SlashCommandSource {
    /// Wire discriminant.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Extension => "extension",
            Self::Prompt => "prompt",
            Self::Skill => "skill",
        }
    }
}

/// Runtime slash command descriptor (extension/prompt/skill).
#[derive(Clone, Debug, PartialEq)]
pub struct SlashCommandInfo {
    /// Command name without leading `/`.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Origin of the command.
    pub source: SlashCommandSource,
    /// Provenance of the backing resource.
    pub source_info: SourceInfo,
}

/// Built-in slash command definition (owned, with live descriptions).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltinSlashCommand {
    /// Command name without leading `/`.
    pub name: String,
    /// Description shown in the command palette.
    pub description: String,
    /// Optional argument hint.
    pub argument_hint: Option<String>,
}

const BUILTIN_STATIC: &[(&str, &str, Option<&str>)] = &[
    ("settings", "Open settings menu", None),
    (
        "model",
        "Select model (opens selector UI)",
        Some("<provider/model>"),
    ),
    (
        "scoped-models",
        "Enable/disable models for Ctrl+P cycling",
        None,
    ),
    (
        "export",
        "Export session (HTML default, or specify path: .html/.jsonl)",
        None,
    ),
    (
        "import",
        "Import and resume a session from a JSONL file",
        None,
    ),
    ("share", "Share session as a secret GitHub gist", None),
    ("copy", "Copy last agent message to clipboard", None),
    ("name", "Set session display name", None),
    ("session", "Show session info and stats", None),
    ("changelog", "Show changelog entries", None),
    ("hotkeys", "Show all keyboard shortcuts", None),
    (
        "fork",
        "Create a new fork from a previous user message",
        None,
    ),
    (
        "clone",
        "Duplicate the current session at the current position",
        None,
    ),
    ("tree", "Navigate session tree (switch branches)", None),
    (
        "trust",
        "Save project trust decision for future sessions",
        None,
    ),
    (
        "login",
        "Configure provider authentication",
        Some("<provider>"),
    ),
    ("logout", "Remove provider authentication", None),
    ("new", "Start a new session", None),
    ("compact", "Manually compact the session context", None),
    ("resume", "Resume a different session", None),
    (
        "reload",
        "Reload keybindings, extensions, skills, prompts, themes, and context files",
        None,
    ),
    ("quit", "", None), // description filled with APP_NAME
];

/// Description for `/quit` (`Quit ${APP_NAME}`).
#[must_use]
pub fn builtin_quit_description() -> String {
    format!("Quit {APP_NAME}")
}

/// Exact 22 built-in slash commands from TypeScript `BUILTIN_SLASH_COMMANDS`.
///
/// `/quit` uses the live [`APP_NAME`] description.
#[must_use]
pub fn builtin_slash_commands() -> Vec<BuiltinSlashCommand> {
    let quit_description = builtin_quit_description();
    BUILTIN_STATIC
        .iter()
        .map(|(name, description, hint)| {
            let description = if *name == "quit" {
                quit_description.clone()
            } else {
                (*description).to_owned()
            };
            BuiltinSlashCommand {
                name: (*name).to_owned(),
                description,
                argument_hint: hint.map(str::to_owned),
            }
        })
        .collect()
}

/// Number of built-in slash commands (always 22).
pub const BUILTIN_SLASH_COMMAND_COUNT: usize = 22;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_twenty_two_builtins() {
        assert_eq!(BUILTIN_STATIC.len(), BUILTIN_SLASH_COMMAND_COUNT);
        let cmds = builtin_slash_commands();
        assert_eq!(cmds.len(), 22);
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "settings",
                "model",
                "scoped-models",
                "export",
                "import",
                "share",
                "copy",
                "name",
                "session",
                "changelog",
                "hotkeys",
                "fork",
                "clone",
                "tree",
                "trust",
                "login",
                "logout",
                "new",
                "compact",
                "resume",
                "reload",
                "quit",
            ]
        );
        assert_eq!(
            cmds.iter()
                .find(|c| c.name == "model")
                .and_then(|c| c.argument_hint.as_deref()),
            Some("<provider/model>")
        );
        assert_eq!(
            cmds.iter()
                .find(|c| c.name == "login")
                .and_then(|c| c.argument_hint.as_deref()),
            Some("<provider>")
        );
        assert_eq!(cmds[21].description, format!("Quit {APP_NAME}"));
    }
}
