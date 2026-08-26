# TUI-G6: color doctrine for capability depth, hyperlinks, and extension palettes (decision record)

| Field | Value |
|---|---|
| Issue | [#63][issue-63] `TUI-G6` (routed decision: presentation and color policy) |
| Decision type | Recorded decision, not implementation |
| Status | RATIFIED (four pins settled; decision-only record) |
| Deliverable | This document and the matching commit, and only these |
| Remediation owners | [TUI-T2 #74][issue-74] (color depth), [TUI-T3 #73][issue-73] (hyperlinks) |
| Verification | [TUI-P2 #58][issue-58], [TUI-V5 #79][issue-79] |

[issue-63]: https://github.com/metaphorics/pi-oxidized/issues/63
[issue-74]: https://github.com/metaphorics/pi-oxidized/issues/74
[issue-73]: https://github.com/metaphorics/pi-oxidized/issues/73
[issue-58]: https://github.com/metaphorics/pi-oxidized/issues/58
[issue-79]: https://github.com/metaphorics/pi-oxidized/issues/79

## Ruling summary

| Ruling | Verdict | Parity status | Owner | Verification |
|---|---|---|---|---|
| 1. Color depth (`caps.true_color`) | Fix: capability-driven `ColorMode` selection is the pinned end state | Current hardcoded `ColorMode::Truecolor` sites recorded as a standing divergence | TUI-T2 #74 | TUI-P2 #58, TUI-V5 #79 |
| 2. Hyperlinks (`caps.hyperlinks`) | Fix: honor the capability at markdown surfaces, URL-text fallback otherwise | Current hardcoded-off state recorded as a standing divergence | TUI-T3 #73 | TUI-V5 #79 |
| 3. Extension `setTheme` guardrails | Accept unchecked palettes | Exact reference parity, not a divergence | None (no code change) | TUI-V5 #79 measures, never gates |
| 4. ANSI-256 to RGB transform | Byte-lock the `55 + 40·v` closed form | Settled parity across all four converters | None; any change is a new recorded divergence | Byte equality of the four sites |

## 1. Scope, authority, and parity doctrine

This document is the single authority for the four color-domain questions raised by issue #63:
capability-driven color depth, hyperlink capability handling, extension `setTheme` palette
guardrails, and the ANSI-256 quantization rule used by HTML export and theme conversion.

Under the repository parity doctrine (issue #25), the TypeScript reference tree
(`.references/pi/…`) is canonical and every deviation is an explicit, recorded decision. Two of
the four rulings below fix a gap toward the reference and record the current Rust state as a
standing divergence pending its remediation ticket. One ruling accepts current behavior as exact
parity. One ruling locks an already-agreeing transform so it cannot drift. This record establishes
policy only and makes no code changes.

## 2. Ruling 1: capability-driven color depth is a fix, not a divergence

### Decision

`caps.true_color`-driven `ColorMode` selection is the pinned end state. The Rust engine must
derive the render depth from detected terminal capabilities exactly as the reference does. The
current Rust constants (`ColorMode::Truecolor` at every resolution site) are recorded as a
standing divergence until remediation lands.

### Reference witnesses

- `.references/pi/packages/coding-agent/src/modes/interactive/theme/theme.ts:630`: `createTheme`
  derives `const colorMode = mode ?? (getCapabilities().trueColor ? "truecolor" : "256color")`.
  Capability inference is the default; an explicit mode argument is the override.
- `theme.ts:663-670`: `loadTheme(name, mode?)` is modeless in practice. Registered themes return
  as-is and file-backed loads call `createTheme(themeJson, mode)` with the caller's mode, which
  public entry points such as `getThemeByName` never supply. Every default theme load therefore
  resolves through the capability check at line 630.

### Rust witnesses

- Engine tail hardcodes the depth. `crates/pi/src/modes/interactive/theme.rs:1676`, `:1679`, and
  `:1683` (inside `resolve_active_theme`'s load path) pass `ColorMode::Truecolor` to
  `load_or_dark` / `load_by_name` unconditionally.
- Every runtime resolution site funnels there:
  - `crates/pi/src/modes/interactive/runtime.rs:5083` (`startup_theme`) resolves the initial theme.
  - `runtime.rs:3806-3810` (`apply_theme_from_settings`) resolves on `/reload` and
    settings-driven changes, resolving at line 3809.
  - `runtime.rs:3852-3854` (`handle_extension_theme_set`, non-persist fallback) calls
    `load_or_dark(&name, ColorMode::Truecolor)` at line 3854.
  - `runtime.rs:3895` resolves after storage-driven reapply.
  - `runtime.rs:4018` resolves theme family selection.
  - `runtime.rs:5184` builds the `theme.update` catalog via
    `available_themes(ColorMode::Truecolor)`.
- Headless export pins the same depth outside the runtime:
  `crates/pi/src/core/export_html/mod.rs:258` (`built_in` uses
  `load_or_dark(name, ColorMode::Truecolor)`) and `:301` (`resolve_export_theme` resolves through
  `resolve_active_theme`). Export has no live terminal, so a fixed depth there is inherent to the
  surface; the divergence being recorded is the interactive path, where a terminal exists and is
  not asked.
- Capabilities are detected and carried but never consulted for depth. `runtime.rs:743` runs
  `TerminalCapabilities::detect()` inside `Options::detect`, and `runtime.rs:617-618` declares the
  `caps` field, yet its live consumers are background polarity (`runtime.rs:747`, `:6594`), kitty
  keyboard state (`runtime.rs:6595`), and the host capability push (`runtime.rs:6608`). No code
  path reads `caps.true_color`.

### Rationale

The reference derives render depth from the terminal, so parity requires the same derivation.
Hardcoding Truecolor pushes depth degradation onto host-side quantization on 256-color terminals
instead of the product's own palette resolution. Recording the current constants as a standing
divergence (rather than ratifying them) keeps the ledger honest and gives TUI-T2 a precise
before-state to replace.

### Rejected alternative

Record deliberate divergence (permanently keep `ColorMode::Truecolor` at every site). Rejected:
the reference contains no truecolor-only branch to mirror, so a permanent pin would be a new
product policy with no upstream precedent, and it would convert a temporary port constant into a
permanent compatibility regression on 256-color terminals.

### Owner

TUI-T2 [#74][issue-74] (classifier: PASS; existing-token selection at existing render sites).
Verification after implementation: TUI-P2 [#58][issue-58] and TUI-V5 [#79][issue-79]. This record
verifies nothing by itself.

## 3. Ruling 2: honor `caps.hyperlinks` at markdown surfaces

### Decision

Markdown render surfaces honor the detected `caps.hyperlinks` capability: OSC 8 hyperlink
encoding when the terminal advertises support, and the text-form fallback (URL printed in
parentheses when it differs from the link text) otherwise. The current hardcoded-off state is
recorded as a standing divergence until remediation lands.

### Reference witnesses

- `.references/pi/packages/tui/src/components/markdown.ts:692`: the inline link renderer gates
  OSC 8 emission on `getCapabilities().hyperlinks` at render time and falls back to printing the
  URL in parentheses (mailto prefix stripped for the equality check) on incapable terminals.
- `markdown.ts:220-229`: the reference `MarkdownOptions` interface exposes no hyperlink option.
  The capability check is environmental, never caller configuration.

### Rust witnesses

- The seam already exists and is caller-driven in `crates/pi-tui/src/components/markdown.rs`:
  - `:161-162` declares `pub hyperlinks: bool` under the documented contract "caller supplies
    capability".
  - `:217-220` provides `set_hyperlinks` (option write plus cache invalidation).
  - `:623-624` is the render-time gate mirroring `markdown.ts:692`, with the same URL-text
    fallback shape.
- The product hardcodes it off:
  - `crates/pi/src/modes/interactive/theme.rs:992-998` (`user_markdown_options`) sets
    `hyperlinks: false` at line 996.
  - `MarkdownOptions::default()` (derived `Default`, `markdown.rs:154-163`) leaves the flag
    `false`, and the product constructs it directly at `header.rs:38`,
    `messages.rs:206`, `:268`, `:473`, `:500`, `:527`, `:554`, and `startup.rs:142`, `:302`
    (all under `crates/pi/src/modes/interactive/`). `messages.rs:335` routes through
    `user_markdown_options`, which is equally off.

### Rationale

The reference emits OSC 8 links whenever the terminal advertises them, and the Rust component
seam was built for exactly that contract, so wiring is presentation-only at existing surfaces.
The fallback path already exists on both sides, so incapable terminals lose nothing.

### Rejected alternative

Keep hyperlinks permanently off (record divergence). Rejected: it drops a working affordance the
reference provides and the seam already supports, and there is no compatibility argument because
the incapable-terminal fallback is the specified behavior on those terminals.

### Owner

TUI-T3 [#73][issue-73] (classifier: PASS; presentation at existing surfaces).

## 4. Ruling 3: extension `setTheme` accepts unchecked palettes (exact parity)

### Decision

Extension `setTheme`, in both its name form and object form, accepts palettes without a
text-on-background warning at the 4.5 threshold and without an error/success hue-swap gate. This
is exact reference parity, not a divergence, and requires no code change.

### Reference witnesses

- `.references/pi/packages/coding-agent/src/modes/interactive/interactive-mode.ts:2467-2478`: the
  extension API `setTheme` either installs a `Theme` instance via `setThemeInstance` or resolves
  and persists a name. It inspects no palette content.
- `.references/pi/packages/coding-agent/src/modes/interactive/theme/theme-controller.ts:101-107`:
  `setThemeInstance` returns `{ success: true }` unconditionally; no validation runs.
- `theme.ts:892-922`: `setTheme` loads the named theme and falls back to `dark` on load failure.
  The only failure mode is a load/parse failure, never palette content.
- The reference tree contains no WCAG, contrast-ratio, or 4.5-threshold logic anywhere in the
  theme path.

### Rust witnesses

- `crates/pi/src/modes/interactive/runtime.rs:3838-3858` (`handle_extension_theme_set`): the
  object form goes through `resolved_theme_from_wire` (`runtime.rs:5153-5174`), which ignores
  unknown slots, resets missing slots, and performs no contrast or hue checks; the name form
  resolves through the engine and optionally persists. This matches the unchecked reference
  behavior.
- The wire type `ThemeSet` is protocol-owned at `crates/pi-ext/src/protocol.rs:1417` and is
  unchanged by this ruling.

### The 4.5 threshold stays a test-scoped, built-in-only oracle

`crates/pi/src/modes/interactive/theme.rs:2364-2411` (the `NATIVE_CONTRAST_PAIRS` admission
oracle), `:2591` (`contrast_ratio`), `:2652-2654` (`ciede2000`), and the `measured >= 4.5`
assertion at `:2824-2828` all live inside `mod tests` (`:1872`). The oracle constrains shipped
built-in palettes only. It never runs against extension input and must not be promoted into a
runtime gate.

### Rationale

Adding a guard would invent a product surface the reference lacks and would reject or warn on
palettes upstream accepts. The extension already holds authority over its own presentation, and
the reference's safety net (load-failure fallback to dark) is already mirrored in the Rust path.

### Rejected alternative

Gate extension palettes (text==bg warning at 4.5, error/success hue-swap check). Rejected as a
new product surface: it breaks parity, and it would require its own decision ticket defining
thresholds, the warning channel, and the override flow before any implementation. If such a guard
is ever wanted, it is a new decision ticket, not an amendment here. TUI-V5 [#79][issue-79]
measures contrast across theme matrices; it never gates.

### Owner

None. No code change is implied by this ruling.

## 5. Ruling 4: the ANSI-256 cube transform is byte-locked

### Decision

The closed-form cube mapping is settled parity, byte-locked across all four converters: cube
component for level `v` (0 through 5) is `0` when `v == 0`, else `55 + 40·v`, yielding the level
set 0, 95, 135, 175, 215, 255. The grayscale ramp (232-255: `8 + 10·g`) and the basic-16 table
agree at all four sites as well; the cube form is the transform issue #63 contested, so it is the
one named here.

### Witnesses (all four agree byte-for-byte)

| Converter | Site | Form |
|---|---|---|
| Reference HTML export | `.references/pi/packages/coding-agent/src/core/export-html/ansi-to-html.ts:49` | `toComponent = (n) => (n === 0 ? 0 : 55 + n * 40)` |
| Reference theme conversion | `.references/pi/packages/coding-agent/src/modes/interactive/theme/theme.ts:1050` | `toHex` over `n === 0 ? 0 : 55 + n * 40` |
| Rust HTML export | `crates/pi/src/core/export_html/ansi_to_html.rs:87-104` (`color_256`, `component` closure) | `if value == 0 { 0 } else { 55 + value * 40 }` |
| Rust export theme | `crates/pi/src/core/export_html/mod.rs:329-350` (`ansi_256_to_hex`, `part` closure) | `if value == 0 { 0 } else { 55 + value * 40 }` |

### Rationale

The four sites already produce identical bytes. Locking the closed form removes the temptation
to improve one site in isolation; any single-site change would silently fork exported colors
between surfaces.

### Rejected alternatives

- Exact-cube lookup table (`[0, 95, 135, 175, 215, 255]`). Rejected: identical output with more
  bytes and four copies to keep in sync; the closed form is the reference's own shape.
- Rounding re-derivations (for example `round(v · 255 / 5)`, which yields 0, 51, 102, 153, 204,
  255). Rejected: different bytes, no upstream mandate, and a parity break at every cube level
  except 0.

### Owner

None. Any change to any of the four sites is a new recorded divergence requiring a new decision
ticket.

## 6. Ownership boundary

- This record changes no Rust source, theme schema, protocol type, settings surface, keybinding,
  or mirror tree content. Its sole artifact is this document.
- TUI-T2 [#74][issue-74] owns color-depth remediation; TUI-T3 [#73][issue-73] owns hyperlink
  wiring. Neither ticket may smuggle in the other's surface, a palette guardrail, or a transform
  change.
- The extension protocol surface (`ThemeSet`, `crates/pi-ext/src/protocol.rs:1417`) is out of
  scope for both remediation tickets.
- The two standing divergences (rulings 1 and 2) remain recorded as divergences in this document
  until their owners land; closing either divergence requires the owning ticket, not an edit to
  this record's verdicts.

## 7. Verification boundary

- This record is decision-only: nothing here is verified by running code, and no test, build,
  formatter, or linter run accompanies it.
- After TUI-T2 lands: TUI-P2 [#58][issue-58] (deterministic contrast measurement prototype) and
  TUI-V5 [#79][issue-79] (theme and contrast matrix with numeric oracle) verify that depth
  selection follows `caps.true_color` and that both depth modes render the built-in palette set
  legibly.
- After TUI-T3 lands: PTY-level transcript assertions verify that OSC 8 emission follows
  `caps.hyperlinks` and that incapable terminals receive the URL-text fallback.
- Ruling 3 needs no verification ticket: TUI-V5 [#79][issue-79] measures contrast outcomes and
  never gates on them.
- Ruling 4 is verified by byte equality of the four witness sites, which holds today; any future
  change to one site fails that equality and is a new recorded divergence.
