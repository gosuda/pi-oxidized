# DEPS-G1 — ratatui + crossterm coupled unit (decision record)

Stable ID `DEPS-G1`, issue metaphorics/pi-oxidized#127, grilling ticket under the
EXT-23 (#23, closed) dependency upgrade policy; blockers #39 (PAR-CLOSE) and #23
closed at recording time. Decision record: **no bump executes here** — as of the
recording date both lines already sit at their latest stable, so there is nothing
to migrate; this record codifies the coupled rule and assembles the migration
dossier the future coupled major must satisfy before its first edit.

| Field | Value |
| --- | --- |
| Recorded | 2026-08-28 |
| Host | Linux 7.0.0-30-generic (x86_64) |
| Registry grounding | crates.io live API + GitHub release surfaces, 2026-08-28 |
| Pins at recording | ratatui `=0.30.2`, crossterm `=0.29.0` (all three manifests) |
| Execution trigger | next stable release on either line (§4.4) |

## 1. Ruling — the coupled rule (codified)

**ratatui and crossterm never move independently.** A future ratatui major
implicitly carries a crossterm major, and a crossterm major is executed only as
the ratatui pairing bump. The unit moves as **exactly one atomic commit**
`chore(deps)!: bump ratatui+crossterm <from>→<to> (<pairing feature>)` touching
the three manifests (crates/pi/Cargo.toml, crates/pi-ext/Cargo.toml,
crates/pi-tui/Cargo.toml), the `Cargo.lock` diff per the lockfile law
(Cargo-only unit; the Bun lockfile is untouched), and the source
callsites the API break forces — nothing else. The unit is Class S (every crate
member links into the shipped `pi` binary on non-dev edges) and carries the full
post-audit of §7.

The coupled rule is a standing amendment to the EXT-23 binning law: it is not a
Bin X member scheduled today (DEPS-R1 §1 Bin X holds base64, serde-saphyr,
typescript only); it is a *dormant coupled unit* that activates on the §4.4
trigger and then follows this dossier instead of the generic one-major-per-commit
X law, because the two crates are one backend pairing, not two dependencies.

## 2. Pairing mechanics (why the coupling is real)

### 2.1 Upstream structure (grounded at 0.30.2 / 0.29.0)

ratatui 0.30 split the backend out of the facade: `ratatui` 0.30.2 depends on
`ratatui-core ^0.1.2`, `ratatui-widgets ^0.3.2`, and optional backend crates
`ratatui-crossterm ^0.1.2`, `ratatui-termina`, `ratatui-termion`,
`ratatui-termwiz`. The crossterm pairing is selected by a **versioned feature**:

- `crossterm_0_29` = `["crossterm", "ratatui-crossterm/crossterm_0_29"]`
- `crossterm_0_28` = `["crossterm", "ratatui-crossterm/crossterm_0_28"]`
- unversioned `crossterm` = `["dep:ratatui-crossterm", "std"]` (feature gate only)

A ratatui release therefore names the crossterm majors it can pair with. When a
future ratatui major ships a new backend pairing (e.g. `crossterm_0_30`) and
drops the old one, the pin's feature string must move with it.

### 2.2 In-tree structure (verified on this tree)

- The three manifests pin both crates exact (`=`) with
  `default-features = false` and select `crossterm_0_29` on every ratatui line
  (crates/pi/Cargo.toml:15,28; crates/pi-ext/Cargo.toml:13,18;
  crates/pi-tui/Cargo.toml:16,19). Feature deltas are deliberate: pi-tui adds
  crossterm `event-stream` + `bracketed-paste`, pi adds `bracketed-paste`,
  pi-ext keeps `events` only; pi/pi-tui add ratatui `scrolling-regions` +
  `underline-color`, pi-ext keeps the bare pairing.
- `Cargo.lock` resolves **exactly one** `crossterm` package: `0.29.0`
  (single `[[package]] name = "crossterm"` entry), shared by the direct deps and
  `ratatui-crossterm 0.1.2`. There are no duplicate crossterm majors in the
  graph today; preserving that singleton is the invariant the rule protects.
- Zero `ratatui::crossterm` re-export uses exist in the tree (census §3): every
  crossterm call goes through the direct `=0.29.0` dep. The singleton is
  therefore a *resolution* fact, not a source-level fact: the exact pins plus
  the `crossterm_0_29` feature keep the backend and the direct terminal code on
  one crate instance, and nothing in the source would stop a manifest edit from
  splitting it (§2.3). The coupling is enforced by the commit law of §1, which
  is exactly why it must be codified.

### 2.3 Consequences of an independent move

The issue's wording is precise: moving crossterm alone **breaks the
`crossterm_0_29` feature pairing** — the feature-selected configuration that
describes which crossterm major the backend speaks — even where the tree still
compiles. Verified mechanics per move:

| Move | Consequence |
| --- | --- |
| crossterm alone → 0.30 | the backend edge keeps resolving crossterm 0.29 through `crossterm_0_29` while the three direct edges resolve 0.30: **two crossterm majors** land in the lock and the shipped binary (`cargo tree -d` shows the dup). It compiles and it runs — the tree's only input decoder stays the direct dep's `EventStream` (input.rs:5,142,225-245), the backend is an output encoder over `Write` (writer.rs:213,256), raw mode keeps its single owner (guard.rs:15). What breaks is the pairing invariant: input and output terminal handling run on two different crossterm majors with no compiler signal, duplicate terminal-stack code ships in the binary, and the resolved configuration matches nothing in the upstream pairing matrix (§4) — unsupported and uninspectable. |
| ratatui alone → next major | three shapes. (a) Adopting the new major while its pairing feature targets a crossterm major ≠ the pinned 0.29 and the direct pins stay: the same two-major resolution as row 1, roles reversed. (b) The new major drops or renames the pairing feature — the expected major-break shape, as 0.30 itself dropped the pre-split direct-dependency model: `crossterm_0_29` becomes an unknown feature and **every one of the three manifests fails to build**. (c) The new major retains a 0.29-targeting feature: both edges still resolve 0.29, no second major appears — the coupling is not yet in play, and the unit moves only when the pairing must. Only shape (b) is compiler-enforced. |
| Both, as two commits | the intermediate commit is either the two-major lock (row 1 / row 2a) or a tree where one manifest builds and another does not (row 2b) — no acceptable intermediate state exists for a shipped TUI surface, which is why the unit is one commit. |

## 3. Callsite census (2026-08-28, at =0.30.2 / =0.29.0)

`grep -Ern "use ratatui|use crossterm" crates/<crate>/{src,tests}` use-site
lines per crate and surface (src includes the `src/bin` fixture binaries,
split out below):

| Crate | src | tests | fixture bins | Total | Files (≥1 counted line) |
| --- | --- | --- | --- | --- | --- |
| pi-tui | 79 | 3 | 18 | 100 | 33 = 24 src + 7 fixture bins + 2 tests (frame, 12 components, terminal/{backend,input,probe,guard,writer}, overlay, focus, link, component, keys, keybindings; tests: pty_no_flicker, pty_grill_adjudication) |
| pi | 27 | 0 | 0 | 27 | 6 (modes/interactive/{runtime,messages,selectors,input,view}.rs, cli/trust_selector.rs) |
| pi-ext | 5 | 2 | 0 | 7 | 2 (src/adapters.rs, tests/scaling.rs) |
| **Total** | **111** | **5** | **18** | **134** | **41 files** |

Files referencing either crate without a counted `use` line exist and are part
of the §3 API surface (pi: modes/interactive/tests.rs, cli/config_selector.rs;
pi-tui: tests/static_frame_evidence.rs; pi-ext: src/protocol.rs) — outside the
line census but inside the migration's blast radius.

API surface the future major can break (distinct module paths in use):

- **ratatui**: `buffer` (Cell/Buffer/Span-heavy diffing), `style`, `layout`,
  `widgets`, `text`, `backend::{Backend, CrosstermBackend}`, root
  `Terminal`/`Frame`. The pi-tui paint pipeline wraps
  `Terminal<GuardedBackend<CrosstermBackend<FrameSink>>>` (writer.rs:213) and the
  render-churn floor ledger pins ratatui `BufferDiff`/`Cell::eq` semantics
  (docs/performance/floors/render-churn-recomposition.md, terminal-paint.md).
- **crossterm**: `event` (Event/EventStream/poll/read, KeyCode/KeyEvent/
  KeyModifiers), `terminal` (enable/disable_raw_mode), `cursor`
  (Hide/MoveTo/Show), `queue!`, `style::ResetColor`. The probe → sync → guard
  ordering (probe.rs:25, guard.rs:8-15, backend.rs:275-284) is PTY-pinned
  (§6), not merely unit-pinned.

Coupling-sensitive seams any codemod must treat as manual: the sole
`EventStream` owner task and its pause/resume probe handshake
(input.rs:37-142,188), the emergency-restore byte contract (guard.rs,
`EMERGENCY_RESTORE_BYTES`), kitty keyboard flag state (keys.rs), and the
synchronized-output framing boundary (backend.rs:275-284).

## 4. Peer compat matrix (live registries, 2026-08-28)

### 4.1 ratatui line (MIT, MSRV floor of facade)

| Version | Date | Pairing surface | Yanked |
| --- | --- | --- | --- |
| 0.30.2 (latest stable) | 2026-06-19 | `crossterm_0_28`, `crossterm_0_29`, termina/termion/termwiz; MSRV 1.88.0 | no |
| 0.30.1 | 2026-06-05 | same feature map | no |
| 0.30.0 | 2025-12-26 | first versioned-pairing release (backend split) | no |
| 0.29.0 | 2024-10-21 | crossterm 0.28 as a direct dep (pre-split, no versioned feature) | no |

### 4.2 crossterm line (MIT)

| Version | Date | State |
| --- | --- | --- |
| 0.29.0 (latest stable) | 2025-04-05 | current pairing target |
| 0.28.1 | 2024-08-01 | not yanked (ratatui `crossterm_0_28` target) |
| 0.28.0 | 2024-07-31 | **yanked** — never a target |
| 0.27.0 | 2023-08-06 | not yanked |

Backend split crate: `ratatui-crossterm` 0.1.2 (2026-06-19), MIT, MSRV 1.88.0.
Licenses re-checked from the live registry records: every member of the pairing
(ratatui facade, ratatui-core, ratatui-widgets, ratatui-crossterm, crossterm) is
MIT — inside the deny.toml allowlist (deny.toml:34); the migration re-runs
`cargo deny check licenses` on the post-bump lock regardless (§5 D5).

### 4.3 Upstream state and master signals

- No ratatui stable after 0.30.2 (2026-06-19); no crossterm stable after 0.29.0
  (2025-04-05). Both lines at latest; the pairing `0.30.2 + 0.29.0` is current.
- ratatui/ratatui publishes `BREAKING-CHANGES.md` at repo root (44,373 bytes at
  recording) and per-crate release notes (`ratatui-v0.30.2`,
  `ratatui-crossterm-v0.1.2`, both 2026-06-19).
- crossterm master is active (pushed 2026-08-21): `Upgrade base64 dependency to
  version 0.23 (#1088)` and `feat: add MouseEventKind::get_button() (#1111)` are
  landed but unreleased. The base64 move foreshadows a peer consideration: the
  coupled unit executes on a tree where DEPS-X1 (base64 0.22.1 → 0.23.x) has
  landed, or the atomic commit absorbs the base64 move if resolution demands —
  never a second commit.
- ratatui 0.30 ships alternative backends (`termina`, `termion`, `termwiz`).
  Re-platforming off crossterm is **out of scope** for the coupled unit; it
  would be a separate decision ticket (it changes the T2/T9 PTY contracts, not
  just pins).

### 4.4 Execution trigger

The coupled unit activates when **any** of these first appears on a stable
channel: (a) ratatui > 0.30.x (new pairing feature set), (b) crossterm > 0.29.0
(new major to pair), (c) ratatui announces deprecation/removal of the versioned
pairing features. At activation, §5's dossier re-grounds from live registries —
bins never execute stale (DEPS-R1 law) — and the numbers in this record are
context, not schedule.

## 5. Migration dossier (assembled before the first edit)

Every item lands as evidence in the ticket executing the bump; none may be
produced from remembered values.

- **D1 — upstream changelog/migration capture.** Read
  `ratatui/ratatui` `BREAKING-CHANGES.md` at the target tag, the ratatui release
  notes for every minor between 0.30.2 and target (the 0.30 split shows the
  per-subcrate release-notes convention), and `crossterm-rs/crossterm`
  `CHANGELOG.md` for every version ≥ 0.29.0. Output: the breaking-change list
  mapped to our census (§3) with a per-item disposition (codemod / manual seam /
  no-op).
- **D2 — callsite census re-run.** The §3 grep protocol re-executed on the
  pre-bump tree; diffed against this record's table; drift explained.
- **D3 — codemod check.** `cargo fix --edition --allow-dirty --locked` first;
  ast-grep patterns for each renamed symbol D1 lists (pattern-per-rename,
  staged proposal, then apply); every §3 manual seam reviewed by hand and
  listed in the commit message with its reason. A rename with no codemod pattern
  is not a blocker; an unreviewed manual seam is.
- **D4 — deprecations-as-errors.** Pre-bump baseline:
  `RUSTFLAGS="-D warnings" cargo check --locked --all-targets` green on the old
  pairing; post-bump the same gate must be green on the new pairing before any
  commit. Upstream-deprecated API the tree still calls must be migrated in the
  atomic commit (workspace lints already deny correctness/unwrap_used —
  Cargo.toml:18-31 — deprecations ride the same `-D warnings` gate, not a new
  lint row).
- **D5 — license re-check.** `cargo deny check licenses` on the post-bump
  lockfile against the deny.toml allowlist; new transitives introduced by either
  line admitted or the bump blocked.
- **D6 — peer compat matrix re-ground.** §4 tables re-fetched at execution
  date; the chosen target must be max-stable of **both** lines with a shipped
  pairing feature; a ratatui target with no matching crossterm stable (or vice
  versa) defers the whole unit — no partial pairing pins.

## 6. Real PTY evidence (the TUI surface demand)

The backend pairing change is a change to the bytes that reach a real terminal.
Unit tests through `CrosstermBackend<FrameSink>` cannot see a broken pairing
(end-to-end they either compile or don't); the evidence bar is the host-tier PTY
lane, already established and verified:

- Harness: `crates/pi-tui/tests/pty_grill_adjudication.rs` (`#![cfg(unix)]`)
  spawning the release-style fixture `pi_tui_pty_fixture` under a real PTY via
  `portable-pty` (`=0.9.0`, `NativePtySystem`) and parsing the raw byte stream
  with `avt` (`=0.18.0`) plus `audit_bytes` — the `testkit` feature
  (crates/pi-tui/Cargo.toml:12,26-27). Companion lanes:
  `crates/pi-tui/tests/pty_no_flicker.rs` (synchronized-output framing, probe-
  before-sync, balance) and `crates/pi/tests/pty_grill_osc52.rs`.
- Claim set re-adjudicated on the **new** pairing: T1 (differential rendering —
  zero full-screen clears, row-local erase + reflow, continuity across
  resizes), T2 (terminal state — probe batch precedes sync, balanced
  `CSI ?2026`, emergency restore on exit, kitty flag), T3 (image encoders gated
  behind capabilities; no raw image bytes on the PTY wire), T9 (sole stdout
  owner through the stage-3 writer, transaction markers) — the same rulings
  verified at the current pairing in docs/PAR-PTY-GRILL-verdict.md.
- The fixture binaries are themselves among the 18 pi-tui bin use-sites (§3), so
  the evidence machinery compiles against the new pairing and exercises it
  end-to-end: probe → render → resize → settle → exit over a real wire. This is
  the headless-stub rejection clause: a musl smoke row or a mock writer is not
  PTY evidence (docs/TUI-CLOSE-evidence.md — musl rows carry the verbatim
  `no PTY/render/synchronized-output/no-clear claims` line; PTY claims ride the
  native rows).
- Byte-stability: the deterministic transcript gates
  (docs/tui-transcript-schema-v1.md; five frozen Tier N topologies) re-run on
  the new pairing; any byte-level drift in escape production is a finding, not
  a baseline update, unless the changelog documents the escape change.

## 7. Commit law and post-audit (Class S, full)

1. **Exactly one atomic major commit:**
   `chore(deps)!: bump ratatui+crossterm 0.30.2/0.29.0→<targets> (<pairing
   feature>)` — three manifests + Cargo.lock (lockfile law: diff committed with
   the manifests, `--locked` builds, difft-reviewed) + the source callsites
   D1/D3 force. No unrelated change rides along.
2. **Full Class S post-audit** (DEPS-R2 §4 gate table, non-compressible):
   - Seven-target lane per `scripts/release/targets.ts:18-26`:
     `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`,
     `aarch64-unknown-linux-gnu`, `aarch64-unknown-linux-musl`,
     `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc` —
     with **both** musl per-artifact proofs (build, static-link check, archive
     unpack, handshake smoke), per the EXT-26 lane recipe pinned in DEPS-R1 §3.
   - `cargo deny check` (advisories/licenses/bans/sources; syntect
     `unused-ignored-advisory` tripwire untouched) + npm advisory scans.
   - SBOM regenerated and diffed vs the DEPS-R1 baseline
     (scripts/verification/fixtures/deps-r1-sbom-baseline.json); every new
     transitive explained via D5.
   - Performance non-regression vs the pre-epoch baseline within RSD < 20%,
     floor ledgers re-validated for terminal-paint and render-churn
     (docs/performance/floors/terminal-paint.md,
     render-churn-recomposition.md) — the ratatui `BufferDiff`/`Cell::eq` share
     (~29.5 µs attribution at recording) is the expected mover; a floor change
     follows the ledger's own protocol, it is not silently absorbed.
3. **Generated-doc-only commit, conditional.** Iff a generator-consumed
   constant moved, a separate `docs(compat): regenerate …` commit carries
   exclusively re-run `scripts/verification/generate-compat-docs.ts` output.
   At recording the generator registers no ratatui/crossterm pin (grep:
   zero matches in generator and docs/compatibility.md), so the constant most
   likely to move is `rust-version` (current pairing MSRV floor 1.88.0 vs our
   1.97.1 floor — headroom exists today; the target pairing decides).
4. **Closure evidence:** the executing ticket carries the commit SHA, the D1–D6
   outputs, the PTY grill claim table (§6), and the seven-target/musl proofs,
   and closes by exact ID.

## 8. Dispositions

- **No HITL question arises.** Every acceptance element resolves from evidence:
  the coupled rule (§1-2), the dossier protocol (§5), the PTY bar (§6), the
  commit law (§7). The target versions are deliberately not picked here —
  picking them now would execute from remembered values; §4.4 defers to the
  live registry at activation.
- **Not scheduled today.** Both lines at latest stable (§4.3); no Bin X slot;
  the unit activates on trigger, not on cadence.
- **Out of scope:** the bump itself; backend re-platforming (termina et al.);
  crossterm feature-set changes (`event-stream`, `bracketed-paste`) — they move
  only with a pairing-motivated need and stay inside the atomic commit.

## Verdict

Decision record complete: the coupled rule is codified as a standing EXT-23
amendment, the pairing mechanics are proven in-tree and upstream, the census and
compat matrix are grounded at 2026-08-28, and the future migration's evidence
bar (dossier, PTY grill, one atomic commit, full Class S post-audit, conditional
generated-doc commit) is pinned. Issue #127 closes on this record.
