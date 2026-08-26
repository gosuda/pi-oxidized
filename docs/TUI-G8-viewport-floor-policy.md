# TUI-G8: narrow-width viewport floor policy (decision record)

| Field | Value |
|---|---|
| Issue | [#56][issue-56] `TUI-G8` (routed decision: viewport policy) |
| Decision type | recorded policy decision, not implementation |
| Status | RATIFIED |
| Implementation | [#83][issue-83] (`TUI-T9`), not executed here |
| Measurement | [#87][issue-87] (`TUI-V4`) measures current viewport semantics under this policy |
[issue-56]: https://github.com/metaphorics/pi-oxidized/issues/56
[issue-83]: https://github.com/metaphorics/pi-oxidized/issues/83
[issue-87]: https://github.com/metaphorics/pi-oxidized/issues/87
[issue-67]: https://github.com/metaphorics/pi-oxidized/issues/67
[issue-25]: https://github.com/metaphorics/pi-oxidized/issues/25

## 1. Scope, authority, and parity doctrine

This document is the single authority for the supported viewport width floor below the
initial 20-column clamp during live terminal resizes. Under the repository parity doctrine
(issue #25), the TypeScript reference tree (`.references/pi/…`) is canonical and every
deviation is an explicit recorded decision. The reference imposes no explicit live-resize
width floor — it reads `terminal.columns` directly and renders at whatever width the
kernel reports — so any floor the Rust port adopts is a new product policy, not a parity
restoration. This record establishes that policy and makes no code changes.

## 2. Decision

**Refuse-and-blank below 20 columns.** The viewport width floor is 20 columns, matching
the initial-size clamp in `initial_terminal_size` (`runtime.rs:6528`,
`width.clamp(20, 1024)`). When a live resize event reports a width below 20, the runtime
must:

1. **Accept the resize event** (update the Tui size cache and ViewState dimensions) so the
   terminal's reported geometry is tracked faithfully.
2. **Blank the render area** — emit no content cells for the frame — instead of attempting
   to wrap content into a sub-20-column viewport.
3. **Resume normal rendering** as soon as a subsequent resize restores width ≥ 20.

Above 20 columns, best-effort wrap continues unchanged: the existing component stack
(header → chat → status → editor → footer), the footer ladder truncation, the markdown
renderer, and the editor all render at the reported width with their current wrap/truncate
logic.

### Floor threshold: 20

The threshold of 20 is chosen from the following evidence:

- **Initial clamp:** `initial_terminal_size` (`runtime.rs:6528`) already clamps startup
  width to `[20, 1024]`. A live-resize floor below the startup floor would be incoherent —
  the TUI would launch at a width it then refuses to render at after a shrink.
- **Footer design floor:** the footer ladder is designed and tested at width 20
  (`footer.rs:367` `footer_ladder_single_row_at_width_20`; `footer.rs:393`
  `visible_width(&stats) <= 20`). Below 20 the footer stats line wraps to multiple rows,
  consuming the editor area and breaking the single-row invariant.
- **Snapshot corpus floor:** every golden snapshot renders at 20/80/160
  (`tests.rs:36` `for &w in &[20_u16, 80, 160]`). No snapshot exists below 20, and the
  render output at sub-20 widths is untested and unstable.
- **PTY fixture ladder:** the TUI-P1 [#67][issue-67] fixture resize plan
  (`pi_tui_pty_fixture.rs:433-457`) exercises sub-20 widths (18, 14, 12, 11, 10, 9, 8)
  against the real `Tui` pipeline. These runs complete without panic thanks to
  defensive `.max(1)` / `.saturating_sub()` guards throughout the render path, but the
  output is degraded: the header wraps to 3+ rows, the footer stats fragment across
  multiple lines, and the editor area is compressed to zero rows. The fixture proves the
  TUI does not crash below 20; it does not prove the output is usable.
- **Editor width floor:** `editor_width` (`runtime.rs:861`) handles sub-2-column areas
  (`if width >= 2 { width - 2 } else { width }`), but the editor needs at least ~18
  columns to render a usable prompt marker, cursor, and one line of text.

### Refuse-and-blank vs. best-effort wrap

Best-effort wrap below 20 is rejected because:

- The footer, header, and editor each have independent minimum-width requirements that
  converge near 20 columns. Below that, the component stack produces overlapping,
  truncated, or zero-height sections that are unreadable, not merely ugly.
- Best-effort wrap would require per-component narrow-mode branches — a new render surface
  per component — with no upstream precedent and no snapshot coverage. That is a
  redesign, not a polish, and it belongs in a scope-classification ticket (TUI-G4 domain),
  not a floor-policy ticket.
- Refuse-and-blank is one guard at the render boundary, not N branches across the
  component tree. It is the minimal policy that prevents degraded output without
  inventing a new layout mode.

## 3. Current-state divergence

The current Rust code has **no live-resize floor**. `handle_resize` (`runtime.rs:3243`)
calls `self.tui.note_resize(width, height)` and `self.view.resize(width, height)` with
the raw reported dimensions — no clamping. `ViewState::resize` (`state.rs:600`) stores
them directly. The render path's defensive `.max(1)` guards prevent panics but produce
degraded output at sub-20 widths.

This is a **recorded divergence from this policy**, to be remediated by TUI-T9 (#83).
TUI-V4 (#87) measures the current behavior (no floor) as a standing invariance baseline;
it does not gate on the floor being implemented.

| Surface | Current behavior | Policy target | Owner |
|---|---|---|---|
| `initial_terminal_size` (`runtime.rs:6528`) | Clamps to `[20, 1024]` | Already compliant | — |
| `handle_resize` (`runtime.rs:3243`) | No width floor; raw dimensions | Blank render below 20 | TUI-T9 #83 |
| `ViewState::resize` (`state.rs:600`) | Stores raw width/height | Store raw; render gate is at compose | TUI-T9 #83 |
| `render_view` / `compose` (`view.rs`) | Renders at any width ≥ 1 | Skip content cells when width < 20 | TUI-T9 #83 |
| PTY fixture ladder (`pi_tui_pty_fixture.rs:433`) | Exercises 8..18 column widths | Continues to exercise; fixture proves no-crash | — |

## 4. Implementation specification: TUI-T9

Implementation is assigned to standalone execution ticket [#83][issue-83] (`TUI-T9`).

### Owned files
- `crates/pi/src/modes/interactive/runtime.rs` — render gate in the commit/reanchor path
- `crates/pi/src/modes/interactive/view.rs` — compose-time blank when width < 20

### Mechanism
1. **Render gate:** when `view.width < 20`, the compose path emits an empty frame (zero
   content cells) or a single-line "terminal too narrow" notice, and the commit path
   writes it without clearing (Reanchor, not a full repaint). The Tui size cache and
   ViewState dimensions still reflect the raw reported size so the next resize ≥ 20
   resumes correctly.
2. **No clamp on stored dimensions:** `ViewState::resize` and `Tui::note_resize` continue
   to store the raw reported width/height. The floor is a render-time gate, not a stored
   clamp. This preserves the invariant that a resize back to ≥ 20 columns immediately
   renders correctly without a stale clamped value.
3. **No new component branches:** the floor is enforced at the compose boundary, not
   inside individual components. Components are never called with width < 20.

### Acceptance criteria for implementation
- A live resize to width < 20 blanks the render area (no content cells, no crash).
- A subsequent resize to width ≥ 20 resumes normal rendering immediately.
- The Tui size cache and ViewState dimensions track the raw reported size at all times.
- No per-component narrow-mode branch is added.
- The existing PTY fixture ladder (which exercises sub-20 widths) continues to complete
  without panic.

## 5. Verification boundary (TUI-V4)

TUI-V4 (#87) measures current resize-storm, settle, and progressive-disclosure behavior
under this policy. It measures the **current** state (no live-resize floor) as a standing
invariance baseline and records the divergence. It does not gate on the floor being
implemented. After TUI-T9 lands, TUI-V4 extends to verify the blank-and-resume contract.

## 6. Ownership boundary

- This record changes no Rust source, no protocol type, no verification script, no
  settings surface, and no keybinding.
- The floor policy is a render-time decision owned by TUI-T9 (#83).
- The alt-screen / scroll-view surface (TUI-G4, #35) is out of scope: the floor applies
  to the main-screen interactive view only. Alt-screen scope is classified separately.
- The width oracle (`width.rs`) is out of scope: the floor is a viewport policy, not a
  character-width measurement. TUI-R2 (#62) owns the width oracle.

## 7. Rejected alternatives

- **Best-effort wrap below 20:** rejected per §2. It is a redesign (per-component
  narrow-mode branches), not a floor policy, and has no upstream precedent or snapshot
  coverage.
- **Floor below 20 (e.g., 10 or 1):** rejected. The footer, header, and editor each
  require ~20 columns to produce coherent output. A lower floor would permit degraded
  output that this policy exists to prevent.
- **Floor above 20 (e.g., 40 or 60):** rejected. The initial clamp is 20, the snapshot
  corpus starts at 20, and the footer is designed for 20. A higher floor would refuse
  to render at widths the TUI already supports at startup.
- **Clamp stored dimensions to 20:** rejected. Storing a clamped width would make the
  next resize ≥ 20 render at a stale value until the event arrives. The floor must be a
  render-time gate, not a stored clamp, to preserve immediate resume.
