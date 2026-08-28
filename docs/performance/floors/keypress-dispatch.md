# Floor ledger: keypress dispatch (key write -> input dispatch to state mutation)

Owning R2 hot rows (lane 4): *PTY key write*, *Input dispatch to state mutation*.
(The paint row is owned by terminal-paint.md.)
State: **AT-FLOOR (terminal, iteration 32, 2026-08-29) — trusted operative lane
(iteration 31), attribution complete, owned floor revalidated:
median(T_owned) / revalidated owned floor = 1.05–1.15x <= 2.0.**

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

## Floor (revalidated, iteration 32 — same-harness measurements)

```
same-harness raw PTY/observer arm (fixture raw mode; script(1)+kernel+Bun
  observation of one balanced synchronized transaction per byte)   81.2 us  (rs 11.32%)
production EventStream decode + wakeup + mpsc handoff
  (paired input-arm differential, 27 interleaved pairs)            18.3 us  (1.23x, 1/27 sign flips)
measured minimal dispatch/state mutation (production D term:
  median 91.3 / p5 81.6 / min 75.2)                          75.2–91.3 us
                                                             -----------
owned floor                                       174.7–190.8 us per keypress
full semantic floor (+ same-key R 69.0 + P 31.0 once)           290.8 us
```

The historical ~13-25 us class estimate is superseded: it priced a pipe write
and two context switches with no observation transport; the same-harness raw
arm alone measures 81.2 us. R and P are excluded from the owned floor and
owned by render-churn-recomposition and terminal-paint.

## Measured cost — R2 protocol (historical, noise-rejected)

R2 lane 4: median 1.935 ms, **rs 26.98% — noise gate FAIL**; collection wall 0.44 s.
The distribution cannot feed a verdict (remediation ladder: pin governor, isolate,
widen samples, enlarge input). **Multiple unproven; OPEN by the fail-closed rule.**

Working observation (not a claim): if the noisy median holds after remediation, the
unit sits ~100x over the ~13-25 us floor, consistent with a full-root rebuild per
keypress (render-churn ledger shows a full frame at ~212 us) plus wait shapes.

## Measured cost — trusted (repaired protocol; operative: iteration 31, 2026-08-29)

Protocol: 3 discarded process warmup rounds, then 27 fresh measured process
rounds; each round = one idle extension-free editor child under `taskset -c 20`
(governor `powersave`, recorded) with 20 discarded warmup key-clear pairs and
200 measured key-clear pairs on a fixed empty editor (`Ctrl+U` clear outside
timing, verified to restore the empty editor: the previous key must be absent
from the next paint and from the clear repaint's printable cells). Interval =
write receipt (elapsed captured immediately before the first `FileSink.write`)
to the arrival of the chunk completing the first balanced DEC 2026 transaction
containing the typed key; a row-local fallback, extra/missing markers, or a
payload mismatch fails the whole round (no sample filtering, no concurrent 1 ms
sampler). Trust estimator: population stddev / median over the 27 round medians;
pooled raw spread disclosed, not gating.

**Operative result (iteration 31, post first-frame stdin fix): trusted —
round-median rs 13.95%** (27 round medians: median 288.26 us, min 273.30 us,
max 445.85 us); collection wall 11.34 s (>= 1 s PASS); 5,400/5,400 samples
synchronized and key-correlated, 0 invalid frames; pooled raw median 291.90
us, p95 437.23 us, p99 532.32 us (behavior gate < 5 ms PASS; pooled raw
spread 64.6% disclosed — one 11.13 ms scheduler hiccup in the tail); binary
sha256 `8af89dd1…` (measurement collector + first-frame stdin fix).

History: the iteration-30 initial capture on a production tree identical to
`6318fa3` (binary `58592a9d…`) measured median 467.59 us pooled / 467.27 us
round-median, rs 2.69% — trusted by the gates but inflated: the startup probe
collector still owned stdin into the first ~30 samples of each round, so early
keys were re-injected late through the synthetic mapper. The iteration-31
first-frame stdin fix removed that interference; the numbers above are the
operative lane. Multiple vs floor: resolved by the iteration-32 attribution
and floor revalidation (AT-FLOOR, below).

## Decomposition and verdict (iteration 32, 2026-08-29 — AT-FLOOR, terminal)

Temporary env-gated S0–S3 stage marks at the plan's seams (sidecar written
only after paint; perturbation +1.14% < 5% PASS, rs 5.60% vs 7.04% uninstr-
umented) plus the raw-vs-EventStream fixture differential decomposed 5,000
correlated measured keys (4,990 clean; 10 correlation failures excluded, never
clamped; 25/27 children fully correlated, 3 of 11,880 cycles lost):

| Stage | Median | Share of T | Ownership |
|---|---|---|---|
| Q (channel/scheduler handoff) | 10.6 us | 3.50% | keypress |
| D (dispatch/state mutation) | 91.3 us | 30.27% | keypress |
| R (render/recomposition) | 69.0 us | 22.88% | render-churn (booked once) |
| P (diff/encode/write/flush) | 31.0 us | 10.29% | terminal-paint (booked once) |
| X (PTY ingress + script(1)/kernel/observer) | 96.6 us | 31.99% | boundary |
| T (inclusive) | 301.7 us | 100% | — |
| **T_owned = Q + D + X** | **201.2 us** | 66.70% | keypress |

Round-median rs per stage 2.95–5.57% (trusted). Zeroing ceilings: Q 1.036x
(< 1.05x, rejected by construction; at the scheduler floor), D 1.434x,
R 1.297x, P 1.115x, X 1.470x. No candidate ladder reached: the owned multiple
closed at <= 2x first.

**Verdict: AT-FLOOR** — median(T_owned) 201.2 us / revalidated owned floor
174.7–190.8 us = **1.05–1.15x <= 2.0** under every dispatch-floor
interpretation (full semantic floor 290.8 us; inclusive multiple 1.04x).
Dominant residual: X (boundary observation stack) and the scheduler hop Q.
Reopen conditions: an observation-transport boundary consent (barred — new
lane), or fresh trusted evidence of a materially cheaper semantically-
equivalent dispatch path. Full E1–E4: `t11-iterations.md` iteration 32.
PERF-T11 remains OPEN for the remaining units.
