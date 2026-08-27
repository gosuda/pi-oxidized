# RPC mode

Ported from `.references/pi/packages/coding-agent/docs/rpc.md` at pin
`8fa7eebd`. The reference is a full JSON-RPC command and event reference for
the TypeScript harness. In this port, the proven surface is the RPC mode
itself; the command-by-command wire reference is pending the parity ledger
close. Claims are bound to [evidence/rpc.json](evidence/rpc.json).

## Enabling RPC mode

The executed `--help` snapshot documents the mode flag:

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
<!-- doc-c:fence=rpc.01 source=target/verification/docs-topics/cli-help/pi--help.txt -->
```

`pi --mode rpc` runs headless: the process reads newline-delimited RPC
requests and writes responses and events to stdout instead of starting the
interactive terminal UI.

## Parity status

The rpc-parity harness replays the authoritative RPC command set through both
the Rust binary in RPC mode and the source-pinned TypeScript CLI, comparing
normalized transcripts. At the reference pin the authoritative command union
includes `clear_queue`, which the replay scenario does not yet cover, so the
harness refuses to run rather than silently excluding a command. Until the
parity ledger closes, this page makes no per-command claims; the JSONL event
stream surface shared with [json.md](json.md) is likewise bounded there.

## Pending port surface

- full RPC command and event reference tables — PAR-CLOSE
- `clear_queue` replay coverage in rpc-parity — PAR-CLOSE
- remote session wire-format documentation — PAR-CLOSE
