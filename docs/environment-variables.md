# Environment variables

Ported from `.references/pi/packages/coding-agent/docs/environment-variables.md` at pin
`8fa7eebd23535522c8104166b4f1f959b4e2f10`. Claims below are bound to the
evidence manifest [evidence/environment-variables.json](evidence/environment-variables.json); anything not
yet provable in the Rust port is listed under "Pending port surface" instead of
being described as working.

The reference groups pi's environment variables into process configuration,
process markers for child processes, and a shell-tool session environment. In
this port the executed `--help` snapshot is the authority for provider keys and
`PI_*` configuration variables; the marker and session-environment families are
listed under "Pending port surface".

## Provider API keys

API-key providers read credentials from the environment before pi starts. The
full block below is the executed `--help` snapshot for this build:

```text
pi - AI coding assistant with read, bash, edit, write tools

Usage:
  pi [options] [@files...] [messages...]

Commands:
  pi install <source> [-l]     Install extension source and add to settings
  pi remove <source> [-l]      Remove extension source from settings
  pi uninstall <source> [-l]   Alias for remove
  pi update [source|self|pi]   Update pi, extensions, or model catalogs
  pi list                      List installed extensions from settings
  pi config [-l]               Open TUI to enable/disable package resources (Tab switches scope)
  pi <command> --help          Show help for install/remove/uninstall/update/list/config

Options:
  --provider <name>              Provider name (default: google)
  --model <pattern>              Model pattern or ID (supports "provider/id" and optional ":<thinking>")
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

Extensions can register additional flags (e.g., --plan from plan-mode extension).

Examples:
  # Interactive mode
  pi

  # Interactive mode with initial prompt
  pi "List all .ts files in src/"

  # Include files in initial message
  pi @prompt.md @image.png "What color is the sky?"

  # Non-interactive mode (process and exit)
  pi -p "List all .ts files in src/"

  # Multiple messages (interactive)
  pi "Read package.json" "What dependencies do we have?"

  # Continue previous session
  pi --continue "What did we discuss?"

  # Start a named session
  pi --name "Refactor auth module"

  # Use different model
  pi --provider openai --model gpt-4o-mini "Help me refactor this code"

  # Use model with provider prefix (no --provider needed)
  pi --model openai/gpt-4o "Help me refactor this code"

  # Use model with thinking level shorthand
  pi --model sonnet:high "Solve this complex problem"

  # Limit model cycling to specific models
  pi --models claude-sonnet,claude-haiku,gpt-4o

  # Limit to a specific provider with glob pattern
<!-- doc-c:fence=environment-variables.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

Grouping:

- Anthropic: `ANTHROPIC_API_KEY`, `ANTHROPIC_OAUTH_TOKEN`.
- Google: `GEMINI_API_KEY`.
- OpenAI and OpenAI-compatible hosts: `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`,
  `GROQ_API_KEY`, `TOGETHER_API_KEY`, `FIREWORKS_API_KEY`,
  `OPENROUTER_API_KEY`, `MISTRAL_API_KEY`, `MINIMAX_API_KEY`,
  `MOONSHOT_API_KEY`, `KIMI_API_KEY`, `NVIDIA_API_KEY`, `CEREBRAS_API_KEY`,
  `XAI_API_KEY`, `OPENCODE_API_KEY`, `AI_GATEWAY_API_KEY`, `ANT_LING_API_KEY`.
- Azure OpenAI: `AZURE_OPENAI_API_KEY` plus the `AZURE_OPENAI_*` base URL,
  resource name, API version, and deployment-map variables.
- Amazon Bedrock: the `AWS_*` profile, key, bearer-token, and region variables.
- Cloudflare: `CLOUDFLARE_API_KEY`, `CLOUDFLARE_ACCOUNT_ID`,
  `CLOUDFLARE_GATEWAY_ID`.
- Regional coding-plan keys: `ZAI_API_KEY`, `ZAI_CODING_CN_API_KEY`, and the
  `XIAOMI_*` variables.

Which key belongs to which provider, and how stored credentials interact with
the environment, is covered in [providers.md](providers.md). The `--api-key`
flag overrides the environment for one run; see the options block quoted in
[usage.md](usage.md).

## Pi process configuration

These variables are read by pi itself. The first six appear verbatim in the
snapshot above; the last three are implemented in the ported source (the
manifest binds the claim for `PI_SKIP_VERSION_CHECK`; the other two are
verified against the same committed sources).

| Variable | Effect |
|----------|--------|
| `PI_CODING_AGENT_DIR` | Config directory; default `~/.pi/agent` |
| `PI_CODING_AGENT_SESSION_DIR` | Session storage directory; `--session-dir` overrides it |
| `PI_PACKAGE_DIR` | Package directory override, for Nix/Guix store paths |
| `PI_OFFLINE` | Disables startup network operations when set to a truthy value; same as `--offline` |
| `PI_TELEMETRY` | Overrides install/update telemetry and provider attribution |
| `PI_SHARE_VIEWER_URL` | Base URL for the share viewer |
| `PI_SKIP_VERSION_CHECK` | Disables the periodic latest-version check |
| `PI_CACHE_RETENTION` | Set to `long` for extended provider prompt caching where supported |
| `PI_HARDWARE_CURSOR` | Hardware cursor fallback when the `showHardwareCursor` setting is unset |

The external editor command resolves through `VISUAL` then `EDITOR` when no
`externalEditor` is configured, and managed HTTP clients carry the proxy URL as
`HTTP_PROXY`/`HTTPS_PROXY`; both fallbacks are implemented in the ported
settings layer, verified against the committed source.

Offline one-shot example:

```bash
PI_OFFLINE=1 pi --print "Work without any startup network access"
<!-- doc-c:fence=environment-variables.02 -->
```

## Pending port surface

- Process markers `AI_AGENT=pi` and `PI_CODING_AGENT=true` set for child
  processes — unported-feature
- Shell-tool session environment (`PI_SESSION_ID`, `PI_SESSION_FILE`,
  `PI_PROVIDER`, `PI_MODEL`, `PI_REASONING_LEVEL`) injected into `bash` tool
  commands — unported-feature
- `PI_TUI_ESC_TIMEOUT` lone-ESC/Alt-key disambiguation timeout — TUI-CLOSE
