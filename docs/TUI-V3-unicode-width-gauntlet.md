# TUI-V3: Unicode and width gauntlet on real terminals

Stable ID: `TUI-V3` (Issue #81).
Blocked by: TUI-P1 (#67, closed), TUI-R2 (#62, closed).

## Question

CJK, ZWJ emoji (family), regional indicators, variation-selector emoji, combining accents, and Thai/Lao AM vowels in editor, assistant markdown, overlays, and paste-atomic segments keep rails and table borders column-aligned and the cursor drift-free across the parity-gate terminals, evidence pinned by the manual emulator spot-check protocol with binary per-scenario verdicts.

## Corpus

The 13-probe width corpus from the TUI-R2 survey (`docs/TUI-R2-terminal-width-table-divergence.md` §3):

| Probe | Input | Contract width | Category |
|---|---|---|---|
| P01 | `OK` | 2 | ASCII baseline |
| P02 | `\t` | 3 | Raw tab (normalised to 3 spaces) |
| P03 | `°±■` | 3 | Latin-1 + geometric |
| P04 | `漢字` | 4 | CJK ideographs |
| P05 | `ｱﾏ` | 2 | Half-width katakana |
| P06 | `Ａ！` | 4 | Full-width Latin + punctuation |
| P07 | `e\u{301}` | 1 | Combining accent |
| P08 | `\u{200b}` | 0 | Zero-width space |
| P09 | `\u{2764}\u{fe0f}` | 2 | Variation-selector emoji |
| P10 | `\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}` | 2 | ZWJ family emoji |
| P11 | `\u{1f1fa}\u{1f1f8}` | 2 | Regional indicator pair (flag) |
| P12 | `\u{1f1fa}` | 2 | Regional indicator singleton |
| P13 | `\u{e17}\u{e33}\u{e97}\u{eb3}` | 4 | Thai/Lao AM vowels (normalised) |

## Surfaces

Each probe is rendered through five surfaces:

1. **Railed** — real `Rail` + `paint_lines` rows, closing `│` sentinel per row at the contract-computed column.
2. **Markdown table** (3 chunks) — real `Markdown` tables with probe cells; grid borders are the alignment oracle. P02 (tab) is excluded — GFM table parsing consumes tabs as cell separators.
3. **Editor cursor** — focused `Input`; cursor parked directly after each probe (hardware cursor oracle via `PI_HARDWARE_CURSOR=1`).
4. **Overlay** — production `write_overlay_cells` compositing probe rows over base rows with a fixed base sentinel beyond the overlay region.
5. **Paste-atomic** — real multiline `Editor` paste events: verbatim atomic multi-line paste, whole-paste undo (atomicity self-check), and the large-paste `[paste #N +N lines]` marker.

## Oracle

The AVT emulator (avt 0.18) serves as the oracle terminal. Snapshot lines are walked with per-character `avt_char_width` (Double iff `unicode-width` reports width 2, otherwise Single) to recover terminal cell columns. The contract width comes from `pi_tui::text::visible_width`.

## Verdict protocol

Per-probe binary M/D (match/diverge) verdicts are recorded in `verdict.json` at `target/verification/tui-transcripts/<row>/unicode-gauntlet/verdict.json`. Divergences between the contract width table and AVT's `char_display_width` are recorded as `diverge` — they are the gauntlet's subject, not a test failure. Probes not visible in the current scroll window are recorded as `not-visible`.

## Repeatability

k=3 byte-identical canonical digest via `run_scenario_k` (same gate as the state-matrix corpus).

## Bug fixes landed

Two pre-existing bugs were found and fixed during gauntlet development:

1. **Hardware cursor never emitted** (`crates/pi-tui/src/terminal/writer.rs`): `commit_frame` read `annotations.borrow().cursor()` inside the `with_annotations` closure, but the thread-local annotations had not yet been committed to the `RefCell`. Fixed by reading from `with_current_annotations` (the thread-local) instead.

2. **`put_line` advanced by character count, not display width** (`crates/pi-tui/src/bin/pi_tui_unicode_gauntlet_fixture.rs`): wide characters (CJK, emoji) overwrote their own trailing cells because the column advanced by 1 per character instead of per display-width. Fixed by walking graphemes with `visible_width` and setting wide-char trailing-half cells.

## Evidence

- **Local gnu-x64**: `cargo test -p pi-tui --features testkit --test transcript_unicode_gauntlet` — green, k=3, per-probe verdicts recorded.
- **Tier N**: rides the same corpus via `PI_TUI_TIER_ROW`; pending Tier-N CI evidence.
