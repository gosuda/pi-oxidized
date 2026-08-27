# Providers

Ported from `.references/pi/packages/coding-agent/docs/providers.md` at pin
`8fa7eebd`. Claims below are bound to the evidence manifest
[evidence/providers.json](evidence/providers.json); anything not yet provable
in the Rust port is listed under "Pending port surface" instead of being
described as working.

## API key authentication

API-key providers read their credentials from environment variables before pi
starts. The executed `--help` snapshot of `target/release/pi` enumerates every
provider credential variable this build recognizes, quoted verbatim below:

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
<!-- doc-c:fence=providers.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

A single run can override the environment with `--api-key <key>`; the same
snapshot documents the flag as "API key (defaults to env vars)". The full
variable table, including the `PI_*` process variables that are not provider
credentials, lives in [environment-variables.md](environment-variables.md).

## Selecting a provider and model

`--provider <name>` selects the provider (default `google`) and `--model`
accepts a `provider/id` prefix so a provider can be pinned per invocation.
The invocation forms are documented in the snapshot's Examples block:

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
<!-- doc-c:fence=providers.02 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

`--list-models` prints the models visible to the configured credentials.
Model patterns, `--models` glob cycling, and thinking-level shorthands are
covered in [models.md](models.md); the surrounding CLI surface is covered in
[usage.md](usage.md).

## Custom providers

Providers that speak a supported API can be registered declaratively, and
extension-based providers can carry their own transport and credential flow.
Both routes are documented in [custom-provider.md](custom-provider.md).

## Pending port surface

- `/login` and `/logout` subscription flows — ChatGPT Plus/Pro (Codex),
  Claude Pro/Max, GitHub Copilot, xAI, OpenRouter PKCE, and the Radius
  gateway (unported-feature)
- `auth.json` credential storage — file format, `0600` permissions, key
  resolution (`!command` execution, environment interpolation,
  provider-scoped `env` objects) and its priority over environment variables
  (unported-feature)
- provider-specific cloud behaviors beyond the variables quoted above — Azure
  OpenAI deployment mapping, Amazon Bedrock ambient credentials and prompt
  caching, Cloudflare AI Gateway and Workers AI routing, Google Vertex AI
  application default credentials (unported-feature)
- the llama.cpp router provider and `/llama` model management
  (unported-feature)
- `models-store.json` catalog refresh and offline caching (unported-feature)
