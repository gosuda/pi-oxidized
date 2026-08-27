# Search tools

Ported from `.references/pi/packages/agent/docs/search.md` at pin `8fa7eebd`.
The reference documents a session-search query interface over committed
session entries; that interface is not ported. The ported page documents the
search surface this build ships: the built-in read-only tools, quoted from the
executed `--help` snapshot and bound to [evidence/search.json](evidence/search.json).

## Built-in tool names

The tool inventory of this build, quoted verbatim from the executed snapshot:

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
<!-- doc-c:fence=search.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

The three search tools are `grep` (search file contents), `find` (find files
by glob pattern), and `ls` (list directory contents). All three are read-only,
and the snapshot marks each of them off by default.

## Opt-in

`read`, `bash`, `edit`, and `write` are enabled by default; the read-only
search tools are opt-in. The tool-selection flags, from the same executed
snapshot:

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
<!-- doc-c:fence=search.02 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

`--tools` takes a comma-separated allowlist that spans built-in, extension, and
custom tools; `--exclude-tools` removes individual names from whatever is
otherwise enabled. The read-only recipe from the snapshot's examples:

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
<!-- doc-c:fence=search.03 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

Because `grep`, `find`, and `ls` cannot modify files, enabling them is the
standard way to give a review-only run search coverage without opening the
mutation tools. Full flag semantics live in [usage.md](usage.md); the trust
model that gates project-local files is covered in [security.md](security.md).

## Pending port surface

- the session-search query contract: the minimal hit identity
  `(sessionId, entryId)` and the async-iterable streaming API with
  `AbortSignal` cancellation (unported-feature)
- the reusable scanning search adapter over session-like readables, and direct
  scanning of already-open sessions and storages (unported-feature)
- SQLite FTS search: extended hits, the lazily created FTS table and triggers,
  and fresh-after-commit semantics (unported-feature)
- Elasticsearch indexing glue for JSONL sessions and the follow-up no-op
  search index sink (unported-feature)
