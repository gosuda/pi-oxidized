# Terminal Width-Table Divergence Survey — TUI-R2 Decision Record (#62)

Status: Recorded (research survey; no source changes by this task)
Issue: [TUI-R2 #62](https://github.com/metaphorics/pi-oxidized/issues/62)
Stable ID: `TUI-R2`
Follow-on owners: TUI-V3 (#81) — Unicode/width gauntlet built from the probe corpus; TUI-P5 — limitations-table owner (stable-ID reservation; issue pending at record time).
Blocked by: None.

External evidence resolved: 2026-08-26 (terminal versions from each project's release channel; width behavior from each project's primary source).

## 1. Decision

The width contract in `crates/pi-tui/src/text/width.rs` stays **terminal-agnostic**: it is the single width oracle for rails, table borders, truncation, and cursor accounting, and it is not branched per terminal. Of the six parity-gate terminals (kitty, iTerm2, WezTerm, Terminal.app, Windows Terminal, alacritty), only kitty matches the contract on all four axes; iTerm2, WezTerm, alacritty, and Windows Terminal diverge on the emoji axis (ledger D-1..D-6), and Terminal.app is provisional on every axis pending the darwin spot-check. Every divergence becomes a **documented parity-gated terminal list entry plus a TUI-P5 limitations-table row** — never a per-terminal special case, and never a runtime escape-sequence patch: pi must not emit `OSC 1337 ; UnicodeVersion=N` (or any other per-terminal mode escape) to coerce a host width table.

This record is the only deliverable of TUI-R2: a survey, a probe corpus, a verdict matrix, and routing. No `.rs`, protocol, verification-script, or ledger file changes.

## 2. The width.rs contract under survey

Contract owner: `grapheme_width` / `visible_width` / `normalize_terminal_output` in `crates/pi-tui/src/text/width.rs`, pinned to `unicode-width = "=0.2.2"` (`crates/pi-tui/Cargo.toml:25`). The module doc (width.rs:1-5) declares parity with the upstream TypeScript helpers `graphemeWidth` / `visibleWidth` (`.references/pi/packages/tui/src/utils.ts:174-235`, `:240-295`) and `normalizeTerminalOutput` (utils.ts:379-384) — those TS helpers are the parity target this survey measures against, via the Rust contract's identical rules.

### 2.1 The four named axes

| Axis | Contract rule | Source anchor (width.rs) |
| --- | --- | --- |
| **A** — ambiguous | East-Asian Ambiguous = 1 (narrow); `east_asian_width` is `c.width().unwrap_or(0)` with no ambiguous-widening | `east_asian_width` 74-76 |
| **B** — wide | East-Asian Wide/Fullwidth = 2 (Han, kana, fullwidth forms) | `east_asian_width` 74-76; consumed at 144 |
| **C** — ZWJ / VS16 / RI forced 2 | RGI-style emoji grapheme = 2 when `could_be_emoji` passes and `is_rgi_emojiish` fires: `segment.width() == 2` plus forced 2 for any cluster containing ZWJ (U+200D), VS16 (U+FE0F), or skin-tone modifiers (U+1F3FB..U+1F3FF). Regional indicators U+1F1E6..U+1F1FF = 2 **including a singleton RI** | `could_be_emoji` 94-106; `is_rgi_emojiish` 108-120; emoji return at 131-133; RI singleton return at 140-142 (mirrors utils.ts:201-206) |
| **D** — Thai/Lao AM +1 | Thai SARA AM (U+0E33) and Lao SARA AM (U+0EB3) add 1 column inside a cluster (base ท = 1, so ทำ = 2) | `grapheme_width` 150-152 (mirrors utils.ts:227-229); `normalize_terminal_output` splits ำ → U+0E4D U+0E32 and ຳ → U+0ECD U+0EB2 at 212-222 |

### 2.2 Surrounding contract rows (surveyed, not axis-gated)

| Row | Contract rule | Source anchor (width.rs) |
| --- | --- | --- |
| Tab | `\t` = 3; `visible_width` expands tabs to three spaces; `normalize_terminal_output` expands standalone tabs outside escapes to three spaces | `grapheme_width` 125-127; `visible_width` 174-176; `normalize_terminal_output` 236-238 |
| Combining / control / zero-width | 0 columns (combining marks, control chars `width() == None`, ZWSP/ZWNJ-only segments) | `is_zero_width_char` 78-80; `is_zero_width_segment` 82-84; zero return at 128-130 |
| Halfwidth/fullwidth forms | Trailing U+FF00..U+FFEF code points inside a grapheme add their `east_asian_width` (halfwidth katakana 1, fullwidth latin 2) | `grapheme_width` 148-149 (mirrors utils.ts:224-226) |
| ANSI/OSC/APC | Escape sequences contribute 0; stripped before measurement | `visible_width` 177-192 |

## 3. The 13-probe spot-check corpus

One row per probe; "Contract" is the column count `visible_width` must produce and a conforming terminal must allocate. This corpus is the seed of the TUI-V3 gauntlet (#81) and the unit of binary verdicts under the protocol in §6.

| # | Probe | Input | Contract cols | Axis |
| --- | --- | --- | --- | --- |
| P1 | ascii-baseline | `OK` (U+004F U+004B) | 2 | — |
| P2 | tab-single | `\t` | 3 | surrounding |
| P3 | eaw-ambiguous | `°±■` (U+00B0 U+00B1 U+25A0) | 3 | A |
| P4 | cjk-wide | `漢字` (U+6F22 U+5B57) | 4 | B |
| P5 | halfwidth-forms | `ｱﾊ` (U+FF71 U+FF8F) | 2 | B |
| P6 | fullwidth-forms | `Ａ！` (U+FF21 U+FF01) | 4 | B |
| P7 | combining-acute | `e` + U+0301 | 1 | surrounding |
| P8 | zero-width | U+200B (ZWSP) | 0 | surrounding |
| P9 | vs16-emoji | U+2764 U+FE0F (❤️) | 2 | C |
| P10 | zwj-family | U+1F468 U+200D U+1F469 U+200D U+1F467 U+200D U+1F466 (family) | 2 | C |
| P11 | ri-pair | U+1F1FA U+1F1F8 | 2 | C |
| P12 | ri-singleton | U+1F1FA | 2 | C |
| P13 | thai-lao-am | `ทำທຳ` (U+0E17 U+0E33 U+0E97 U+0EB3) | 4 (2 + 2) | D |

Derivations: P3/P4/P5/P6 follow `east_asian_width` per code point; P7/P8 are zero-width segments; P9/P10 hit the `is_rgi_emojiish` forced-2 rules (FE0F contained; ZWJ contained); P11/P12 hit the U+1F1E6..U+1F1FF return at width.rs:140-142 (grapheme cluster or singleton); P13 = ท (1) + ำ (+1) and ທ (1) + ຳ (+1).

## 4. Six-terminal survey (version + primary-source evidence)

### 4.1 kitty — v0.48.2 (current stable line, late 2026-08)

- Width tables are generated from Unicode 17 data by `gen/wcwidth.py` (kitty has shipped Unicode 17 tables since 0.44; changelog). Ambiguous defaults narrow → axis A matches.
- `wcswidth.c` implements the emoji-presentation upgrade: VS16 sequences (P9) resolve to 2 columns, ZWJ clusters (P10) are measured as one emoji cell (2), and regional indicators are wide — RI pair (P11) 2 and singleton RI (P12) 2.
- Thai/Lao: U+0E33/U+0EB3 carry the generated table's default width of 1 column each, so the P13 clusters total 1 + 1 = 2 columns — axis D matches.
- **All four axes MATCH.**

### 4.2 iTerm2 — 3.6.11 (stable, 2026-06-02)

- `iTermCharacterWidth.c` carries generated tables with paired Unicode-8 / Unicode-9 width columns; the active column is selected by the per-profile Unicode version, whose default is 9 (`iTermProfilePreferences.m`).
- `DefaultBookmark.plist` ships `"Ambiguous Double Width" = false` → ambiguous = 1 → axis A matches. Wide/fullwidth = 2 on the U9 column → axis B matches.
- VS16: `ScreenChar.m` upgrades a base + U+FE0F pair to a double-width cell → P9 = 2. RI pair renders as the flag glyph at 2 columns (P11 = 2).
- **No ZWJ merge**: each constituent of a family sequence is measured separately at 2 columns → P10 = **8 columns** vs contract 2 → axis C diverges (D-1). Singleton RI (P12) is measured by the table as 1 in ambiguous-off configs and stays protocol-pending; the axis verdict is already D from P10.
- Thai/Lao AM cluster allocates 1 + 1 → axis D matches.
- **Axes A/B/D MATCH; axis C DIVERGES (D-1).**

### 4.3 WezTerm — rolling release (no version number exists to pin)

- Documentation (`unicode_version` config): default is **Unicode 9**, chosen for cross-terminal consistency; **emoji/text presentation selectors (VS15/VS16) influence width only when `unicode_version >= 14`**.
- At the default config, P9 (❤️) measures **1 column** vs contract 2 → axis C diverges at default config (D-2). With `unicode_version = 14+` it measures 2 — a config axis, so the spot-check captures both settings (§6.2).
- Ambiguous widening is an opt-in option, default off → axis A matches. ZWJ family, RI pair, wide, and Thai/Lao AM all match (WezTerm clusters emoji sequences).
- **Axes A/B/D MATCH; axis C DIVERGES at default config (D-2).**

### 4.4 Terminal.app — closed source

- No public width-table source exists; every verdict would be inference. **All rows stay P (provisional)** until the darwin spot-check (§6) produces schema-v1 captures on the `darwin-x64` / `darwin-arm64` rows. Secondary reports (ambiguous narrow default; family emoji split into 8 columns) are recorded as unconfirmed context only and seed no routing.

### 4.5 Windows Terminal — v1.24.11911.0 (stable 1.24 servicing line; 1.24 stable since 2026-03-06)

- `CodepointWidthDetector` (measurement mode defaults to **Graphemes**): cluster-level measurement, `_ambiguousWidth = 1` → axis A matches.
- Explicit width lookup gives U+FE0F width 2 → P9 = 2; the ZWJ family cluster measures 2 (P10); RI pair 2 (P11). The width table's only per-codepoint override is U+FE0F, and regional indicators carry their UAX#11 Neutral width of 1, so a singleton RI (P12) measures **1 column** vs contract 2 → axis C diverges (D-6); the §6 capture graduates this cell.
- Wide/fullwidth = 2; Thai/Lao AM cluster allocates 2 columns → axes B/D match.
- **Axes A/B/D MATCH; axis C DIVERGES (D-6).**

### 4.6 alacritty — v0.17.0 (stable, 2026-04-06)

- Width is per-code-point `UnicodeWidthChar::width` from the `unicode-width` release resolved in `Cargo.lock`; there is no grapheme-cluster summation.
- Ambiguous = 1 and wide/fullwidth = 2 come from the same East-Asian data family as this crate's contract → axes A/B match. Thai (U+0E33) and Lao (U+0EB3) are East-Asian Neutral = 1 per code point, so a P13 cluster allocates 1 + 1 = 2 → axis D matches.
- **Axis C diverges on three sub-probes**: P10 family = 4 × 2 = **8 columns** (D-3); P9 VS16 = base 1 + FE0F 0 = **1 column** (D-4); P12 singleton RI = **1 column** (D-5). The RI pair (P11) does sum to 2 and matches.
- **Axes A/B/D MATCH; axis C DIVERGES (D-3, D-4, D-5).**

## 5. Verdict matrix and divergence ledger

### 5.1 Six-terminal × four-axis binary verdicts

M = match (observed columns equal contract columns), D = diverge, P = provisional (not yet captured; closed source or pending protocol run). One cell per axis; sub-probe outcomes are in §4.

| Terminal | A ambiguous=1 | B wide=2 | C ZWJ/VS16/RI=2 | D AM +1 |
| --- | --- | --- | --- | --- |
| kitty v0.48.2 | M | M | M | M |
| iTerm2 3.6.11 | M | M | **D** (D-1) | M |
| WezTerm (rolling) | M | M | **D** at default config (D-2) | M |
| Terminal.app | P | P | P | P |
| Windows Terminal v1.24.11911.0 | M | M | **D** (D-6) | M |
| alacritty v0.17.0 | M | M | **D** (D-3, D-4, D-5) | M |

### 5.2 Divergence ledger D-1..D-6

Every row routes to exactly the parity-gated terminal list plus a TUI-P5 limitations-table entry. None routes to a width.rs change, a per-terminal branch, a new `CapabilityProfile` variant, or an emitted escape.

| ID | Divergence (observed vs contract) | Terminals | Probe | Disposition |
| --- | --- | --- | --- | --- |
| D-1 | ZWJ family = 8 cols vs 2 (no ZWJ merge) | iTerm2 3.6.11 | P10 | Parity-gated terminal list + TUI-P5 entry |
| D-2 | VS16 emoji = 1 col vs 2 at default `unicode_version` 9 (presentation selectors honored only ≥ 14) | WezTerm (default config) | P9 | Parity-gated terminal list + TUI-P5 entry; documented user-side remedy (`unicode_version = 14`); pi never emits `OSC 1337 ; UnicodeVersion=N` |
| D-3 | ZWJ family = 8 cols vs 2 (per-codepoint sum, no cluster) | alacritty v0.17.0 | P10 | Parity-gated terminal list + TUI-P5 entry |
| D-4 | VS16 emoji = 1 col vs 2 (FE0F width 0 per-codepoint) | alacritty v0.17.0 | P9 | Parity-gated terminal list + TUI-P5 entry |
| D-5 | Singleton RI = 1 col vs 2 | alacritty v0.17.0 | P12 | Parity-gated terminal list + TUI-P5 entry |
| D-6 | Singleton RI = 1 col vs 2 (no RI override; only U+FE0F is forced wide) | Windows Terminal v1.24.11911.0 | P12 | Parity-gated terminal list + TUI-P5 entry |
**OSC 1337 prohibition (explicit).** WezTerm and iTerm2 accept a `UnicodeVersion` escape (`OSC 1337`). Emitting it to realign a host width table would be a per-terminal code special case mutating user configuration out of band. pi and pi-tui must never emit it; divergences are documented, not patched.

**Terminal.app P rows are not ledger entries.** A P cell is unresolved, not divergent; it graduates to M or D only through §6 captures.

## 6. Manual emulator spot-check protocol (binding)

This is the named evidence driver for the matrix in §5.1 and for TUI-V3's gauntlet verdicts. It binds how every cell above was or will be resolved.

1. **Named environment.** Each run records terminal name, exact version, and host platform, drawn from the frozen Tier N row set (§7.3): darwin rows host Terminal.app / iTerm2 checks; the windows row hosts Windows Terminal; gnu rows host kitty / WezTerm / alacritty.
2. **Named config.** Default configuration, except where a divergence names a config axis — D-2 requires capturing WezTerm at both `unicode_version = 9` (default) and `= 14`. iTerm2 is captured with the shipped default profile (`Ambiguous Double Width = false`, Unicode version 9).
3. **Scripted scenario.** Every probe from §3 is emitted through the same deterministic render path (no interactive typing), settle-bounded, so runs are repeatable.
4. **Schema-v1 capture, k ≥ 3.** Each scenario records a `TerminalSnapshot` (`crates/pi-tui/src/testkit/driver.rs:115`) inside a schema-v1 `TranscriptArtifact` (`pi-tui-transcript/1`, `crates/pi-tui/src/testkit/transcript.rs`), repeated k ≥ 3 via `run_k` (`crates/pi-tui/src/testkit/repeat.rs`) with digest-stable canonical bytes. Captures land under `target/verification/tui-transcripts/<row>/<scenario>/run-{1,2,3}/`.
5. **Binary per-scenario verdicts.** Per probe: M when observed columns equal the contract column count, D otherwise. No partial or averaged scores; every P cell and every M/D confirmation cell in §5.1 must eventually be backed by such captures.

## 7. Downstream routing

### 7.1 TUI-V3 (#81) — Unicode and width gauntlet

TUI-V3 (blocked by TUI-P1 #67 and this survey) builds its gauntlet scenarios from the 13-probe corpus in §3 across editor, assistant markdown, overlays, and paste-atomic segments. Every P cell and every M/D confirmation cell in §5.1 must become binary schema-v1 `TerminalSnapshot` captures under §6 before the gauntlet is considered evidenced; rails and table borders stay column-aligned and the cursor drift-free wherever the matrix is M.

### 7.2 TUI-P5 — limitations table

The limitations-table owner receives D-1..D-6 as rows: terminal, version, probe, contract columns, observed columns, disposition (documented divergence; no code action). Terminal.app rows are added only when P cells graduate.

### 7.3 Frozen five-row Tier N topology

The runner topology stays frozen at the five `RowId` rows — `gnu-x64`, `gnu-arm64`, `darwin-x64`, `darwin-arm64`, `windows-x64` (`docs/tui-transcript-schema-v1.md` §6). Divergence handling adds documentation and limitations rows only: never a runner row, never a `CapabilityProfile` variant (`crates/pi-tui/src/testkit/transcript.rs:127-142`), never a width.rs branch.

## 8. Scope

TUI-R2 changes no source. `width.rs` is the surveyed contract, not an edit target; the testkit, protocol, verification scripts, gate ledgers, and issue state are untouched. This record — contract, corpus, matrix, ledger, protocol, and routing — is the deliverable, and every verdict it asserts is confirmed or graduated only through the §6 protocol.
