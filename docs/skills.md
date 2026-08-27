# Skills

Ported from `.references/pi/packages/coding-agent/docs/skills.md` at pin
`8fa7eebd`, at reduced depth. Claims are bound to the evidence manifest
[evidence/skills.json](evidence/skills.json); the skill file-format reference
has not been ported yet and is listed under "Pending port surface".

## What skills are

Skills are self-contained capability packages that the agent loads on demand:
a skill provides specialized workflows, setup instructions, helper scripts, and
reference documentation for specific tasks.

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
  --fork <path|id>               Fork specific session file or partial UUID into a new session
  --session-dir <dir>            Directory for session storage and lookup
  --no-session                   Don't save session (ephemeral)
<!-- doc-c:fence=skills.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

`--skill` is repeatable, and explicit `--skill` paths stay applied when
`--no-skills` skips package and settings discovery. The reference discovery
contract loads skills from `~/.pi/agent/skills/` and `~/.agents/skills/`
(global), `.pi/skills/` and `.agents/skills/` (project-local, only after the
project is trusted), package `skills/` directories, and the settings `skills`
array.

The e2e-smoke harness runs the Rust binary with `--no-skills` in its base
invocation, exercising the gating flag end to end (see
[evidence/skills.json](evidence/skills.json) for the transcript binding).

Security note: skills can instruct the model to perform any action and may
include executable code the model invokes. Review skill content before use;
see [security.md](security.md) for the trust rules that gate project-local
resources.

## Pending port surface

- `SKILL.md` format, frontmatter field table, and name rules (unported-feature)
- `/skill:name` command surface and argument append behavior (unported-feature)
- progressive-disclosure loading mechanics (unported-feature)
- validation and warning behavior, covering name length, invalid characters,
  missing descriptions, and collisions (unported-feature)
- cross-harness sharing via the settings `skills` array and skill repository
  pointers (unported-feature)
