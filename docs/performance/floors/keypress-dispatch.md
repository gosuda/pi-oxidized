# Floor ledger: keypress dispatch (key write -> input dispatch to state mutation)

Owning R2 hot rows (lane 4): *PTY key write*, *Input dispatch to state mutation*.
(The paint row is owned by terminal-paint.md.) State: **OPEN (fail-closed —
measurement noise-rejected).**

## Contract (from call sites, tests, signatures — never internals)

- Harness side: one key write per sample into the child PTY (scripts/verification/pty.ts writeKeys :145-155); boundary = PTY key write to first complete synchronized-output paint (performance.ts runKeypressBenchmark :1492-1524, frameObservation :1010-1030); immediate repaint bypasses the background coalescer (inputPaintBypassesBackgroundCoalescer, performance.ts :1926).
- Input leg: the crossterm EventStream is owned by one task (crates/pi-tui/src/terminal/input.rs:142); `Event::Key` maps to `UiEvent::Key` (:225) and is published to an mpsc; `TerminalInput::recv` (:56) awaits it.
- Dispatch leg: the run loop's `tokio::select!` input arm (crates/pi/src/modes/interactive/runtime.rs:1757-1764) -> `handle_ui_event` (:1977-2177): editor event handling, InputMapper map (:2126), action dispatch (:2153-2172), and `needs_immediate_repaint` kicks `paint_frame` (:2148, :2174).
- State mutation: `dispatch_action` (:2261) applies ViewActions (paste, clear, focus, ...) to view/editor state.

Boundary classification: the key encoding surface (what byte sequences the app must
accept) is **boundary** (terminal input contract; TUI-P1 harness pins the width
ladder). The dispatch machinery is **interior**. Unresolved channels: none — the
census found a fixed select-loop topology.

## Floor (computed)

```
one PTY key write (harness side)                  ~0.6 us  (floorkit pipe write)
read + wakeup of the input task (scheduler RT)   ~10-20 us (2 context switches; epoll-ready
                                                    wakeups observed immediate in strace)
crossterm decode of 1-6 B escape input            ~1 us
action dispatch + state mutation                  ~1 us
changed-line re-render + paint write              ~2 us  (render-churn per-line + paint floor)
                                                 ---------
floor                                            ~13-25 us per keypress-to-paint
```

## Measured cost — noise-rejected

R2 lane 4: median 1.935 ms, **rs 26.98% — noise gate FAIL**; collection wall 0.44 s.
The distribution cannot feed a verdict (remediation ladder: pin governor, isolate,
widen samples, enlarge input). **Multiple unproven; OPEN by the fail-closed rule.**

Working observation (not a claim): if the noisy median holds after remediation, the
unit sits ~100x over the ~13-25 us floor, consistent with a full-root rebuild per
keypress (render-churn ledger shows a full frame at ~212 us) plus wait shapes.

## Decomposition status

No trusted lane total; no attributed categories asserted. Prerequisite: noise
remediation per the R2 ladder, then the lane re-enters with paint subtracted
(terminal-paint.md) and the remainder attributed onto the input/dispatch legs.
