# Doc-Evidence Program

The doc-evidence program (DOC-A) is a closed-class evidence ledger that
mechanically verifies every public documentation and release surface
enumerated in the EXT-24 inventory remains synchronized with the port
contract. It is the foundation for all downstream documentation tickets
(DOC-B through DOC-F).

## Architecture

```
docs-evidence.json (ledger)  ──►  docs-evidence.ts (checker)
         │                              │
         │                              ├──► docs-evidence-runners.ts (7 closed classes)
         │                              ├──► docs-inventory.json (surface count assertion)
         │                              └──► target/verification/docs-evidence/ (sidecars + run-manifest.json)
         │
         └── referencePin: 853a80d26c90a14c1886f0ebb8ffaae133ca2185
```

## Files

| File | Role |
|------|------|
| `scripts/verification/docs-evidence.json` | Ledger — one row per surface, each with owner + status + target + closed class |
| `scripts/verification/docs-evidence.ts` | Checker entrypoint — validates, runs, writes sidecars + run manifest |
| `scripts/verification/docs-evidence-runners.ts` | Seven closed evidence-class runner implementations |
| `scripts/verification/fixtures/docs-inventory.json` | Inventory artifact — EXT-24 surface enumeration |
| `scripts/tests/docs-evidence.test.ts` | Mutation suite + structural assertions |
| `docs/evidence.md` | This document |

## Seven Closed Evidence Classes

No row may carry a command or argv string. Each class has a fixed param
shape; the checker rejects unknown classes and missing params.

| Class | Params | What it verifies |
|-------|--------|-----------------|
| `version-pin` | `label`, `expected`, `source` | A version constant at a source path matches the expected value |
| `generated-block` | `generator`, `artifact` | A generated artifact is traced to its generator source |
| `fenced-compile` | `topic`, `fenceMarker` | A fenced code block exists in a docs topic (path-registered) |
| `transcript-claim` | `source`, `claim` | A CLI help source contains a claimed string |
| `matrix-count` | `source`, `expectedCount`, `countMethod`, `countKey` | A matrix file has the expected item count |
| `review-only-prose` | `source` | A prose surface file exists; sync-docs may not auto-edit it |
| `changelog-unreleased` | `source` | A CHANGELOG has a `## [Unreleased]` append slot |

Every row also carries two more required fields:

- **status** — the row's evidence lifecycle: `present` (surface verified
  today), `pending-port` (not yet ported), or `pending-evidence` (exists but
  lacks fresh evidence). The committed ledger pins all 77 rows to `present`.
- **target** — the registered surface the row verifies. The ledger sets it
  equal to `surface` on every row: this ledger attests the exact registered
  surface.

The checker rejects unknown statuses and empty targets.

## Sidecar Binding

Each run produces a sidecar artifact under
`target/verification/docs-evidence/` binding three values:

- **contentHash** — SHA-256 of the surface content (class-specific)
- **toolVersion** — `pi.docs.evidence.v1` (the checker schema version)
- **runId** — ISO timestamp of the current run

Staleness fails the run:
- contentHash mismatch → surface content changed since the prior sidecar
- toolVersion mismatch → checker version changed
- runId older than the 7-day re-proof interval → evidence is stale

## Run Manifest

A clean run also emits `run-manifest.json` beside the sidecars (schema
`pi.docs.evidence.run.v1`), binding one run to the whole ledger:

| Field | Meaning |
|-------|---------|
| `runId` | ISO timestamp of the run (same value printed on success) |
| `referencePin` | The ledger's pinned commit |
| `ledgerHash` | SHA-256 of the ledger's canonical JSON (object keys sorted recursively) |
| `rowCount` | Ledger rows total |
| `presentCount` | Rows with status `present` |
| `entries` | One `{rowId, status, contentHash}` per ledger row, sorted by `rowId` |

The manifest is written only when the run finishes with zero problems, and
any prior manifest is removed at run start — a failing run can never leave a
manifest that falsely claims a current, clean state. Individual sidecars
carry no status; the manifest is the only status-bearing artifact.

When the required `docs-evidence` compatibility row passes,
`target/verification/compat-matrix/result.json` embeds this validated manifest
as `docsEvidence`. The release workflow retains that file in the
`compatibility-performance-x86_64-unknown-linux-gnu` artifact. The artifact
therefore binds its Git commit to the run ID and all 77 row hashes without a
second CI upload path.

## sync-docs Policy

The sync-docs auto-edit tool (future) is confined to:

1. **Version-pin labels** — manifest version string updates in package
   manifests and README badges, only for labels registered in the ledger
   as `version-pin` rows.
2. **`## [Unreleased]` appends** — appending entries under existing
   `## [Unreleased]` sections in CHANGELOGs registered as
   `changelog-unreleased` rows.

All other surfaces are review-only: import path migrations, code snippet
updates, CLI flag prose, architecture explanations, and telemetry/protocol
schemas require human review and fresh evidence.

## Reference Pin

The ledger records exactly one canonical reference-pin literal:
`853a80d26c90a14c1886f0ebb8ffaae133ca2185` (pointing to `.references/pi-2.0`).

The checker rejects every reference hash other than the canonical pin above.

## Usage

```sh
# Run the checker (exits 0 on a clean tree, nonzero on any violation)
bun run scripts/verification/docs-evidence.ts

# Run the mutation suite
bun test scripts/tests/docs-evidence.test.ts
```

On success the checker prints one line: the `DOCS_EVIDENCE_OK` sentinel, the
runId, the ledger row count, and the path of the emitted run manifest.
