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

- **`Scenario`** (13 variants):
  - *Fixture (4)*: `fixture-stream-settle`, `fixture-resize-ladder`, `fixture-resize-storm`, `fixture-paste-cursor`.
  - *Product (9)*: `cold-start`, `wizard`, `trust-selector`, `trust-dialog`, `streaming`, `selectors`, `overlays`, `product-resize-ladder`, `product-resize-storm`.
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

| Runner row ID (`RowId`) | Platform | Required driver (`TierN`) | Local driver allowed |
|---|---|---|---|
| `gnu-x64` | Linux x86-64 | `DriverKind::PosixPty` | `PosixPty`, `QemuUserSmoke` |
| `gnu-arm64` | Linux AArch64 | `DriverKind::PosixPty` | `PosixPty`, `QemuUserSmoke` |
| `darwin-x64` | macOS x86-64 | `DriverKind::PosixPty` | `PosixPty` |
| `darwin-arm64` | macOS Apple Silicon | `DriverKind::PosixPty` | `PosixPty` |
| `windows-x64` | Windows x86-64 | `DriverKind::ConPty` | `ConPty` only |

`RowTier::TierN` artifacts require a non-empty `runner_image` string (`ValidatorError::TierNMissingRunnerImage`).

### QEMU contingency rules
`QemuUserSmokeDriver` (`crates/pi-tui/src/testkit/qemu.rs`) executes binaries via piped stdio (`DriverSession` only, never `RenderSession`).
1. Mode must be `TranscriptMode::Contingency` (`ValidatorError::QemuNonContingencyMode`).
2. Claims must be a subset of `{ ClaimClass::Execution, ClaimClass::Protocol }` (`ValidatorError::QemuClaimOutsideAllowed`).
3. Render-class events (`Snapshot`, `Resize`, `ResizeStorm`) and claims (`Render`, `Pty`, `SynchronizedOutput`, `NoClear`, `Snapshot`) are forbidden (`ValidatorError::QemuSnapshotOrRenderClaim`).
4. `RowTier::TierN` is prohibited for QEMU artifacts (`ValidatorError::QemuTierN`).

## 7. Repeatability gate (k >= 3) and settle policies

`run_k` (`crates/pi-tui/src/testkit/repeat.rs`) runs a transcript producer k times (k >= 3):
- Encodes canonical bytes with `encode_canonical` and computes `digest_canonical`.
- Asserts all k iterations yield identical canonical JSON bytes and identical SHA-256 digests.
- Returns `RepeatError::Divergence { first_divergent_seq, left_digest, right_digest }` on mismatch.
- `SettlePolicy` (`crates/pi-tui/src/testkit/driver.rs`): default `quiet: 250ms`, `ceiling: 10s`. Product corpus uses `quiet: 150ms`, `ceiling: 45s` (`ProductRun::open` in `crates/pi/tests/tui_transcripts.rs`).

## 8. Corpus scenarios, prerequisites, and validator CLI

### Scenario matrix and artifact layout
Transcripts land at `target/verification/tui-transcripts/<row>/<scenario>/run-{1,2,3}/transcript.artifact.json` (`write_artifact` in `crates/pi/tests/tui_transcripts.rs`).

| Corpus | Scenarios | Primary assertions and flow | Prerequisites |
|---|---|---|---|
| **Fixture** (`crates/pi-tui/tests/transcript_fixture.rs`) | `stream-settle`<br>`resize-ladder`<br>`resize-storm`<br>`paste-cursor` | Deterministic VT state, OSC transaction markers, balanced `no-clear`, paste and cursor handling | `pi_tui_pty_fixture` binary (`crates/pi-tui/src/bin/pi_tui_pty_fixture.rs`) |
| **Product** (`crates/pi/tests/tui_transcripts.rs`) | `cold-start`<br>`wizard`<br>`trust-selector`<br>`trust-dialog`<br>`streaming`<br>`selectors`<br>`overlays`<br>`product-resize-ladder`<br>`product-resize-storm` | Startup prompts, theme selection, streaming output, `/resume` selector, `/trust` selector, resize resilience to 1x1 | `CARGO_BIN_EXE_pi`, extension host (`pi-extension-host`), test extension (`verification-profile`) |

### Trust selector and trust dialog mechanics
- `Scenario::TrustSelector` (`run_trust_selector` in `crates/pi/tests/tui_transcripts.rs`): Bootstraps a trust-requiring workspace. Presses Enter on the `"Trust project folder?"` prompt, asserts that `ProjectTrustStore` persisted the decision in `trust.json`, verifies that the project prompt resource loads, opens the `/trust` selector to verify the default project trust surface, and dismisses the selector with Escape.
- `Scenario::TrustDialog` (`run_trust_dialog` in `crates/pi/tests/tui_transcripts.rs`): Bootstraps a trust-requiring workspace. Presses Escape on the `"Trust project folder?"` prompt, asserts that no trust decision is persisted, verifies that the project prompt resource remains inaccessible, opens the `/trust` selector, dismisses the selector, and exits.

### Validator CLI
`pi_tui_transcript_validator` (`crates/pi-tui/src/bin/pi_tui_transcript_validator.rs`):
```bash
cargo run -p pi-tui --bin pi_tui_transcript_validator -- <path-to-artifact.json | dir>
```
Recursively scans for `*.artifact.json`. Fails (exit code 1) on any validation error or if zero artifacts match.

## 9. Current evidence boundary

- **Local Host Evidence**: Fully supported across Unix (`PosixPtyDriver` in `crates/pi-tui/src/testkit/posix.rs`) and Windows (`ConPtyDriver` in `crates/pi-tui/src/testkit/conpty.rs`).
- **Tier-N Five-Runner CI Evidence**: **PENDING**. The schema, driver interfaces, validator rules, and corpus suites are implemented; multi-runner CI artifacts from all five Tier N platforms are not yet committed.
