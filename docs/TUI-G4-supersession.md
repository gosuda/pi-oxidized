# TUI-G4 Supersession — Approved Fullscreen Scope (DES-12)

Status: Approved scope record, pending implementation (documentation only; no source changes by this task).
Supersedes: `docs/TUI-G4-alt-screen-scope.md` (TUI-G4 #35, `fd36be9`) deferred-by-design ruling for issue #12.
Candidate: C = `9767ba275f3e9a5ee0f5c5342249b629ab1b2282`.
Authority: Main's inclusion steering under `.outline/plans/follow-upstream-pi.md` DES-12, following HUMAN scope ratification (design gate Q1). DES-12 precedes DES-07/DES-08 implementation; this record is the formal re-opening, not implementation or verification evidence.
Frozen design input: `local://fullscreen-rust-interface-contract.md` (Formal evidence fields section and current documents; implementation design only, no fullscreen verification claimed).

## Historical exclusion preserved

The prior five-affordance exclusion remains findable in `docs/TUI-G4-alt-screen-scope.md` (Status, Decision, all five Affordance classification rows, final-for-issue-12 wording) and is preserved verbatim there. It is no longer live. This record does not rewrite history as if fullscreen previously existed.

## Superseded affordances (all five, as one screen-model unit)

| Superseded affordance | Approved fullscreen scope | Owner |
|---|---|---|
| Fullscreen scroll-view | Retained document with borrowed styled rows plus fullscreen viewport (`alt_screen/document.rs`, `alt_screen/mod.rs`) | Framework: `pi-tui`; composition: `pi` product runtime |
| Mouse capture | Native `UiEvent::Mouse` plus child-first dispatch, capture, and focus targeting (`component.rs`, `components/mouse_region.rs`) | Framework dispatch helpers: `pi-tui`; single active target/capture and focus transitions: `pi` product runtime/root |
| Search overlay | `TranscriptSearch` plus `SearchIndex` mounted through the existing overlay authority (`alt_screen/search.rs`) | Framework component: `pi-tui`; overlay admission/restoration: `pi` product runtime/root |
| Flash confirmations | Bounded native flash for copy success/failure with runtime-driven expiry | Framework flash surface: `pi-tui`; copy effect and completion generation: `pi` product runtime |
| Alt-screen prompt navigation | Prompt-zone metadata on document blocks with previous/next boundary navigation | Framework metadata/index: `pi-tui`; block insertion: `pi` product code |

## Framework/product ownership

- Framework (`pi-tui`) owns: `ScreenMode`, `UiEvent::Mouse` parsing, row-source seam, `paint_keyed_line`, document index, viewport state, mouse types and `handle_mouse`/`dispatch_mouse_event` helpers, selection and search index, flash surface, keybinding registry extension (31 to 45), terminal lifecycle writer/session operations, image row references, and the unchanged-wire adapter boundary.
- Product (`pi`) owns: `InteractiveRuntime::handle_ui_event` precedence, `InteractiveRoot` composition and fixed dock, settings (`tuiMode`, scrollbar, copy-on-select, `fullscreenExitOutput`), themes (`ScrollbarTrack`/`ScrollbarThumb`/`SearchMatchBg`/`SearchMatchText`), clipboard effects through the DES-06 async path with generation-tagged completion, overlay admission via existing `FocusArea::Overlay` and `OverlayKind` state, and mode-switch guards.
- Live focus/overlay authority is the product-owned runtime/root (`crates/pi/src/modes/interactive/runtime.rs` `InteractiveRuntime` and `InteractiveRoot`, `state.rs` `ViewState`/`FocusArea`/`OverlayKind`) composed from the retained framework primitives (`overlay.rs` compositing trio, `focus.rs` `FocusId`/`Focusable`). No deleted registry is cited as a live owner. The extension wire (`UiEventWire`, `UiSlot`, `SanitizedSlot`, `OverlayMarginWire`, `tui_overlay_spec`) is intentionally unchanged.

## Reopened obligations (pending, no pass asserted)

- `.outline/GATES.md::G29`: current regular/probe ownership evidence retained; new mouse/fullscreen ownership obligation appended with no pass assertion. Live wording names the product runtime/root plus retained framework compositing/`FocusId` primitives.
- `docs/PARITY_LEDGER.md::T6`: closed `UiEvent` consumer proof reopened for `Mouse` and native component dispatch. The seven compile-breaking matches requiring explicit disposition are `pi-tui/components/editor/mod.rs::Editor::handle_event`, `pi-ext/adapters.rs::map_ui_event`, `pi/modes/interactive/runtime.rs::ui_event_wire`, `pi/modes/interactive/runtime.rs::encode_terminal_input`, `pi/modes/interactive/input.rs::InputMapper::map`, `pi-tui/bin/pi_tui_ext_fixture.rs::handle_event`, and `pi-tui/bin/pi_tui_pty_fixture.rs::handle_event`, plus `terminal/input.rs::map_event` becoming `Some(UiEvent::Mouse(event))`. The extension wire decision is unchanged (no new `UiEventWire` variant, protocol bump, or host mouse branch; extensions observe reconstructed SGR bytes through the existing raw `terminalInput` hook). Re-ratification with mouse is planned with evidence living in DES-07, not here.
- `docs/PARITY_LEDGER.md` DES-07/DES-08 rows: pending implementation/witness status linked to this supersession.
- T1/T2/T9 parity notes and `docs/PAR-PTY-GRILL-verdict.md`: historical regular evidence retained; fresh fullscreen no-clear/diff, terminal-mode restoration, single-writer, and resume-size evidence required. The old fixture passing result is not fullscreen proof.
- Note on naming: the frozen contract verification section names G29, T6, T1/T2/T9, and fullscreen obligations. It names no T27 obligation; no T27 row is created or altered here.
- New viewport/scroll/mouse/selection/search/flash/theme/mode/suspend-resume checks are all pending implementation under DES-07/DES-08 with Main-run verification (program smoke items 1-9 in the frozen contract). No fabricated counts, no completed checkboxes, and no inference from old local PTY proof are made here.

## Explicitly retained out-of-scope and limitation boundaries

- TUI-P5 real-newline handling stays out of scope under its limitations-table owner (TUI-P5 stable-ID reservation per `docs/TUI-R2-terminal-width-table-divergence.md` §7.2; issue pending at record time). This supersession amends no TUI-P5 row and claims no real-newline behavior.
- `TranscriptMode`/JSON `mode` means `standard` or `contingency` (`docs/tui-transcript-schema-v1.md` §1-§2), not regular/fullscreen. Screen mode belongs in the recorded `Spawn.argv` and the designated corpus partition/scenario registration under Main's frozen harness contract. No `fullscreen` value is added to that field and no replacement Scenario/RowId/DriverKind name is invented here.
- Windows/platform, manual screen-reader sign-off, intermittent resize-ladder, musl-loader, and default-palette contrast limitations carried in `docs/TUI-CLOSE-evidence.md` §6 and sibling evidence are retained unchanged. This design closes none of them.
- Old regular-mode proofs (regular input/overlay/writer evidence, T1/T2/T9 host-tier PTY results, theme/contrast and keyboard/extension cases) are historical regular evidence, not fullscreen evidence.

This record changes no Rust source, public terminal API, key binding, viewport policy, or theme schema. All fullscreen behavior is pending.
