# Terminal Rail Doctrine — TUI-G3 Decision Record (#40)

Status: Ratified (decision-only; no source changes by this task)
Issue: [TUI-G3 #40](https://github.com/metaphorics/pi-oxidized/issues/40)
Stable ID: `TUI-G3`
Follow-on owners: TUI-T7 (#69) — inert theme slots + TruncatedText; TUI-T4 (#65) — EditorBorder wiring; TUI-G4 (#35) — alt-screen / scroll-view.
Blocked by: [Audit canonical terminal interaction flows #25](https://github.com/metaphorics/pi-oxidized/issues/25)

## 1. Decision

The native Rust interactive TUI renders the event blocks covered by this decision through **rail-only chrome**: every user message, custom message, tool block, bash block, compression summary, branch summary, and skill-invocation block is drawn in a one-cell gutter rail at column 0 with a foreground-colored `│`/`┃` glyph, followed by content indented to the shared column-2 edge. Assistant text and thinking use the same column-2 edge without a rail. None of these transcript blocks paints a background slab. This doctrine is ratified and is the canonical rendering contract going forward; it is protected by test and by this record.

TUI-G3 is a decision record only: it changes **no source**. Every consumption, deletion, or wiring action named below is assigned to a follow-on task and executes exclusively in that task's own decision track.

## 2. Source evidence for rail-only chrome

- `crates/pi/src/modes/interactive/messages.rs`
  - `railed(glyph, color, theme, child)` (line 29) is the single rail constructor: it builds a `Rail` with a foreground-applied glyph via `theme_snapshot.fg(color, s)` (line 36). No background is ever applied.
  - `build_user` (line 324) renders a user message as `railed("│", ThemeColor::BorderAccent, …)` (line 339).
  - `build_custom` (line 456) renders a custom message as a `CustomMessageLabel` label plus `railed("│", ThemeColor::CustomMessageLabel, …)` (line 477).
  - `build_bash` (line 402) renders bash as `railed("│", ThemeColor::BashMode, …)` (line 450).
  - `build_tool_blocks` (line 353) renders tool execution with a phase-colored rail: `ThemeColor::Muted` pending, `ThemeColor::Success` success, `ThemeColor::Error` error (lines 356-360), emitted via `railed` (line 396).
  - Compression and branch summaries use `railed("│", ThemeColor::BorderMuted, …)` (lines 504, 531); skill invocation uses `ThemeColor::CustomMessageLabel` for its label and `ThemeColor::Accent` for its rail (lines 535-553).
- `crates/pi/src/modes/interactive/tests.rs`
  - `rail_not_slab_for_user_block` (line 289) asserts the user block draws the rail glyph at column 0 and that **no rail row's ANSI snapshot contains a background SGR** (`!ansi[i].contains("\x1b[48;")`, line 315).
  - `transcript_rows_share_column_two_edge` (lines ~357-368) asserts every transcript row starts at the shared column-2 edge via rail or indent (D3).
  - `tool_error_uses_heavy_rail_glyph` (line 539) asserts errored tool blocks carry the heavy `┃` rail (D5).
- No call site in the interactive view passes a `ThemeBg` background applicator (e.g. `ResolvedTheme::bg_ansi`) into any block renderer. Background application exists in the theme resolver (`bg_ansi`, theme.rs line 435) and in pi-tui containers, but no chat render path consumes it.

## 3. Disposition of inert surfaces

The following dispositions are ratified; any structural change executes only under the named follow-on task. `KEEP` means the token remains part of the schema and wire contract and must not be deleted without a schema-compatible migration decided in the named task.

| Surface | Source symbol / path | Currently | Disposition | Owner |
|---|---|---|---|---|
| ThemeBg::SelectedBg | theme.rs enum line 151; `ALL_BG` line 218; `ALL_BG_SLOTS` line 1862; `REQUIRED_COLORS` line 1769 | Resolved by `resolve_owned`; **no terminal render path reads it** | **KEEP** — schema/wire-required token; terminal-inert today | TUI-T7 #69 |
| ThemeBg::ScrollbarThumb | theme.rs enum line 153; `ALL_BG` line 219; `ALL_BG_SLOTS` line 1863 | Optional key, falls back to `selectedBg` (theme.rs lines 1338-1341); **no terminal scrollbar paints it** | **KEEP** — schema/wire token consumed by fallback and tests; scroll-view usage domain is alt-screen | TUI-T7 #69 (+ TUI-G4 #35 domain) |
| ThemeBg::UserMessageBg | theme.rs enum line 155; `ALL_BG` line 220; `ALL_BG_SLOTS` line 1864; `REQUIRED_COLORS` line 1770 | **Inert in terminal** — `build_user` paints a `BorderAccent` rail, no bg slab | **KEEP** — schema-required; wire-or-drop decision deferred | TUI-T7 #69 |
| ThemeBg::CustomMessageBg | theme.rs enum line 157; `ALL_BG` line 221; `ALL_BG_SLOTS` line 1865; `REQUIRED_COLORS` line 1773 | **Inert in terminal** — `build_custom` paints a `CustomMessageLabel` rail, no bg slab | **KEEP** — schema-required; wire-or-drop decision deferred | TUI-T7 #69 |
| ThemeBg::ToolPendingBg | theme.rs enum line 159; `ALL_BG` line 222; `ALL_BG_SLOTS` line 1866; `REQUIRED_COLORS` line 1778 | **Inert in terminal** — tool blocks use phase-colored foreground rails, no bg slab | **KEEP** — schema-required; wire-or-drop decision deferred | TUI-T7 #69 |
| ThemeBg::ToolSuccessBg | theme.rs enum line 161; `ALL_BG` line 223; `ALL_BG_SLOTS` line 1867; `REQUIRED_COLORS` line 1779 | **Inert in terminal** — success rendered via `ThemeColor::Success` rail foreground | **KEEP** — schema-required; wire-or-drop decision deferred | TUI-T7 #69 |
| ThemeBg::ToolErrorBg | theme.rs enum line 163; `ALL_BG` line 224; `ALL_BG_SLOTS` line 1868; `REQUIRED_COLORS` line 1780 | **Inert in terminal** — error rendered via `ThemeColor::Error` heavy rail foreground | **KEEP** — schema-required; wire-or-drop decision deferred | TUI-T7 #69 |
| userMessageText | theme.rs `ThemeColor::UserMessageText` line 76; `ALL_FG` line 179; `ALL_FG_SLOTS` line 1824; `REQUIRED_COLORS` line 1772; `make_fg` line 586 | Registered and resolved, but **no render path consumes it** — user blocks color via `BorderAccent` | **KEEP** — schema-required; wire-or-drop decision deferred | TUI-T7 #69 |
| customMessageText | theme.rs `ThemeColor::CustomMessageText` line 78; `ALL_FG` line 180; `ALL_FG_SLOTS` line 1825; `REQUIRED_COLORS` line 1774; `make_fg` line 587 | Registered and resolved, but **no render path consumes it** — custom blocks color via `CustomMessageLabel` | **KEEP** — schema-required; wire-or-drop decision deferred | TUI-T7 #69 |
| TruncatedText | `crates/pi-tui/src/components/truncated_text.rs` (struct line 12) | **Zero callsites** outside its own module; referenced only by re-export (`crates/pi-tui/src/components/mod.rs` line 32) and its own unit tests | **DELETE** — dispose unless a follow-on wires it; decision ratified now, removal deferred | TUI-T7 #69 |
| EditorBorder::Muted | written in runtime.rs line 4481 (thinking-level off ⊢ Muted); asserted in tests line 7413 | Written into `view.editor.border`; **no paint path reads the editor border** | **WIRE** — editor must consume EditorBorder via EditorTheme so Muted renders | TUI-T4 #65 |
| EditorBorder::Bash | written in runtime.rs `dispatch_bash` line 2620 and sync line 4479; asserted in tests line 7397 | Write-only state; **no paint path reads it** | **WIRE** — editor must consume EditorBorder via EditorTheme so Bash renders | TUI-T4 #65 |
| EditorBorder::Thinking | written in runtime.rs sync line 4483; asserted in tests line 7702 | Write-only state; **no paint path reads it** | **WIRE** — editor must consume EditorBorder via EditorTheme so Thinking renders | TUI-T4 #65 |
| EditorTheme.border_color | `crates/pi-tui/src/components/editor/mod.rs` `EditorTheme` line 56, field line 56; stored into `Editor.border_color` line 177; constructed with `EditorTheme::default()` (identity, lines 59-66) at `runtime.rs` line 1358 | Stored but **never read in any paint fn**; `default()` supplies the identity closure, so today nothing changes | **WIRE** — the live editor consumes EditorBorder via EditorTheme instead of `EditorTheme::default()` at runtime.rs:1358 | TUI-T4 #65 |

### Notes

The eight required inert slots (`SelectedBg`, `UserMessageBg`, `CustomMessageBg`, `ToolPendingBg`, `ToolSuccessBg`, `ToolErrorBg`, `userMessageText`, `customMessageText`) are members of `REQUIRED_COLORS` (theme.rs line 1758). `ThemeJson::from_value` hard-fails a theme that omits any required slot (`ThemeError::MissingColor`, line 1315), so these tokens cannot be removed without a coordinated schema migration. This is why their disposition is **KEEP** under TUI-G3 with the wire-or-drop structural decision deferred.
- `scrollbarThumb` is the one background slot not in `REQUIRED_COLORS`; the parser synthesizes it from `selectedBg` when absent (theme.rs lines 1338-1341). It is retained as a schema/wire token and its only plausible terminal renderer (an alt-screen scroll thumb) lives in TUI-G4's domain.
The EditorBorder family and `EditorTheme.border_color` form the single genuinely write-only surface: the runtime maintains the border state faithfully (and its tests pin it), but the `Editor` is constructed with the identity theme, so the state never reaches a glyph. Wiring is TUI-T4's job, not TUI-G3's.

## 4. Deferred execution owners

Everything that would change a `.rs` file is out of scope for TUI-G3 and is routed to exactly one follow-on decision track:

- **TUI-T7 (#69)** — owns: wiring or deleting the inert `ThemeBg` slots and `userMessageText`/`customMessageText` (either rendering them or removing them with a schema-compatible migration), and disposing `TruncatedText`.
- **TUI-T4 (#65)** — owns: making the live editor consume `EditorBorder` state via `EditorTheme` instead of `EditorTheme::default()` at `runtime.rs:1358`.
- **TUI-G4 (#35)** — owns: the alt-screen / scroll-view surface, the only domain where a `scrollbarThumb` renderer would ever exist.

## 5. Scope

TUI-G3 changes no source. The rails, the theme resolver, the editor, and `truncated_text` are each owned by the tasks above and are untouched here. This record is the only deliverable of TUI-G3.