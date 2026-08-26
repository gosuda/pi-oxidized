# TUI-G2: hardware cursor visibility and IME positioning policy (decision record)

- **Issue:** [#53][issue-53] — `TUI-G2` (routed decision: settings category)
- **Decision type:** recorded decision, not implementation
- **Deliverable:** this document and the matching commit, and only these
- **Decision:** Promote hardware cursor visibility to a first-class boolean setting (`showHardwareCursor`, default `false`), with `PI_HARDWARE_CURSOR=1` as environment fallback, while retaining existing fake/styled focus indicators.

[issue-53]: https://github.com/metaphorics/pi-oxidized/issues/53

## Selected option

Promote `showHardwareCursor` to a first-class boolean configuration setting with default `false`. The environment variable `PI_HARDWARE_CURSOR=1` serves as an environment fallback.

When `showHardwareCursor` is `false` (default) and `PI_HARDWARE_CURSOR` is unset:
- The terminal hardware cursor remains hidden.
- The renderer continues to calculate hardware cursor coordinates from component layout and emits cursor position commands during frame composition.
- This hidden-by-default behavior is compatibility-safe across standard terminal emulators and prevents visual double-cursor artifacts where a terminal hardware cursor overlaps the software-rendered cell cursor.
- Terminals that query cursor location while the cursor is hidden, such as standard IME integrations, receive correct cursor placement for candidate windows without visible cursor clutter.

When `showHardwareCursor` is `true` or `PI_HARDWARE_CURSOR=1`:
- The terminal hardware cursor is made visible at the calculated cursor position when a focusable input component has active focus.
- This opt-in visibility resolves IME candidate window placement in environments such as Windows Subsystem for Linux (WSL) and WezTerm, where the terminal emulator requires a visible hardware cursor to anchor operating-system CJK (Chinese, Japanese, Korean) IME candidate windows to the active text entry point.

Component focus and cursor styling:
- Components (such as `Editor` and `Input`) always retain their software-styled focus borders, highlight styles, and inverted or colored text cursor indicators in the cell buffer regardless of hardware cursor visibility.

Current seams:

- Upstream reference (`.references/pi`):
  - `packages/coding-agent/src/core/settings-manager.ts`: declares `showHardwareCursor?: boolean` in `Settings` (line 132), provides `getShowHardwareCursor(): boolean` checking `this.settings.showHardwareCursor ?? process.env.PI_HARDWARE_CURSOR === "1"` (line 1282), and `setShowHardwareCursor(enabled: boolean): void` saving to global settings (line 1286).
  - `packages/coding-agent/src/modes/interactive/components/settings-selector.ts`: registers the `show-hardware-cursor` settings item (lines 76, 753–758) with `id: "show-hardware-cursor"`, label `"Show hardware cursor"`, description `"Show the terminal cursor while still positioning it for IME support"`, and values `["true", "false"]`.
  - `packages/coding-agent/src/cli/startup-ui.ts`: `createStartupTui` (line 82) passes `settingsManager.getShowHardwareCursor()` to `TuiMainScreen`.
  - `packages/coding-agent/src/modes/interactive/interactive-mode.ts`: `createInteractiveTui` (lines 360, 371, 386) accepts `showHardwareCursor`, propagates state across mode switches (lines 579, 848, 862), and supplies it to settings-selector callbacks (line 4557).
  - Upstream documentation: `docs/settings.md` (line 67) documents `showHardwareCursor` (`boolean`, default `false`, `"Show the terminal cursor while TUI positions it for IME support"`), `docs/terminal-setup.md` (lines 84–85) specifies WSL/WezTerm CJK IME candidate window positioning setup, and `docs/tui.md` (lines 50–60) details `CURSOR_MARKER` APC sequences, `Focusable` interface propagation, and hidden-by-default cursor positioning.
- Rust current seams:
  - `crates/pi-tui/src/terminal/writer.rs:217`: `Tui::new` currently reads only `hardware_cursor: std::env::var_os("PI_HARDWARE_CURSOR").is_some()`.
  - `crates/pi-tui/src/terminal/writer.rs:313`: `commit_frame` executes `if hardware_cursor && let Some(pos) = annotations.borrow().cursor() { frame.set_cursor_position(pos); }`.
  - `crates/pi-tui/src/frame.rs`: `FrameAnnotations` collects hardware cursor requests through `set_cursor(Position)`.
  - `crates/pi-tui/src/components/editor/mod.rs` (line 1846) and `crates/pi-tui/src/components/input.rs` (line 573): active input components calculate cursor position and invoke `set_cursor(...)` while rendering styled cell cursors.
  - `crates/pi/src/core/settings.rs`: lines 589, 666, 763, 2108–2115 define `show_hardware_cursor` field mapping, `get_show_hardware_cursor()`, and `set_show_hardware_cursor()`.
  - `crates/pi/src/modes/interactive/runtime.rs`: lines 610, 708 define `hardware_cursor` in `InteractiveRuntimeOptions`.

## Rationale (one line)

Promoting `showHardwareCursor` to a first-class boolean setting matching upstream reference behavior satisfies issue #25's parity-first rule, eliminates undocumented env-only divergence, and resolves WSL/WezTerm CJK IME anchoring without compromising compatibility in standard terminals.

## Invariant and sign-off traceability (issue #25)

This decision is made against the issue #25 terminal interaction and settings contract:

- Reference parity default. Issue #25 establishes that "reference parity is the default and every deviation is an explicit, recorded decision". Upstream provides `showHardwareCursor` as an official configuration key, a `/settings` toggle, and a documented option alongside `PI_HARDWARE_CURSOR=1`. Full parity requires exposing this setting identically in the product settings interface and configuration layer.
- Dual-mode positioning invariant. The renderer must compute hardware cursor coordinates during frame composition regardless of whether cursor visibility is enabled. Terminals that query cursor coordinates while the cursor is hidden depend on this invariant.
- Focus indicator preservation. Hardware cursor activation must not disable, alter, or replace cell-level reverse-video or themed focus styling. Software-styled indicators remain the primary focus representation.

## Rejected alternatives

- Option (a), styled-indicators-only (no hardware cursor visibility setting). Rejected. Relying solely on cell-styled cursors or restricting cursor visibility to an undocumented environment variable leaves CJK IME users on terminals such as WezTerm under WSL without working candidate window positioning. It also fails reference parity by omitting the documented `showHardwareCursor` setting and its `/settings` UI item.
- Option (b), visible hardware cursor by default. Rejected. Enabling the hardware cursor unconditionally introduces visual double-cursor artifacts on standard terminals where the terminal emulator draws a box or line cursor over the application's styled cell cursor. Upstream defaults `showHardwareCursor` to `false`.

## Ownership boundary

- No Rust source change, settings schema alteration, persistence modification, or runtime wiring lands under TUI-G2. This ticket records only the policy decision in `docs/TUI-G2-hardware-cursor-policy.md`.
- All settings persistence, runtime propagation, and settings-selector UI wiring changes remain routed to their designated implementation tickets under the issue #25 governance framework.
- Settings and persistence changes cannot be smuggled into polish or refactoring tickets.

## Deterministic verification contract (TUI-V2 / TUI-P1)

- Verification consumer: `TUI-V2` automated test harness.
- Carrier schema: `TUI-P1` canonical schema-v1 `TerminalSnapshot` (`crates/pi-tui/src/testkit/driver.rs`).

The verification suite evaluates the following deterministic assertions on `TerminalSnapshot` instances:

1. Position invariance. After an identical pinned cursor movement sequence in a focused editor (such as typing $N$ characters or navigating cursor offsets), `TerminalSnapshot.cursor_col` and `TerminalSnapshot.cursor_row` must be identical between `showHardwareCursor: false` and `showHardwareCursor: true` configurations.
2. Hidden-by-default visibility. With default settings (`showHardwareCursor: false` and `PI_HARDWARE_CURSOR` unset), `TerminalSnapshot.cursor_visible` must assert `false`.
3. Opt-in visibility. When `showHardwareCursor: true` or `PI_HARDWARE_CURSOR=1`, `TerminalSnapshot.cursor_visible` must assert `true` while the editor component retains focus.
4. Focus transfer and overlay suppression. When focus transfers to a component without active text input (such as an unfocused list, command palette navigation, or non-input modal overlay), `TerminalSnapshot.cursor_visible` must assert `false` regardless of the `showHardwareCursor` setting.
5. Deterministic execution. All assertions must hold across $k \ge 3$ consecutive runs with digest-identical `TerminalSnapshot` streams. Tests must rely on AVT virtual terminal state assertions and must not use arbitrary timing delays, thread sleeps, or visual screenshot comparisons.
