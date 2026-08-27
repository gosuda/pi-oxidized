# Development

Ported from `.references/pi/packages/coding-agent/docs/development.md` at pin
`8fa7eebd`. Claims below are bound to the evidence manifest
[evidence/development.json](evidence/development.json); anything not yet
provable in the Rust port is listed under "Pending port surface" instead of
being described as working.

## Build

The port is a cargo workspace. The release binary is the artifact every
executed snapshot in this documentation tree quotes:

```bash
cargo build --release -p pi
<!-- doc-c:fence=development.01 -->
```

`cargo build --release -p pi` was executed in this run and produced
`target/release/pi`; the `--help` and `--version` snapshots cited by
[quickstart.md](quickstart.md), [search.md](search.md), and
[telemetry-schema.md](telemetry-schema.md) are captures of that binary.

## Verify

Two verification entry points gate this tree. The doc-evidence ledger checker
runs every registered evidence row, recaptures the CLI and release-flag
snapshots, and fails on stale sidecars:

```bash
bun run scripts/verification/docs-evidence.ts
<!-- doc-c:fence=development.02 -->
```

The verification harness suite exercises the checkers themselves, including the
ported-topic checker that gates this page:

```bash
bun test scripts
<!-- doc-c:fence=development.03 -->
```

End-to-end behavior (flags, shortcuts, dialogs, custom UI input, reload
preservation, tools, steering, compaction, fork/resume, and TypeScript session
reopening) is proven separately by `bun run scripts/verification/e2e-smoke.ts`;
the passing run for this tree is bound in the manifests of the topics it
covers.

## Release script flags

Release artifacts are cut by `scripts/package-release.ts` through the argument
parser in `scripts/release/args.ts`. The executed probe below is the
authoritative flag surface: every documented flag is accepted, `--help` is a
help request, and unknown arguments are rejected:

```text
--dry-run accepted
--no-cargo accepted
--no-handshake accepted
--out REJECTED: MissingTargetError
--out-dir REJECTED: MissingTargetError
--runtime-cache REJECTED: MissingTargetError
--source-date-epoch REJECTED: MissingTargetError
--target REJECTED: InvalidTargetError
--target accepted
--help accepted (help request)
-h accepted (help request)
--bogus rejected: UnknownArgError
<!-- doc-c:fence=development.04 source=target/verification/docs-topics/release-flags.txt -->
```

The probe imports the real parser module and never restates the flag set by
hand; the checker recaptures it on every run.

## Project structure

- `crates/pi`: the coding-agent binary (CLI, TUI, sessions, built-in tools)
- `crates/pi-agent`: agent turn loop, queues, tool scheduling, events
- `crates/pi-ai`: provider abstraction and wire types
- `crates/pi-ext` and `crates/pi-tui`: extension and TUI support crates
- `packages/extension-host`, `packages/pi-remote-protocol`,
  `packages/pi-tui-protocol`: TypeScript host and protocol packages bundled
  beside the release binary

The built-in tool inventory is quoted verbatim from the executed snapshot in
[search.md](search.md), not here. The crate entrypoints are documented in
[sdk.md](sdk.md).

## Pending port surface

- npm monorepo setup (`git clone`, `npm install`, `npm run build`) and the
  `pi-test.sh` run-from-source wrapper (unported-feature)
- forking and rebranding through `package.json` `piConfig` fields
  (unported-feature)
- `src/config.ts` path-resolution guidance for package assets
  (unported-feature)
- the hidden `/debug` command and its `~/.pi/agent/pi-debug.log` capture
  (unported-feature)
- the TypeScript test entry points `./test.sh` and `npm test`
  (unported-feature)
