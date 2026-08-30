# PERF-T7: Symmetric install-footprint accounting

> Resolves [issue #91](https://github.com/metaphorics/pi-oxidized/issues/91).
> Runner: `bun run scripts/verification/footprint.ts` (`verify:footprint`).
> Artifact: `target/bench/install-footprint.json` (generated per run; not committed).
> Upstream baseline: the [settled reference pin](compatibility.md#settled-reference-pin).

## Why an accounting contract is needed

The two distributions are structurally different, so a naive byte sum is never
semantically equal:

- The Rust release is one self-contained native launcher plus a TypeScript
  extension-host sidecar (a `bun build --compile` binary that embeds the Bun
  runtime), theme/docs/examples/assets, and a `release.json` manifest. Nothing
  outside the unpacked archive is required at run time.
- The upstream reference ships as an npm package (`@earendil-works/pi-coding-agent`)
  whose launcher (`dist/bundle/cli.js`) is JavaScript: the bytes on disk after
  `npm install` are the package payload plus a 136-entry production dependency
  closure (per the upstream installer's own `install-lock` lockfile), and an
  external Node.js (Node engine floor in [docs/compatibility.md#engine-floors](compatibility.md#engine-floors)) or Bun
  interpreter that the package does not ship.
- The upstream tree also carries a second launcher shape, the `dist/pi`
  compiled binary (produced only by upstream's `build:binary` script, never by
  the `prepublishOnly` publish flow), which embeds the Bun runtime the way the
  Rust launcher embeds nothing.

Any installed-size comparison must therefore fix, before any number is quoted:
what counts as a launcher, what counts as the distribution's own runtime
payload, what counts as shipped dependencies, what is an external interpreter
prerequisite (never summed), and what each side's mechanical authority for
those sets is. That is this contract.

## Accounting classes

Every class is populated by both sides from a recorded mechanical authority,
or carries an explicit empty-with-reason note. Classes are reported
separately and never merged silently; the comparable total is the sum
C1 + C2 + C3.

| Class | Definition | Rust side authority | Upstream side authority |
| --- | --- | --- | --- |
| C1 launcher | The entrypoint artifact the user executes | `pi` binary in the assembled release tree (`release.json` manifest, produced by `assembleRelease` from `scripts/release/stage.ts`) | `dist/bundle/cli.js` from the `npm pack --dry-run --json` file list of the pinned reference package |
| C2 runtime payload | Every non-launcher byte the distribution itself ships | Everything else in the assembled release tree (extension-host sidecar, `theme/`, `assets/`, `docs/`, `examples/`, `CHANGELOG.md`, `README.md`, `LICENSE*`, `release.json`) | All remaining `npm pack` file-list bytes except the compiled-launcher file `dist/pi` (see exclusions) |
| C3 shipped dependencies | Bytes installed to satisfy declared runtime dependencies | None — statically linked into the launcher; recorded as empty with reason, never silently omitted | Production closure of `install-lock/package-lock.json`: third-party packages measured as installed `node_modules/<pkg>` directories, first-party `@earendil-works/pi-*` workspace packages measured as their own `npm pack --dry-run --json` file lists |
| C4 external interpreter | Runtime prerequisite the distribution does not ship | None — launcher and sidecar are self-contained | Node.js or Bun (engine floors in [docs/compatibility.md#engine-floors](compatibility.md#engine-floors)) — recorded as name, version constraint, and on-machine binary size for context, **excluded from every total** |

## Measurement invariants (both sides)

1. **Unit**: apparent file bytes (`lstat` size). Symlinks are never followed
   and count zero bytes (they are counted and reported). On-disk walks and
   `npm pack` sizes are the same unit — npm's per-file sizes are the file's
   byte length.
2. **Same platform**: both sides are measured on the same machine and OS/ARCH.
   Foreign-platform optional dependencies (npm `os`/`cpu` filtering) are absent
   from a real install on this platform and are excluded with names recorded.
3. **Distributions, not single numbers**: the full accounting is recomputed
   `FOOTPRINT_SCAN_SAMPLES` (5) times; every class total and side total is
   reported as a distribution (count/median/p95/p99/min/max/stddev/relative
   spread) and gated by the D4 noise protocol (`stddev > 20%` of median
   rejects the run as noise, never as a size verdict). Static byte
   measurements are expected to be degenerate (spread 0); the gate still runs.
4. **Paired provenance**: the artifact records both sides side by side —
   commands (label/cwd/argv) and authority files (path + SHA-256) for each
   implementation, plus upstream reference pin.
5. **Double-counting ban**: the coding-agent package is measured exactly once
   (as C1 + C2); its closure entry is excluded from C3 with that reason.
   Workspace symlink entries in the reference `node_modules` are never walked
   into (the symlink is the mapping to the workspace, not installed bytes).
6. **No thresholds**: this lane defines accounting only. It adds no size
   target, gate, or budget; `pass` in the artifact means "accounting complete
   and quiet", never "small enough". (Zero-new-numbers invariant.)

## Exclusions (named, byte-counted, never silently dropped)

| Exclusion | Applies to | Reason |
| --- | --- | --- |
| `dist/pi` compiled launcher file | Upstream C2 | Produced only by upstream `build:binary`; the npm publish flow (`prepublishOnly` -> `build`) never ships it. Reported as the compiled-launcher variant in the comparison context and stays under the D7 "executable artifact size" naming (lane 11 in `docs/PERF-R2-workload-surface-ranking.md`). |
| Foreign-platform optional dependency dirs | Upstream C3 | npm's own `os`/`cpu` filter skips them on this platform (8 `@mariozechner/clipboard-*` binaries at the pinned revision). Names recorded in the artifact. |
| External interpreter bytes | Upstream totals | C4 is context, never summed (the Rust side ships none, so summing it would be asymmetric by construction). |
| Installer-generated metadata | Upstream | npm's install-time bookkeeping (`.package-lock.json`, `.bin` links) is generated by the installer, not shipped by the distribution. The Rust `release.json` **is** counted: the release archive ships it. |

## What each side measures, mechanically

**Rust** (`target/bench/footprint-staging/<archiveDir>/`):

1. `cargo build -p pi --release --locked` (skipped when the binary is fresh;
   command recorded either way).
2. Extension host built through the production builder (`buildHost` from
   `scripts/release/host.ts` — the same path `package-release.ts` uses).
3. The release tree is assembled by the production assembler
   (`assembleRelease` from `scripts/release/stage.ts`) with the same
   docs/examples/assets sources `package-release.ts` stages, so the measured
   tree is the real distribution, not a re-implementation of its layout.
4. The staged tree is walked per scan; `pi` is classified C1, every other
   file C2; `release.json` (which lists every file and size) is recorded as
   an authority.

**Reference** (`.references/pi-2.0`, read-only):

1. `npm pack --dry-run --json` in `packages/coding-agent` — the exact shipped
   payload file list and per-file bytes of the pinned package.
2. The install-lock closure (`packages/coding-agent/install-lock/package-lock.json`,
   the lockfile root the upstream Pi installer itself uses) is parsed:
   - real third-party `node_modules/<pkg>` directories are walked (C3),
     with nested installs counted once inside their parent's directory;
   - `@earendil-works/pi-*` entries resolve through their `node_modules`
     symlinks to workspace directories, verified to match the lock's
     name@version, and are measured by their own `npm pack --dry-run --json`
     lists (C3, first-party);
   - the `@earendil-works/pi-coding-agent` entry itself is excluded from C3
     (double-counting ban);
   - nested lock entries (`node_modules/<parent>/node_modules/<child>`) are
     not separate walk roots: a nested install lives inside its parent's
     installed directory and is counted exactly once by the parent walk;
   - the workspace `@earendil-works/*` symlink entries themselves are
     reported in the symlink count (zero bytes, never followed; invariant 5).
   - absent foreign-platform optional entries are excluded with names.
3. The external interpreter constraint comes from the install lock's
   `engines` field; the on-machine `node` binary size is context only.

## Result reporting rules

- Numbers from this lane may be quoted only as "installed footprint under the
  PERF-T7 symmetric accounting" with the artifact path cited.
- Launcher-only numbers keep the D7 naming ("executable artifact size",
  never "package size" or "distribution size").
- The comparison block reports the npm-variant totals (Rust C1+C2+C3 vs
  upstream C1+C2+C3) with the upstream/Rust ratio, plus the compiled-launcher
  context line. It asserts no winner: superiority claims live in other lanes'
  gates, and this lane sets none.

## D8 registry effect

The decision-plan D8 entry "total installed/distribution footprint"
(non-claim, unblocked by "symmetric footprint accounting") flips to
**measurable** once this runner's artifact exists: the accounting is
symmetric, mechanical, and reproducible on demand via `verify:footprint`.
