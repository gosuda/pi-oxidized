# pi-ext

TypeScript extension-host protocol and Rust adapters.

## Workspace topology

`pi-ext` depends on `pi-ai`, `pi-agent`, and `pi-tui`. It is depended on by
`pi`.

```
pi-ai  (no workspace deps)
  ↑
pi-agent → pi-ai
pi-ext   → {pi-ai, pi-agent, pi-tui}
pi       → {pi-ai, pi-agent, pi-ext, pi-tui}
```

The full topology is owned by the root `AGENTS.md` and generated from workspace
`Cargo.toml` edges so this README and `AGENTS.md` share one source.

## Public modules

| Module | Description |
|---|---|
| `adapters` | Rust adapters for extension protocol |
| `client` | Extension client |
| `host` | Extension host |
| `protocol` | JSONL envelope protocol (authority) |
| `sanitize` | ANSI sanitizer for extension-rendered UI |
| `server` | Extension server |

## Protocol authority

`pi-ext/src/protocol.rs` is the authority for the extension JSONL envelope.
`packages/pi-tui-protocol` is its portable TypeScript mirror. The shared
cross-language wire witness is `packages/pi-tui-protocol/tests/fixtures/frames.jsonl`.

## Handshake symmetry

The handshake asymmetry (Mode 1 TypeScript-compat hosts validate both
`protocolVersion` and `compatibilityVersion`; Mode 2 lean and Mode 3 native
endpoints validate only `protocolVersion`) is documented in
`docs/extension-compatibility-contract.md`, the single owner doc. This README
references it; other docs point there rather than restating it.
