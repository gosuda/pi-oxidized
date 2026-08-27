# Floor ledger: memory resource units (terminal state, stream-load growth)

Owning R2 hot rows (lane 10): *Terminal state*, *Stream-load memory growth* — hot by
input-scaling, graded in the resource currency. State: **OPEN (fail-closed — no
recorded distribution).**

## Contract (from call sites, tests, signatures — never internals)

- Terminal state: the TUI's cell buffers and styled-line storage must hold the viewport grid (100x30 in the lanes) and the rendered transcript lines for diffing (Tui::commit consumers, writer.rs:283-343; transcript fixtures pin content correctness).
- Stream-load growth: per streamed turn the visible transcript and the session's in-memory entry list grow by the turn's text (drain/pump contract, stream-frame-pipeline.md); peak RSS is reported by the session-timing artifact (instrumented counters: peakRssBytes per sample).

Boundary classification: retained-state shapes feeding the wire diff are **interior**;
their observable is resource usage only. Unresolved channels: none.

## Floor (computed, bytes currency)

```
terminal state floor:  grid cells x cell width (100 x 30 x ~24 B) + transcript line bytes
                       ~= 72 KiB + O(transcript bytes)
stream growth floor:   the turn's text retained once (view + session entry)
                       ~= transcript bytes + ~170 B/entry
```

Allocated churn above the retained floor is owned by the timing ledgers (render churn
measures 28.3 KiB allocated per frame — allocation reuse is booked there, not here).

## Measured cost — none recorded

R8: the memory collectors exist (PERF-T1) but the current performance-comparison
artifact predates their completion (`idleProcessTreeMemory`/`streamProcessTreeMemory`
keys absent); the lane re-run was blocked by concurrent pi-tui compile breakage at R8
time. Session-timing records peak RSS per sample as a paired counter (Rust 2-16 MB vs
TS 52-105 MB across cells, R8) but no time-series distribution.

**Multiple unproven; OPEN by the fail-closed rule.** Prerequisite: one full
`verify:performance` run capturing the memory keys (non-gating, post-verdict), then
retained-vs-floor comparison per unit. These units may only graduate at Phase-6 cold
grading in the resource currency; they can never carry a wall-clock claim.
