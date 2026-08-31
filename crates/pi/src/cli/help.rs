//! Help text for the `pi` CLI, matching coding-agent `printHelp`.

use crate::core::config::{APP_NAME, CONFIG_DIR_NAME, ENV_AGENT_DIR, ENV_SESSION_DIR};
use std::fmt::Write as _;

/// Extension CLI flag metadata rendered under the help "Extension CLI Flags" section.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionFlagHelp {
    /// Flag name without the leading `--`.
    pub name: String,
    /// Optional human description.
    pub description: Option<String>,
    /// Whether the flag expects a string value.
    pub takes_value: bool,
    /// Absolute or package-relative extension path used for the default description.
    pub extension_path: String,
}

/// Options controlling help rendering.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HelpStyle {
    /// When true, bold section headers with ANSI styles.
    pub styled: bool,
}

/// Format the full help text without writing to stdout.
///
/// When `style.styled` is false the output is plain text (tests / redirected
/// consumers). When true, section headers use `anstyle` bold, matching chalk
/// in the TypeScript `printHelp` path.
#[must_use]
pub fn format_help(extension_flags: Option<&[ExtensionFlagHelp]>, style: HelpStyle) -> String {
    let bold = if style.styled {
        anstyle::Style::new().bold()
    } else {
        anstyle::Style::new()
    };
    let bold_s = bold.render();
    let bold_e = bold.render_reset();

    let extension_flags_text = format_extension_flags(extension_flags, style);

    let env_agent = format!("{ENV_AGENT_DIR:<32}");
    let env_session = format!("{ENV_SESSION_DIR:<32}");

    format!(
        "{bold_s}{APP_NAME}{bold_e} - AI coding assistant with read, bash, edit, write tools

{bold_s}Usage:{bold_e}
  {APP_NAME} [options] [@files...] [messages...]

{bold_s}Commands:{bold_e}
  {APP_NAME} install <source> [-l]     Install extension source and add to settings
  {APP_NAME} remove <source> [-l]      Remove extension source from settings
  {APP_NAME} uninstall <source> [-l]   Alias for remove
  {APP_NAME} update [source|self|pi]   Update pi, extensions, or model catalogs
  {APP_NAME} list                      List installed extensions from settings
  {APP_NAME} config [-l]               Open TUI to enable/disable package resources (Tab switches scope)
  {APP_NAME} <command> --help          Show help for install/remove/uninstall/update/list/config

{bold_s}Options:{bold_e}
  --provider <name>              Provider name (default: google)
  --model <pattern>              Model pattern or ID (supports \"provider/id\" and optional \":<thinking>\")
  --api-key <key>                API key (defaults to env vars)
  --system-prompt <text>         System prompt (default: coding assistant prompt)
  --append-system-prompt <text>  Append text or file contents to the system prompt (can be used multiple times)
  --mode <mode>                  Output mode: text (default), json, or rpc
  --print, -p                    Non-interactive mode: process prompt and exit
  --continue, -c                 Continue previous session
  --resume, -r                   Select a session to resume
  --session <path|id>            Use specific session file or partial UUID
  --session-id <id>              Use exact project session ID, creating it if missing
  --fork <path|id>               Fork specific session file or partial UUID into a new session
  --session-dir <dir>            Directory for session storage and lookup
  --no-session                   Don't save session (ephemeral)
  --name, -n <name>              Set session display name
  --models <patterns>            Comma-separated model patterns for Ctrl+P cycling
                                 Supports globs (anthropic/*, *sonnet*) and fuzzy matching
  --no-tools, -nt                Disable all tools by default (built-in and extension)
  --no-builtin-tools, -nbt       Disable built-in tools by default but keep extension/custom tools enabled
  --tools, -t <tools>            Comma-separated allowlist of tool names to enable
                                 Applies to built-in, extension, and custom tools
  --exclude-tools, -xt <tools>   Comma-separated denylist of tool names to disable
                                 Applies to built-in, extension, and custom tools
  --thinking <level>             Set thinking level: off, minimal, low, medium, high, xhigh, max
  --extension, -e <path>         Load an extension file (can be used multiple times)
  --no-extensions, -ne           Disable extension discovery (explicit -e paths still work)
  --skill <path>                 Load a skill file or directory (can be used multiple times)
  --no-skills, -ns               Disable skills discovery and loading
  --prompt-template <path>       Load a prompt template file or directory (can be used multiple times)
  --no-prompt-templates, -np     Disable prompt template discovery and loading
  --theme <path>                 Load a theme file or directory (can be used multiple times)
  --no-themes                    Disable theme discovery and loading
  --no-context-files, -nc        Disable AGENTS.md and CLAUDE.md discovery and loading
  --export <file>                Export session file to HTML and exit
  --list-models [search]         List available models (with optional fuzzy search)
  --verbose                      Force verbose startup (overrides quietStartup setting)
  --approve, -a                  Trust project-local files for this run
  --no-approve, -na              Ignore project-local files for this run
  --offline                      Disable startup network operations (same as PI_OFFLINE=1)
  --help, -h                     Show this help
  --version, -v                  Show version number

Extensions can register additional flags (e.g., --plan from plan-mode extension).{extension_flags_text}

{bold_s}Examples:{bold_e}
  # Interactive mode
  {APP_NAME}

  # Interactive mode with initial prompt
  {APP_NAME} \"List all .ts files in src/\"

  # Include files in initial message
  {APP_NAME} @prompt.md @image.png \"What color is the sky?\"

  # Non-interactive mode (process and exit)
  {APP_NAME} -p \"List all .ts files in src/\"

  # Multiple messages (interactive)
  {APP_NAME} \"Read package.json\" \"What dependencies do we have?\"

  # Continue previous session
  {APP_NAME} --continue \"What did we discuss?\"

  # Start a named session
  {APP_NAME} --name \"Refactor auth module\"

  # Use different model
  {APP_NAME} --provider openai --model gpt-4o-mini \"Help me refactor this code\"

  # Use model with provider prefix (no --provider needed)
  {APP_NAME} --model openai/gpt-4o \"Help me refactor this code\"

  # Use model with thinking level shorthand
  {APP_NAME} --model sonnet:high \"Solve this complex problem\"

  # Limit model cycling to specific models
  {APP_NAME} --models claude-sonnet,claude-haiku,gpt-4o

  # Limit to a specific provider with glob pattern
  {APP_NAME} --models \"github-copilot/*\"

  # Cycle models with fixed thinking levels
  {APP_NAME} --models sonnet:high,haiku:low

  # Start with a specific thinking level
  {APP_NAME} --thinking high \"Solve this complex problem\"

  # Read-only mode (no file modifications possible)
  {APP_NAME} --tools read,grep,find,ls -p \"Review the code in src/\"

  # Disable one tool while keeping the rest available
  {APP_NAME} --exclude-tools ask_question

  # Export a session file to HTML
  {APP_NAME} --export ~/{CONFIG_DIR_NAME}/agent/sessions/--path--/session.jsonl
  {APP_NAME} --export session.jsonl output.html

{bold_s}Environment Variables:{bold_e}
  ANTHROPIC_API_KEY                - Anthropic Claude API key
  ANTHROPIC_OAUTH_TOKEN            - Anthropic OAuth token (alternative to API key)
  ANT_LING_API_KEY                 - Ant Ling API key
  OPENAI_API_KEY                   - OpenAI GPT API key
  AZURE_OPENAI_API_KEY             - Azure OpenAI API key
  AZURE_OPENAI_BASE_URL            - Azure OpenAI/Cognitive Services base URL (e.g. https://{{resource}}.openai.azure.com)
  AZURE_OPENAI_RESOURCE_NAME       - Azure OpenAI resource name (alternative to base URL)
  AZURE_OPENAI_API_VERSION         - Azure OpenAI API version (default: v1)
  AZURE_OPENAI_DEPLOYMENT_NAME_MAP - Azure OpenAI model=deployment map (comma-separated)
  DEEPSEEK_API_KEY                 - DeepSeek API key
  NVIDIA_API_KEY                   - NVIDIA NIM API key
  GEMINI_API_KEY                   - Google Gemini API key
  GROQ_API_KEY                     - Groq API key
  CEREBRAS_API_KEY                 - Cerebras API key
  XAI_API_KEY                      - xAI Grok API key
  FIREWORKS_API_KEY                - Fireworks API key
  TOGETHER_API_KEY                 - Together AI API key
  OPENROUTER_API_KEY               - OpenRouter API key
  AI_GATEWAY_API_KEY               - Vercel AI Gateway API key
  ZAI_API_KEY                      - ZAI Coding Plan API key (Global)
  ZAI_CODING_CN_API_KEY            - ZAI Coding Plan API key (China)
  MISTRAL_API_KEY                  - Mistral API key
  MINIMAX_API_KEY                  - MiniMax API key
  MOONSHOT_API_KEY                 - Moonshot AI API key
  OPENCODE_API_KEY                 - OpenCode Zen/OpenCode Go API key
  KIMI_API_KEY                     - Kimi For Coding API key
  CLOUDFLARE_API_KEY               - Cloudflare API token (Workers AI and AI Gateway)
  CLOUDFLARE_ACCOUNT_ID            - Cloudflare account id (required for both)
  CLOUDFLARE_GATEWAY_ID            - Cloudflare AI Gateway slug (required for AI Gateway)
  XIAOMI_API_KEY                   - Xiaomi MiMo API key (api.xiaomimimo.com billing)
  XIAOMI_TOKEN_PLAN_CN_API_KEY     - Xiaomi MiMo Token Plan API key (China region)
  XIAOMI_TOKEN_PLAN_AMS_API_KEY    - Xiaomi MiMo Token Plan API key (Amsterdam region)
  XIAOMI_TOKEN_PLAN_SGP_API_KEY    - Xiaomi MiMo Token Plan API key (Singapore region)
  AWS_PROFILE                      - AWS profile for Amazon Bedrock
  AWS_ACCESS_KEY_ID                - AWS access key for Amazon Bedrock
  AWS_SECRET_ACCESS_KEY            - AWS secret key for Amazon Bedrock
  AWS_BEARER_TOKEN_BEDROCK         - Bedrock API key (bearer token)
  AWS_REGION                       - AWS region for Amazon Bedrock (e.g., us-east-1)
  {env_agent} - Config directory (default: ~/{CONFIG_DIR_NAME}/agent)
  {env_session} - Session storage directory (overridden by --session-dir)
  PI_PACKAGE_DIR                   - Override package directory (for Nix/Guix store paths)
  PI_OFFLINE                       - Disable startup network operations when set to 1/true/yes
  PI_TELEMETRY                     - Override install telemetry when set to 1/true/yes or 0/false/no
  PI_SHARE_VIEWER_URL              - Base URL for /share command (default: https://pi.dev/session/)
  PI_HYPERLINKS                    - Hyperlink override: 1, 0, auto
  PI_IMAGE_PROTOCOL                - Image protocol override: kitty, iterm2, none, 0, auto
  PI_TRUE_COLOR                    - True color override: 1, 0, auto

{bold_s}Built-in Tool Names:{bold_e}
  read   - Read file contents
  bash   - Execute bash commands
  edit   - Edit files with find/replace
  write  - Write files (creates/overwrites)
  grep   - Search file contents (read-only, off by default)
  find   - Find files by glob pattern (read-only, off by default)
  ls     - List directory contents (read-only, off by default)
"
    )
}

fn format_extension_flags(
    extension_flags: Option<&[ExtensionFlagHelp]>,
    style: HelpStyle,
) -> String {
    let Some(flags) = extension_flags else {
        return String::new();
    };
    if flags.is_empty() {
        return String::new();
    }

    let bold = if style.styled {
        anstyle::Style::new().bold()
    } else {
        anstyle::Style::new()
    };
    let bold_s = bold.render();
    let bold_e = bold.render_reset();

    let mut lines = String::new();
    lines.push('\n');
    let Ok(()) = writeln!(lines, "{bold_s}Extension CLI Flags:{bold_e}") else {
        return lines;
    };
    for flag in flags {
        let value = if flag.takes_value { " <value>" } else { "" };
        let left = format!("  --{}{value}", flag.name);
        let description = flag
            .description
            .clone()
            .unwrap_or_else(|| format!("Registered by {}", flag.extension_path));
        let padded = if left.len() < 30 {
            format!("{left:<30}")
        } else {
            left
        };
        lines.push_str(&padded);
        lines.push_str(&description);
        lines.push('\n');
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_help_contains_required_sections() {
        let text = format_help(None, HelpStyle { styled: false });
        assert!(text.starts_with("pi - AI coding assistant with read, bash, edit, write tools"));
        assert!(text.contains("Usage:"));
        assert!(text.contains("  pi [options] [@files...] [messages...]"));
        assert!(text.contains("Commands:"));
        assert!(text.contains("Options:"));
        assert!(text.contains("--list-models [search]"));
        assert!(text.contains("--thinking <level>"));
        assert!(text.contains("Examples:"));
        assert!(text.contains("Environment Variables:"));
        assert!(text.contains("PI_CODING_AGENT_DIR"));
        assert!(text.contains("PI_CODING_AGENT_SESSION_DIR"));
        assert!(text.contains("Built-in Tool Names:"));
        assert!(text.contains("  read   - Read file contents"));
        assert!(text.contains("  ls     - List directory contents (read-only, off by default)"));
        assert!(!text.contains("Extension CLI Flags:"));
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn styled_help_uses_ansi_bold_on_headers() {
        let text = format_help(None, HelpStyle { styled: true });
        assert!(text.contains("\u{1b}[1mpi\u{1b}[0m") || text.contains("\u{1b}[1mpi\u{1b}[m"));
        assert!(text.contains("Usage:"));
        assert!(text.contains('\u{1b}'));
    }

    #[test]
    fn extension_flags_section_is_optional_and_padded() {
        let flags = [
            ExtensionFlagHelp {
                name: "plan".to_owned(),
                description: Some("Enable plan mode".to_owned()),
                takes_value: false,
                extension_path: "/tmp/plan.ts".to_owned(),
            },
            ExtensionFlagHelp {
                name: "depth".to_owned(),
                description: None,
                takes_value: true,
                extension_path: "/tmp/depth.ts".to_owned(),
            },
        ];
        let text = format_help(Some(&flags), HelpStyle { styled: false });
        assert!(text.contains("Extension CLI Flags:"));
        assert!(text.contains("  --plan"));
        assert!(text.contains("Enable plan mode"));
        assert!(text.contains("  --depth <value>"));
        assert!(text.contains("Registered by /tmp/depth.ts"));

        let empty = format_help(Some(&[]), HelpStyle { styled: false });
        assert!(!empty.contains("Extension CLI Flags:"));
    }

    #[test]
    fn export_examples_use_config_dir_name() {
        let text = format_help(None, HelpStyle::default());
        assert!(text.contains("--export ~/.pi/agent/sessions/--path--/session.jsonl"));
        assert!(text.contains("--export session.jsonl output.html"));
    }

    #[test]
    fn help_lists_terminal_capability_override_variables_and_values() {
        let text = format_help(None, HelpStyle { styled: false });
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines.contains(&"  PI_HYPERLINKS                    - Hyperlink override: 1, 0, auto")
        );
        assert!(lines.contains(&"  PI_IMAGE_PROTOCOL                - Image protocol override: kitty, iterm2, none, 0, auto"));
        assert!(
            lines.contains(&"  PI_TRUE_COLOR                    - True color override: 1, 0, auto")
        );
    }
}
