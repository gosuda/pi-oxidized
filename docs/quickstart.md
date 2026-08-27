# Quickstart

Ported from `.references/pi/packages/coding-agent/docs/quickstart.md` at pin
`8fa7eebd`. Claims below are bound to the evidence manifest
[evidence/quickstart.json](evidence/quickstart.json); anything not yet provable
in the Rust port is listed under "Pending port surface" instead of being
described as working.

## Install

The Rust port is built from this repository with cargo:

```bash
cargo build --release -p pi
<!-- doc-c:fence=quickstart.01 -->
```

```text
0.1.0
<!-- doc-c:fence=quickstart.02 source=target/verification/docs-topics/cli-help/pi--version.txt -->
```

Release artifacts for the seven supported targets are cut by the release
script; see [development.md](development.md) for the verified flag set.

## Authenticate

API-key providers read their keys from environment variables before pi starts:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
pi
<!-- doc-c:fence=quickstart.03 -->
```

The full executed environment-variable table lives in
[environment-variables.md](environment-variables.md); the provider topic lists
which key belongs to which provider.

## First session

Run pi in the project directory you want it to work on. The executed `--help`
snapshot documents the invocation forms this build accepts:

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
<!-- doc-c:fence=quickstart.04 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

The default tool set of this build, from the same executed snapshot:

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
<!-- doc-c:fence=quickstart.05 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

`read`, `bash`, `edit`, and `write` are enabled by default; `grep`, `find`,
and `ls` are opt-in through the tool flags documented in
[usage.md](usage.md).

## Give pi project instructions

Context-file discovery is part of startup; the flags that gate it
(`--no-context-files`, `-nc`) appear in the executed snapshot quoted in
[usage.md](usage.md). `AGENTS.md` and `CLAUDE.md` loading follows the same
discovery contract as the reference, exercised end to end by the e2e-smoke
harness (see [evidence/quickstart.json](evidence/quickstart.json) for the
transcript binding).

## Pending port surface

- npm distribution and the curl installer — the port ships cargo builds and
  release artifacts only (unported-feature)
- `/login` subscription flows — interactive credential UI pending (unported-feature)
- image paste and drag-drop (terminal-visual, TUI-CLOSE)
- `/reload` and slash-command inventory — pending the interactive command
  surface being exercised (unported-feature)
