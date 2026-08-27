# Models

Ported from `.references/pi/packages/coding-agent/docs/models.md` at pin
`8fa7eebd23535522c8104166b4f1f959b4e2f10`. Claims below are bound to the
evidence manifest [evidence/models.json](evidence/models.json); anything not
yet provable in the Rust port is listed under "Pending port surface" instead of
being described as working.

## Selecting a model on the command line

The model-related option lines from the executed `--help` snapshot:

```text
pi - AI coding assistant with read, bash, edit, write tools

Usage:
  pi [options] [@files...] [messages...]
<!-- doc-c:fence=models.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

```text
pi - AI coding assistant with read, bash, edit, write tools

Usage:
<!-- doc-c:fence=models.02 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

```text
pi - AI coding assistant with read, bash, edit, write tools
<!-- doc-c:fence=models.03 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

```text
pi - AI coding assistant with read, bash, edit, write tools

Usage:
  pi [options] [@files...] [messages...]

Commands:
  pi install <source> [-l]     Install extension source and add to settings
  pi remove <source> [-l]      Remove extension source from settings
  pi uninstall <source> [-l]   Alias for remove
  pi update [source|self|pi]   Update pi, extensions, or model catalogs
<!-- doc-c:fence=models.04 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

In this build `--provider` defaults to `google`; `--model` accepts a
`provider/id` prefix and an optional `:<thinking>` suffix; `--thinking` spans
`off` through `max`; `--models` constrains Ctrl+P cycling and supports globs
and fuzzy matching; `--list-models` lists the catalog with an optional fuzzy
search; `--api-key` bypasses stored credentials for the run.

The model examples from the same snapshot:

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
<!-- doc-c:fence=models.05 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

## Built-in catalog

Model metadata for built-in providers is compiled into the binary: the catalog
module loads `data/builtin-models.json` through `include_str!` at build time,
so no Node or Bun runtime is needed to resolve models. Entries carry per-model
metadata including reasoning flags and `thinkingLevelMap`, the per-level
thinking controls.

## Custom models with models.json

Custom providers and models load from `~/.pi/agent/models.json`. Provider
objects accept `name`, `baseUrl`, `apiKey` (literal, `$ENV`, or `!command`
template), `api`, `headers` (templated), `authHeader`, an explicit `models`
list, and `modelOverrides`; overrides are merged onto the effective model list
and never mutate built-in entries. Per-model `thinkingLevelMap` entries map
each pi thinking level to the provider's effort string, or `null` to hide the
level; built-in catalog entries already use them.

Auth for custom providers resolves from stored credentials, the `--api-key`
flag, or the provider `apiKey` template; see [providers.md](providers.md) for
the credential store and [custom-provider.md](custom-provider.md) for adding
whole providers with custom transports.

Provider adapters honor `compat` objects, including `supportsDeveloperRole`,
`supportsReasoningEffort`, `forceAdaptiveThinking`,
`supportsEagerToolInputStreaming`, and `allowEmptySignature` on
Anthropic-compatible adapters, and `thinkingFormat`, `cacheControlFormat`, and
`chatTemplateKwargs` on OpenAI-compatible adapters.

`pi update` covers model catalogs alongside pi and extensions; see the Commands
block quoted in [packages.md](packages.md). Remote catalog refresh in the model
runtime defaults to offline.

## Pending port surface

- `samplingParams` free-form merge into request bodies — unported-feature
- Cost tiers (`inputTokensAbove` alternate rate sets) — unported-feature
- `/model` picker and `/scoped-models` slash commands — unported-feature
- Executed `models.json` fixture walkthroughs for this page — DOC-D
