# Shell aliases

Ported from `.references/pi/packages/coding-agent/docs/shell-aliases.md` at
pin `8fa7eebd`. Claims below are bound to the evidence manifest
[evidence/shell-aliases.json](evidence/shell-aliases.json); anything not yet
provable in the Rust port is listed under "Pending port surface" instead of
being described as working.

This port ships a plain `pi` binary, so launcher aliases are ordinary shell
aliases. Every flag used in the recipes below appears in the executed
`--help` snapshot of this build. The session-management flags the recipes
rely on:

```text
pi - AI coding assistant with read, bash, edit, write tools

Usage:
  pi [options] [@files...] [messages...]

Commands:
  pi install <source> [-l]     Install extension source and add to settings
  pi remove <source> [-l]      Remove extension source from settings
  pi uninstall <source> [-l]   Alias for remove
  pi update [source|self|pi]   Update pi, extensions, or model catalogs
<!-- doc-c:fence=shell-aliases.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

## Session recipes

```bash
# Continue the previous session
alias pic='pi --continue'

# Pick a stored session to resume
alias pir='pi --resume'

# Ephemeral session: nothing is saved
alias pix='pi --no-session'

# Keep sessions under a dedicated directory
alias pis='pi --session-dir ~/pi-sessions'

# Start with a display name: pin "Refactor auth module"
alias pin='pi --name'
<!-- doc-c:fence=shell-aliases.02 -->
```

## Model and provider recipes

```bash
# Pin a provider (default is google)
alias pig='pi --provider google'

# Pin a model, with or without a provider prefix
alias pi4='pi --model openai/gpt-4o'

# Start at a fixed thinking level
alias pith='pi --thinking high'

# List the models visible to this build
alias pilist='pi --list-models'
<!-- doc-c:fence=shell-aliases.03 -->
```

## Read-only and print recipes

The read-only invocation form is documented in the snapshot's Examples
block:

```text
pi - AI coding assistant with read, bash, edit, write tools

Usage:
<!-- doc-c:fence=shell-aliases.04 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

```bash
# Read-only tools only
alias piro='pi --tools read,grep,find,ls'

# Process one prompt and exit
alias pip='pi -p'
<!-- doc-c:fence=shell-aliases.05 -->
```

Add the aliases to `~/.bashrc` or `~/.zshrc` to make them permanent.

## Pending port surface

- the reference recipe that makes pi's internal bash tool expand shell
  aliases via the `shellCommandPrefix` setting in `settings.json`
  (unported-feature)
