# JSON mode

Ported from `.references/pi/packages/coding-agent/docs/json.md` at pin
`8fa7eebd`. The reference documents the JSONL event stream of the TypeScript
harness. In this port the mode flag is proven by the executed snapshot; the
event-by-event schema is bounded by the same parity ledger as
[rpc.md](rpc.md). Claims are bound to [evidence/json.json](evidence/json.json).

## Enabling JSON mode

The executed `--help` snapshot documents the mode flag and the non-interactive
companion flag:

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
<!-- doc-c:fence=json.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

`pi --mode json -p "prompt"` processes one prompt and exits, emitting
newline-delimited JSON events on stdout. The same event vocabulary backs RPC
mode, so the per-event schema claims wait for the parity close documented in
[rpc.md](rpc.md).

## Pending port surface

- JSONL event schema reference tables — PAR-CLOSE
- streaming event examples end to end — PAR-CLOSE
