# Floor ledger: keypress dispatch (key write -> input dispatch to state mutation)

Owning R2 hot rows (lane 4): *PTY key write*, *Input dispatch to state mutation*.
(The paint row is owned by terminal-paint.md.)
State: **OPEN — measurement trusted (iteration 30 repaired protocol, 2026-08-29);
attribution and floor revalidation pending.**

## Contract (from call sites, tests, signatures — never internals)

- Harness side: one key write per sample into the child PTY (scripts/verification/pty.ts
  `PtyProcess.writeKeys`); the latency start is the write **receipt** (`outputOffset`
  + `startedElapsedMs`, captured immediately before the first `FileSink.write`,
  after pre-encoding); the stop is the arrival of the chunk completing the first
  balanced DEC 2026 transaction correlated to the typed key (`keySyncTransaction`
  in scripts/verification/performance.ts); immediate repaint bypasses the
  background coalescer (inputPaintBypassesBackgroundCoalescer, performance.ts).
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

This class estimate predates the campaign; it is being revalidated from current
same-harness measurements (raw PTY/observer arm, EventStream decode arm) before
any multiple is computed (iteration 28+).

## Measured cost — R2 protocol (historical, noise-rejected)

R2 lane 4: median 1.935 ms, **rs 26.98% — noise gate FAIL**; collection wall 0.44 s.
The distribution cannot feed a verdict (remediation ladder: pin governor, isolate,
widen samples, enlarge input). **Multiple unproven; OPEN by the fail-closed rule.**

Working observation (not a claim): if the noisy median holds after remediation, the
unit sits ~100x over the ~13-25 us floor, consistent with a full-root rebuild per
keypress (render-churn ledger shows a full frame at ~212 us) plus wait shapes.

## Measured cost — trusted (iteration 30 repaired protocol, 2026-08-29)

Protocol: 3 discarded process warmup rounds, then 27 fresh measured process
rounds; each round = one idle extension-free editor child under `taskset -c 20`
(governor `powersave`, recorded; binary sha256 `58592a9d…`, production sources
identical to `6318fa3`) with 20 discarded warmup key-clear pairs and 200 measured
key-clear pairs on a fixed empty editor (`Ctrl+U` clear outside timing). Interval
= write receipt (elapsed captured immediately before the first `FileSink.write`)
to the arrival of the chunk completing the first balanced DEC 2026 transaction
containing the typed key; a row-local fallback, extra/missing markers, or a
payload mismatch fails the whole round (no sample filtering, no concurrent 1 ms
sampler). Trust estimator: population stddev / median over the 27 round medians;
pooled raw spread disclosed, not gating.

Result: **trusted — round-median rs 2.69%** (27 round medians: median 467.27 us,
min 438.01 us, max 506.47 us); collection wall 16.52 s (>= 1 s PASS); 5,400/5,400
samples synchronized and key-correlated, 0 invalid frames; pooled raw median
467.59 us, p95 548.75 us, p99 888.09 us (< 5 ms behavior gate PASS), pooled raw
spread 55.99% (disclosed; per-key tails include a 15.88 ms scheduler hiccup).
**The former 1.935 ms median was an artifact of the old protocol (200-key editor
growth across the round, snapshot-start boundary, accepted fallback frames,
concurrent 1 ms sampler), not an operating point of the lane.** Multiple vs
floor: unproven until attribution and floor revalidation.

## Decomposition status

Trusted inclusive lane total T = 467.6 us median (above). Attribution into
T = X + Q + D + R + P with ownership separation (R once to render-churn, P once
to terminal-paint, charged once) plus the raw-vs-EventStream fixture differential
is the iteration-28 prerequisite. No verdict asserted yet.
