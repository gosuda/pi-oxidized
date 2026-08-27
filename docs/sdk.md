# SDK

Ported from `.references/pi/packages/coding-agent/docs/sdk.md` at pin
`8fa7eebd`. The reference documents the TypeScript SDK entrypoints. This port
ships Rust crates; the public entrypoints are `pi`, `pi-agent`, and `pi-ai`,
and every symbol below is verified against the crate roots. Claims are bound
to [evidence/sdk.json](evidence/sdk.json).

## Crates

| Crate | Role |
|-------|------|
| `pi-ai` | Provider contracts, model types, streaming options |
| `pi-agent` | Agent turn loop, steering queues, events |
| `pi` | The coding agent: CLI, modes, sessions, extensions |

`pi-ai` re-exports the provider contract and stream options from its crate
root; `pi-agent` re-exports the agent loop and queue contracts; `pi` is the
binary workspace the executed snapshots quote.

## Provider contract

```rust
use pi_ai::{Provider, ProviderResponse, StreamOptions};
<!-- doc-c:fence=sdk.01 -->
```

`Provider` is the trait every provider adapter implements;
`ProviderResponse` carries the assistant message and usage; `StreamOptions`
parameterizes a completion stream. All three names are public re-exports at
the `pi-ai` crate root.

## Agent loop

```rust
use pi_agent::{Agent, AgentEvent, AgentOptions};
<!-- doc-c:fence=sdk.02 -->
```

`Agent` is the stateful wrapper over the low-level run loop, `AgentEvent` is
the event enum, and `AgentOptions` configures a run. See
[harness.md](harness.md) for the lifecycle these types expose.

## Pending port surface

- full public API tour per crate — DOC-D
- generated import tables from live crate surfaces — DOC-D
- TypeScript SDK entrypoints — unported-feature
