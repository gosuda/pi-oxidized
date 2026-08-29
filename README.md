# pi

Rust rewrite of the pi coding agent. The workspace builds a native coding-agent
binary and a TypeScript extension host sidecar from one source-pinned tree, then
packages them into deterministic release archives for seven targets.

## Purpose

Provide one Rust workspace implementation of the pi coding agent:
the product executable, the agent turn loop, the AI provider layer, the
extension-host protocol, and the terminal UI components. The root package
orchestrates the TypeScript extension host and the cross-target release scripts
that bundle the Rust binary beside its source-pinned host sidecar
([`package.json`](package.json) `description`).

## Workspace crates

| Crate | Path | Responsibility |
| --- | --- | --- |
| `pi` | [crates/pi](crates/pi) | Coding-agent product services and executable |
| `pi-agent` | [crates/pi-agent](crates/pi-agent) | Agent turn loop, queues, tool scheduling, and events |
| `pi-ai` | [crates/pi-ai](crates/pi-ai) | Provider contracts, transports, models, and credentials |
| `pi-ext` | [crates/pi-ext](crates/pi-ext) | TypeScript extension-host protocol and Rust adapters |
| `pi-tui` | [crates/pi-tui](crates/pi-tui) | Product-agnostic terminal components and lifecycle |

Members are defined in [`Cargo.toml`](Cargo.toml) `[workspace] members`.

## Development

Rust toolchain at the registered `rust-version` floor
([`Cargo.toml`](Cargo.toml) `[workspace.package]`).

```bash
cargo build --workspace --locked
cargo test --workspace --all-targets --no-fail-fast --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

TypeScript extension host and verification scripts:

```bash
bun install --frozen-lockfile
bun run check
bun run test
```

## Build artifacts

The `pi` crate produces the release binary (`crates/pi/src/main.rs`):

```bash
cargo build -p pi --release --locked      # → target/release/pi
```

For development, compile the TypeScript extension host sidecar with the root
`build:extension-host` script:

```bash
bun run build:extension-host
```

The release scripts compile their own host sidecar and assemble full archives
(binary + host + runtime) for all seven targets; see
[Releases and supply chain](#releases-and-supply-chain).

## Verification

Each command below is a root script in [`package.json`](package.json). Run them
from the repository root after `bun install`.

| Check | Command | Authority |
| --- | --- | --- |
| package.json alignment | `bun run verify:alignment` | [`scripts/verification/alignment.ts`](scripts/verification/alignment.ts) |
| compatibility matrix | `bun run verify:compatibility` | [`docs/compatibility.md`](docs/compatibility.md) |
| dependency exposure | `bun run verify:dependency-exposure` | [`scripts/verification/dependency-exposure.ts`](scripts/verification/dependency-exposure.ts) |
| e2e smoke | `bun run verify:e2e` | [`scripts/verification/e2e-smoke.ts`](scripts/verification/e2e-smoke.ts) |
| execution map ledger | `bun run verify:map-ledger` | [`docs/EXECUTION_MAP.md`](docs/EXECUTION_MAP.md) |
| gates | `bun run verify:gates` | [`scripts/verification/gates.ts`](scripts/verification/gates.ts) |
| snippets | `bun run verify:snippets` | [`scripts/verification/snippet-harness.ts`](scripts/verification/snippet-harness.ts) |
| extension scaling | `bun run verify:extension-scaling` | [`docs/extension-compatibility-contract.md`](docs/extension-compatibility-contract.md) |
| performance | `bun run verify:performance` | [`docs/performance/PERF-CLOSE-evidence.md`](docs/performance/PERF-CLOSE-evidence.md) |

## Releases and supply chain

The release track owns packaging, the seven-target matrix, and the deterministic
archive pipeline. This README copies no version pins or target triples. Consult
the authorities below.

| Topic | Authority |
| --- | --- |
| Packaging and archive pipeline | [`docs/release.md`](docs/release.md) |
| Supported release targets | [`docs/supported-platforms.md`](docs/supported-platforms.md) |
| Compatibility and version constants | [`docs/compatibility.md`](docs/compatibility.md) |
| Documentation evidence program | [`docs/evidence.md`](docs/evidence.md) |
| Extension compatibility boundaries | [`docs/extension-compatibility-contract.md`](docs/extension-compatibility-contract.md) |

Release archive commands (root scripts in [`package.json`](package.json)):

```bash
bun run package-release:dry     # skip cargo and host, run the full archive pipeline
bun run package-release         # cargo + host + runtime + archive + smoke
```

## Documentation index

| Document | Topic |
| --- | --- |
| [docs/release.md](docs/release.md) | release instructions |
| [docs/supported-platforms.md](docs/supported-platforms.md) | supported release platforms |
| [docs/compatibility.md](docs/compatibility.md) | compatibility matrix |
| [docs/evidence.md](docs/evidence.md) | doc-evidence program |
| [docs/extension-compatibility-contract.md](docs/extension-compatibility-contract.md) | extension compatibility contract |
| [docs/EXECUTION_MAP.md](docs/EXECUTION_MAP.md) | execution map ledger |
| [docs/PARITY_LEDGER.md](docs/PARITY_LEDGER.md) | parity ledger |
| [docs/performance/PERF-CLOSE-evidence.md](docs/performance/PERF-CLOSE-evidence.md) | performance acceptance evidence |
| [docs/terminal-rail-doctrine.md](docs/terminal-rail-doctrine.md) | terminal rail doctrine |
| [docs/STYLE_LEDGER.md](docs/STYLE_LEDGER.md) | style ledger |
| [CHANGELOG.md](CHANGELOG.md) | changelog |

## License

Licensed under MIT, as declared in the [workspace manifest](Cargo.toml)
(`[workspace.package] license`).
