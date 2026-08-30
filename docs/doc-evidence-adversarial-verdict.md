# DOC-G2 Adversarial Audit Verdict

**Issue:** [#135 — Adversarial review of the doc-evidence program before DOC-C/D/E fan out](https://github.com/metaphorics/pi-oxidized/issues/135)
**Stable ID:** DOC-G2
**Date:** 2026-08-27
**Auditor:** automated mutation suite + human verdict

## Summary

The landed DOC-A checker and DOC-B generator were adversarially audited against
the five drift classes named in the issue acceptance criteria.  Each drift class
was injected as a named failing mutation and verified to produce a distinct
checker failure.  Two gaps were found and fixed within DOC-A/DOC-B boundaries:
disguised example-product imports and evidence-free Unreleased entries.  The
verdict is **PASS** — all five drift classes produce distinct checker failures,
and the docs phase has not regained release-packaging implementation scope.

## Drift class results

| # | Drift class | Caught? | Mechanism | Mutation test |
|---|-------------|---------|-----------|---------------|
| 1 | Stale-sidecar reuse after code change | Yes | DOC-A contentHash recompute in `checkStaleness` | `stale-sidecar-reuse after code change` |
| 2 | TS/Rust constant fork accepted by single-source read | Yes | DOC-B `collectPins` cross-assert (TS ↔ Rust ↔ ext-host) | `constant-fork-ts-rust` (2 tests) |
| 3 | Out-of-band deps doc edit touching non-DOC-B-owned blocks | Yes | DOC-A `review-only-prose` contentHash mismatch | `out-of-band-deps-doc-edit` |
| 4 | Disguised example-product import in fixtures | Yes | DOC-A `scanForExampleProductImports` (new, DOC-G2) | `disguised-example-product-import` (3 tests) |
| 5 | Evidence-free Unreleased entry | Yes | DOC-A `checkUnreleasedEntriesHaveEvidence` (new, DOC-G2) | `evidence-free-unreleased-entry` (6 tests) |

## Fixes landed within DOC-A/DOC-B boundaries

### 1. `scanForExampleProductImports` (DOC-A, `docs-evidence-runners.ts`)

Scans `scripts/tests/` and `scripts/verification/` `.ts` files for **value**
import statements (not type-only — type-only imports are erased at runtime and
do not accrete behavior) referencing the `.references/pi-2.0/` example-product tree.
Wired into `runCheck` as step 2b; findings appear as `[example-product-import]`
problems.

**Pre-existing finding:** `scripts/verification/foundation.test.ts` has a
type-only import from `.references/pi-2.0/` — not flagged because it does not
accrete runtime behavior.  No value imports were found on the current tree.

### 2. `checkUnreleasedEntriesHaveEvidence` (DOC-A, `docs-evidence-runners.ts`)

Checks that every bullet entry under `## [Unreleased]` carries commit evidence:
a commit SHA (7+ hex chars), a PR/issue reference (`[#NNN]` or `(#NNN)`), or a
URL.  Integrated into the `changelog-unreleased` runner; entries lacking all
three produce `[row-id] evidence-free Unreleased entry` problems.

**Current tree status:** All nine CHANGELOG files in the ledger pass — entries
that exist carry `[#NNN]` references; empty Unreleased sections are accepted.

## Scope boundary observation

The docs phase has **not** regained release-packaging implementation scope:

- The seven closed evidence classes are documentation evidence classes, not
  release-packaging classes.  No class shells out, stages, or packages releases.
- The `FORBIDDEN_FIELDS` list (`command`, `argv`, `cmd`, `args`, `shell`, `exec`)
  prevents any ledger row from carrying command/argv strings, blocking the
  checker from implementing release logic.
- Release surfaces (`scripts/release/`, `release.json`, compiled binaries) appear
  only as `review-only-prose` and `generated-block` rows — the checker hashes
  their content but does not execute or implement them.
- The DOC-B generator reads pin constants and emits a compatibility matrix; it
  does not stage, sign, or package releases.

## Gate decision

**PASS** — gates DOC-C/DOC-E start.  All five drift classes produce distinct
checker failures via named mutations.  No blocking objections remain.

## Test artifacts

- `scripts/tests/docs-evidence-adversarial.test.ts` — 14 tests, 5 drift classes
- `scripts/tests/docs-evidence.test.ts` — 22 tests (existing, still green)
- `scripts/tests/generate-compat-docs.test.ts` — 25 tests (existing, still green)
- Combined: 61 pass, 0 fail
