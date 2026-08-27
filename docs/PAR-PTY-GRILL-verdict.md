# PAR-PTY-GRILL Verdict (issue #46)

Adjudication of landed T1–T3/T9 runtime claims with real host-tier PTY evidence.

Date: 2026-08-27
Host: Linux 7.0.0-30-generic (x86_64)
Fixture: `pi_tui_pty_fixture` under `portable-pty` with `avt` byte-stream parser

## Rulings

### T1: Differential rendering engine — VERIFIED

**Evidence**: `grill_t1_differential_rendering_no_clears_continuous_content`

Host-tier PTY evidence proves:
- Zero full-screen clears (CSI 2J and CSI 3J counts are 0) across 12 resizes
- Row-local erase (CSI 2K) is followed immediately by reflowed content in the same transaction
- Content remains continuous across all resizes (STATUS/STREAM/FOOTER always visible)
- No intermediate blank frames
- Settle `insert_before` + redraw share one serialized write

**Ruling**: verified. The frame buffer diff engine renders via per-cell diffs under real PTY conditions.

### T2: Terminal state management — VERIFIED

**Evidence**: `grill_t2_terminal_state_probes_before_sync_kitty_flag_emergency_restore`, `grill_t2_kitty_keyboard_flag_and_key_matching`

Host-tier PTY evidence proves:
- Probe query batch (DA1, cursor position, OSC 11, Kitty keyboard disable) is emitted on the wire before any synchronized output
- Synchronized output markers (CSI ?2026 h/l) are balanced
- Terminal restoration bytes (cursor show or emergency restore) present on exit
- Kitty keyboard protocol flag toggles correctly (unit-level)
- Structured key matching works on every host; legacy `modifyOtherKeys` omission documented

**Ruling**: verified. Raw mode, alternate screen, ANSI, OSC, and keyboard protocols share one owner with correct probe-before-sync ordering.

### T3: Terminal image rendering — VERIFIED (unit-level + negative PTY witness)

**Evidence**: `grill_t3_kitty_graphics_encoder`, `grill_t3_iterm2_encoder`, `grill_t3_image_fallback`, `grill_t3_no_raw_image_bytes_on_pty_wire`

Unit-level evidence proves:
- Kitty graphics encoder produces correct `ESC _Ga=T,f=100,q=2` sequences with chunked `m=1`/`m=0` for large payloads
- iTerm2 encoder produces correct `OSC 1337;File=` sequences with `inline=1`
- Image fallback produces text descriptions when no graphics protocol is available

Host-tier PTY negative witness proves:
- The fixture never emits raw Kitty (`ESC _G`) or iTerm2 (`OSC 1337;File=`) image sequences on the PTY wire — image bytes flow through frame annotations, not direct terminal writes

**Ruling**: verified. Kitty, iTerm2, and fallback selection stay behind terminal capabilities; no raw image bytes leak to the terminal.

### T9: Terminal interfaces — VERIFIED

**Evidence**: `grill_t9_terminal_interfaces_sole_stdout_owner`

Host-tier PTY evidence proves:
- The fixture is the sole stdout owner after probes (probe batch present, no clears, balanced sync, transactions exist)
- Transaction markers present (stage-3 writes are instrumented)
- Probe query batch emitted through the terminal interface
- Final VT text contains rendered content (STATUS/FOOTER/DONE)

**Ruling**: verified. Callers never emit terminal escape bytes directly; all output flows through the Tui stage-3 writer.

### OSC52: Clipboard encoder — VERIFIED (unit-level)

**Evidence**: `crates/pi/tests/pty_grill_osc52.rs::grill_osc52_encoder_correct_sequence`, `grill_osc52_rejects_oversized`

Unit-level evidence proves:
- OSC 52 encoder produces correct `ESC ]52;c;<base64> BEL` sequences
- Oversized payloads (>100,000 base64 chars) are rejected

The PTY fixture does not exercise clipboard actions; OSC 52 is not emitted on the PTY wire.

**Ruling**: verified. Generic OSC52 emission belongs to pi-tui terminal capabilities (C13 planned); the encoder is correct and bounded.

### T4: LaTeX math rendering — VERIFIED (re-adjudicated under PAR-CLOSE)

**Evidence**: `grill_t4_math_rendering_landed` (re-adjudicated; originally `grill_t4_math_rendering_unverified_gap`)

The test confirms two gaps:
1. `ENABLE_MATH` is not enabled in the pulldown-cmark parser options (markdown.rs:351-354), so `$...$` and `$$...$$` are treated as literal text, not as InlineMath/DisplayMath events
2. Even if `ENABLE_MATH` were enabled, `InlineMath` and `DisplayMath` events are silently dropped (`=> {}`) in the consume method (markdown.rs:458-459)

The raw-literal fallback path described in `docs/PAR-MATH-latex-strategy.md` is not implemented. Math markdown renders as literal text with visible dollar signs and LaTeX commands.

**Original ruling (2026-08-26)**: unverified. The math rendering path was not implemented despite PAR-MATH (issue #37) being closed with evidence that existed in no repository ref.

**Re-adjudication (PAR-CLOSE, #39)**: verified. The engine re-landed as T4 stage 1 (`text/latex.rs`, commit 0c27a40) and the markdown math-path integration landed as stage 2 under PAR-CLOSE: the pre-parse `preprocess_math` pass implements the upstream marked-extension delimiter contract (block `$$…$$`/`\[…]`, inline `$$…$$`/`\(…\)`/single `$…$`, four rejection rules, code-span/fence exclusion, escaped-dollar handling, `MarkdownOptions.render_latex` gate). The original gap mechanism (no ENABLE_MATH; dropped InlineMath/DisplayMath events) is intentionally superseded by the preprocessing approach from the settled strategy — the events remain dropped because math never reaches pulldown-cmark. The witness now asserts math renders (no surviving delimiters, superscript and summation present) and unsupported input falls back to raw source.

## Summary

| Claim | Ruling | Evidence |
| --- | --- | --- |
| T1 Differential rendering | verified | PTY: no clears, row-local erase + reflow, continuous content |
| T2 Terminal state | verified | PTY: probes before sync, balanced sync, restoration on exit; unit: kitty flag, key matching |
| T3 Image rendering | verified | Unit: Kitty/iTerm2 encoders, fallback; PTY: no raw image bytes on wire |
| T9 Terminal interfaces | verified | PTY: sole stdout owner, transaction markers, probe emission |
| OSC52 Clipboard | verified | Unit: correct encoding, oversized rejection |
| T4 Math rendering | verified (re-adjudicated) | Test: `grill_t4_math_rendering_landed` — math renders to Unicode, unsupported falls back to raw |
