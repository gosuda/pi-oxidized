# pi

A native Rust rewrite of the pi coding agent that preserves TypeScript extension compatibility.

[![Release Verification](https://github.com/gosuda/pi-oxidized/actions/workflows/release-verification.yml/badge.svg)](https://github.com/gosuda/pi-oxidized/actions/workflows/release-verification.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

## Evidence-backed proof points

- In the accepted benchmark, cold `--version` startup was 29.884x faster (19.75 ms versus 590.11 ms), the cold first frame was 8.117x faster (82.88 ms versus 672.76 ms), and stream frames were 2.229x faster (0.938 ms/frame versus 2.090 ms/frame). Synchronized keypress latency measured 0.516 ms p99. See [`docs/performance/PERF-CLOSE-evidence.md`](docs/performance/PERF-CLOSE-evidence.md) for the workloads and limits.
- A bundled sidecar runs existing TypeScript extensions. The bridge recognizes the same 17 serialized method names in Rust and TypeScript, and the shared JSONL witness keeps the two implementations in lockstep. All 10 end-to-end compatibility steps passed in the accepted run. See the [extension compatibility contract](docs/extension-compatibility-contract.md) and [performance acceptance evidence](docs/performance/PERF-CLOSE-evidence.md).
- The release workflow packages seven documented Linux, macOS, and Windows targets twice and requires identical SHA-256 files. It also produces checksums and SLSA provenance attestations. See [`docs/supported-platforms.md`](docs/supported-platforms.md) and [`docs/release.md`](docs/release.md).

## First-run preview

```text
  pi v0.1.0  •  type a message to begin
  Pi can explain its own features and look up its docs. Ask it how to use or extend Pi.
  /hotkeys shortcuts · ctrl+o expand tools · shift+tab thinking

❯ In one sentence, explain what this repository builds.
  [assistant response streams here]
```

## Quickstart

Build the bundled TypeScript extension host and native binary from source:

```bash
mkdir -p .references
git clone https://github.com/earendil-works/pi.git .references/pi-2.0
git -C .references/pi-2.0 checkout --detach 853a80d26c90a14c1886f0ebb8ffaae133ca2185
test "$(git -C .references/pi-2.0 rev-parse HEAD)" = "853a80d26c90a14c1886f0ebb8ffaae133ca2185"
bun install --frozen-lockfile
bun run scripts/reconstruct-provider-data.ts
npm ci --ignore-scripts --prefix .references/pi-2.0
bun run build:extension-host --target x86_64-unknown-linux-gnu
cargo build -p pi --release --locked
```

Set your API credential without exposing it in shell history or process
arguments:

```bash
read -rsp "Enter Gemini API key: " GEMINI_API_KEY && export GEMINI_API_KEY
printf '\n'
```

Launch the agent with an explicit provider and model:

```bash
PI_EXTENSION_HOST="$PWD/dist/release/.staging-host/host/x86_64-unknown-linux-gnu/pi-extension-host" \
  target/release/pi --provider google --model gemini-flash-latest
```

For prerequisites, authentication details, troubleshooting, and next steps,
follow the [getting-started guide](docs/getting-started.md).

## Architecture

The native product and its TypeScript compatibility boundary have separate owners:

- Five workspace crates (`pi`, `pi-agent`, `pi-ai`, `pi-ext`, and `pi-tui`) own the native terminal UI, agent loop, provider transports, tool execution, and host process supervision.
- An out-of-process TypeScript sidecar runs existing extensions without requiring a Rust port.
- `pi-ext` validates inbound frames and converts raw extension UI slots to `SanitizedSlot`. `pi-tui` paints only the sanitized form. See the [extension compatibility contract](docs/extension-compatibility-contract.md#7-extension-ui-sanitization-boundary).

## Project status and limitations

The repository currently has these limitations:

- Pre-built binary packages are not yet published to GitHub Releases. Installation requires a source build.
- The macOS release path does not sign or notarize its binaries.
- The repository records automated accessibility invariants, but manual Orca and VoiceOver sign-off remains incomplete.
- The [parity ledger](docs/PARITY_LEDGER.md) records covered surfaces and explicit exclusions. This project does not claim complete upstream behavior parity.

## Contributing

Contributions are welcome across Rust crates, the TypeScript extension host, protocol verification, and platform packaging. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for local environment setup, architecture invariants, and required validation gates.

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

For development, compile and probe the TypeScript extension host sidecar for
your release target:

```bash
bun run build:extension-host --target x86_64-unknown-linux-gnu
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
| execution map ledger | `bun run verify:map-ledger` | [scripts/verification/fixtures/execution-map/current.md](scripts/verification/fixtures/execution-map/current.md) |
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
| [docs/getting-started.md](docs/getting-started.md) | first-run and setup guide |
| [CONTRIBUTING.md](CONTRIBUTING.md) | contribution guidelines and gates |
| [docs/release.md](docs/release.md) | release instructions |
| [docs/supported-platforms.md](docs/supported-platforms.md) | supported release platforms |
| [docs/compatibility.md](docs/compatibility.md) | compatibility matrix |
| [docs/evidence.md](docs/evidence.md) | doc-evidence program |
| [docs/extension-compatibility-contract.md](docs/extension-compatibility-contract.md) | extension compatibility contract |
| [scripts/verification/fixtures/execution-map/current.md](scripts/verification/fixtures/execution-map/current.md) | execution map (current-generation pointer) |
| [docs/PARITY_LEDGER.md](docs/PARITY_LEDGER.md) | parity ledger |
| [docs/performance/PERF-CLOSE-evidence.md](docs/performance/PERF-CLOSE-evidence.md) | performance acceptance evidence |
| [docs/terminal-rail-doctrine.md](docs/terminal-rail-doctrine.md) | terminal rail doctrine |
| [docs/STYLE_LEDGER.md](docs/STYLE_LEDGER.md) | style ledger |
| [CHANGELOG.md](CHANGELOG.md) | changelog |

## License

Licensed under MIT, as declared in the [workspace manifest](Cargo.toml)
(`[workspace.package] license`).
