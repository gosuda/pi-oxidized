# TUI-CLOSE — Terminal polish track closure evidence

- **Ticket:** [TUI-CLOSE #82](https://github.com/metaphorics/pi-oxidized/issues/82) · parent [#17](https://github.com/metaphorics/pi-oxidized/issues/17) (closed; sub-issues 32/32 once #82 closes)
- **Tree:** `feat/tui-close-consolidated-evidence` from `20be789` (campaign HEAD after the TUI-V4 merge)
- **Executed:** 2026-08-28 · all commands run in the isolated worktree at this branch
- **Scope:** the settled terminal polish track — retained Pass-class changes, their surfaces, and the deterministic evidence machinery — per the arbitration ruling pinned on #17

## 1. Consolidated better-interface review

Single consolidated review routed to all six owning `better-*` domain skills (accessibility, layout, writing, typography, colors, UI/motion), each executed as an independent inspection of the settled surfaces (`crates/pi-tui/src/**`, `crates/pi/src/modes/interactive/**`) against the track's own settled conventions (`docs/STYLE_LEDGER.md`, `docs/TUI-G6-color-doctrine.md`, `docs/TUI-G1-reduced-motion-policy.md`, `docs/TUI-G8-viewport-floor-policy.md`, `docs/terminal-rail-doctrine.md`). Out of scope per the ruling: exported-HTML accessibility, i18n, `/privacy`, screen-model redesign, pi-ext internals, performance, alt-screen/scroll-view/mouse implementation.

| Domain | Evidence inspected | Result |
| --- | --- | --- |
| Accessibility | selectors/startup/status/input/messages/progress + three-invariant lane + V6 evidence | 1 MEDIUM (shared root cause, row 1) |
| Layout | rail/spacer/wrap/slice/layout + view/header/footer/tool_renderers + V4 test module | 1 MEDIUM (row 2) |
| Writing | every user-visible literal vs STYLE_LEDGER axes 1–10 | 1 MEDIUM (row 3), 1 LOW |
| Typography | width oracle, paint paths, markdown/truncation, V3 gauntlet | 1 MEDIUM (row 4), 2 LOW |
| Colors | theme.rs census + NATIVE_CONTRAST_PAIRS oracle + committed P2 report + palettes | 1 MEDIUM (row 1), 1 HIGH **outside settled scope** (§1.1) |
| UI / motion | loader/reduced-motion seam, sync-output/no-clear discipline, borders, V4 tests | 1 MEDIUM (row 5) |

### Findings within the settled polish scope

| Severity | Domain | Location | Current | Fix | Why |
| --- | --- | --- | --- | --- | --- |
| MEDIUM | Colors, Accessibility | `crates/pi/src/modes/interactive/messages.rs:357-360` | Pending `│`/Muted vs Success `│`/Success (same glyph) | distinct glyph per phase (Error already has `┃`) | Pending↔Success is color-only state on the tool rail |
| MEDIUM | Layout | `crates/pi/src/modes/interactive/tool_renderers.rs:49-60` + V4 tests `runtime.rs:11654,11705` | `… N more lines · ctrl+o` cue wraps to two rows at the 20-col floor; tests assert substring presence, not row integrity | single-row floor cue + row-integrity assertion | verified non-blocking: `wrap_text_with_ansi` word-wraps whole tokens (`crates/pi-tui/src/text/wrap.rs:102-114`), so no content is hidden — reviewer-HIGH downgraded to MEDIUM on token-integrity evidence |
| MEDIUM | Writing | `header.rs:50-90`, `startup.rs:256-278`, `selectors.rs:31` | mixed hint casing (sentence-case header vs Title-case `/hotkeys`) + lowercase `esc` in `  esc to cancel` | one display-key casing policy per ledger axis | consistency on key-hint surfaces |
| MEDIUM | Typography | `crates/pi-tui/src/components/editor/mod.rs:2045-2046` | `visible_width(grapheme).max(1)` in the editor cursor painter | advance 0 for standalone zero-width graphemes, matching `util.rs::paint_line` and the TUI-R2 contract | caret/column divergence on the editing surface; V3 P08 ZWSP oracle is the tripwire |
| MEDIUM | UI | `crates/pi/src/modes/interactive/progress.rs:77-84,140-155,159-172` | auth/compaction/retry spinners build `Loader::new(..., None)` — reduced-motion override honored only on the status line (`status.rs:27-32`) | thread `indicator_frames` through the three secondary builders | TUI-G1 static-frame setting incomplete on secondary surfaces |

LOW (work-to-do, no action required for closure): truncation-marker styling divergence (`slice.rs` plain vs footer/theme dim `…`); H2/H3+ markdown heading face collapse (`markdown.rs:1067-1081`, reference-parity default); extension Confirm button casing (`runtime.rs:3597-3602`); footer `>90%` context band uses the Error hue — **byte-level reference parity** (`footer.ts:154-157` at canonical pin `853a80d`), owned upstream.

### 1.1 HIGH finding outside the settled polish scope — recorded and routed

**Default light-theme semantic text fails WCAG AA-normal.** Measured by the track's own committed oracle (`prototype/tui-p2-contrast/report.txt`, light/truecolor): `error` `#fc0035` on white = **4.0459**, `mdLinkUrl`/`syntaxComment` `#8f8f8f` = **3.2340** (all flagged `wcag-aa-normal<4.5`); error renders normal-weight (`messages.rs:313`, no bold bit) so 4.5 is the required ratio; antd-light `error` 3.25 same root cause.

Introduced by the **pre-track Geist rebase** `68bf5b5` (2026-07-31, "rebase dark/light themes onto the Geist palette") — before any TUI-track ticket; the reference pinned at `853a80d` ships `error=red → vars.red #aa5555` = 5.06 (passes). TUI-R1's admission matrix covered only the eight non-default palettes; V5's oracle encodes only the 10 `NATIVE_CONTRAST_PAIRS` (`theme.rs:2456`), which exclude fg-on-default-bg semantic pairs — so the gap was measured and flagged by P2 but never triaged or gated.

Disposition: a palette-value change **requires its own decision ticket** per `docs/TUI-G6-color-doctrine.md` ruling 5 precedent; changing it inside TUI-CLOSE would violate the polish classification this close exists to prove. Recorded here at full severity, reported to Main for ticket minting. **Zero open HIGH findings remain within the settled polish scope.**

### Verification

Per-suite deterministic runs on this tree (§2); contrast/oracle claims carried by the committed P2 report and `theme_contrast_matrix` (5/5 green, re-run here); wrap-token integrity verified from `wrap.rs` source; parity claims verified against `.references/pi-2.0` at `853a80d26c90a14c1886f0ebb8ffaae133ca2185`.

**Verdict: Approve** — no HIGH findings within the settled polish scope; five MEDIUM + three LOW work-to-do rows above.

## 2. Five-row Tier N deterministic transcript gate

Canonical frozen topology (`docs/tui-transcript-schema-v1.md` §6): `gnu-x64`, `gnu-arm64`, `darwin-x64`, `darwin-arm64`, `windows-x64` (`RowId`, `transcript.rs:103`; `RowTier::TierN`:97). Structural guarantees re-verified in source: local runs cannot claim Tier N (`crates/pi/tests/tui_transcripts.rs:372-374`); Tier N requires a runner image (`ValidatorError::TierNMissingRunnerImage`, `validate.rs:290`); the five-row set is frozen (TUI-R2 §7.3).

Fresh deterministic corpus runs on this tree (host = local gnu-x64 row, k=3 byte-identical digests inside each):

| Suite | Result |
| --- | --- |
| `render_churn_bench` | 3 passed |
| `theme_contrast_matrix` (V5 oracle) | 5 passed |
| `transcript_state_matrix` (V1) | 1 passed (k=3) |
| `transcript_unicode_gauntlet` (V3) | 1 passed, 13.91s (k=3) |
| `transcript_a11y_invariants` (V6) | 5 passed |
| `transcript_ext_gauntlet` (P3) | 1 passed (k=3) |
| `transcript_fixture` (P1 corpus: stream-settle, resize-ladder, resize-storm, paste-cursor) | post-fix: four green runs, one failed on the intermittent resize-ladder k=3 divergence (§6.7) — the close deadlock is gone; the divergence is pre-existing and routed |

**Tier-N five-runner CI evidence: PENDING** — the standing, schema-documented limitation (`tui-transcript-schema-v1.md` §9): multi-runner artifacts land with REL-T4 #108 wiring, which this ticket unblocks.

## 3. Musl release rows — packaging/protocol claims only

Fresh lane run on this tree: `PI_TUI_MUSL_ROW=musl-x64 PI_TUI_MUSL_ROOT=/tmp/musl-root cargo test -p pi-tui --features testkit --test transcript_musl_smoke` — artifact-execution **pass** k=3, static-link **pass** (no `PT_INTERP`), bundled-Bun-fallback-protocol **pass** k=3 (glibc-host stand-in limitation carried), unpack-integrity `limitation:archive-not-supplied`, compiled-host-protocol `limitation` (musl loader `/lib/ld-musl-x86_64.so.1` absent on this host — CI-owned with REL-T4). Every verdict record carries the verbatim absence line `no PTY/render/synchronized-output/no-clear claims`; the schema-v1 validator structurally rejects any stronger claim for `DriverKind::QemuUserSmoke` (`validate.rs` QEMU rules) — no interaction claim is possible on either musl row.

**musl-arm64: filed local limitation** — no cross-execution facility on this host (no `qemu-user`, no `aarch64-linux-musl-gcc`; zigbuild can build but not run). Native CI witness on `ubuntu-24.04-arm` is the designed evidence path (REL-T4/REL-T5); QEMU substitution is barred for these rows by the arbitration ruling.

## 4. Retained-change classification index

Every retained change stayed within its ticket's classification (copy / presentation / measurement / decided-mechanism); no executed ticket landed outside its class.

| Ticket | Classifier | Landed | Surface class |
| --- | --- | --- | --- |
| TUI-P1 #67 | harness | `76cb01f`,`678c878`,`e899472` | testkit + schema doc |
| TUI-P2 #58 | prototype | `f8d8a50` | contrast oracle + report |
| TUI-P3 #70 | fixture | `afdcca7`,`097c0bf` | ext-gauntlet corpus |
| TUI-P4 #84 | prototype | `cdfb4b0` (also `7a981a7`,`a7973e6`) | static-frame evidence |
| TUI-R1 #66 | audit | `9826475` | palette admissions (code: five lightness roles) |
| TUI-R2 #62 | survey | `b90362d` | doc + frozen-topology pin |
| TUI-T1 #80 | PASS — presentation truthfulness | `a7ca859`,`0e60c24` | hints derived from keybinding registry |
| TUI-T2 #74 | PASS — existing-token selection | `04f91de`,`e747706`,`dc9d412` | capability-driven depth |
| TUI-T3 #73 | PASS — presentation | `583b2f5`,`701bdd3` | OSC 8 hyperlinks (+critical wire fix) |
| TUI-T5 #68 | PASS — copy | inside `e747706` (mixed landing; STYLE_LEDGER rows 2-5,8 witness the copy sites) | error-copy taxonomy |
| TUI-T6 #57 | PASS — copy | `fb2ef77`,`3afeb84` | selector consequences/empty states |
| TUI-T8 #64 | PASS — presentation | `9c330c6` + witness `48696b5` | truncation glyph unification + ctrl+o recovery |
| TUI-T10 #75 | PASS — copy | `441a455`,`03812ae` | onboarding/consent copy |
| TUI-V1 #76 | measurement | `c0181a9` | state-matrix corpus + musl lane |
| TUI-V2 #77 | measurement | `54f573d`,`62f4edf`,`a38488a`,`93689ce` | keyboard gauntlet (+DSR responder — see §6.1) |
| TUI-V3 #81 | measurement (+2 bug fixes) | `1ae605c`,`8b9e8fa` | unicode gauntlet; hardware-cursor + `put_line` width fixes |
| TUI-V4 #87 | measurement | `51ecbba` → cherry-pick `20be789` | resize-storm/settle/progressive-disclosure tests |
| TUI-V5 #79 | measurement | `482d189` | theme/contrast numeric oracle |
| TUI-V6 #72 | measurement | `620c15a`,`c17d5df`,`4167c0e`,`e3db239` | a11y invariants + evidence |

## 5. Routed decision index (every Route-class item → its decision ticket)

| Routed item | Decision ticket | Decision | Status |
| --- | --- | --- | --- |
| TUI-T4 editor-border state #65 | TUI-G3 #40 (`25c250c`) | rail-only doctrine; consumption ratified | executed `a9e33ae` |
| TUI-T7 dead-token disposition #69 | TUI-G3 #40 | disposition list ratified | executed `cc5d6d8` |
| TUI-T9 narrow-width floor #83 | TUI-G8 #56 (`c80f7c0`) | refuse-and-blank below 20 columns | implemented `c527481` on `feat/tui-t9-floor-impl` (pushed) — **not yet integrated into the campaign line; see §6.2** |
| TUI-T11 reduced-motion #78 | TUI-G1 #49 (`14e3973`) | upstream no-preference-gate; static-frame programmatic seam | executed `f008886`, merged `4dc0900` |
| Confirm selection/Esc semantics | TUI-G7 #61 (`3a7d020`,`1fce102`) | settled dispatch policy | landed |
| Alt-screen / scroll-view / mouse / search-overlay / flash-confirm | TUI-G4 #35 (`fd36be9`) | deferred-by-design roadmap | recorded |
| Copy policy authority | TUI-G5 #50 → `docs/STYLE_LEDGER.md` (repair `1c99543`) | ledger-pinned copy | live |
| Color doctrine / hyperlinks / depth | TUI-G6 #63 (`4a3895c`,`442e214`,`e9bfed3`) | five rulings | live |
| Hardware cursor | TUI-G2 #53 → `docs/TUI-G2-hardware-cursor-policy.md` | first-class setting, default off | decided; parity indicator retained (see row 4, §1) |

## 6. Limitations and integration ledger

1. **transcript_fixture serve-mode close deadlock (fixed in this close).** First full-suite run since `54f573d` (tui-v2 DSR responder) deadlocked all three serve-mode scenarios. Root cause: `SessionIo::close_writer()` drops only the session's `SharedWriter` Arc handle; the reader pump's DSR-responder thread holds the second Arc, so the inner `portable-pty` `UnixMasterWriter` never drops, its `\n`+VEOT master-EOF stand-in (`portable-pty-0.9.0/src/unix.rs:393-403`) never reaches the serve-mode child (which waits for the Ctrl+D terminator), `child.wait()` blocks, and `join_readers()` — which would release the pump's handle — runs only after the wait: structural deadlock. Fix: `crates/pi-tui/src/testkit/posix.rs` `close()` now emits the stand-in explicitly before `close_writer()` (own commit; approved by Main; disjoint from all sibling-owned files). Pre-fix reproduction: fixture silent in `epoll_wait`, harness blocked in `waitpid`, slave winsize already at the final ladder step 1x1 — i.e. the ladder had completed and only the close hung.
2. **TUI-T9 floor implementation not on the campaign line.** `c527481` (and first attempt `75e2dab`) live on pushed branches only; `VIEWPORT_WIDTH_FLOOR` is absent from `20be789`. TUI-V4's merged `sub20_resize_storm_coalesces_with_zero_clear_bytes` (`runtime.rs:11621`) asserts sub-20 storms commit non-empty reanchor bytes, which may conflict with refuse-and-blank's empty-buffer render (`floor_blanks_render_below_20` asserts blank at 10 cols). Merge reconciliation (cherry-pick + sub20-vs-blank test matrix) is Main-owned; flagged as a **pending REL-T4 wiring blocker** — do not wire the five-runner gate before this lands.
3. **Tier-N five-runner CI evidence: PENDING** (schema doc §9) — lands with REL-T4 #108, which this close unblocks.
4. **Manual screen-reader sign-off:** flagged degraded-verdict limitation row (`docs/TUI-V6-a11y-evidence.md`) — headless host; script + criteria pinned for desktop-equipped execution.
5. **Musl compiled-host loader + musl-arm64 local execution:** §3 — CI-owned.
6. **Default-palette semantic-text contrast (§1.1):** routed to a new decision ticket; not closeable inside the polish track.
7. **Intermittent resize-ladder k=3 divergence (pre-existing, routed).** Across post-fix runs of `transcript_fixture` (four green, one failed) the corpus intermittently reports `resize-ladder: run-to-run divergence at seq 1` (two distinct sha256 digests, e.g. `64272d27…` vs `34be1214…`). Observed independent of the §6.1 fix (the divergence is in emitted canonical bytes; the fix touches only close-time I/O after artifact capture, and one pre-fix run escaped the deadlock and also passed only once). This nondeterminism must be diagnosed before REL-T4 wires the corpus into the release gate (a flaky gate would flake CI); flagged to Main. All other corpora were deterministic in every run here.

## 7. Close record

Issue #17 is already CLOSED (definition ratified by the arbitration ruling); its sub-issue graph completes 32/32 with this ticket's closure. The consolidated record is posted to #82 (SHA + evidence) and cross-linked on #17.
