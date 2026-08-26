# Docs snippet fixtures

Harness-owned fenced snippets for `bun run verify:snippets` (DOC-G1).

## Contract

- Every fence is a complete compilable unit and carries its own imports.
- Forbidden content: the five excluded example products named by DOC-G1, any
  reference-tree import specifier, any example-product path segment, and any
  copied example-product code.
- Files named `negative-*.md` are mutation fixtures only; they are not
  registered in the lane corpus.
- The protocol package has no `Codec` export; fixtures cover the codec function
  set instead.

## Entrypoint coverage

| Entrypoint | Fixture | Fence |
|---|---|---|
| `pi_ai::estimate_text_tokens` | `rust/pi-ai.md` | first `rust` fence |
| `pi_agent::{QueueMode, PendingMessageQueue}` | `rust/pi-agent.md` | first `rust` fence |
| `pi_ext::protocol::FLAGS_SET_METHOD` | `rust/pi-ext.md` | first `rust` fence |
| `pi_tui::keys::is_kitty_protocol_active` | `rust/pi-tui.md` | first `rust` fence |
| `pi::VERSION` | `rust/pi.md` | first `rust` fence |
| `@earendil-works/pi-tui-protocol` codec + protocol surface | `ts/protocol.md` | first `ts` fence |
| `@earendil-works/pi-extension-host` public API | `ts/extension-host.md` | first `ts` fence |
| `@earendil-works/pi-coding-agent` landed contract types | `ts/extension-host.md` | second `ts` fence |
