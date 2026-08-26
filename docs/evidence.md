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
         │                              └──► target/verification/docs-evidence/ (sidecars)
         │
         └── referencePin: 8fa7eebd235355522c8104166b4f1f959b4e2f10
```

## Files

| File | Role |
|------|------|
| `scripts/verification/docs-evidence.json` | Ledger — one row per surface, each with owner + closed class |
| `scripts/verification/docs-evidence.ts` | Checker entrypoint — validates, runs, writes sidecars |
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

The ledger records exactly one reference-pin literal:
`8fa7eebd235355522c8104166b4f1f959b4e2f10`. The checker rejects the stale
hash `4488ad55c18f07ae89a489096c90de8667b3adfb`.

## Usage

```sh
# Run the checker (exits 0 on a clean tree, nonzero on any violation)
bun run scripts/verification/docs-evidence.ts

# Run the mutation suite
bun test scripts/tests/docs-evidence.test.ts
```
