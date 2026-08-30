# Supported release platforms

Stable ID `REL-T9`, issue metaphorics/pi-oxidized#116, parent metaphorics/pi-oxidized#26.
Authored 2026-08-28 against the seven-target release definition.

Grounding rule: every normative sentence in this document cites a code line, a manifest
field, a workflow step, or a dated channel pin. Citations use `path:line` form;
`workflow:NN` abbreviates `.github/workflows/release-verification.yml:NN`. The two
byte-identity carriers — the five-row Tier N statement and the musl absence line — are
pinned against their owning sources by `scripts/tests/supported-platforms.test.ts`.

## 1. Seven release targets and their native runners

The release surface is exactly seven Rust triples (`RUST_TARGETS`,
`scripts/release/targets.ts:18-26`). `buildPlan`
(`scripts/release/targets.ts:89-119`) derives each plan's archive format,
libc class, Bun target, and archive directory from the triple alone so every
packaging phase reads one source of truth.

| Rust target | Runner (workflow matrix) | Archive | Archive dir | Bun runtime asset |
|---|---|---|---|---|
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` (`workflow:32-33`) | `tar.gz` | `pi-linux-x64-base` | `bun-linux-x64-baseline.zip` |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` (`workflow:34-35`) | `tar.gz` | `pi-linux-arm64` | `bun-linux-aarch64.zip` |
| `x86_64-apple-darwin` | `macos-15-intel` (`workflow:36-37`) | `tar.gz` | `pi-darwin-x64-base` | `bun-darwin-x64-baseline.zip` |
| `aarch64-apple-darwin` | `macos-15` (`workflow:38-39`) | `tar.gz` | `pi-darwin-arm64` | `bun-darwin-aarch64.zip` |
| `x86_64-pc-windows-msvc` | `windows-2025`, image-pinned (`workflow:40-44`) | `zip` | `pi-windows-x64-base` | `bun-windows-x64-baseline.zip` |
| `x86_64-unknown-linux-musl` | `ubuntu-24.04`, musl leg (`workflow:47-53`) | `tar.gz` | `pi-linux-x64-musl-base` | `bun-linux-x64-musl-baseline.zip` |
| `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm`, musl leg (`workflow:56-66`) | `tar.gz` | `pi-linux-arm64-musl` | `bun-linux-aarch64-musl.zip` |

- Archive naming is code, not convention: `pi-<version>-<archiveDir>.tar.gz` (`.zip` on
  windows only) and the sidecar `<archive>.sha256`
  (`scripts/release/targets.ts:110,162-169`).
- x86_64 Bun targets carry the `-baseline` suffix to stay under Bun's AVX2 floor
  (`scripts/release/targets.ts:86-87,100-101`).
- Both aarch64 legs run the "Verify native arm64 runner" gate: `uname -m` must be
  `aarch64` (Linux) or `arm64` (macOS), `RUNNER_ARCH` must be `ARM64`, and a registered
  `qemu-aarch64` binfmt handler fails the leg as a native-only topology violation
  (`workflow:87-106`); the gate records
  `native-arm64-verified=<ImageOS>@<ImageVersion>` (`workflow:107-108`).
- The windows leg pins its image label to `windows-2025` rather than the floating
  `windows-latest` alias so conhost-derivation changes are image changes, not silent
  ones (REL-R3 §5.2 condition 5; `workflow:41-44`); the ConPTY witness records
  `runner_image_pin=windows-2025` plus the resolved `ImageOS`/`ImageVersion`
  (`workflow:644-650`).
- Every Tier N witness records `runner_name`/`runner_os`/`runner_arch` and
  `image_os`/`image_version` into `witness-environment.txt` (`workflow:609-620`), and
  every musl leg records the same image fields plus `rustc`/`bun` versions into its
  environment evidence (`workflow:231-243`).
- Toolchain versions and workspace floors are generated in
  [docs/compatibility.md](compatibility.md). The workflow installs those versions through
  `dtolnay/rust-toolchain@4cda84d5c5c54efe2404f9d843567869ab1699d4`
  (`workflow:110-114`) and
  `oven-sh/setup-bun@0c5077e51419868618aeaa5fe8019c62421857d6`
  (`workflow:116-119`).

## 2. Verbatim tier statement — five Tier N rows, two musl rows

The release-tier census is normative. The compat matrix's own census reads
`exactly five release rows carry the Tier N terminal-conformance claim`
(`scripts/tests/compat-matrix.test.ts:400-409`), and this document repeats the
statement byte-identically:

> Exactly five release rows carry the `Tier N terminal-conformance row` claim —
> `release-x86_64-linux`, `release-aarch64-linux`, `release-x86_64-darwin`,
> `release-aarch64-darwin`, `release-x86_64-windows` — and the two musl legs carry
> only host-native execution, static-link/unpack/integrity, and the two-mode JSONL
> protocol smokes, `no PTY/render/synchronized-output/no-clear claims`.

Carrier map for the byte-identity contract (`scripts/tests/supported-platforms.test.ts`
pins each fragment against every carrier): the census sentence
`exactly five release rows carry the Tier N terminal-conformance claim` is carried
byte-identically by the compat matrix's `tierCensus` field
(`scripts/verification/compat-matrix.json:3`), by the transcript matrix's runner-matrix
section (`docs/tui-transcript-schema-v1.md:120`), by the compat matrix's own tier census
(`scripts/tests/compat-matrix.test.ts:400`), and by this document (quoted above); the
per-row `Tier N terminal-conformance row` claim lives in the compat matrix's five release
rows (`scripts/verification/compat-matrix.json:467,477,488,499,510`); and the absence
line lives in the transcript-lane constant, both musl rows, and the transcript matrix's
musl smoke-lane row (`crates/pi-tui/tests/transcript_musl_smoke.rs:57`;
`scripts/verification/compat-matrix.json:526,536`; `docs/tui-transcript-schema-v1.md:150`).

- Each of the five rows carries the phrase `Tier N terminal-conformance row` with its
  interaction witness — `pty_no_flicker` under portable-pty `posix_openpt` on the unix
  rows, ConPTY on windows (`scripts/verification/compat-matrix.json:467,477,488,499,510`).
- The five-row runner topology mirrors the transcript matrix's five `RowId` rows
  `gnu-x64`, `gnu-arm64`, `darwin-x64`, `darwin-arm64`, `windows-x64`
  (`docs/tui-transcript-schema-v1.md:34,106-119`; `RowId` at
  `crates/pi-tui/src/testkit/transcript.rs:103`).
- Host-native execution: the two musl rows — `release-x86_64-linux-musl`,
  `release-aarch64-linux-musl` — run natively on `ubuntu-24.04` /
  `ubuntu-24.04-arm` under the native-image gate — no cross-compilation, no QEMU
  (`workflow:87-106`) — and the musl transcript lane executes the archive's artifacts
  through the provisioned loader (`workflow:571-591`).
- Static-link: the archive's `pi` must show zero `NEEDED` entries and a static `ldd`
  verdict, else the leg fails (`workflow:446-458`).
- Unpack/integrity: every archive is extracted and smoked from its own bytes
  (`scripts/package-release.ts:318-336,382-462`), and the CI archive-integrity gate
  reconciles the extracted file set, the archive member set, and the `release.json`
  manifest set (`workflow:519-568`).
- Two-mode JSONL protocol smokes: the compiled-sidecar `hello` handshake and the
  bundled `pi-extension-host.js`-under-`bun` `hello` handshake both run from the same
  unpacked archive (`scripts/package-release.ts:202-254,429-456`; the musl smoke-lane
  row in `docs/tui-transcript-schema-v1.md:150`).
- The absence line `no PTY/render/synchronized-output/no-clear claims` is owned by the
  musl transcript lane's `ABSENCE_LINE` constant
  (`crates/pi-tui/tests/transcript_musl_smoke.rs:57`), carried byte-identically by both
  musl rows' evidence (`scripts/verification/compat-matrix.json:526,536`) and by the
  transcript matrix's musl smoke-lane row (`docs/tui-transcript-schema-v1.md:150`).

## 3. QEMU contingency label

- The musl lanes run under `DriverKind::QemuUserSmoke` in `TranscriptMode::Contingency`,
  labeled packaging/protocol-only: claims must stay inside `{execution, protocol}`,
  render-class events and claims are rejected, and `RowTier::TierN` is prohibited
  (`docs/tui-transcript-schema-v1.md:122-126` "QEMU contingency rules";
  `crates/pi-tui/src/testkit/validate.rs:86-92,118,296-309`).
- QEMU is never native evidence: the bakeoff deleted the QEMU substitution and the
  aarch64 native-image gate fails any registered binfmt handler
  (`docs/REL-R1-musl-toolchain-bakeoff.md` §4 "QEMU"; `workflow:103-106`).
- Both musl rows therefore scope every claim as "packaging/protocol scope only" with the
  absence line attached (`scripts/verification/compat-matrix.json:526,536`).

## 4. Archive, checksum, release.json, and provenance contract

- Determinism: every leg stamps `SOURCE_DATE_EPOCH=1735689600` (`workflow:17-18`) into
  the manifest's `sourceDateEpoch`/`createdAt` (`scripts/package-release.ts:271-274`)
  and the archive member mtimes (`scripts/package-release.ts:297-302`); every leg
  packages twice and `diff -u dist/pass1/*.sha256 dist/pass2/*.sha256` must be empty
  (`workflow:593-603`); the musl legs repackage from the prebuilt `pi` with `--no-cargo`
  (`workflow:413-419`).
- `release.json` carries the schema owned by `RELEASE_MANIFEST_SCHEMA`
  (`scripts/release/stage.ts:19`; see
  [docs/compatibility.md#runtime-and-release-constants](compatibility.md#runtime-and-release-constants))
  with fields `schema`, `version`, `rustTarget`, `bunTarget`, `hostKind`,
  `compatibilityVersion`, `protocolVersion`, `sourceDateEpoch`, `createdAt`,
  `files` (`scripts/release/stage.ts:34-46`); each `files` entry carries `path`,
  `size`, `sha256`, `executable` (`scripts/release/stage.ts:22-31`).
- Staged contents are ordered by `stagedInputs` (`scripts/release/stage.ts:123-215`):
  the `pi` binary, the host artifact, the musl fallback pair, mandatory `CHANGELOG.md`
  and `README.md`, optional `LICENSE`/`LICENSE-MIT`, the docs tree, optional
  `assets`/`theme`, then `release.json`
  (`scripts/release/stage.ts:171-199,208-213`).
- Docs-staged contents: the repository `docs/` tree is copied verbatim into
  `<archiveDir>/docs/` of every archive, and a release without it fails staging rather
  than shipping doc-less (`scripts/package-release.ts:275`;
  `scripts/release/stage.ts:89-94,188-192`).
- Per-file digest coverage: the CI gate requires the unpacked file set, the archive
  member set, and the manifest set to be equal — `release.json` must be present as an
  archive member but is excluded from set equality because it cannot carry its own
  digest — and re-derives every member's sha256 and size against the manifest
  (`workflow:519-568`).
- Checksum sidecar format is `<lowercase-hex-sha256>  <archive-name>\n`
  (`scripts/release/archive.ts:592-594`), computed from the finalized archive bytes
  (`scripts/package-release.ts:304-308`).
- Provenance (REL-T8): `actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8`
  (release-channel version v4.2.2) attests each leg's archive plus its `.sha256` sidecar
  as subjects (`workflow:695-717`); the verification step re-derives each subject
  digest, checks it against the DSSE signature chain to GitHub's Sigstore root, pins the
  signer to this repository via `--signer-repo`, and copies the bundle beside the
  archive as `attestation.json` (`workflow:719-748`).

## 5. Host modes and the offline pre-cache rule

- Compiled mode: `bun build --compile --target <plan.bunTarget>` produces
  `pi-extension-host[.exe]`; the artifact must pass a runtime-import probe (`hello` plus
  `extensions.load` of a Type-based tool fixture) and an independent `hello` handshake,
  and a failing probe fails the release instead of substituting a different runtime
  graph (`scripts/release/host.ts:128-142,253-285`).
- Runtime-bundle mode: plain-JS `pi-extension-host.js` plus the checksum-verified pinned
  `bun[.exe]` (`scripts/release/host.ts:399-430`), staged as the pair
  (`scripts/release/stage.ts:140-153`).
- Selection: compiled first, runtime-bundle fallback, both failing raising
  `HostBuildError` (`scripts/release/host.ts:144-163`); the outcome is recorded in the
  manifest's `hostKind` field (`scripts/release/stage.ts:40`).
- Musl rows: require the compiled sidecar — a musl host build that produced a
  runtime-bundle throws — and additionally stage the `pi-extension-host.js` plus musl
  `bun` fallback beside `pi` so both host execution paths ride one archive
  (`scripts/package-release.ts:202-254`).
- Offline pre-cache rule (REL-T6): `--runtime-cache <dir>`
  (`scripts/package-release.ts:22`) is consulted BEFORE any fetch
  (`scripts/release/runtime.ts:96-110,161-172`); cache bytes pass the same pinned-sha256
  verification as downloads, so a corrupted cache fails byte-identically to a corrupted
  download (`scripts/release/runtime.ts:120-136`); with a cache configured, any fetch
  failure raises `BunRuntimeProvisionError` naming the expected cache path and asset
  filename instead of silently going online (`scripts/release/runtime.ts:183-190`); the
  accepted Windows cache contract is recorded at
  github.com/metaphorics/pi-oxidized#110 (comment 5426692845)
  (`scripts/release/runtime.ts:103-108`).

## 6. musl link model

- `pi` is fully static: `readelf -d` must show zero `NEEDED` entries and `ldd` must
  report a static binary, else the leg fails (`workflow:446-458`).
- Bun is dynamic-against-musl: staged and unpacked `pi-extension-host` and `bun` must
  each carry exactly one `PT_INTERP` equal to `/lib/ld-musl-<arch>.so.1` — zero or
  multiple interpreter records fail, and `--list` alone is not sufficient proof
  (`workflow:460-478`; `docs/REL-R1-musl-toolchain-bakeoff.md` §3 userland step 7).
- Loader/libstdc++ contract: the Alpine minirootfs loader is installed at
  `/lib/ld-musl-<arch>.so.1` and `/etc/ld-musl-<arch>.path` is written with exactly
  `$USERLAND_DIR/usr/lib`, replacing musl's default search path so no host
  `/lib`//`/usr/lib` fallback resolves (`workflow:374-411`;
  `docs/REL-R1-musl-toolchain-bakeoff.md` §3 userland steps 2 and 6); any
  `ld-musl --list` resolution outside the userland except the pinned loader fails the
  leg (`workflow:479-517`).
- The C++ runtime rides the same contract: `libstdc++`/`libgcc` are content-addressed
  `15.2.0-r5` APK pins asserted via `.PKGINFO` (`pkgname`/`pkgver`/`arch`) before
  extraction — never an `APKINDEX` or signature-verification claim
  (`workflow:344-411`; `docs/REL-R1-musl-toolchain-bakeoff.md` §3 steps 4-5 and §5 pins
  tables, grounded 2026-08-26).

## 7. Windows guarantees — ConPTY witness and taskkill teardown

- The fifth Tier N row executes the packed `pi.exe` under ConPTY through the REL-R3
  harness (`prototype/rel-r3-conpty`), with exit codes 0 (all hard assertions pass), 1
  (hard failure, named in the verdict event), and 3 (harness/PTY error) gating the row
  (`workflow:622-634,636-693`).
- Sideload guard: a `conpty.dll`/`openconsole.exe` in the harness directory, the step
  CWD, or the unpacked archive root fails the leg, because portable-pty 0.9.0 prefers a
  sideloaded `conpty.dll` over the OS conhost (REL-R3 §5.2 condition 5;
  `workflow:664-681`).
- Hard assertions: `pi.exe --version`, the archive's TUI-ready marker
  (`--expect-ready 'type a message'`), render `/echo`/`resize` on the avt-decoded frame,
  and the DEC2026-fallback sync discipline (`workflow:626-630,683-693`).
- Teardown: `taskkill /PID <pid> /T /F` against the archive's actual process tree with
  the conhost-reap EOF (`workflow:626-630`;
  `prototype/rel-r3-conpty/src/witness.rs:78-80,354-375`); console-mode cleanup is
  recorded as advisory, not a hard assertion (`workflow:630`;
  `docs/REL-R3-conpty-witness-prototype.md` §4).
- No-go path: a hard failure on this pinned leg records the prototype findings verbatim
  and raises the topology reopen as a blocking release objection — the row is never
  weakened in place (`workflow:631-634`).

## 8. macOS — the unsigned ship decision and Gatekeeper/quarantine instructions

- Decision: the darwin archives ship unsigned. REL-R2's verdict is NO-GO for the signed
  and notarized channel as of 2026-08-26 because no Apple credential or Actions secret
  is observable from the repository, and the unsigned seven-target release definition
  remains the ship contract (`docs/REL-R2-macos-signing.md` §6, §1).
- Consequence: downloaded darwin archives are neither Developer-ID-signed nor
  notarized — exactly the `codesign`/`notarytool`/`stapler`/`spctl` chain described in
  `docs/REL-R2-macos-signing.md` §5 is absent — so Gatekeeper treats first launch as an
  unidentified-developer open.
- Recipient instructions (externally grounded, accessed 2026-08-28):
  1. Extract `pi-<version>-pi-darwin-<arch>-base.tar.gz`; files downloaded through a
     browser carry the `com.apple.quarantine` attribute that Gatekeeper assesses on
     first launch (Apple Platform Security, "Safely open apps on your Mac",
     https://support.apple.com/HT202491, accessed 2026-08-28).
  2. Approve via System Settings → Privacy & Security ("Allow Anyway"), or clear the
     attribute explicitly with `xattr -d com.apple.quarantine ./pi` (`xattr(1)` man
     page, macOS 15, accessed 2026-08-28).
  3. Verify bytes, not signatures: check the archive against its `.sha256` sidecar
     (`scripts/release/archive.ts:592-594`), the per-file digests inside `release.json`
     (`scripts/release/stage.ts:22-31`), and — when present — the REL-T8 attestation
     bundle (`workflow:719-748`).
- Revisit path: the signed channel is a credential-gated follow-on and stays non-gating
  for releases (`docs/REL-R2-macos-signing.md` §6 "non-gating by design").

## 9. Dated Bun-bump procedure — re-pinning all seven assets plus the Alpine userland

1. Pick the new Bun version from the release channel and record the grounding date.
   `BUN_RUNTIME_VERSION` in `scripts/release/runtime.ts:8` owns the version;
   [docs/compatibility.md#runtime-and-release-constants](compatibility.md#runtime-and-release-constants)
   publishes it.
2. Re-pin all seven `ASSET_PINS` entries (`scripts/release/runtime.ts:25-61`) from the
   new release's official `SHASUMS256.txt` — the vendor-verification channel used for
   the current pins (REL-T2 #104; `docs/REL-R1-musl-toolchain-bakeoff.md` §5 Bun rows,
   SHASUMS256-verified 2026-08-27): `bun-linux-x64-baseline.zip`,
   `bun-linux-x64-musl-baseline.zip`, `bun-linux-aarch64.zip`,
   `bun-linux-aarch64-musl.zip`, `bun-darwin-x64-baseline.zip`,
   `bun-darwin-aarch64.zip`, `bun-windows-x64-baseline.zip`.
3. Move the workflow's `bun-version:` step to the same version (`workflow:116-119`).
4. Re-pin the Alpine userland: `MUSL_APT_VERSION` and `ALPINE_GCC_RUNTIME_VERSION` in
   the workflow env block (`workflow:19-22`; currently musl `1.2.4-2`, minirootfs
   `3.24.1`, `libstdc++`/`libgcc` `15.2.0-r5`), the per-arch minirootfs asset + sha256
   and `libstdc++`/`libgcc` sha256 matrix pins (`workflow:47-66`), and the candidate-B
   `zig`/`cargo-zigbuild` pins (`workflow:21-22,65-66`), keeping the
   content-addressed-pin-plus-acquisition-date verification semantics — never an
   `APKINDEX` signature claim (`docs/REL-R1-musl-toolchain-bakeoff.md` §3 step 4, §5).
5. Record acquisition dates: every CI direct download flows through the `fetch()` helper
   that appends `artifact`/`url`/`sha256`/`acquired_utc`/`vendor_verification` rows to
   `pins-ledger.tsv` (`workflow:244-245,288-299,357-368`); the local equivalents are the
   dated pins tables in `docs/REL-R1-musl-toolchain-bakeoff.md` §5.
6. Regenerate the generated compat doc so the bump propagates: `docs/compatibility.md`
   is generator-owned — "Do not edit by hand — rerun the generator after any pin
   constant changes" (`docs/compatibility.md:3-7`), and `BUN_RUNTIME_VERSION` appears in
   its Runtime and Release Constants table (`docs/compatibility.md:39`).
7. Re-prove the pins end to end: `bun run verify:compatibility` runs the compat matrix
   including both musl rows (`workflow:199-201`;
   `scripts/verification/compat-matrix.json:513-538`), and the release-verification
   workflow's musl gates re-execute the static-link, interpreter, isolation, integrity,
   and two-mode protocol smokes against the new runtime
   (`workflow:421-591`).
