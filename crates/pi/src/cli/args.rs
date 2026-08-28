//! Single-pass CLI argument parser matching coding-agent `cli/args.ts`.

use indexmap::IndexMap;
use pi_ai::ModelThinkingLevel;
use std::ops::{Deref, DerefMut};

/// Output mode requested via `--mode`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Plain text print mode.
    Text,
    /// JSONL event print mode.
    Json,
    /// Headless RPC mode.
    Rpc,
}

/// Result of parsing `--list-models [search]`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ListModels {
    /// Flag was not present.
    #[default]
    None,
    /// Flag present with no search term.
    All,
    /// Flag present with a search term.
    Search(String),
}

/// Value captured for an unknown/extension long flag.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FlagValue {
    /// Boolean long flag (`--flag`).
    Bool,
    /// String long flag (`--flag value` or `--flag=value`).
    Str(String),
}

/// Severity of a parse diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticLevel {
    /// Non-fatal issue.
    Warning,
    /// Fatal issue.
    Error,
}

/// Parser diagnostic that main reports without panicking.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    /// Severity.
    pub level: DiagnosticLevel,
    /// Human-readable message.
    pub message: String,
}

/// Informational command-line switches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArgsFlags {
    /// `--help` / `-h`.
    pub help: bool,
    /// `--version` / `-v`.
    pub version: bool,
    /// `--verbose`.
    pub verbose: bool,
    execution: ExecutionFlags,
}

/// Execution-mode command-line switches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecutionFlags {
    /// `--print` / `-p`.
    pub print: bool,
    /// `--offline`.
    pub offline: bool,
    session: SessionFlags,
}

/// Session command-line switches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionFlags {
    /// `--continue` / `-c`.
    pub r#continue: bool,
    /// `--resume` / `-r`.
    pub resume: bool,
    /// `--no-session`.
    pub no_session: bool,
    tools: ToolFlags,
}

/// Tool command-line switches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolFlags {
    /// `--no-tools` / `-nt`.
    pub no_tools: bool,
    /// `--no-builtin-tools` / `-nbt`.
    pub no_builtin_tools: bool,
    resources: ResourceFlags,
}

/// Extension and resource command-line switches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResourceFlags {
    /// `--no-extensions` / `-ne`.
    pub no_extensions: bool,
    /// `--no-skills` / `-ns`.
    pub no_skills: bool,
    /// `--no-prompt-templates` / `-np`.
    pub no_prompt_templates: bool,
    context: ContextFlags,
}

/// Context resource command-line switches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextFlags {
    /// `--no-themes`.
    pub no_themes: bool,
    /// `--no-context-files` / `-nc`.
    pub no_context_files: bool,
}

macro_rules! deref_flags {
    ($from:ty, $field:ident, $to:ty) => {
        impl Deref for $from {
            type Target = $to;

            fn deref(&self) -> &Self::Target {
                &self.$field
            }
        }

        impl DerefMut for $from {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.$field
            }
        }
    };
}

deref_flags!(ArgsFlags, execution, ExecutionFlags);
deref_flags!(ExecutionFlags, session, SessionFlags);
deref_flags!(SessionFlags, tools, ToolFlags);
deref_flags!(ToolFlags, resources, ResourceFlags);
deref_flags!(ResourceFlags, context, ContextFlags);

/// Parsed CLI arguments.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Args {
    /// Boolean command-line switches.
    pub flags: ArgsFlags,
    /// `--provider`.
    pub provider: Option<String>,
    /// `--model`.
    pub model: Option<String>,
    /// `--api-key`.
    pub api_key: Option<String>,
    /// `--system-prompt`.
    pub system_prompt: Option<String>,
    /// Repeated `--append-system-prompt`.
    pub append_system_prompt: Vec<String>,
    /// `--thinking`.
    pub thinking: Option<ModelThinkingLevel>,
    /// `--mode`.
    pub mode: Option<Mode>,
    /// `--name` / `-n`.
    pub name: Option<String>,
    /// `--session`.
    pub session: Option<String>,
    /// `--session-id`.
    pub session_id: Option<String>,
    /// `--fork`.
    pub fork: Option<String>,
    /// `--session-dir`.
    pub session_dir: Option<String>,
    /// Comma-split `--models`.
    pub models: Vec<String>,
    /// Comma-split `--tools` / `-t`.
    pub tools: Vec<String>,
    /// Comma-split `--exclude-tools` / `-xt`.
    pub exclude_tools: Vec<String>,
    /// Repeated `--extension` / `-e`.
    pub extensions: Vec<String>,
    /// `--export`.
    pub export: Option<String>,
    /// Repeated `--skill`.
    pub skills: Vec<String>,
    /// Repeated `--prompt-template`.
    pub prompt_templates: Vec<String>,
    /// Repeated `--theme`.
    pub themes: Vec<String>,
    /// `--list-models [search]`.
    pub list_models: ListModels,
    /// `--approve`/`-a` or `--no-approve`/`-na`.
    pub project_trust_override: Option<bool>,
    /// Positional message strings.
    pub messages: Vec<String>,
    /// `@file` paths without the leading `@`.
    pub file_args: Vec<String>,
    /// Unknown long flags in encounter order.
    pub unknown_flags: IndexMap<String, FlagValue>,
    /// Warnings and errors collected during parse.
    pub diagnostics: Vec<Diagnostic>,
}

impl Deref for Args {
    type Target = ArgsFlags;

    fn deref(&self) -> &Self::Target {
        &self.flags
    }
}

impl DerefMut for Args {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.flags
    }
}

const VALID_THINKING_LEVELS: &str = "off, minimal, low, medium, high, xhigh, max";

/// Parse argv tokens the same way as TypeScript `parseArgs`.
///
/// `args` is the token list after the program name (mirrors Node's `process.argv.slice(2)`).
#[must_use]
pub fn parse_args(args: &[String]) -> Args {
    let mut result = Args::default();
    let mut i = 0usize;

    while i < args.len() {
        let arg = args[i].as_str();
        if parse_general_arg(arg, args, &mut i, &mut result)
            || parse_session_arg(arg, args, &mut i, &mut result)
            || parse_tool_arg(arg, args, &mut i, &mut result)
            || parse_resource_arg(arg, args, &mut i, &mut result)
        {
            i += 1;
            continue;
        }

        if let Some(path) = arg.strip_prefix('@') {
            result.file_args.push(path.to_owned());
        } else if let Some(long_arg) = arg.strip_prefix("--") {
            if let Some((name, value)) = long_arg.split_once('=') {
                result
                    .unknown_flags
                    .insert(name.to_owned(), FlagValue::Str(value.to_owned()));
            } else if let Some(next) = args.get(i + 1)
                && !next.starts_with('-')
                && !next.starts_with('@')
            {
                result
                    .unknown_flags
                    .insert(long_arg.to_owned(), FlagValue::Str(next.clone()));
                i += 1;
            } else {
                result
                    .unknown_flags
                    .insert(long_arg.to_owned(), FlagValue::Bool);
            }
        } else if arg.starts_with('-') {
            result.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Error,
                message: format!("Unknown option: {arg}"),
            });
        } else {
            result.messages.push(arg.to_owned());
        }

        i += 1;
    }

    result
}

fn parse_general_arg(arg: &str, args: &[String], i: &mut usize, result: &mut Args) -> bool {
    match arg {
        "--help" | "-h" => result.help = true,
        "--version" | "-v" => result.version = true,
        "--continue" | "-c" => result.r#continue = true,
        "--resume" | "-r" => result.resume = true,
        "--print" | "-p" => {
            result.print = true;
            if let Some(next) = args.get(*i + 1).filter(|next| is_print_message_token(next)) {
                result.messages.push(next.clone());
                *i += 1;
            }
        }
        "--verbose" => result.verbose = true,
        "--offline" => result.offline = true,
        "--approve" | "-a" => result.project_trust_override = Some(true),
        "--no-approve" | "-na" => result.project_trust_override = Some(false),
        "--mode" if *i + 1 < args.len() => {
            *i += 1;
            match args[*i].as_str() {
                "text" => result.mode = Some(Mode::Text),
                "json" => result.mode = Some(Mode::Json),
                "rpc" => result.mode = Some(Mode::Rpc),
                _ => {}
            }
        }
        "--provider" if *i + 1 < args.len() => {
            *i += 1;
            result.provider = Some(args[*i].clone());
        }
        "--model" if *i + 1 < args.len() => {
            *i += 1;
            result.model = Some(args[*i].clone());
        }
        "--api-key" if *i + 1 < args.len() => {
            *i += 1;
            result.api_key = Some(args[*i].clone());
        }
        "--system-prompt" if *i + 1 < args.len() => {
            *i += 1;
            result.system_prompt = Some(args[*i].clone());
        }
        "--append-system-prompt" if *i + 1 < args.len() => {
            *i += 1;
            result.append_system_prompt.push(args[*i].clone());
        }
        "--thinking" if *i + 1 < args.len() => {
            *i += 1;
            let level = args[*i].as_str();
            if let Some(thinking) = parse_thinking_level(level) {
                result.thinking = Some(thinking);
            } else {
                result.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Warning,
                    message: format!(
                        "Invalid thinking level \"{level}\". Valid values: {VALID_THINKING_LEVELS}"
                    ),
                });
            }
        }
        "--export" if *i + 1 < args.len() => {
            *i += 1;
            result.export = Some(args[*i].clone());
        }
        _ => return false,
    }
    true
}

fn parse_session_arg(arg: &str, args: &[String], i: &mut usize, result: &mut Args) -> bool {
    match arg {
        "--name" | "-n" => {
            if *i + 1 < args.len() {
                *i += 1;
                result.name = Some(args[*i].clone());
            } else {
                result.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    message: "--name requires a value".to_owned(),
                });
            }
        }
        "--no-session" => result.no_session = true,
        "--session" if *i + 1 < args.len() => {
            *i += 1;
            result.session = Some(args[*i].clone());
        }
        "--session-id" if *i + 1 < args.len() => {
            *i += 1;
            result.session_id = Some(args[*i].clone());
        }
        "--fork" if *i + 1 < args.len() => {
            *i += 1;
            result.fork = Some(args[*i].clone());
        }
        "--session-dir" if *i + 1 < args.len() => {
            *i += 1;
            result.session_dir = Some(args[*i].clone());
        }
        _ => return false,
    }
    true
}

fn parse_tool_arg(arg: &str, args: &[String], i: &mut usize, result: &mut Args) -> bool {
    match arg {
        "--models" if *i + 1 < args.len() => {
            *i += 1;
            result.models = split_comma_list(&args[*i], false);
        }
        "--no-tools" | "-nt" => result.no_tools = true,
        "--no-builtin-tools" | "-nbt" => result.no_builtin_tools = true,
        "--tools" | "-t" if *i + 1 < args.len() => {
            *i += 1;
            result.tools = split_comma_list(&args[*i], true);
        }
        "--exclude-tools" | "-xt" if *i + 1 < args.len() => {
            *i += 1;
            result.exclude_tools = split_comma_list(&args[*i], true);
        }
        "--list-models" => {
            if *i + 1 < args.len()
                && !args[*i + 1].starts_with('-')
                && !args[*i + 1].starts_with('@')
            {
                *i += 1;
                result.list_models = ListModels::Search(args[*i].clone());
            } else {
                result.list_models = ListModels::All;
            }
        }
        _ => return false,
    }
    true
}

fn parse_resource_arg(arg: &str, args: &[String], i: &mut usize, result: &mut Args) -> bool {
    match arg {
        "--extension" | "-e" if *i + 1 < args.len() => {
            *i += 1;
            result.extensions.push(args[*i].clone());
        }
        "--no-extensions" | "-ne" => result.no_extensions = true,
        "--skill" if *i + 1 < args.len() => {
            *i += 1;
            result.skills.push(args[*i].clone());
        }
        "--prompt-template" if *i + 1 < args.len() => {
            *i += 1;
            result.prompt_templates.push(args[*i].clone());
        }
        "--theme" if *i + 1 < args.len() => {
            *i += 1;
            result.themes.push(args[*i].clone());
        }
        "--no-skills" | "-ns" => result.no_skills = true,
        "--no-prompt-templates" | "-np" => result.no_prompt_templates = true,
        "--no-themes" => result.no_themes = true,
        "--no-context-files" | "-nc" => result.no_context_files = true,
        _ => return false,
    }
    true
}

fn is_print_message_token(token: &str) -> bool {
    !token.starts_with('@') && (!token.starts_with('-') || token.starts_with("---"))
}

fn split_comma_list(raw: &str, filter_empty: bool) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|part| !filter_empty || !part.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_thinking_level(level: &str) -> Option<ModelThinkingLevel> {
    match level {
        "off" => Some(ModelThinkingLevel::Off),
        "minimal" => Some(ModelThinkingLevel::Minimal),
        "low" => Some(ModelThinkingLevel::Low),
        "medium" => Some(ModelThinkingLevel::Medium),
        "high" => Some(ModelThinkingLevel::High),
        "xhigh" => Some(ModelThinkingLevel::Xhigh),
        "max" => Some(ModelThinkingLevel::Max),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_version_flags() {
        assert!(parse_args(&args(&["--version"])).version);
        assert!(parse_args(&args(&["-v"])).version);
        let result = parse_args(&args(&["--version", "--help", "some message"]));
        assert!(result.version);
        assert!(result.help);
        assert!(result.messages.iter().any(|m| m == "some message"));
    }

    #[test]
    fn parses_help_flags() {
        assert!(parse_args(&args(&["--help"])).help);
        assert!(parse_args(&args(&["-h"])).help);
    }

    #[test]
    fn parses_print_and_optional_prompt() {
        assert!(parse_args(&args(&["--print"])).print);
        assert!(parse_args(&args(&["-p"])).print);

        let prompt = "---\ntitle: hello\n---\nSay hi.";
        let result = parse_args(&args(&["-p", prompt]));
        assert!(result.print);
        assert_eq!(result.messages, vec![prompt]);
        assert!(result.unknown_flags.is_empty());

        let result = parse_args(&args(&["-p", "--provider", "openai", "Say hi."]));
        assert!(result.print);
        assert_eq!(result.provider.as_deref(), Some("openai"));
        assert_eq!(result.messages, vec!["Say hi."]);
    }

    #[test]
    fn parses_continue_and_resume() {
        assert!(parse_args(&args(&["--continue"])).r#continue);
        assert!(parse_args(&args(&["-c"])).r#continue);
        assert!(parse_args(&args(&["--resume"])).resume);
        assert!(parse_args(&args(&["-r"])).resume);
    }

    #[test]
    fn parses_flags_with_values() {
        assert_eq!(
            parse_args(&args(&["--provider", "openai"]))
                .provider
                .as_deref(),
            Some("openai")
        );
        assert_eq!(
            parse_args(&args(&["--model", "gpt-4o"])).model.as_deref(),
            Some("gpt-4o")
        );
        assert_eq!(
            parse_args(&args(&["--api-key", "sk-test-key"]))
                .api_key
                .as_deref(),
            Some("sk-test-key")
        );
        assert_eq!(
            parse_args(&args(&["--system-prompt", "You are a helpful assistant"]))
                .system_prompt
                .as_deref(),
            Some("You are a helpful assistant")
        );
        assert_eq!(
            parse_args(&args(&["--append-system-prompt", "Additional context"]))
                .append_system_prompt,
            vec!["Additional context".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&[
                "--append-system-prompt",
                "Context A",
                "--append-system-prompt",
                "Context B"
            ]))
            .append_system_prompt,
            vec!["Context A".to_owned(), "Context B".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&["--mode", "json"])).mode,
            Some(Mode::Json)
        );
        assert_eq!(parse_args(&args(&["--mode", "rpc"])).mode, Some(Mode::Rpc));
        assert_eq!(
            parse_args(&args(&["--session", "/path/to/session.jsonl"]))
                .session
                .as_deref(),
            Some("/path/to/session.jsonl")
        );
        assert_eq!(
            parse_args(&args(&["--session-id", "orchestrated-session"]))
                .session_id
                .as_deref(),
            Some("orchestrated-session")
        );
        let forked = parse_args(&args(&["--fork", "1234abcd"]));
        assert_eq!(forked.fork.as_deref(), Some("1234abcd"));
        assert!(forked.messages.is_empty());
        assert_eq!(
            parse_args(&args(&["--export", "session.jsonl"]))
                .export
                .as_deref(),
            Some("session.jsonl")
        );
        assert_eq!(
            parse_args(&args(&["--thinking", "high"])).thinking,
            Some(ModelThinkingLevel::High)
        );
        assert_eq!(
            parse_args(&args(&["--models", "gpt-4o,claude-sonnet,gemini-pro"])).models,
            vec![
                "gpt-4o".to_owned(),
                "claude-sonnet".to_owned(),
                "gemini-pro".to_owned()
            ]
        );
    }

    #[test]
    fn parses_name_flag() {
        assert_eq!(
            parse_args(&args(&["--name", "my-session"])).name.as_deref(),
            Some("my-session")
        );
        assert_eq!(
            parse_args(&args(&["-n", "quick-session"])).name.as_deref(),
            Some("quick-session")
        );
        assert_eq!(parse_args(&args(&["--name", ""])).name.as_deref(), Some(""));
        assert_eq!(
            parse_args(&args(&["--name"])).diagnostics,
            vec![Diagnostic {
                level: DiagnosticLevel::Error,
                message: "--name requires a value".to_owned(),
            }]
        );
        let result = parse_args(&args(&[
            "--name",
            "named-run",
            "--print",
            "--model",
            "gpt-4o",
            "hello",
        ]));
        assert_eq!(result.name.as_deref(), Some("named-run"));
        assert!(result.print);
        assert_eq!(result.model.as_deref(), Some("gpt-4o"));
        assert_eq!(result.messages, vec!["hello".to_owned()]);
    }

    #[test]
    fn parses_session_and_resource_flags() {
        assert!(parse_args(&args(&["--no-session"])).no_session);
        assert_eq!(
            parse_args(&args(&["--extension", "./my-extension.ts"])).extensions,
            vec!["./my-extension.ts".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&["-e", "./my-extension.ts"])).extensions,
            vec!["./my-extension.ts".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&["--extension", "./ext1.ts", "-e", "./ext2.ts"])).extensions,
            vec!["./ext1.ts".to_owned(), "./ext2.ts".to_owned()]
        );
        let no_ext = parse_args(&args(&["--no-extensions", "-e", "foo.ts", "-e", "bar.ts"]));
        assert!(no_ext.no_extensions);
        assert_eq!(
            no_ext.extensions,
            vec!["foo.ts".to_owned(), "bar.ts".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&["--skill", "./skill-dir"])).skills,
            vec!["./skill-dir".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&["--skill", "./skill-a", "--skill", "./skill-b"])).skills,
            vec!["./skill-a".to_owned(), "./skill-b".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&["--prompt-template", "./prompts"])).prompt_templates,
            vec!["./prompts".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&[
                "--prompt-template",
                "./one",
                "--prompt-template",
                "./two"
            ]))
            .prompt_templates,
            vec!["./one".to_owned(), "./two".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&["--theme", "./theme.json"])).themes,
            vec!["./theme.json".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&[
                "--theme",
                "./dark.json",
                "--theme",
                "./light.json"
            ]))
            .themes,
            vec!["./dark.json".to_owned(), "./light.json".to_owned()]
        );
        assert!(parse_args(&args(&["--no-skills"])).no_skills);
        assert!(parse_args(&args(&["--no-prompt-templates"])).no_prompt_templates);
        assert!(parse_args(&args(&["--no-themes"])).no_themes);
        assert!(parse_args(&args(&["--no-context-files"])).no_context_files);
        assert!(parse_args(&args(&["-nc"])).no_context_files);
    }

    #[test]
    fn parses_project_verbose_offline_tool_flags() {
        assert_eq!(
            parse_args(&args(&["--approve"])).project_trust_override,
            Some(true)
        );
        assert_eq!(
            parse_args(&args(&["-a"])).project_trust_override,
            Some(true)
        );
        assert_eq!(
            parse_args(&args(&["--no-approve"])).project_trust_override,
            Some(false)
        );
        assert_eq!(
            parse_args(&args(&["-na"])).project_trust_override,
            Some(false)
        );
        assert!(parse_args(&args(&["--verbose"])).verbose);
        assert!(parse_args(&args(&["--offline"])).offline);
        assert!(parse_args(&args(&["--no-tools"])).no_tools);
        assert!(parse_args(&args(&["-nt"])).no_tools);
        assert!(parse_args(&args(&["--no-builtin-tools"])).no_builtin_tools);
        assert!(parse_args(&args(&["-nbt"])).no_builtin_tools);
        assert_eq!(
            parse_args(&args(&["--tools", "read,bash"])).tools,
            vec!["read".to_owned(), "bash".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&["-t", "read,bash"])).tools,
            vec!["read".to_owned(), "bash".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&["--exclude-tools", "read,bash"])).exclude_tools,
            vec!["read".to_owned(), "bash".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&["-xt", "read,bash"])).exclude_tools,
            vec!["read".to_owned(), "bash".to_owned()]
        );
        let no_tools = parse_args(&args(&["--no-tools", "--tools", "read,bash"]));
        assert!(no_tools.no_tools);
        assert_eq!(no_tools.tools, vec!["read".to_owned(), "bash".to_owned()]);
        let no_builtin = parse_args(&args(&["--no-builtin-tools", "--tools", "read,bash"]));
        assert!(no_builtin.no_builtin_tools);
        assert_eq!(no_builtin.tools, vec!["read".to_owned(), "bash".to_owned()]);
    }

    #[test]
    fn parses_messages_files_and_unknown_flags() {
        assert_eq!(
            parse_args(&args(&["hello", "world"])).messages,
            vec!["hello".to_owned(), "world".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&["@README.md", "@src/main.ts"])).file_args,
            vec!["README.md".to_owned(), "src/main.ts".to_owned()]
        );
        let mixed = parse_args(&args(&["@file.txt", "explain this", "@image.png"]));
        assert_eq!(
            mixed.file_args,
            vec!["file.txt".to_owned(), "image.png".to_owned()]
        );
        assert_eq!(mixed.messages, vec!["explain this".to_owned()]);

        let unknown_str = parse_args(&args(&["--unknown-flag", "message"]));
        assert!(unknown_str.messages.is_empty());
        assert_eq!(
            unknown_str.unknown_flags.get("unknown-flag"),
            Some(&FlagValue::Str("message".to_owned()))
        );
        assert_eq!(
            parse_args(&args(&["--unknown-flag"]))
                .unknown_flags
                .get("unknown-flag"),
            Some(&FlagValue::Bool)
        );
        assert_eq!(
            parse_args(&args(&["--unknown-flag=value"]))
                .unknown_flags
                .get("unknown-flag"),
            Some(&FlagValue::Str("value".to_owned()))
        );
    }

    #[test]
    fn parses_complex_combination() {
        let result = parse_args(&args(&[
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet",
            "--print",
            "--thinking",
            "high",
            "@prompt.md",
            "Do the task",
        ]));
        assert_eq!(result.provider.as_deref(), Some("anthropic"));
        assert_eq!(result.model.as_deref(), Some("claude-sonnet"));
        assert!(result.print);
        assert_eq!(result.thinking, Some(ModelThinkingLevel::High));
        assert_eq!(result.file_args, vec!["prompt.md".to_owned()]);
        assert_eq!(result.messages, vec!["Do the task".to_owned()]);
    }

    #[test]
    fn list_models_optional_search_and_thinking_diagnostics() {
        assert_eq!(
            parse_args(&args(&["--list-models"])).list_models,
            ListModels::All
        );
        assert_eq!(
            parse_args(&args(&["--list-models", "sonnet"])).list_models,
            ListModels::Search("sonnet".to_owned())
        );
        assert_eq!(
            parse_args(&args(&["--list-models", "--verbose"])).list_models,
            ListModels::All
        );
        assert!(parse_args(&args(&["--list-models", "--verbose"])).verbose);
        assert_eq!(
            parse_args(&args(&["--list-models", "@file.txt"])).list_models,
            ListModels::All
        );
        assert_eq!(
            parse_args(&args(&["--list-models", "@file.txt"])).file_args,
            vec!["file.txt".to_owned()]
        );

        let bad = parse_args(&args(&["--thinking", "nope"]));
        assert!(bad.thinking.is_none());
        assert_eq!(
            bad.diagnostics,
            vec![Diagnostic {
                level: DiagnosticLevel::Warning,
                message: format!(
                    "Invalid thinking level \"nope\". Valid values: {VALID_THINKING_LEVELS}"
                ),
            }]
        );
    }

    #[test]
    fn unknown_short_option_and_stable_unknown_long_order() {
        assert_eq!(
            parse_args(&args(&["-z"])).diagnostics,
            vec![Diagnostic {
                level: DiagnosticLevel::Error,
                message: "Unknown option: -z".to_owned(),
            }]
        );

        let ordered = parse_args(&args(&[
            "--beta",
            "--alpha=1",
            "--gamma",
            "value",
            "--delta",
        ]));
        let keys: Vec<&str> = ordered.unknown_flags.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["beta", "alpha", "gamma", "delta"]);
        assert_eq!(ordered.unknown_flags.get("beta"), Some(&FlagValue::Bool));
        assert_eq!(
            ordered.unknown_flags.get("alpha"),
            Some(&FlagValue::Str("1".to_owned()))
        );
        assert_eq!(
            ordered.unknown_flags.get("gamma"),
            Some(&FlagValue::Str("value".to_owned()))
        );
        assert_eq!(ordered.unknown_flags.get("delta"), Some(&FlagValue::Bool));
    }

    #[test]
    fn tools_filter_empty_segments_models_keep_trimmed() {
        assert_eq!(
            parse_args(&args(&["--tools", "read,,bash, ,edit"])).tools,
            vec!["read".to_owned(), "bash".to_owned(), "edit".to_owned()]
        );
        assert_eq!(
            parse_args(&args(&["--models", " a , b "])).models,
            vec!["a".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    fn invalid_mode_is_ignored_and_bare_mode_is_unknown() {
        assert_eq!(parse_args(&args(&["--mode", "xml"])).mode, None);
        let bare = parse_args(&args(&["--mode"]));
        assert_eq!(bare.mode, None);
        assert_eq!(bare.unknown_flags.get("mode"), Some(&FlagValue::Bool));
    }

    #[test]
    fn print_does_not_consume_at_file() {
        let result = parse_args(&args(&["-p", "@prompt.md"]));
        assert!(result.print);
        assert!(result.messages.is_empty());
        assert_eq!(result.file_args, vec!["prompt.md".to_owned()]);
    }

    // ──────────────────────────────────────────────────────────────────
    // XC-9 / M21: spawn CLI flags witness — -e/--extension parsing
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn m21_extension_flag_e_and_long_form_equivalent() {
        let short = parse_args(&args(&["-e", "./ext.ts"]));
        let long = parse_args(&args(&["--extension", "./ext.ts"]));
        assert_eq!(short.extensions, long.extensions);
        assert_eq!(short.extensions, vec!["./ext.ts".to_owned()]);
    }

    #[test]
    fn m21_multiple_e_flags_accumulate_in_order() {
        let result = parse_args(&args(&["-e", "./a.ts", "-e", "./b.ts", "-e", "./c.ts"]));
        assert_eq!(
            result.extensions,
            vec![
                "./a.ts".to_owned(),
                "./b.ts".to_owned(),
                "./c.ts".to_owned()
            ]
        );
    }
}
