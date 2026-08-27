# Themes

Ported from `.references/pi/packages/coding-agent/docs/themes.md` at pin
`8fa7eebd`, at reduced depth. Claims are bound to the evidence manifest
[evidence/themes.json](evidence/themes.json); the theme file-format reference
has not been ported yet and is listed under "Pending port surface".

## What themes are

Themes are JSON files that define the colors pi renders in the terminal.

## Discovery flags

The executed `--help` snapshot of this build documents the loading flags:

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
<!-- doc-c:fence=themes.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

`--theme` is repeatable, and explicit paths stay applied when `--no-themes`
skips package and settings discovery. The reference discovery contract loads
themes from built-in `dark` and `light`, `~/.pi/agent/themes/*.json` (global),
`.pi/themes/*.json` (project-local, only after the project is trusted), package
`themes/` directories, and the settings `themes` array.

The e2e-smoke harness runs the Rust binary with `--no-themes` in its base
invocation, exercising the gating flag end to end (see
[evidence/themes.json](evidence/themes.json) for the transcript binding).

## Pending port surface

- theme JSON format and the 51-token color reference, covering core UI,
  backgrounds, markdown, tool diffs, syntax, thinking borders, and bash mode
  (unported-feature)
- color value formats and terminal truecolor compatibility notes
  (unported-feature)
- theme selection via `/settings` and terminal background detection
  (TUI-CLOSE)
- hot reload of the active custom theme file (TUI-CLOSE)
- `--use-theme` initial-theme flag from the reference; this build exposes the
  `--theme` loading flag instead (unported-feature)
