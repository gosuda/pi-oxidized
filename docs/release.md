# Release instructions

Stable ID `DOC-E`, issue metaphorics/pi-oxidized#136.
Authored 2026-08-28 against the seven-target release definition.

Every command in a fenced block below is extraction-tested by
`scripts/tests/release-docs.test.ts` through `parseReleaseArgs` and dry-run
execution, so the instructions can never name a flag the parser rejects.
The parser's accepted flag set is defined in `scripts/release/args.ts`
(`parseReleaseArgs`, lines 109-184): `--target`, `--out`/`--out-dir`,
`--runtime-cache`, `--source-date-epoch`, `--dry-run`, `--no-cargo`,
`--no-handshake`, `--help`/`-h`.

## 1. Prerequisites

- Rust toolchain at the registered `rust-version` floor (`Cargo.toml`
  `[workspace.package]`; value in [compatibility.md](compatibility.md) → Engine Floors).
- Bun at the registered `engines.bun` floor (`package.json`); release-verification CI
  pins the Bun runtime recorded as `BUN_RUNTIME_VERSION`
  ([compatibility.md](compatibility.md) → Runtime and Release Constants,
  `workflow:116-119`).
- The root `CHANGELOG.md` must carry a non-empty `## [Unreleased]` section.
  The release-path CHANGELOG gate (`scripts/package-release.ts:121-123`,
  `changelogGateFailure` at lines 65–98) fails every build mode — dry-run,
  no-cargo, and full — before any build work starts when the file is missing
  or the section is empty.

## 2. Building a release archive (dry-run)

Dry-run skips cargo and host compilation, assembling from stub binaries so
the full archive pipeline (staging, manifest, archive, checksum, unpack smoke)
runs without a toolchain:

```bash
bun run scripts/package-release.ts --target x86_64-unknown-linux-gnu --dry-run
```

Dry-run with an explicit output directory and source-date-epoch:

```bash
bun run scripts/package-release.ts --target x86_64-unknown-linux-gnu --dry-run --out dist/release --source-date-epoch 1735689600
```

## 3. Building a release archive (full)

Full build runs cargo, compiles the host sidecar, provisions the Bun runtime,
assembles, archives, and smokes the unpacked result:

```bash
bun run scripts/package-release.ts --target x86_64-unknown-linux-gnu
```

Full build with an explicit output directory:

```bash
bun run scripts/package-release.ts --target x86_64-unknown-linux-gnu --out dist/pass1
```

## 4. Building with a pre-built binary (no-cargo)

When the Rust binary is already built (e.g. by a musl toolchain step),
`--no-cargo` skips cargo but still compiles the host and archives:

```bash
bun run scripts/package-release.ts --target x86_64-unknown-linux-musl --no-cargo --out dist/pass1
```

## 5. Skipping the host handshake

`--no-handshake` skips the host `hello` handshake verification:

```bash
bun run scripts/package-release.ts --target x86_64-unknown-linux-gnu --no-handshake
```

## 6. Offline Bun runtime cache

`--runtime-cache <dir>` is consulted before any network fetch; cached bytes
pass the same pinned-sha256 verification as downloads
(`scripts/release/runtime.ts:96-110,161-172`):

```bash
bun run scripts/package-release.ts --target x86_64-unknown-linux-gnu --runtime-cache .runtime-cache
```

## 7. All seven release targets

The seven supported Rust triples (`RUST_TARGETS`, `scripts/release/targets.ts:18-26`)
and their archive directories (`buildPlan`, `scripts/release/targets.ts:89-119`):

| Rust target | Archive dir | Archive | Bun target |
|---|---|---|---|
| `x86_64-unknown-linux-gnu` | `pi-linux-x64-base` | `tar.gz` | `bun-linux-x64-baseline` |
| `x86_64-unknown-linux-musl` | `pi-linux-x64-musl-base` | `tar.gz` | `bun-linux-x64-musl-baseline` |
| `aarch64-unknown-linux-gnu` | `pi-linux-arm64` | `tar.gz` | `bun-linux-arm64` |
| `aarch64-unknown-linux-musl` | `pi-linux-arm64-musl` | `tar.gz` | `bun-linux-arm64-musl` |
| `x86_64-apple-darwin` | `pi-darwin-x64-base` | `tar.gz` | `bun-darwin-x64-baseline` |
| `aarch64-apple-darwin` | `pi-darwin-arm64` | `tar.gz` | `bun-darwin-arm64` |
| `x86_64-pc-windows-msvc` | `pi-windows-x64-base` | `zip` | `bun-windows-x64-baseline` |

Each target's dry-run command:

```bash
bun run scripts/package-release.ts --target x86_64-unknown-linux-gnu --dry-run
```

```bash
bun run scripts/package-release.ts --target x86_64-unknown-linux-musl --dry-run
```

```bash
bun run scripts/package-release.ts --target aarch64-unknown-linux-gnu --dry-run
```

```bash
bun run scripts/package-release.ts --target aarch64-unknown-linux-musl --dry-run
```

```bash
bun run scripts/package-release.ts --target x86_64-apple-darwin --dry-run
```

```bash
bun run scripts/package-release.ts --target aarch64-apple-darwin --dry-run
```

```bash
bun run scripts/package-release.ts --target x86_64-pc-windows-msvc --dry-run
```

## 8. Determinism verification

Every CI leg packages twice and asserts the checksum sidecars are identical
(`workflow:593-603`). Reproduce locally:

```bash
bun run scripts/package-release.ts --target x86_64-unknown-linux-gnu --out dist/pass1
```

```bash
bun run scripts/package-release.ts --target x86_64-unknown-linux-gnu --out dist/pass2
```

Then compare: `diff -u dist/pass1/*.sha256 dist/pass2/*.sha256`.

## 9. release.json schema

`release.json` is shipped inside every archive with the schema registered as
`RELEASE_MANIFEST_SCHEMA` (`scripts/release/stage.ts:19`; value in
[compatibility.md](compatibility.md) → Runtime and Release Constants). The
`ReleaseManifest` interface (`scripts/release/stage.ts:34-46`) defines the fields:

- `schema`: the `RELEASE_MANIFEST_SCHEMA` constant (value in
  [compatibility.md](compatibility.md) → Runtime and Release Constants).
- `version`: the workspace version ([compatibility.md](compatibility.md) →
  Workspace Version).
- `rustTarget`: the Rust target triple.
- `bunTarget`: the Bun compile target (incl. `-baseline` for x86_64, `-musl` for musl).
- `hostKind`: `"compiled"` sidecar or `"runtime-bundle"` fallback.
- `compatibilityVersion`: the host compatibility version.
- `protocolVersion`: the host protocol version.
- `sourceDateEpoch`: the `SOURCE_DATE_EPOCH` timestamp used for archive mtimes.
- `createdAt`: ISO 8601 timestamp (fixed from `sourceDateEpoch` for reproducibility).
- `files`: array of `ManifestFile` entries (`scripts/release/stage.ts:22-31`), each
  carrying `path` (POSIX-style, relative to archive root), `size` (bytes),
  `sha256` (lowercase hex digest), and `executable` (boolean).

Staged contents are ordered by `stagedInputs` (`scripts/release/stage.ts:123-215`):
the `pi` binary, the host artifact, the musl fallback pair, mandatory
`CHANGELOG.md` and `README.md`, optional `LICENSE`/`LICENSE-MIT`, the docs tree,
optional `assets`/`theme`, then `release.json`.

## 10. Generated artifacts

Three offline deterministic generators produce checked-in fixtures against canonical reference `.references/pi-2.0` at pinned SHA `853a80d26c90a14c1886f0ebb8ffaae133ca2185` as the canonical authority. Each rerun
is a byte-stable no-op when the source has not changed:

- `scripts/generate-builtin-models.ts` — `crates/pi-ai/data/builtin-models.json`
  from `.references/pi-2.0/packages/ai/src/models.generated.ts`.
- `scripts/generate-session-fixtures.ts` —
  `.agent-tasks/pi-rust-rewrite/fixtures/sessions/` from the reference
  `SessionManager` in `.references/pi-2.0`.
- `scripts/generate-tool-schemas.ts` —
  `.agent-tasks/pi-rust-rewrite/fixtures/tool-schemas/` from the reference
  tool registry in `.references/pi-2.0`.

```bash
bun run scripts/generate-builtin-models.ts
```

```bash
bun run scripts/generate-session-fixtures.ts
```

```bash
bun run scripts/generate-tool-schemas.ts
```

## 11. Extension compatibility evidence

The extension compatibility contract (`docs/extension-compatibility-contract.md`)
cites `packages/pi-tui-protocol/tests/fixtures/frames.jsonl` as the shared
cross-language lockstep corpus (line 525): the 80-line JSONL fixture carries
every `(method, kind)` pair the codec test witnesses, and both the TypeScript
and Rust sides consume it by name. The `witness manifest lockstep` test
(`packages/pi-tui-protocol/tests/codec.test.ts`) pins the total line count,
every manifest pair, and the modifier-combo key-event kinds against
`frames.jsonl`.

## 12. Help

```bash
bun run scripts/package-release.ts --help
```
