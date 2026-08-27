# Settings

Ported from `.references/pi/packages/coding-agent/docs/settings.md` at pin
`8fa7eebd`. Claims below are bound to the evidence manifest
[evidence/settings.json](evidence/settings.json); anything not yet provable
in the Rust port is listed under "Pending port surface" instead of being
described as working.

## Config directory

Global pi state lives under a config directory that defaults to `~/.pi/agent`
and can be relocated with `PI_CODING_AGENT_DIR`. The executed `--help`
snapshot documents the directory variables verbatim:

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
<!-- doc-c:fence=settings.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

`PI_CODING_AGENT_SESSION_DIR` moves session storage and is overridden by the
`--session-dir` flag; session handling is covered in
[sessions.md](sessions.md).

## The config TUI

`pi config` opens the resource configuration TUI for enabling and disabling
package resources. The executed `pi config --help` snapshot is quoted
verbatim:

```text
Usage:
  pi config [-l] [--approve|--no-approve]

Open the resource configuration TUI to enable or disable package resources.
Without -l, starts in global settings (~/.pi/agent/settings.json).
Press Tab in the TUI to switch between global and project-local modes.

Options:
  -l, --local       Edit project overrides (.pi/settings.json)
  -a, --approve     Trust project-local files for this command with -l
  -na, --no-approve Ignore project-local files for this command with -l
<!-- doc-c:fence=settings.02 source=target/verification/docs-topics/cli-help/pi-config--help.txt -->
```

Without `-l` the TUI starts in global settings (`~/.pi/agent/settings.json`);
`-l` edits project overrides (`.pi/settings.json`) and Tab switches scope.
The `--approve`/`--no-approve` trust overrides are the same flags documented
for the main invocation in [usage.md](usage.md), and the package subcommands
share them as [packages.md](packages.md) documents.

## Pending port surface

- the `settings.json` key reference — model and thinking settings
  (`defaultThinkingLevel`, `thinkingBudgets`), UI and display, network,
  warnings, compaction, branch summary, retry, message delivery, terminal and
  images, shell (`shellCommandPrefix`, `npmCommand`), tools (`defaultTools`),
  sessions (`sessionDir`), model cycling, markdown, and resource discovery
  paths (unported-feature)
- project override merge semantics for `.pi/settings.json` beyond the
  `pi config -l` editor surface (unported-feature)
- the `/settings` interactive dialog and `/trust` project-trust persistence
  in `~/.pi/agent/trust.json`, including `defaultProjectTrust` fallbacks
  (unported-feature)
