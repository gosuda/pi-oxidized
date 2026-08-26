# TUI-G7: confirm dialog default-selection and Esc/Ctrl+D dispatch policy (decision record)

- **Issue:** [#61][issue-61] — `TUI-G7` (routed decision: focus/navigation + dispatch)
- **Decision type:** recorded policy decision, not implementation
- **Status:** APPROVED (pins settled by ReviewTuiG7Policy)
- **Measurement:** TUI-V2 measures landed dispatch semantics after execution
- **Implementation:** [#146][issue-146] (`TUI-G7-IMPL`), not complete here

[issue-61]: https://github.com/metaphorics/pi-oxidized/issues/61
[issue-146]: https://github.com/metaphorics/pi-oxidized/issues/146

## 1. Scope, authority, and parity doctrine

This document is the single authority for default landing selection, Enter/Esc resolution, and Ctrl+D dispatch precedence across all interactive TUI confirm dialogs, selectors, and overlays. Under the repository parity doctrine (issue #25, `STYLE_LEDGER.md`), TypeScript reference behavior is canonical (`.references/pi/…`), and every deviation is an explicit recorded decision. This record establishes policy only and makes no code changes.

## 2. Confirm dialog dispatch matrix

| Flow / context | Items and order | Landing index | Enter behavior | Esc / Ctrl+C behavior | Parity status and notes |
|---|---|---|---|---|---|
| **Import replace** (`/import`, `ImportConfirm`) | `[Yes, No]` | `0` (`Yes`) | Executes import if Yes; cancels if No | Closes dialog; emits `Import cancelled` notice | Exact reference parity (`interactive-mode.ts:2384`) |
| **Import CWD retry** (`/import`, `ImportCwdConfirm`) | `[Yes, No]` | `0` (`Yes`) | Retries import with fallback CWD if Yes | Closes dialog; emits `Import cancelled` notice | Exact reference parity (`runtime.rs:2443`) |
| **Extension confirm** (`ui.confirm`) | `[Yes, No]` | `0` (`Yes`) | Resolves `confirmed: true` (Yes) / `false` (No) | Resolves `Confirm { confirmed: false }` | Exact reference parity (`interactive-mode.ts:2548`) |
| **Logout removal** (`/logout`, `Logout`) | `[Cancel, Credential 1..N]` | `0` (`Cancel`) | `Cancel`: silent close; `Cred`: removes auth | Silent close; no credential removal; no notice | **Recorded deviation** (safe sentinel row; silent) |
| **First-run analytics** (`OverlayKind::FirstTimeSetup`) | `[Share…, Don't share]` | `0` (`Share…`) | Advances wizard step; persists selection | Restores pre-wizard theme; dismisses wizard | Excluded wizard flow; Esc is dismissal, not No vote |

## 3. Landing, Enter, and Esc decisions

1. **Binary confirm landing keeps reference order:** `[Yes, No]` with initial landing on index `0` (`Yes`) for `/import` replace-session, `/import` missing-CWD retry, and all extension `ui.confirm` dialogs. Safety is guaranteed by the cancellation contract rather than inverting landing defaults.
2. **Esc and Ctrl+C (`tui.select.cancel`) resolve to No/cancel:** Built-in confirms abort the flow with their canonical outcome (`Import cancelled` notice for import); extension confirms resolve `confirmed: false` over the extension channel.
3. **Enter executes the focused item:** Enter confirms the active selection across all dialogs.
4. **First-run wizard exclusion:** First-run setup (`startup-ui.ts:168-182`, `startup.rs:72-99`) is a multi-step modal wizard. Esc dismisses without persisting analytics choices (`finish(undefined)`). The binary confirm `Esc == No` clause does not apply to wizard steps.

## 4. Recorded logout deviation and justification

- **Mechanism:** item 0 is an explicit `Cancel` sentinel row preceding credentials (`indices 1..N`). Landing index defaults to `0` (`Cancel`).
- **Rationale:** the `/logout` picker is unique because its entire payload list is destructive without a prior confirmation prompt. In contrast to `/import` (where the user typed an explicit path to replace), bare `/logout` followed by reflex-Enter would delete whichever credential sorts first.
- **Silence contract:** Enter on `Cancel` and Esc behave identically: silent close, no credential removal, and no notice. Unlike `/import`, logout cancellation emits no notice, matching reference `oauth-selector.ts` onCancel silence.
- **Sentinel safety:** the `Cancel` sentinel identifier is isolated and cannot collide with any provider ID. Esc remains fully functional; the row is an additive safe-landing affordance.

## 5. Exhaustive Ctrl+D precedence and focus matrix

Ctrl+D must **never** exit the application while any selector, confirm dialog, or overlay is open.

| Focus / context | Buffer / state | Ctrl+D precedence and action | Parity and contract notes |
|---|---|---|---|
| **Editor (focused)** | Zero-length (`len == 0`) | Dispatches `app.exit` (clean shutdown) | `app.exit` evaluated before `deleteCharForward` |
| **Editor (focused)** | Non-empty or whitespace-only | Consumed by `tui.editor.deleteCharForward` | Whitespace is not empty; forward-deletes char |
| **Session selector** (`/resume`) | Session row selected | Arms inline delete confirmation for item | Triggers `startDeleteConfirmationForSelectedSession` |
| **Session selector** (`/resume`) | Active session selected | No-op; status: `Cannot delete the currently active session` | Guard matches reference `session-selector.ts:397` |
| **Tree selector** (`/tree`) | Open | Sets filter to `default` (`app.tree.filter.default`) | Part of filter family (`ctrl+d/t/u/l`) |
| **Extension editor dialog** | Open | Follows empty-editor rule (zero-length -> shutdown) | Reference `interactive-mode.ts:2704->3911` |
| **All other selectors / overlays** | Open (model, fork, auth, settings, confirms, overlays) | **Ignored entirely** (no action, no exit, no close) | Selectors return Ignored; app mapper suppresses exit |

- **Empty editor definition:** "Empty" strictly means zero-length buffer (`len == 0`), matching upstream (`custom-editor.ts:61`, `getText().length === 0`). Whitespace-only buffers (`"   "`) are non-empty and must not trigger `app.exit`.
- **Session selector delete family:** `Ctrl+D` (`app.session.delete`) and `Ctrl+Backspace` (`app.session.deleteNoninvasive`, empty-query alias forwarding to search input) share inline delete confirmation state.

## 6. Innermost Esc and focus restoration

Esc cancels the innermost active state in strict reverse hierarchical order:
1. **Armed delete confirmation** inside the session selector: first Esc cancels only the confirmation prompt, returning focus to the session list with hint bar restored (`Delete session? <enter> confirm · <esc> cancel`).
2. **Active selector / confirm dialog:** second Esc (or first Esc when unconfirmed) cancels and closes the selector, restoring `FocusArea::Editor`.
3. **Active overlay:** dismisses the overlay and restores prior focus.
4. **Editor:** non-empty clears editor text; empty double-Esc triggers tree/fork per `input.rs` `map_escape` (377–445).

Closing a selector or overlay is mutually exclusive by construction (`close_selector` and `open_overlay`/`open_selector` clear counterpart slots) and unconditionally restores `FocusArea::Editor`.

## 7. Implementation specification: TUI-G7-IMPL

Implementation is assigned to standalone execution ticket [#146][issue-146] (`TUI-G7-IMPL`).

### Owned files
- `crates/pi-tui/src/components/editor/mod.rs`
- `crates/pi-tui/src/keybindings.rs`
- `crates/pi/src/core/keybindings.rs`
- `crates/pi/src/modes/interactive/input.rs`
- `crates/pi/src/modes/interactive/runtime.rs`
- `crates/pi/src/modes/interactive/selectors.rs`

### Five dispatch mechanisms
1. **`T-G7-IMPL-1` (Editor / app.exit unblock and focus guard):** check `app.exit` before `tui.editor.deleteCharForward` at focused editor. Dispatch exit iff editor buffer length is exactly zero; otherwise `deleteCharForward` consumes. Tighten `input.rs` exit gate from `trim().is_empty()` to `is_empty()`. Guard `app.exit` against firing while `FocusArea::Selector`, `FocusArea::Overlay`, a pending extension Select/Confirm/Input dialog, or the first-run overlay is active; an extension Editor dialog follows the empty-editor rule.
2. **`T-G7-IMPL-2` (Session delete and inline confirm):** wire `app.session.delete` (`ctrl+d`) and `app.session.deleteNoninvasive` (`ctrl+backspace`) in the session selector. Implement inline confirmation state (`Enter` confirms, `Esc` cancels confirmation only). Emit error status `"Cannot delete the currently active session"` on active session delete attempt.
3. **`T-G7-IMPL-3` (Tree filter family):** wire full `app.tree.filter.*` keybinding family in tree selector in canonical order: `ctrl+d` (default), `ctrl+t` (no-tools), `ctrl+u` (user-only), and `ctrl+l` (labeled-only).
4. **`T-G7-IMPL-4` (Logout cancel sentinel row):** insert `Cancel` sentinel row at index 0 in logout selector (`runtime.rs` `handle_logout_command`/`handle_logout_confirm`). Map sentinel selection to silent close without removal; maintain silent Esc behavior.
5. **`T-G7-IMPL-5` (Tests and keybinding registry):** extend input mapper unit tests (`input.rs:680-700`) for selector-open ignore and zero-length vs whitespace gates. Add editor empty-Ctrl+D fallthrough test and logout sentinel test near `runtime.rs:9806`. Keep the 31-id keybindings exactness test green.

### Acceptance criteria for implementation
- `Ctrl+D` on zero-length editor exits; `Ctrl+D` on whitespace-only buffer deletes forward.
- `Ctrl+D` while any selector, confirm, or overlay is open never exits the application.
- Session selector `Ctrl+D` opens inline delete confirmation; first `Esc` cancels confirmation; second `Esc` closes selector.
- Logout dialog lands on index 0 `Cancel`; pressing `Enter` or `Esc` closes silently without notices or credential changes.
- All binary confirms land on index 0 `Yes`; `Esc` cancels with canonical flow outcomes.

## 8. Verification boundary (TUI-V2)

`TUI-G7` records settled policy and dispatch contracts only. Automated verification and measurement of landed dispatch mechanics are owned by `TUI-V2` upon completion of `TUI-G7-IMPL`.
