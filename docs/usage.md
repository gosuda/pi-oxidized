# Usage

Ported from `.references/pi/packages/coding-agent/docs/usage.md` at pin
`8fa7eebd23535522c8104166b4f1f959b4e2f10`. Claims below are bound to the
evidence manifest [evidence/usage.json](evidence/usage.json); anything not
yet provable in the Rust port is listed under "Pending port surface" instead of
being described as working.

## Interactive mode

Run `pi` in the directory you want it to work on and type prompts in the
editor. While the agent is working you can queue steering messages that are
delivered after the current assistant turn finishes its tool calls. The
interactive loop, tool execution, steering, context compaction, session fork
and resume, extension dialogs, custom extension UI, and extension reload are
exercised end to end by the e2e smoke harness bound in the manifest.

Shortcuts and their customization live in [keybindings.md](keybindings.md);
session mechanics in [sessions.md](sessions.md); context compaction in
[compaction.md](compaction.md).

## CLI reference

The executed `--help` snapshot documents the invocation forms this build
accepts:

```text
pi - AI coding assistant with read, bash, edit, write tools

Usage:
  pi [options] [@files...] [messages...]
<!-- doc-c:fence=usage.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

The complete options block from the same snapshot:

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
<!-- doc-c:fence=usage.02 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

The snapshot adds, directly below the options list, that extensions can
register additional flags, citing `--plan` from the plan-mode extension as the
example. Prefix file arguments with `@` to include them in the initial message;
the snapshot examples include `pi @prompt.md @image.png "What color is the
sky?"`.

### Modes

| Mode | Invocation | Notes |
|------|------------|-------|
| Interactive (default) | `pi [messages...]` | TUI session in the current directory |
| Print | `--print`, `-p` | Process the prompt and exit |
| JSON events | `--mode json` | See [json.md](json.md) |
| RPC | `--mode rpc` | See [rpc.md](rpc.md); full parity is pending |
| Export | `--export <file>` | Export a session file to HTML and exit |

### Command surface

The package-management subcommands (`pi install`, `pi remove`, `pi uninstall`,
`pi update`, `pi list`, `pi config`) are quoted with their executed snapshot in
[packages.md](packages.md). The trust flags `--approve`/`-a` and
`--no-approve`/`-na` gate project-local files for a single run, and `--offline`
disables startup network operations, all per the options block above.

## Slash commands

The reference documents an interactive slash-command inventory invoked by
typing `/` in the editor. That inventory is not bound to evidence in this port;
see "Pending port surface". The provable command surface is the flag and
subcommand set quoted above: modes, session flags (`--continue`, `--resume`,
`--session`, `--fork`, `--no-session`, `--name`), tool flags (`--tools`,
`--exclude-tools`, `--no-tools`, `--no-builtin-tools`), resource flags
(`--extension`, `--skill`, `--prompt-template`, `--theme`, and their `--no-*`
disables), and the package subcommands.

## Sessions

```bash
pi -c                   # Continue most recent session
pi -r                   # Select a session to resume
pi --no-session         # Ephemeral mode; do not save
pi --name "my task"     # Set session display name at startup
<!-- doc-c:fence=usage.03 -->
```

Session storage layout, fork, and resume semantics are covered in
[sessions.md](sessions.md).

## Context files

`AGENTS.md` and `CLAUDE.md` discovery runs at startup and is gated by
`--no-context-files`/`-nc` in the options block above. The e2e smoke harness
exercises interactive startup including context loading; see
[quickstart.md](quickstart.md) for the discovery contract.

## Pending port surface

- Interactive slash-command inventory (`/login`, `/logout`, `/model`,
  `/scoped-models`, `/settings`, `/resume`, `/new`, `/name`, `/session`,
  `/tree`, `/trust`, `/fork`, `/clone`, `/compact`, `/copy`, `/export`,
  `/import`, `/share`, `/reload`, `/hotkeys`, `/changelog`, `/quit`) —
  unported-feature
- Editor image support: paste, drag-drop, and inline images via the Kitty
  graphics protocol — TUI-CLOSE
- `--tui-mode` and `--use-theme` run flags from the reference CLI —
  unported-feature
- `/llama` command and llama.cpp router integration — unported-feature
- Windows Terminal Alt+Enter fullscreen remapping guidance — TUI-CLOSE
- Full `--mode rpc` surface parity, including the known `clear_queue` boundary
  gap — PAR-CLOSE
