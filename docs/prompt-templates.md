# Prompt templates

Ported from `.references/pi/packages/coding-agent/docs/prompt-templates.md` at
pin `8fa7eebd`, at reduced depth. Claims are bound to the evidence manifest
[evidence/prompt-templates.json](evidence/prompt-templates.json); the template
file-format reference has not been ported yet and is listed under
"Pending port surface".

## What prompt templates are

Prompt templates are Markdown snippets that expand into full prompts: typing
`/name` in the editor invokes the template whose filename is `name.md`.

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
<!-- doc-c:fence=prompt-templates.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

`--prompt-template` is repeatable, and explicit paths stay applied when
`--no-prompt-templates` skips package and settings discovery. The reference
discovery contract loads templates from `~/.pi/agent/prompts/*.md` (global),
`.pi/prompts/*.md` (project-local, only after the project is trusted), package
`prompts/` directories, and the settings `prompts` array; discovery inside
`prompts/` directories is non-recursive.

The e2e-smoke harness runs the Rust binary with `--no-prompt-templates` in
its base invocation, exercising the gating flag end to end (see
[evidence/prompt-templates.json](evidence/prompt-templates.json) for the
transcript binding).

## Pending port surface

- template file format, `description` and `argument-hint` frontmatter
  (unported-feature)
- argument expansion: positional `$1`, `$@`, defaults, and slicing
  (unported-feature)
- autocomplete dropdown rendering for templates (TUI-CLOSE)
