# DEPS-R2 — Out-of-band CVE/yanked-version remediation runbook

Stable ID `DEPS-R2`, issue metaphorics/pi-oxidized#128, child of the dependency upgrade
policy (EXT-23, #23). Landed 2026-08-27. This document is **binding execution law**, not
research: it codifies the fastest-correct-path process for a CVE or yanked-version
remediation that arrives outside the scheduled dependency epochs, and it lands the
shipped-exposure predicate checker that decides, mechanically and fail-closed, whether a
proposed remediation may skip the seven-target lane.

Tooling landed with this runbook:

- `scripts/verification/dependency-exposure.ts` — the checker (CLI: `capture-reference`,
  `classify`, `self-check`; output schema `pi.deps.exposure.v1`).
- `scripts/verification/dependency-exposure.test.ts` — 31 tests over every check and the
  fail-closed algebra.
- `scripts/verification/fixtures/dependency-exposure/reference/` — the canonical
  hash-chained reference (capture head `b90362dc`, 2026-08-26, 2493 metafile inputs).
- `package.json` → `verify:dependency-exposure` script.
- `docs/DEPS-R2-verdict-ledger.md` — the per-change verdict ledger (Section 9).

---

## 1. Supersession and diversion

A CVE or yank has no calendar. The rule set:

1. **Supersedes cadence and epoch scheduling.** An out-of-band CVE/yanked-version
   remediation may land at any point in the campaign — before, between, or during any of
   the five dependency epochs, and independently of the freshness re-scan cadence. No
   epoch boundary, freeze gate, or scheduled bin delays a security fix. The only ordering
   constraint that survives is atomicity: the remediation is its own commit and shares no
   commit with parity, behavior, or optimization work (the git-log separation audit from
   EXT-23 applies unchanged).
2. **Diversion is recorded, never silent.** When the remediated member is also a member
   of a scheduled bin (Bin P / Bin M / a major / the toolchain unit), that bin member is
   marked **diverted** with: the advisory or yank citation, the out-of-band commit SHA,
   and the Class S/E verdict. The record lives in the DEPS-R1 re-grounding ledger note
   for that bin and in the verdict ledger (Section 9).
3. **The fix routes back into the bin schedule.** At the next epoch re-grounding
   (mandatory per EXT-23 Phase 1: bins never execute stale), the diverted member is
   either dropped from the bin (already at or beyond the remediated version — cite the
   out-of-band SHA) or re-targeted to the then-current re-grounded version. The bin
   sweep then proceeds over the remaining members exactly as scheduled. Divergence never
   deletes a bin member without a ledger citation, and never re-upgrades past the
   remediated floor just to satisfy a stale bin date.
4. **Cadence resume.** After the remediation lands and its post-audit is recorded, the
   interrupted epoch resumes from where it stopped; the freshness re-scan re-dates every
   remaining bin member as usual.

## 2. Class S / Class E — decision over shipped inputs, not ecosystems

The predicate is over **shipped inputs**. Which package manager a dependency lives in is
irrelevant; what matters is whether changing it can move shipped bytes. There is **no
Rust-touching carve-out** and no npm/Bun exemption: an npm remediation on a
runtime-bundled package re-proves both musl artifacts exactly as a Rust CVE would.

| Shipped input changed | Class | Why |
|---|---|---|
| Any shipped Rust crate (normal or build edge into a shipped binary) | S | linked into the shipped `pi` binary |
| Any non-exempt lockfile subgraph (transitive of the above) | S | same bytes, one hop deeper |
| npm package on a non-dev field of any `package.json` surface (e.g. `typebox` in `packages/extension-host` `dependencies`) | S | bundled into the shipped `--compile` sidecar (E2 reachability) |
| Extension-host runtime dependency (any `dependencies`/`peerDependencies`/`optionalDependencies` entry on any surface) | S | same sidecar bundling surface |
| Bun binary / bundler version bump (`tool:bun-bundler`) | S | produces the compiled sidecar bytes |
| Bun embedded-runtime version bump (`tool:bun-runtime`) | S | runtime embedded by `--compile`; staged into the runtime-bundle archive |
| Rust toolchain bump (`tool:rust-toolchain`) | S | produces the shipped `pi` binary |
| Any compiled host (`bun build --compile` artifacts, entry or flags) | S | is shipped bytes |
| Anything staged into a release archive | S | shipped bytes in the artifact |
| Complete E1–E4 pass bundle (dev/docs-only, provably zero shipped bytes) | E | only the seven-target lane is skippable |

Sanity anchors (checked in by the checker's `self-check`, Section 8): `npm:typebox` →
**Class S** (prod-field position + bundled into the sidecar); `npm:@types/bun` →
**Class E** (its recorded verdict); `tool:bun-runtime` → **Class S**.

## 3. The E1–E4 predicate (as enforced by the checker)

- **E1 — field/edge position.** npm side: the subject must appear exclusively in
  `devDependencies` on **every** `package.json` surface (root, `packages/extension-host`,
  `packages/pi-tui-protocol` — the surface set is discovered, and a surface-set change is
  undecidable), **before and after** the change; a member removed from a non-dev field
  also fails (removing a shipped dep is Class S). Rust side: every edge into the subject
  in **both** the pre-change and post-change `cargo metadata` dep-graphs
  (`--locked --offline --all-features`) must be `kind = "dev"`; manifest text is never
  consulted — the graph wins over what `Cargo.toml` claims. Cross-ecosystem
  byte-identity: an npm subject additionally requires every Cargo manifest and
  `Cargo.lock` to be byte-identical to the reference capture (and vice versa for crate
  subjects), so a `Cargo.toml`-only edge or feature change fails closed even when the
  lockfile is untouched.
- **E2 — zero bundler-metafile reachability on every `--compile` entry.** The
  `--compile` entry set is enumerated from three origins — the release authority
  (`scripts/release/host.ts` `hostBundleCommands().compiled`), the extension-host
  `package.json` build script, and the CI workflow build statements — and every entry
  must conform to the authority argv (entrypoint and flags; `--target` values may be
  omitted or host-specific). Each metafile input is attributed to its owning package via
  npm innermost-`node_modules` resolution, with workspace sources attributed to their
  surface package. The subject must own **zero** of the bundled inputs, compared against
  the pre-change hash-pinned metafile projection. Tools: `bun-runtime`/`bun-bundler`
  bumps fail E2 by definition (they change the compiled sidecar bytes).
- **E3 — no shipped-byte production.** Every `CommandRunner.run` call site in
  `scripts/release/**` plus both release entry scripts is scanned; build-capable commands
  (`bun`, `cargo`, `tsc`, `npm`, `npx`, `yarn`, `pnpm`) whose arguments cannot be
  attributed to literals or the authority argvs are **undecidable** when they carry emit
  intent (`build`, `--compile`, `--outfile`, `--outdir`, `--release`). Every direct
  `bun build` / `cargo build` statement in `.github/workflows/*.yml` is scanned. Tool
  subjects fail E3 by definition. Crate subjects get linkage closure: BFS over non-dev
  edges from workspace members must not reach the crate.
- **E4 — no archive staging.** The staged-input table is enumerated by importing the
  **byte-verified** `scripts/release/stage.ts` `stagedInputs()` for both host kinds
  (compiled and runtime-bundle), with an inert Fs seam that throws on any accidental
  read. The subject must not source any staged input: npm subjects fail if any row
  sources from `node_modules/<name>/`, tool subjects fail if their product kinds
  (`rust-binary`, `host-binary`, `host-bundle`, `bun-runtime`) are staged.

**Verdict algebra (fail-closed):** any check `fail` **or** `undecidable` ⇒ Class S.
Class E requires all four `pass`. An unclassifiable/missing reference, a crashed
classifier, or a malformed input never yields an exemption: the CLI crash path emits
`DEPENDENCY_EXPOSURE_FAILED_CLOSED` with exit 1; a decided-but-degraded classification
emits a Class S report with the `DEPENDENCY_EXPOSURE_OK` sentinel and exit 0.
Classification is **per change, computed on the current tree at every invocation** —
never a permanent package label. A package newly imported at runtime immediately fails
E2 on the next classification.

## 4. Permanence table

| Gate | Class S | Class E | Compressible? |
|---|---|---|---|
| Seven-target lane: build + test on `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`, plus per-musl-artifact proof (build, static-link check, archive unpack, handshake smoke) on both `x86_64/aarch64-unknown-linux-musl` | **mandatory, non-compressible** | skipped (the only skippable gate) | never — neither depth nor batching may shrink it for Class S |
| Lockfile law: lockfile diff committed with the manifest change, difft-reviewed, `--locked`/`--frozen-lockfile` in CI, `package-lock.json` never introduced | mandatory | mandatory | never |
| `cargo deny check` (advisories/licenses/bans/sources; `unused-ignored-advisory = "deny"` syntect tripwire) + npm advisory scan on all three surfaces | mandatory | mandatory | never |
| SBOM regenerated and diffed vs baseline (license/provenance drift) | mandatory | mandatory | never |
| Both syntect ignores confirmed load-bearing (or retired on the qualifying release, DEPS-R3) | mandatory | mandatory | never |
| Performance non-regression vs pre-change baseline (RSD < 20%) | mandatory | vacuous for Class E — the E1–E4 bundle is the proof that zero shipped bytes moved | never (when applicable) |
| Freshness re-scan with re-dated bins | at the next cadence point after the fix (the out-of-band fix itself re-dates its own member) | same | cadence-governed |
| Changelog depth: primary upstream changelog read per member | full dossier for minor/major jumps; a patch-level CVE/yank bump may substitute the advisory/yank citation for the dossier | same | **compressible** (patch-level CVE fixes only) |
| Cross-fix batching: several concurrently-urgent fixes in one commit | allowed iff every member carries its own E1–E4 verdict, its own ledger row, and the post-audit covers the union; never batch with unrelated behavior work | same | **compressible** |
| Verdict ledger row (Section 9) | mandatory | mandatory | never — per change |

The two compressible gates exist because this is the *fastest-correct-path* lane; they
compress documentation and batching, never evidence. The lane itself is never
compressible for Class S.

## 5. Reference integrity

The canonical reference (`scripts/verification/fixtures/dependency-exposure/reference/`)
is a hash-chained manifest, not a convenience snapshot:

- `reference.json` pins the sha256 of both projections (`metafile-projection.json`,
  `cargo-graph-projection.json`), the dep-field content and hash of every npm surface,
  the hash of every Cargo manifest + `Cargo.lock`, and the hash of the three release
  authority modules (`scripts/release/{host,targets,stage}.ts`).
- The metafile projection pins the sha256 of **every** module-graph input (2493 at
  capture) plus the metafile itself, and records the exact authority `--compile` argv.
- The cargo graph projection records workspace members and every dep-graph edge with
  kinds, captured with `cargo metadata --locked --offline --all-features`.
- Authority modules are byte-compared against their pins **before** they are dynamically
  imported; a drifted authority makes E2/E4 undecidable (fail-closed), never
  untrusted-executed.

Capture procedure: `bun run verify:dependency-exposure capture-reference --out <dir>` on
the **clean pre-change commit** — the command refuses a dirty relevant tree (all three
`package.json` surfaces + `bun.lock`s, `Cargo.toml`/`Cargo.lock` + crate manifests,
`scripts/release/**` + the two release entry scripts, `packages/extension-host/src`,
`packages/pi-tui-protocol/src`). `--allow-dirty-relevant` exists solely for dirt that is
provably dep-field-free and is recorded in `manifest.relevantTreeStatus`; the checked-in
capture records `M package.json` from a sibling's scripts-line-only edit (dep fields
unaffected).

**Reference refresh law.** The canonical fixture is a living artifact. Any commit that
changes a relevant input — any Cargo manifest or `Cargo.lock`, extension-host or
protocol sources under the metafile graph, the release authority scripts, or dep fields
on any surface — must regenerate the canonical reference in the same commit
(`capture-reference` on the clean tree, then replace the fixture). This is mechanical
regeneration, like generated docs. Without it the checked-in `self-check` fails closed
by design: cross-identity, metafile-input, or authority pins go stale and known members
classify Class S. A red self-check after a relevant change means "refresh the reference",
never "ignore the gate".

## 6. Redesign disposition (why this shape)

A first prototype was reverted at `8051e59` for systemic fail-open paths. This
implementation closes each finding by construction (also documented in the checker file
header):

1. **No `--input auto`, no change-detection short-circuit.** Subjects are explicit;
   every invocation recomputes over the full current tree; cross-ecosystem byte-identity
   closes the "npm subject rides along with Rust drift" hole.
2. **E3 covers the real seam.** The release pipeline's `CommandRunner.run` call sites and
   the CI workflows' direct build statements, with unattributable emit-args undecidable
   rather than exempt.
3. **The classifier never executes head-side package code.** No `bun install`, no
   `bun build`, no lifecycle scripts at classification time; the metafile comes from the
   pre-change reference capture. The only child process is `cargo metadata` (no build
   scripts), overridable with `--cargo-metadata-file` for hermetic runs.
4. **Hash-chained reference.** Authority modules are byte-pinned before import;
   tampering with any projection is detected, not tolerated.
5. **No process-group teardown surface.** Nothing is built here, so there is no
   descendant tree to contain; the single child is killed with SIGKILL on timeout.

## 7. Operating procedure

0. **Trigger.** An advisory, a yank, or a publisher-confirmed exploit lands out of band.
1. **Classify** on the current pre-fix tree:
   `bun run verify:dependency-exposure classify --subject npm:<name> --reference scripts/verification/fixtures/dependency-exposure/reference --emit-ledger-row`
   (subject kinds: `npm:<name>`, `crate:<name>`, `tool:rust-toolchain|bun-runtime|bun-bundler`).
   A crash (`DEPENDENCY_EXPOSURE_FAILED_CLOSED`, exit 1) is a Class S verdict.
2. **Fix.** One atomic remediation commit: version pins + lockfiles + (iff a rendered
   constant moved) the generated-doc commit, per the pin-changing commit law. Nothing
   else in the commit.
3. **Post-audit per class.** Class S: the full post-audit — lockfile law, deny + npm
   advisory scans, SBOM diff, syntect tripwire, perf non-regression, and the complete
   seven-target lane including both musl per-artifact proofs. Class E: everything except
   the lane. Changelog depth may compress to the advisory citation for patch-level
   bumps; cross-fix batching per the permanence table.
4. **Record.** Paste the `--emit-ledger-row` output as a new row in
   `docs/DEPS-R2-verdict-ledger.md` (same commit), with the advisory citation and the
   gates-run evidence in the records list beneath the table.
5. **Divert and route back.** Mark the affected scheduled bin member diverted (SHA +
   verdict) in the DEPS-R1 re-grounding ledger; the next epoch re-grounding drops or
   re-targets it (Section 1).
6. **Refresh.** If the remediation changed a relevant input, regenerate the canonical
   reference in the same commit (Section 5) and re-run `self-check`.

## 8. CI wiring contract

`package.json` wires the tool: `"verify:dependency-exposure":
"bun run scripts/verification/dependency-exposure.ts"`. The unit suite
(`bun test scripts`, already a CI step) includes the 31-test file covering the checks,
the verdict algebra, and a canonical-reference self-check test.

The workflow wiring is a **binding contract**, deferred only because
`.github/workflows/release-verification.yml` is under concurrent WIP outside DEPS-R2's
atomic boundary. The next commit that owns that file must add, in the
`release-verification` job immediately after the "Install reference dependencies" step
(the checker hashes reference metafile inputs, so the reference checkout's node_modules
must exist first; no Rust toolchain is needed — npm/tool subjects never spawn cargo):

```yaml
      - name: Dependency exposure predicate self-check
        run: bun run verify:dependency-exposure self-check
```

Semantics: exit 0 with the `DEPENDENCY_EXPOSURE_OK` sentinel is the only green state;
exit 1 (any `FAIL` line or `DEPENDENCY_EXPOSURE_FAILED_CLOSED`) blocks the run. A red
self-check means a broken checker or a stale canonical reference (Section 5) — the
resolution is a reference-refresh commit, never a skip, an `continue-on-error`, or a
widened expectation.

## 9. Verdict ledger

Every classification — sanity, live remediation, or diversion — is recorded per change
in **`docs/DEPS-R2-verdict-ledger.md`** (format spec and seed rows there). The ledger is
the DEPS-R2 view of the campaign's invariant ledger: every shipped-input change commit
carries either seven-target evidence or a complete E1–E4 bundle, and the row is the
citation. Verdicts are per-change; no package ever carries a permanent label.

## 10. Landing evidence (2026-08-27)

`bun run scripts/verification/dependency-exposure.ts self-check` → exit 0:

```
ok   npm:typebox: expected S, got S — E1, E2 failed: non-dev field position: pre packages/extension-host/package.json dependencies; post packages/extension-host/package.json dependencies | bundled into the shipped sidecar: ../../.references/pi-2.0/node_modules/typebox/build/index.mjs, ../../.references/pi-2.0/node_modules/typebox/build/type/extends/index.mjs, ../../.references/pi-2.0/node_modules/typebox/build/typebox.mjs, …
ok   npm:@types/bun: expected E, got E — complete E1–E4 exemption bundle; only the seven-target lane is skippable
ok   tool:bun-runtime: expected S, got S — E2, E3, E4 failed: bun-runtime version bump changes the compiled sidecar bytes (bundler compiles / runtime is embedded via --compile) | bun-runtime produces shipped bytes by definition (pi binary / sidecar compile / embedded runtime) | bun-runtime product staged into the archive: bun-runtime -> bun (runtime-bundle)
ok   fail-closed probe (tampered reference hash chain): expected S, got S — fail-closed (E1, E2, E3, E4 undecidable): reference load: reference hash-chain broken: cargo-graph-projection.json does not match reference.json | reference load: reference hash-chain broken: cargo-graph-projection.json does not match reference.json | reference load: reference hash-chain broken: cargo-graph-projection.json does not match reference.json | reference load: reference hash-chain broken: cargo-graph-projection.json does not match reference.json
DEPENDENCY_EXPOSURE_OK
```

`bun test scripts/verification/dependency-exposure.test.ts` → `31 pass / 0 fail, 71
expect() calls`. The synthetic fail-closed probes: a tampered reference hash chain
classifies Class S (decided report, exit 0); a crashed classifier (unusable subject)
emits `DEPENDENCY_EXPOSURE_FAILED_CLOSED` with exit 1. Full check-by-check detail for the
two anchors: `npm:typebox` → E1 fail (non-dev field, pre and post), E2 fail (bundled
sidecar inputs), E3 pass (10 seam sites, 10 CI build statements attributed), E4 pass (23
staged inputs, none from `node_modules/typebox`); `npm:@types/bun` → E1 pass
(devDependencies-only across all three surfaces), E2 pass (zero of 2493 inputs), E3 pass,
E4 pass.
