# TUI-P1: schema-v1 transcript specification and maintainer contract

Stable ID: `TUI-P1` (Issue #67).
Schema ID: `pi-tui-transcript/1` (`SCHEMA_ID` in `crates/pi-tui/src/testkit/transcript.rs`).
Sole Integration Path: `RecordingSession<S>` (`crates/pi-tui/src/testkit/session.rs`).

## 1. Artifact envelope and top-level structure

A schema-v1 artifact (`TranscriptArtifact` in `crates/pi-tui/src/testkit/transcript.rs`) records an interactive or scripted terminal session. All fields use camelCase and reject unknown fields (`serde(deny_unknown_fields)`).

| JSON field | Type | Description | Digest scope | Source symbol |
|---|---|---|---|---|
| `schema` | `String` | Must equal `"pi-tui-transcript/1"` | Canonical | `SCHEMA_ID` (`transcript.rs`) |
| `scenario` | `Scenario` | Target scenario identifier | Canonical | `Scenario` (`transcript.rs`) |
| `row` | `RunnerRow` | Runner tier, ID, and optional container image | Excluded | `RunnerRow` (`transcript.rs`) |
| `geometry` | `Geometry` | Initial terminal dimensions (`cols`, `rows` >= 1) | Canonical | `Geometry` (`transcript.rs`) |
| `capabilityProfile` | `CapabilityProfile` | Terminal capabilities and probe responses | Canonical | `CapabilityProfile` (`transcript.rs`) |
| `driver` | `DriverDescriptor` | Transport wrapper (`kind: DriverKind`) | Canonical | `DriverDescriptor` (`transcript.rs`) |
| `mode` | `TranscriptMode` | `standard` or `contingency` | Canonical | `TranscriptMode` (`transcript.rs`) |
| `claims` | `Vec<ClaimClass>` | Sorted, deduplicated observable assertions | Canonical | `ClaimClass` (`transcript.rs`) |
| `canonical` | `CanonicalDoc` | Ordered events and applied normalizations | Canonical | `CanonicalDoc` (`transcript.rs`) |
| `digest` | `String` | SHA-256 hash formatted as `"sha256:<hex>"` | Excluded | `digest_canonical` (`transcript.rs`) |
| `timing` | `TimingEnvelope` | Non-canonical execution timing and audits | Excluded | `TimingEnvelope` (`transcript.rs`) |

## 2. Type system and closed enumerations

All enumerations serialize with kebab-case values (`serde(rename_all = "kebab-case")` in `crates/pi-tui/src/testkit/transcript.rs`):

- **`Scenario`** (19 variants):
  - *Fixture (8)*: `fixture-stream-settle`, `fixture-resize-ladder`, `fixture-resize-storm`, `fixture-paste-cursor`, `fixture-ext-gauntlet`, `fixture-state-matrix`, `fixture-unicode-gauntlet`, `fixture-a11y-gauntlet`.
  - *Product (10)*: `cold-start`, `wizard`, `trust-selector`, `trust-dialog`, `streaming`, `selectors`, `overlays`, `product-resize-ladder`, `product-resize-storm`, `keyboard-gauntlet`.
  - *Release (1)*: `musl-packaging-smoke`.
- **`RowTier`** (2 variants): `local`, `tier-n`.
- **`RowId`** (5 variants): `gnu-x64`, `gnu-arm64`, `darwin-x64`, `darwin-arm64`, `windows-x64`.
- **`DriverKind`** (3 variants): `posix-pty`, `conpty` (`#[serde(rename = "conpty")]`), `qemu-user-smoke`.
- **`TranscriptMode`** (2 variants): `standard`, `contingency`.
- **`ClaimClass`** (7 variants): `execution`, `protocol`, `pty`, `render`, `synchronized-output`, `no-clear`, `snapshot`.
- **`CapabilityProfile`** (7 variants): `xterm256-color-truecolor`, `xterm256-color`, `dumb`, `terminal-app`, `iterm2`, `windows-terminal-vt`, `conhost-vt-dec2026-fallback`.
- **`EventKind`** (7 variants): `spawn`, `input`, `output`, `snapshot`, `resize`, `resize-storm`, `exit`.
- **`NormalizationKind`** (7 variants in `NORMALIZATION_TABLE_V1`): `path-home`, `path-cwd`, `time-iso8601`, `time-relative`, `id-session`, `snapshot-trailing-space-trim`, `resize-collapse`.

## 3. Canonical envelope and timing quarantine

### Digest calculation and excluded fields
`digest_canonical` computes SHA-256 canonical identity over `CanonicalDigestInput` (`crates/pi-tui/src/testkit/transcript.rs`). The serialized digest input contains:
`{ schema, scenario, geometry, capabilityProfile, driverKind, mode, claims, events, appliedNormalizations }`.
`claims` are sorted and deduplicated when finalizing the transcript recorder.

The non-canonical `TimingEnvelope` (`TimingEnvelope` in `transcript.rs`) contains:
- `wallMs`: Total recording wall-clock milliseconds (`u64`).
- `chunkLog`: List of `ChunkTiming { eventSeq, byteLen, deltaMs }`.
- `settleWindowsMs`: Durations of successful quiescence settle windows (`Vec<u64>`).
- `abortCeiling`: Present only on timeout: `AbortCeiling { ceilingMs, observedMs }`.
- `rawLogB64`: Complete unnormalized raw I/O byte stream in base64.
- `outputAudits`: Reconstruction proofs (`OutputAudit`) for every canonical output event.

### Cross-contamination rejection
`validate_value` (`reject_cross_contamination` in `crates/pi-tui/src/testkit/validate.rs`) strictly rejects field leakage:
- Timing fields (`wallMs`, `chunkLog`, `settleWindowsMs`, `abortCeiling`, `rawLogB64`, `outputAudits`, `rawBytesB64`, `homeB64`, `cwdB64`, `deltaMs`, `byteLen`, `eventSeq`, `ceilingMs`, `observedMs`) are banned inside `canonical` or `events` (`ValidatorError::TimingLikeCanonicalField`).
- Canonical fields (`events`, `normalizations`, `seq`, `kind`, `bytesB64`, `argv`, `cursor`, `lines`, `sizes`, `code`, `success`, `schema`, `scenario`, `claims`, `digest`) are banned inside `timing` or `chunkLog` (`ValidatorError::CanonicalLikeTimingField`).

## 4. Canonical event model and sequencing

### Sequence rules and invariants
1. Sequence numbers `seq` start at `0` and increment strictly by 1 (`validate_rules` in `validate.rs`). Gaps or disorder trigger `ValidatorError::SequenceGapOrOrder`.
2. First event must be `CanonicalEvent::Spawn { seq: 0, argv }`. Missing spawn triggers `ValidatorError::MissingSpawnOrExit`.
3. Final event must be `CanonicalEvent::Exit { seq, code: Option<i32>, success: bool }`. Missing exit triggers `ValidatorError::MissingSpawnOrExit`.
4. Dimensions across `geometry`, `Snapshot`, `Resize`, and `ResizeStorm` must be non-zero (`cols >= 1`, `rows >= 1`; `reject_zero_geometry` in `validate.rs`).

### Event variants
- `Spawn { seq, argv: Vec<String> }`: Normalized startup command vector (`argv[0]` is the program).
- `Input { seq, bytesB64: String }`: Raw standard base64 input bytes written to child stdin.
- `Output { seq, bytesB64: String }`: Normalized standard base64 output batch collected during settle.
- `Snapshot { seq, cols, rows, cursor: [u16; 2], lines: Vec<String> }`: Settled AVT screen frame with trimmed lines.
- `Resize { seq, cols, rows }`: Terminal size change.
- `ResizeStorm { seq, sizes: Vec<Geometry> }`: Coalesced resize sequence, storing only post-storm observable sizes.
- `Exit { seq, code: Option<i32>, success: bool }`: Child exit boundary.

### Atomic output and snapshot pairing
`TranscriptRecorder::output_and_snapshot` (`crates/pi-tui/src/testkit/transcript.rs`) reserves consecutive sequences `output_seq` and `snapshot_seq = output_seq + 1` before updating state. If either reservation overflows `u32`, the recorder aborts without state mutation. On `DriverKind::QemuUserSmoke`, snapshots are omitted and only output is appended.

`RecordingSession::read_settled_frame` (`crates/pi-tui/src/testkit/session.rs`) orchestrates settle reading and invokes `output_and_snapshot` atomically.

## 5. Normalization pipeline and output audits

### Replacement hierarchy and token normalization
`normalize_raw_bytes` (`crates/pi-tui/src/testkit/transcript.rs`) applies substitutions via `NormalizationContext` (`home`, `cwd` bytes):
1. `PathCwd`: Replaces `cwd` with `<CWD>` (applied first so nested paths under home do not truncate cwd prefixes).
2. `PathHome`: Replaces `home` with `<HOME>`.
3. Volatile token replacement (`match_volatile_at` in `transcript.rs`):
   - `IdSession`: 36-byte UUIDs -> `<SESSION>`.
   - `TimeIso8601`: ISO-8601 timestamps -> `<TS>`.
   - `TimeRelative`: Durations matching `\d+(ms|[smh]) ago` -> `<AGO>`.
4. `SnapshotTrailingSpaceTrim`: Right-trims trailing whitespace from snapshot lines (`TranscriptRecorder::snapshot`).
5. `ResizeCollapse`: Collapses intermediate resize storm entries to the final observed size (`TranscriptRecorder::resize_storm`).

### Output audit re-derivation
For every `CanonicalEvent::Output`, `timing.output_audits` must supply exactly one `OutputAudit` (`validate_output_audits` in `validate.rs`):
- `eventSeq`: Matches the canonical output sequence.
- `rawBytesB64`: Unnormalized raw bytes in base64.
- `context`: `NormalizationAuditContext { homeB64, cwdB64 }`.
- `applied`: List of `NormalizationEntry` records.

The validator decodes `rawBytesB64` and `context`, runs `normalize_raw_bytes`, and asserts that the resulting bytes and `applied` set match `CanonicalEvent::Output` exactly (`ValidatorError::OutputAuditMismatch`). Un-enumerated volatile tokens in `rawLogB64` fail validation (`ValidatorError::UnenumeratedVolatile` in `validate.rs`).

## 6. Runner matrix, driver pairings, and QEMU contingency

### Driver and runner row constraints
`validate_driver_row_pairing` (`crates/pi-tui/src/testkit/validate.rs`) enforces:

| Runner row ID (`RowId`) | Platform | Required driver (`TierN`) | Typical local driver |
|---|---|---|---|
| `gnu-x64` | Linux x86-64 | `DriverKind::PosixPty` | `PosixPty`, `QemuUserSmoke` |
| `gnu-arm64` | Linux AArch64 | `DriverKind::PosixPty` | `PosixPty`, `QemuUserSmoke` |
| `darwin-x64` | macOS x86-64 | `DriverKind::PosixPty` | `PosixPty` |
| `darwin-arm64` | macOS Apple Silicon | `DriverKind::PosixPty` | `PosixPty` |
| `windows-x64` | Windows x86-64 | `DriverKind::ConPty` | `ConPty` only (enforced on `TierN` and local) |

The TierN column is validator-enforced; on the local tier only `windows-x64` hard-requires `ConPty` — the remaining local "typical" pairings are guidance, not asserted.

### QEMU contingency rules
`QemuUserSmokeDriver` (`crates/pi-tui/src/testkit/qemu.rs`) executes binaries via piped stdio (`DriverSession` only, never `RenderSession`).
1. Mode must be `TranscriptMode::Contingency` (`ValidatorError::QemuNonContingencyMode`).
2. Claims must be a subset of `{ ClaimClass::Execution, ClaimClass::Protocol }` (`ValidatorError::QemuClaimOutsideAllowed`).
3. Render-class events (`Snapshot`, `Resize`, `ResizeStorm`) are forbidden (`ValidatorError::QemuSnapshotOrRenderClaim`); render-class *claims* (and any claim outside `Execution`/`Protocol`) are rejected earlier by rule 2 as `ValidatorError::QemuClaimOutsideAllowed`.
4. `RowTier::TierN` is prohibited for QEMU artifacts (`ValidatorError::QemuTierN`).

## 7. Repeatability gate (k >= 3) and settle policies

`run_k` (`crates/pi-tui/src/testkit/repeat.rs`) runs a transcript producer k times (k >= 3):
- Encodes canonical bytes with `encode_canonical` and computes `digest_canonical`.
- Asserts all k iterations yield identical canonical JSON bytes and identical SHA-256 digests.
- Returns `RepeatError::Divergence { first_divergent_seq, left_digest, right_digest }` on mismatch.
- `SettlePolicy` (`crates/pi-tui/src/testkit/driver.rs`): default `quiet: 250ms`, `ceiling: 10s`. Product corpus uses `quiet: 150ms`, `ceiling: 45s` (`ProductRun::open` in `crates/pi/tests/tui_transcripts.rs`).

## 8. Corpus scenarios, prerequisites, and validator CLI

### Scenario matrix and artifact layout
Transcripts land at `target/verification/tui-transcripts/<row>/<scenario>/run-{1,2,3}/transcript.artifact.json` (`write_artifact` in the corpus tests; the musl lane nests one level deeper at `<row>/musl-smoke/<axis>/run-{1,2,3}/`). The validator CLI requires the `.artifact.json` suffix (`is_artifact_file`), so any other filename (e.g. bare `artifact.json`) is invisible to directory scans — keep writers and validator on this one spelling.

| Corpus | Scenarios | Primary assertions and flow | Prerequisites |
|---|---|---|---|
| **Fixture** (`crates/pi-tui/tests/transcript_fixture.rs`) | `stream-settle`<br>`resize-ladder`<br>`resize-storm`<br>`paste-cursor` | Deterministic VT state, OSC transaction markers, balanced `no-clear`, paste and cursor handling | `pi_tui_pty_fixture` binary (`crates/pi-tui/src/bin/pi_tui_pty_fixture.rs`) |
| **Extension gauntlet** (`crates/pi-tui/tests/transcript_ext_gauntlet.rs`) | `ext-gauntlet` | Extension UI gauntlet with sanitization floor (`fixture-ext-gauntlet`) | `pi_tui_ext_fixture` binary (`crates/pi-tui/src/bin/pi_tui_ext_fixture.rs`) |
| **Product** (`crates/pi/tests/tui_transcripts.rs`) | `cold-start`<br>`wizard`<br>`trust-selector`<br>`streaming`<br>`selectors`<br>`overlays`<br>`product-resize-ladder`<br>`product-resize-storm` | Startup prompts, theme selection, streaming output, `/resume` selector, `/trust` selector, resize resilience to 1x1 | `CARGO_BIN_EXE_pi`, extension host (`pi-extension-host`), test extension (`verification-profile`) |
| **State matrix** (`crates/pi-tui/tests/transcript_state_matrix.rs`) | `fixture-state-matrix` | Eight-state conversation matrix (empty, loading, retry, queue, streaming, error, focus-marked, ext-ui) driven through the stepped fixture with per-state OSC 999 quality-bar checkpoints; per-state verdict and k=3 digest repeatability recorded in `verdict.json` | `pi_tui_state_matrix_fixture` binary (`crates/pi-tui/src/bin/pi_tui_state_matrix_fixture.rs`) |
| **Unicode gauntlet** (`crates/pi-tui/tests/transcript_unicode_gauntlet.rs`) | `fixture-unicode-gauntlet` | 13-probe width corpus (CJK, ZWJ emoji, regional indicators, variation-selector emoji, combining accents, Thai/Lao AM vowels, zero-width space, raw tab) across railed, markdown table, editor cursor, overlay compositing, and paste-atomic surfaces; per-probe binary M/D verdicts with AVT column-walking oracle; k=3 digest repeatability | `pi_tui_unicode_gauntlet_fixture` binary (`crates/pi-tui/src/bin/pi_tui_unicode_gauntlet_fixture.rs`) |
| **A11y gauntlet** (`crates/pi-tui/tests/transcript_a11y_invariants.rs`) | `fixture-a11y-gauntlet` | Accessibility invariants over canonical settled content: notice persistence (product `push_notice` shape held across a content tick), static sufficiency (kind + elapsed + cancel hint in every spinner-status frame), anti-chatter (identical announcement at most one settled frame consecutively per settled stage, counted over sequence numbers); the 2s notice urgency window is a quarantined measured field against the timing envelope (pinned tolerance, verdict `tolerated`); synthetic negative probes prove each invariant fails on its mutated shape; k=3 digest repeatability | `pi_tui_a11y_fixture` binary (`crates/pi-tui/src/bin/pi_tui_a11y_fixture.rs`); evidence record `docs/TUI-V6-a11y-evidence.md` |
| **Musl smoke lane** (`crates/pi-tui/tests/transcript_musl_smoke.rs`) | `musl-packaging-smoke` | Release-row packaging/protocol axes only — artifact execution, ELF static-link, unpack/integrity, compiled-host and bundled-Bun-fallback JSONL `hello` — under `DriverKind::QemuUserSmoke` in contingency mode with claims limited to `execution` + `protocol`; every verdict record carries the verbatim absence line `no PTY/render/synchronized-output/no-clear claims` and named limitations (absent musl loader, archive not supplied, Bun standin) | `PI_TUI_MUSL_ROW=musl-x64\|musl-arm64`, `PI_TUI_MUSL_ROOT` artifact root (`pi`, `pi-extension-host`, `bun`, `pi-extension-host.js`); optional `PI_TUI_MUSL_QEMU` prefix and `PI_TUI_MUSL_ARCHIVE` |

`Scenario::TrustDialog` exists in the closed enumeration (§2) but is **not** exercised by the current product corpus: production interactive boot passes `ui: None` into `resolve_project_trusted`, so the interactive `TrustUi` prompt never renders (named limitation `limitation:absent-production-interactive-TrustUi-prompt`, asserted in `run_trust_selector`). `trust-selector` instead verifies the boot-trust observation surface: no interactive prompt appears under `--approve`, `/trust` surfaces `Default project trust`, and Escape dismisses it.

### Validator CLI
`pi_tui_transcript_validator` (`crates/pi-tui/src/bin/pi_tui_transcript_validator.rs`):
```bash
cargo run -p pi-tui --bin pi_tui_transcript_validator -- <path-to-artifact.json | dir>
```
Recursively scans for `*.artifact.json`. Fails (exit code 1) on any validation error or if zero artifacts match.

## 9. Current evidence boundary
- **Local Host Evidence**: Fixture corpus (`crates/pi-tui/tests/transcript_fixture.rs`) and extension gauntlet (`crates/pi-tui/tests/transcript_ext_gauntlet.rs`) run headless and green with `cargo test -p pi-tui --features testkit`: all four fixture scenarios record through the width ladder 80x24 → 1x1 and the 24-size resize storm, k=3 byte-identical canonical bytes and digests; the ext gauntlet records the sanitization-floor corpus. Product corpus (`crates/pi/tests/tui_transcripts.rs`) is implemented but currently **blocked before first assertions**: the product renders its opening frame and never reaches the ready chrome (`type a message to begin`) under the verification launch. Verified at the corpus-landing commit itself (`678c878`) with both the pre-xc-6 and xc-6 extension-host builds, warm and cold `HOME` bun caches — the stall predates this harness work and is product-side (extension-provider registration to first full paint), not a driver, schema, or validator defect. Product-corpus artifacts from an earlier working state exist under `target/verification/tui-transcripts/local/` and validate cleanly (24/24 `PASS` via the validator CLI, including the freshly regenerated fixture artifacts).
- **Tier-N Five-Runner CI Evidence**: **PENDING**. The schema, driver interfaces, validator rules, and corpus suites are implemented; `PI_TUI_TIER_ROW=tier-n/<row>@<image>` selects the row and enforces non-empty `runner_image` (`ValidatorError::TierNMissingRunnerImage`), and local runs structurally cannot claim Tier N (`write_artifact` + test assertion). Multi-runner CI artifacts from all five Tier N platforms are not yet committed.
- **TUI-V1 State-Matrix Evidence (local gnu-x64)**: the state-matrix corpus (`crates/pi-tui/tests/transcript_state_matrix.rs`) runs green headless with `cargo test -p pi-tui --features testkit`: all eight states (empty, loading, retry, queue, streaming, error, focus-marked, ext-ui) settle on per-state OSC 999 checkpoints with snapshot-content, no-clear, and balanced-sync assertions, k=3 byte-identical canonical digest (verdict at `target/verification/tui-transcripts/local/state-matrix/verdict.json`). Tier N conformance for the remaining four rows rides the same corpus via `PI_TUI_TIER_ROW` and lands with the Tier-N CI evidence above.
- **TUI-V1 Musl Release-Row Evidence**: the musl-x64 lane (`PI_TUI_MUSL_ROW=musl-x64 PI_TUI_MUSL_ROOT=<root> cargo test -p pi-tui --features testkit --test transcript_musl_smoke`) runs green on a glibc host with host-native execution: artifact-execution and bundled-Bun-fallback-protocol pass at k=3, static-link passes (no `PT_INTERP`), while compiled-host-protocol records the named limitation `musl loader /lib/ld-musl-x86_64.so.1 absent on this host; compiled-host protocol smoke pending musl userland (REL-T3)` and unpack-integrity records `limitation:archive-not-supplied` (needs `PI_TUI_MUSL_ARCHIVE`). Every verdict carries the verbatim absence line `no PTY/render/synchronized-output/no-clear claims` and no interaction claim. The musl-arm64 row is **blocked on the cross toolchain** (`aarch64-linux-musl-gcc` absent; `cargo build --target aarch64-unknown-linux-musl -p pi` fails in `zstd-sys` C compilation) — tracked by REL-T3 #112, which also owns provisioning the musl loader userland for the compiled-host axis.
- **TUI-V6 Accessibility-Gauntlet Evidence (local gnu-x64)**: the a11y-gauntlet corpus (`crates/pi-tui/tests/transcript_a11y_invariants.rs`) runs green headless with `cargo test -p pi-tui --features testkit --test transcript_a11y_invariants`: ten scripted frames (notice pair, working 4/5/6s, retry 2/3s, compaction 7/8s, completion) each settle on a unique OSC 999 checkpoint; the three invariants (notice persistence, static sufficiency, anti-chatter) pass over the canonical settled content with k=3 byte-identical digest (verdict at `target/verification/tui-transcripts/local/a11y-gauntlet/verdict.json`, measured urgency window quarantined as `tolerated`); synthetic negative probes prove each checker fails on its mutated shape. The manual Orca/VoiceOver sign-off is closed with the explicitly flagged degraded-verdict limitation row `limitation:manual-screen-reader-signoff-not-executed-headless-host` (see `docs/TUI-V6-a11y-evidence.md` §4). Tier N conformance rides the same corpus via `PI_TUI_TIER_ROW`.
- **TUI-V3 Unicode/Width Gauntlet Evidence (local gnu-x64)**: the unicode-gauntlet corpus (`crates/pi-tui/tests/transcript_unicode_gauntlet.rs`) runs green headless with `cargo test -p pi-tui --features testkit`: all 13 probes (P01–P13) across railed, markdown table (3 chunks), editor cursor, overlay compositing, and paste-atomic (verbatim, atomic-undo, large-paste marker) surfaces settle on per-phase OSC 999 checkpoints with per-probe binary M/D verdicts recorded in `verdict.json` (AVT column-walking oracle), k=3 byte-identical canonical digest. Divergences between the contract width table (`visible_width`) and AVT's `char_display_width` are recorded as `diverge` verdicts without hard-failure — they are the gauntlet's subject (see `docs/TUI-V3-unicode-width-gauntlet.md`). Tier N conformance rides the same corpus via `PI_TUI_TIER_ROW`.
