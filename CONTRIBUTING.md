# Contributing to pi-oxidized

This repository contains a native Rust rewrite of the pi coding agent while maintaining compatibility with the TypeScript extension ecosystem.

## Workspace Architecture

The workspace consists of five root Rust crates and three supporting TypeScript packages:

```
pi (crates/pi)
├── pi-ext (crates/pi-ext)
│   ├── pi-agent (crates/pi-agent)
│   │   └── pi-ai (crates/pi-ai)
│   ├── pi-ai (crates/pi-ai)
│   └── pi-tui (crates/pi-tui)
├── pi-agent (crates/pi-agent)
│   └── pi-ai (crates/pi-ai)
├── pi-ai (crates/pi-ai)
└── pi-tui (crates/pi-tui)
```

Allowed crate dependency edges:
- `pi-ai`: Provider integrations, model catalogs, streaming abstractions. No workspace dependencies.
- `pi-tui`: Product-agnostic terminal UI primitives, text measurement, layout rendering. No workspace dependencies.
- `pi-agent`: Agent loop, tool execution, session persistence, conversation state. Depends on `pi-ai`.
- `pi-ext`: Extension host bridge, protocol serialization/deserialization, UI slot sanitization, sidecar process lifecycle. Depends on `pi-ai`, `pi-agent`, `pi-tui`.
- `pi`: Product CLI entrypoint, argument parsing, configuration, top-level terminal lifecycle. Depends on `pi-ai`, `pi-agent`, `pi-ext`, `pi-tui`.

TypeScript workspace packages under `packages/`:
- `packages/extension-host`: Bundled TypeScript extension host sidecar executed under Bun.
- `packages/pi-tui-protocol`: TypeScript mirror of the wire protocol and shared witness fixtures.
- `packages/pi-remote-protocol`: Protocol definitions for remote agent sessions.

### Dependency Rules

- Do not introduce unauthorized dependency edges between workspace crates. `pi-ai` and `pi-tui` must remain free of workspace dependencies.
- Add external dependencies only when needed by their first callsite. Pin exact versions (`=x.y.z`), enumerate only used features, and disable default features that pull in unused dependencies.

## Prerequisites

Ensure the following tools are installed before building:

- **Rust**: 1.98.0 toolchain (pinned in `rust-toolchain.toml`, edition 2024) with `clippy` and `rustfmt` components.
- **Bun**: 1.4.0 (or Bun >= 1.3.0) for TypeScript package builds, scripts, and extension host bundling.
- **Node.js & npm**: Node.js 24 LTS and npm for managing pinned reference dependencies in `.references/pi-2.0`.
- **Native build toolchain**: C and C++ compiler (`gcc` or `clang`), `cmake`, and `pkg-config`.

## Setup

1. Materialize the canonical reference checkout:
   ```bash
   mkdir -p .references
   git clone https://github.com/earendil-works/pi.git .references/pi-2.0
   git -C .references/pi-2.0 checkout --detach 853a80d26c90a14c1886f0ebb8ffaae133ca2185
   test "$(git -C .references/pi-2.0 rev-parse HEAD)" = "853a80d26c90a14c1886f0ebb8ffaae133ca2185"
   ```

2. Install root workspace dependencies:
   ```bash
   bun install --frozen-lockfile
   ```

3. Reconstruct reference provider data:
   ```bash
   bun run scripts/reconstruct-provider-data.ts
   ```
   This generates the gitignored `.references/pi-2.0/packages/ai/src/providers/data/*.json` files from `crates/pi-ai/data/builtin-models.json` deterministically without network requests.

4. Install pinned reference dependencies (required for extension compatibility, fixture generation, or tool schema scripts):
   ```bash
   npm ci --ignore-scripts --prefix .references/pi-2.0
   ```

5. Build the TypeScript extension host sidecar:
   ```bash
   bun run build:extension-host
   ```

6. Build the Rust binary:
   ```bash
   cargo build -p pi --release --locked
   ```

## Development and Testing

### Targeted Tests

Run scoped tests for the crate or package you are modifying:

```bash
# Rust crate tests
cargo test -p pi-ai
cargo test -p pi-agent
cargo test -p pi-ext
cargo test -p pi-tui
cargo test -p pi

# Targeted Rust test or module filter
cargo test -p <crate> <filter>

# TypeScript packages and scripts
bun test packages/pi-tui-protocol
bun test packages/extension-host
bun test scripts
bun test scripts/verification/xc-matrix.test.ts
```

### Verification Gates

All pull requests must pass the full verification gate suite:

```bash
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked
cargo fmt --all -- --check
bun run check
bun run test
```

## Generator-Owned Artifacts

Do not hand-edit generated files. Always regenerate them using their owning scripts under `scripts/`. All generators must remain offline-deterministic with zero network requests:

| Generated File | Owning Script | Source of Truth |
| --- | --- | --- |
| `crates/pi-ai/data/builtin-models.json` | `scripts/generate-builtin-models.ts` | `.references/pi-2.0/packages/ai/src/models.generated.ts` |
| `.agent-tasks/pi-rust-rewrite/fixtures/tool-schemas/*.json` | `scripts/generate-tool-schemas.ts` | `.references/pi-2.0/packages/coding-agent/src/core/tools/index.ts` |
| `.agent-tasks/pi-rust-rewrite/fixtures/sessions/**` | `scripts/generate-session-fixtures.ts` | `.references/pi-2.0/packages/coding-agent/src/core/session-manager.ts` |

Run a generator with Bun:
```bash
bun run scripts/generate-builtin-models.ts
bun run scripts/generate-tool-schemas.ts
bun run scripts/generate-session-fixtures.ts
```

## Architecture and Protocol Invariants

Contributors must preserve the following architectural and protocol boundaries:

1. **Protocol Authority and Wire Witness**:
   `crates/pi-ext/src/protocol.rs` is the authoritative specification for the extension JSONL envelope. `packages/pi-tui-protocol` is a portable TypeScript mirror. `packages/pi-tui-protocol/tests/fixtures/frames.jsonl` and `packages/pi-tui-protocol/tests/fixtures/witness-manifest.json` serve as the shared cross-language wire witness; the TypeScript mirror must not become a competing authority.

2. **Handshake Asymmetry**:
   Mode 1 (TypeScript-compatible hosts) validates both `protocolVersion` and `compatibilityVersion`. Mode 2 (lean endpoints) and Mode 3 (native endpoints) validate only `protocolVersion` because lean and native endpoints do not run the pinned TypeScript runtime.

3. **Extension Host Resolution**:
   The TypeScript extension host executable must resolve only from the explicit `PI_EXTENSION_HOST` environment variable or sibling packaged assets (`dist/release/...`). The runtime must never search `PATH` or fall back to arbitrary executables.

4. **Rust Sanitization Boundary**:
   Rust is the security and rendering trust boundary for extension-provided UI. Every inbound `UiSlot` must pass through `pi_ext::sanitize` and render exclusively as `SanitizedSlot`. Raw extension bytes must never be painted directly to the terminal, preventing split or unescaped ANSI control sequence injections.

5. **Bun Runtime Boundary**:
   Bun is permitted solely inside the bundled TypeScript extension host sidecar and extension-defined custom providers. Native terminal paint, built-in provider HTTP requests, and the agent loop must remain pure native Rust without runtime dependencies on Bun or Node.js.

6. **TypeScript Extension Compatibility**:
   Preserve observable extension compatibility with `.references/pi-2.0/packages/{ai,agent,coding-agent,tui}` as specified in `docs/extension-compatibility-contract.md`. Match serialized names and observable runtime behavior rather than JavaScript property order, class identity, or internal object layout.

## Pull Request Guidelines

- **Atomic Changes**: Keep pull requests focused on a single logical change or subsystem. Avoid combining refactoring, protocol changes, and new features into a single PR.
- **Evidence-Backed Claims**: Any PR modifying performance or parity must include verifiable evidence:
  - Performance changes require benchmark comparisons against documented baselines (see `docs/performance/PERF-CLOSE-evidence.md`).
  - Parity changes must reference corresponding ledger entries or regression tests verifying behavior against `.references/pi-2.0`.
- **No Unlocked Dependencies**: Do not update Cargo or Bun lockfiles unless adding or upgrading an explicitly required dependency.
