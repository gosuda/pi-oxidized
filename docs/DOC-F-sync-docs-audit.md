# DOC-F full-tree documentation audit

Issue: [#138](https://github.com/metaphorics/pi-oxidized/issues/138)

Audit date: 2026-08-29

## Verdict

| Severity | Open findings |
|---|---:|
| HIGH | 0 |
| MEDIUM | 0 |
| LOW | 3 |

The audit compared the root and crate READMEs, compatibility and parity records,
extension contracts, terminal records, release and dependency records, and every
performance record with their source files and current verification artifacts.
The three remaining LOW items are dated records. They do not state current
versions or current measurements.

## LOW dispositions

| Item | Record | Disposition | Owner |
|---|---|---|---|
| The original SBOM capture records `20be789` and its 2026-08-27 digest. | `docs/DEPS-R1-regrounded-bins.md` §4 | RETAIN-HISTORICAL. The record now identifies itself as the seed capture and points to the DEPS-R2 re-anchor at `849122647411`. | DEPS-R1 / DEPS-R2 |
| The npm list records the dependency campaign's input versions. | `docs/DEPS-R1-regrounded-bins.md` §5 | RETAIN-HISTORICAL. The heading now labels these values as from-version pins and points to DEPS-R2 for successor targets. | DEPS-R1 / DEPS-R2 |
| The exposure seed rows record the `20be789e` capture and its 2,493-input projection. | `docs/DEPS-R1-regrounded-bins.md` §§6-7 | RETAIN-HISTORICAL. The record now points to the final-tree `eb91d6b1d4fa` re-anchor and its 2,491-input projection. | DEPS-R1 / DEPS-R2 |

## Findings closed during DOC-F

- Replaced the README's unprovisioned `cargo nextest` command with the CI-owned
  `cargo test --workspace --all-targets --no-fail-fast --locked` gate.
- Pointed each README verification command at its actual source or owner record.
- Corrected the two aarch64 Bun compile targets in `docs/release.md`.
- Updated the DEPS-R1 epoch gate from the old five-row state to the landed
  seven-leg release matrix.
- Corrected stale release, platform, extension-host, and terminal source anchors.
- Recorded the implemented remote stack, editor border paint path, and
  sub-20-column viewport floor.
- Corrected the performance tool-dispatch iteration, stream floor, campaign
  count, first-frame attestation label, and per-run memory comparison.
- Removed duplicate registered version pins from hand-written documents. The
  generated compatibility document remains their single owner.

## Evidence inputs

- `scripts/verification/docs-evidence.json` and its current sidecar set
- `docs/compatibility.md` and `scripts/verification/compat-matrix.json`
- `docs/PARITY_LEDGER.md`
- `docs/extension-compatibility-contract.md` and `docs/xc-mutation-log.md`
- `docs/TUI-CLOSE-evidence.md`
- `docs/performance/PERF-CLOSE-evidence.md`
- `docs/release.md`, `docs/supported-platforms.md`, and release archive outputs
- `docs/DEPS-R1-regrounded-bins.md` and `docs/DEPS-R2-verdict-ledger.md`

The DOC-F closure record binds the final hashes, run ID, archive results, and
prerequisite closure evidence after verification completes.
