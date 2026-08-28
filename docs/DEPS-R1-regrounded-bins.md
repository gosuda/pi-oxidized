# DEPS-R1 — Re-grounded dependency bins (execution-date schedule)

Stable ID `DEPS-R1`, issue metaphorics/pi-oxidized#117, Phase 1 of the EXT-23
(#23) dependency upgrade policy. Research artifact: **no schedule-affecting
repo edits** — this document re-dates the bins, pins the seven-target lane
recipe by reference, anchors the SBOM baseline, and seeds the per-member
exposure records. The 2026-08-26 schedule is superseded; zero members execute
from remembered values.

| Field | Value |
|---|---|
| Execution date | 2026-08-27 (registry queries timestamped 2026-08-27T14:55–14:58Z) |
| Tree head at re-grounding | `20be789` (reference-refresh `0d65c6a`, SBOM `c4ed50c`) |
| crates.io | `https://crates.io/api/v1/crates/<name>{,/versions}` |
| npm | `https://registry.npmjs.org/<name>` |
| Rust channel | `https://static.rust-lang.org/dist/channel-rust-stable.toml` |
| Advisories | `https://api.osv.dev/v1/query` (aggregates RustSec + GitHub/npm) |

## 1. Re-dated bins (2026-08-27)

Binning rule (EXT-23): same-major patch → **P**; same-major ≥1.0 minor → **M**;
any 0.x minor step, any major step, toolchain channel → **X**.

### Bin P — patch sweep (DEPS-B1, one commit)

| Member | Pinned | 2026-08-26 target | **2026-08-27 latest stable** | Bin verdict |
|---|---|---|---|---|
| futures | 0.3.32 | 0.3.34 | 0.3.34 | P (unchanged) |
| globset | 0.4.19 | 0.4.20 | 0.4.20 | P (unchanged) |
| ignore (crate) | 0.4.30 | 0.4.33 | 0.4.33 | P (unchanged) |
| jiff | 0.2.32 | 0.2.35 | 0.2.35 | P (unchanged) |
| schemars | 1.2.1 | 1.2.2 | 1.2.2 | P (unchanged) |
| serde | 1.0.228 | 1.0.229 | 1.0.229 | P (unchanged) |
| serde_json | 1.0.150 | 1.0.151 | 1.0.151 | P (unchanged) |
| thiserror | 2.0.18 | 2.0.20 | 2.0.20 | P (unchanged) |
| tokio-util | 0.7.18 | 0.7.19 | 0.7.19 | P (unchanged) |
| ignore (npm) | 7.0.5 | 7.0.6 | 7.0.6 | P (unchanged) |

### Bin M — minor sweep (DEPS-B2, one commit)

| Member | Pinned | 2026-08-26 target | **2026-08-27 latest stable** | Bin verdict |
|---|---|---|---|---|
| aws-config | 1.9.0 | 1.11.0 | 1.11.0 | M (unchanged) |
| aws-sdk-bedrockruntime | 1.136.0 | 1.142.0 | 1.142.0 | M (unchanged) |
| google-cloud-auth | 1.14.0 | 1.15.0 | **1.16.0** | **M (re-targeted; 1.15→1.16 is a same-major minor)** |
| tokio | 1.52.4 | 1.53.1 | 1.53.1 | M (unchanged) |
| uuid | 1.24.0 | 1.25.0 | **1.26.0** | **M (re-targeted; 1.25→1.26 is a same-major minor)** |
| @types/bun (npm) | 1.3.9 | 1.4.0 | 1.4.0 | M (unchanged) |
| typebox (npm) | 1.1.38 | 1.3.19 | 1.3.19 | M (unchanged) |

### Bin X — majors, one dependency per commit (DEPS-X1/X2/X3)

| Member | Pinned | 2026-08-26 target | **2026-08-27 latest stable** | Bin verdict |
|---|---|---|---|---|
| base64 | 0.22.1 | 0.23.1 | 0.23.1 | X (unchanged) |
| serde-saphyr | 0.0.29 | 1.1.0 | 1.1.0 | X (unchanged) |
| typescript (npm) | 5.9.3 | 7.0.2 | 7.0.2 | X (unchanged) |

### Toolchain unit (DEPS-T1)

| Member | Pinned | Policy floor | **2026-08-27 stable channel** |
|---|---|---|---|
| rust-toolchain | 1.97.1 | ≥1.98.0 | **1.98.0** (`88d9e12ae 2026-08-18`, channel date 2026-08-20) |

`rust-std` publishes both musl triples on the 1.98.0 channel
(`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`: `available = true`),
so the EXT-26 musl legs stay provisionable on the re-grounded toolchain.

**Moved-member ledger:** exactly two members moved since 2026-08-26 —
`google-cloud-auth` 1.15.0→1.16.0 and `uuid` 1.25.0→1.26.0 — both same-major
minor steps, both re-binned to (remain in) Bin M with re-dated targets. No
member crossed a bin boundary; no member dropped or was added. npm `jiti`
2.7.0 (direct runtime pin, not a bin member) is already at latest.

## 2. Yank and advisory state (zero diversions)

- **Yanked versions:** none. Every scheduled target version is `yanked = false`
  on crates.io (per-version check, not the crate summary); npm has no yank
  mechanism and no scheduled target carries a `deprecated` flag.
- **Active advisories:** zero. OSV queries (2026-08-27T14:55–14:58Z) over the
  **from** version (20 queries) and the **to** version (20 queries) of every
  scheduled member returned 0 vulnerabilities each — including the two
  re-targeted members' new targets (`google-cloud-auth@1.16.0`, `uuid@1.26.0`).
- **Diversion ledger:** empty. No member diverts to the DEPS-R2 out-of-band
  CVE/yank path; there is no Class S/E divergence verdict to record. The two
  syntect-transitive deny.toml ignores (RUSTSEC-2025-0141, RUSTSEC-2024-0320)
  are out of bin scope and remain governed by DEPS-R3; the scheduled member
  set does not touch them.
- The deny.toml tripwire (`unused-ignored-advisory = "deny"`,
  `yanked = "deny"`) re-proves both facts mechanically at every epoch
  post-audit (`cargo deny check`), so this section is re-verified — not
  remembered — at each epoch boundary.

## 3. Seven-target post-audit lane recipe (pinned by reference)

The universal post-audit lane is **defined by EXT-26 (issue #26, landed)** and
consumed read-only here — never re-implemented:

- **Targets:** `scripts/release/targets.ts` `RUST_TARGETS` (seven triples,
  REL-T1) — the five build/test triples `x86_64-unknown-linux-gnu`,
  `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`,
  `x86_64-pc-windows-msvc`, plus per-musl-artifact proof on
  `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`.
- **Musl artifact proofs (four axes, per EXT-26):** host-native artifact
  execution from the unpacked archive; static-link/unpack/integrity gates
  (zero `readelf -d` NEEDED, `ldd` "not a dynamic executable", ELF whitelist,
  pass1/pass2 checksum diff); compiled-host JSONL handshake smoke (protocol
  v1 / compatibility 0.80.10); bundled-Bun-fallback JSONL smoke. Musl legs
  carry zero interaction claims; QEMU is a labeled contingency only.
- **Runner topology and evidence rules:** EXT-26's binding five-Tier-N table
  with resolved-image-version evidence fields.
- **Per-epoch gate:** the DEPS-R2 exposure checker
  (`scripts/verification/dependency-exposure.ts`) decides Class S/E per
  change; scheduled epoch members always run the full lane (no pre-approved
  exemptions, EXT-23).

**Epoch-start gate status (honest, at re-grounding date):** EXT-26 has landed
and the target/asset-pin model is in the tree, but the seven-leg CI wiring
itself is REL-T4 (#108, open) — the workflow matrix currently holds the five
build/test triples. Per EXT-23 Phase 1 ("EXT-26 lanes usable or epochs do not
start"), **no epoch (DEPS-B1 onward) may start until REL-T4 wires the seven
legs**; this re-grounding does not downgrade the musl requirement.

## 4. SBOM baseline (the per-epoch diff anchor)

- Fixture: `scripts/verification/fixtures/deps-r1-sbom-baseline.json`
  (schema `pi.deps.sbom.v1`, captured 2026-08-27 against the clean tree at
  `20be789`, content sha256 `7a54b9d2bfe2…`).
- Tooling: `scripts/verification/deps-sbom.ts` (`verify:sbom`). Content =
  499 locked Rust packages (48 direct pins, license + direct/dev-only edge
  position each), toolchain channel + CI pin, three package.json surfaces,
  both bun.lock files of record (322 + 320 resolutions), bundled Bun runtime
  1.3.14 + its seven sha256 asset pins, seven release targets.
- Every epoch post-audit regenerates (`capture`) and diffs against this
  baseline; `verify` fails closed on drift (license/provenance/version
  movement). 8-test suite `deps-sbom.test.ts` pins the digest chain,
  live-tree anchor, determinism, and drift sensitivity.
- **Reference-refresh law applies:** any commit changing an SBOM input
  (manifest, lockfile, surface, toolchain/CI pin, asset pins) refreshes the
  baseline in the same commit — a red verify means "refresh", never "skip".

## 5. License re-verification of all direct pins

`cargo deny check licenses` semantics (OR-satisfaction against the deny.toml
allowlist) hold for every direct pin, re-verified at execution date from the
live registries:

- **48 Rust direct external pins** (via `cargo metadata --locked` at `20be789`):
  all license expressions are OR-composites of allowlisted atoms —
  `MIT`, `Apache-2.0`, `Unlicense` (and `rustix`'s
  `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT`, satisfied by its
  allowlisted OR branches). Full table in the SBOM fixture
  (`content.rust.packages[direct=true].license`).
- **Scheduled targets' licenses (live, from-version → to-version):** all
  unchanged across every scheduled pair — e.g. serde `MIT OR Apache-2.0`,
  jiff/globset/ignore/memchr `Unlicense OR MIT`, aws-* / google-cloud-auth
  `Apache-2.0`, schemars/tokio-util `MIT`; npm: ignore/typebox/@types/bun
  `MIT`, typescript `Apache-2.0`. Zero license drift among scheduled pairs.
- **npm direct pins:** ignore 7.0.5, @types/bun 1.3.9, typescript 5.9.3,
  typebox 1.1.38, jiti 2.7.0 (latest, MIT) — all allowlisted. `file:`
  workspace deps are local MIT sources, not registry pins.

## 6. Exposure-record seed (invariant ledger)

Checker: `bun run verify:dependency-exposure classify --subject <s> --reference
scripts/verification/fixtures/dependency-exposure/reference --emit-ledger-row`,
run on the **refreshed** canonical reference (capture head `20be789`, see §7);
following the verdict-ledger convention for sanity rows, the `head` column cites
the reference capture head. The refreshed reference landed as commit `0d65c6a`.
Per-change verdicts; rows below are the seed, not permanent labels.

| head | date | subject | class | checks |
|---|---|---|---|---|
| 20be789e | 2026-08-27 | crate:futures | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:globset | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:ignore | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:jiff | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:schemars | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:serde | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:serde_json | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:thiserror | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:tokio-util | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:aws-config | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:aws-sdk-bedrockruntime | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:google-cloud-auth | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:tokio | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:uuid | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:base64 | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | crate:serde-saphyr | S | E1:fail E2:pass E3:fail E4:pass |
| 20be789e | 2026-08-27 | npm:ignore | S | E1:fail E2:fail E3:pass E4:pass |
| 20be789e | 2026-08-27 | npm:@types/bun | E | E1:pass E2:pass E3:pass E4:pass |
| 20be789e | 2026-08-27 | npm:typebox | S | E1:fail E2:fail E3:pass E4:pass |
| 20be789e | 2026-08-27 | npm:typescript | E | E1:pass E2:pass E3:pass E4:pass |
| 20be789e | 2026-08-27 | tool:rust-toolchain | S | E1:pass E2:pass E3:fail E4:fail |
| 0348ecf0 | 2026-08-28 | npm:typebox | S | E1:fail E2:fail E3:pass E4:pass |
| 0348ecf0 | 2026-08-28 | npm:@types/bun | E | E1:pass E2:pass E3:pass E4:pass |
| 5e28ac5d | 2026-08-28 | crate:base64 | S | E1:fail E2:pass E3:fail E4:pass |
| 7f325058 | 2026-08-28 | crate:serde-saphyr | S | E1:fail E2:pass E3:fail E4:pass |

Reading: every crate member links into the shipped `pi` binary on non-dev
edges (E3 fail) → Class S. `npm:ignore` sits in the root `dependencies`
production field and bundles into the sidecar → S. `@types/bun` and
`typescript` currently carry complete E1–E4 bundles (dev-only, unbundled) —
**recorded, not exercised**: per EXT-23, zero scheduled epoch member is
pre-classified exempt, so both keep their full seven-target gates in Bin M /
Bin X respectively. The toolchain produces shipped bytes → S, and DEPS-T1's
bump carries the seven-target lane plus its own generated-doc commit.

## 7. Landing notes

- **Exposure reference refresh (commit `0d65c6a`):** commits after DEPS-R2's
  `b90362dc` capture (par-math's `pi-tui->pi-bench-alloc` edge; tui-v6 era
  extension-host `src/host.ts`, `src/lean-runner.ts`, `src/virtual-modules.ts`
  drift) had left the checked-in reference stale and the self-check red
  (`npm:@types/bun` decided S fail-closed). Refreshed per the runbook §5
  reference-refresh law: capture on the clean tree (2493 metafile inputs,
  unchanged count), self-check green, 33/33 checker tests pass. The §6 seed
  rows were emitted against the refreshed reference.
- **Pre-existing tsc noise:** `bun run check` reports errors in
  `scripts/verification/xc-matrix.ts` (lines 94/98) at this head — untouched
  by DEPS-R1, owned elsewhere; `deps-sbom.*` files are clean under the same
  check.
- **Residual drift for the next cadence:** this schedule is dated 2026-08-27;
  DEPS-B1 re-verifies its own members at epoch start per EXT-23 rule 1.
