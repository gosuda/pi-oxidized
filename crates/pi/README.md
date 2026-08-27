# pi

Coding-agent product services and executable.

## Workspace topology

`pi` is the top-level product crate. It depends on all four other workspace
crates: `pi-ai`, `pi-agent`, `pi-ext`, and `pi-tui`.

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
| `cli` | CLI entry point and argument parsing |
| `core` | Core product services |
| `modes` | Operating modes (TUI, RPC, JSON, print) |
| `remote` | Remote session server and transport |

## Public re-exports

| Symbol | Kind |
|---|---|
| `VERSION` | const (`&'static str`, from `CARGO_PKG_VERSION`) |

## Handshake symmetry

The handshake asymmetry (Mode 1 TypeScript-compat hosts validate both
`protocolVersion` and `compatibilityVersion`; Mode 2 lean and Mode 3 native
endpoints validate only `protocolVersion`) is documented in
`docs/extension-compatibility-contract.md`, the single owner doc. This README
references it; other docs point there rather than restating it.
