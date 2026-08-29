# TUI-P2: Deterministic Contrast Measurement Prototype

- [#58](https://github.com/metaphorics/pi-oxidized/issues/58) (`TUI-P2`): prototype complete, acceptance evidenced

## Question

Resolve settled-frame fg/bg pairs from canonical schema-v1 snapshots
(built-in theme JSON, never timing-dependent captures) to RGB via the
pinned 256-palette table and report numeric WCAG ratios and ΔE2000
rung deltas on dark+light terminals in truecolor+forced-256, flagging
every pair below the pinned thresholds (4.5 / 3.0 / 1.3 / ΔE2000 2.3 +
ratio 1.25).

## Implementation

`prototype/tui-p2-contrast/` — standalone crate (empty `[workspace]`
table keeps it out of the shipped crate graph).

### Modules

| Module | Responsibility |
|--------|---------------|
| `color.rs` | sRGB → Lab, WCAG contrast ratio, CIEDE2000 ΔE — validated against Sharma et al. (2005) reference data |
| `palette.rs` | Pinned 256-color palette table (xterm standard: 16 ANSI + 6³ cube + 24 grayscale) and `rgb_to_256` downsampling (ported from `theme.rs`) |
| `theme.rs` | Canonical snapshot loading (compile-time `include_str!` of `dark.json`/`light.json`), fg/bg pair enumeration, threshold evaluation |
| `report.rs` | Per-pair measurement, rung-delta computation, text + JSON rendering |
| `main.rs` | CLI entry point (`--json` flag) |

### Canonical Snapshots

Colors are resolved from the built-in theme JSON files
(`crates/pi/assets/theme/dark.json`, `light.json`) embedded at compile
time via `include_str!`. These are deterministic schema-v1 sources —
never timing-dependent PTY captures.

### Inspected Pairs

56 fg/bg pairs per theme × color-mode combination, covering:
- Core text (text, dim, muted, thinkingText, toolOutput)
- Markdown (quote, link, link-suffix, heading, code, code-block, list-bullet, hr)
- Diff (added, removed, context)
- Syntax (comment, keyword, function, variable, string, number, type, operator, punctuation)
- Border/rail hues (border, borderMuted, borderAccent, quote-border, code-block-border, thinking tiers, bash-mode)
- Accent/status (accent, success, error, warning, tool-title)
- Contextual fg-on-theme-bg (user-message, custom-message, tool-pending/success/error, selected, diff-on-bg)

### Color Modes

Each pair is measured in both:
- **Truecolor** — 24-bit RGB as specified in the theme JSON
- **Forced-256** — RGB downsampled to the nearest 256-color palette index via `rgb_to_256`

### Rung Deltas

For each pair, the perceptual distance (ΔE2000) between the truecolor
and forced-256 rendering is computed for both fg and bg, quantifying
information loss from palette downsampling.

## Pinned Thresholds

| Threshold | Value | Meaning |
|-----------|-------|---------|
| WCAG AA normal | 4.5 | Minimum contrast for normal-size text |
| WCAG AA large | 3.0 | Minimum contrast for large text (18pt+ / 14pt bold) |
| WCAG minimum | 1.3 | Absolute floor below which text is unreadable |
| ΔE2000 + ratio | 2.3 + 1.25 | Perceptual indistinctness: colors too similar AND contrast too low |

## Results Summary

224 total pair measurements (56 pairs × 2 themes × 2 color modes):

| Metric | Count |
|--------|-------|
| Below WCAG AA normal (4.5) | 32 |
| Below WCAG AA large (3.0) | 22 |
| Below WCAG minimum (1.3) | 6 |
| Below ΔE2000+ratio (2.3+1.25) | 0 |
| **Total flagged** | **32** |

### Key Findings

1. **Border/rail hues** consistently fail WCAG AA on both dark and light
   themes. `border`, `borderMuted`, `mdQuoteBorder`, `mdCodeBlockBorder`
   are low-contrast against the default background. This is expected:
   borders are decorative, not text-critical.

2. **`mdHr`** (horizontal rule) fails WCAG AA on both themes, also
   decorative.

3. **`toolDiffRemoved` on `toolErrorBg`** fails WCAG AA normal (4.42)
   on dark/truecolor only; forced-256 snaps it above 4.5. Red diff text
   on dark-red background is the only contextual pair that flags.

4. **No pairs fail the ΔE2000+ratio threshold**, meaning no fg/bg pairs
   are perceptually indistinct.

5. **Forced-256 downsampling** generally improves WCAG ratios slightly
   (palette snapping can increase luminance separation) but introduces
   perceptual color shifts (rung deltas up to ~11 ΔE for some syntax
   colors).

6. **Light theme flags more pairs** (10/56) than dark (6/56) per color
   mode, driven by `dim` (#767676) and `syntaxComment` (#8f8f8f)
   falling below 4.5 on white.

## Verification
- 13 unit tests pass (color science: WCAG ratio with mid-luminance guards,
  CIEDE2000 Sharma reference values; palette: cube/grayscale indexing,
  downsampling)
- Text report: `prototype/tui-p2-contrast/report.txt`
- JSON report: `prototype/tui-p2-contrast/report.json`

## Run

```sh
cargo run --manifest-path prototype/tui-p2-contrast/Cargo.toml
cargo run --manifest-path prototype/tui-p2-contrast/Cargo.toml -- --json
```
