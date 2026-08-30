# Floor ledger: memory resource units (terminal state, stream-load growth)

> **Historical regression witness**: The measured distributions and degradation disclosures in this ledger reflect historical runs against previous reference checkouts. All legacy paths and commit identifiers herein are preserved historical witnesses and are excluded from canonical closure metrics.

Owning R2 hot rows (lane 10): *Terminal state*, *Stream-load memory growth* — hot by
input-scaling, graded in the resource currency. State: **GRADED — PERF-T14 cold grading
in the bytes currency (docs/performance/PERF-T14-cold-grading.md); distribution
recorded at PERF-T11 iteration 29; no wall-clock claim.**

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

## Measured cost — distribution recorded (PERF-T11 iteration 29, 2026-08-29)

Prerequisite satisfied: one full `verify:performance` run on the canonical tree
(`6318fa3` + measurement-harness resilience commit) captured the PERF-T1 memory keys
(`idleProcessTreeMemory`, `streamProcessTreeMemory`; process-tree RSS/PSS, 5 samples per
implementation, 50 ms cadence, 1 s steady/load windows; artifact
`target/bench/performance-comparison.json`). Full-run context: the wall-clock gates ran
first and their verdict (noise rejection on the keypress wall lane, rs 29.49% — owned by
the keypress-dispatch unit) completed before memory collection, so the keys remain
non-gating and post-verdict.

**Terminal state** (extension-free idle after first frame, steady-window max tree RSS):

| impl | RSS median | RSS min-max | PSS median | floor | retained/floor (RSS) |
|---|---|---|---|---|---|
| rust | 25,362,432 B | 25,255,936-25,489,408 | 16,147,456 B | 72,000 B | ~352x |
| typescript | 125,042,688 B | 124,301,312-125,812,736 | 118,487,040 B | 72,000 B | ~1,737x |

Floor: 100x30x24 B = 72,000 B; transcript empty at idle. Dominant retained term: the
process baseline (binary text/data, runtime heaps, allocator arenas) — the grid plus
empty-transcript state is <=~100 KiB of the measured total.

**Stream-load growth** (one streamed turn, 256 x 24 B = 6,144 B transcript):

| impl | load-window max tree RSS median | min-max | PSS median |
|---|---|---|---|
| rust | 145,068,032 B | 142,938,112-146,386,944 | 133,730,304 B |
| typescript | not captured (n=0) | — | — |

Growth over the idle lane (rust): ~119.7 MB RSS. Floor: 6,144 B + ~170 B/entry -> 6,314 B
(single message entry) to 49,664 B (256-frame sensitivity); retained/floor ~18,959x to
~2,410x. Caveat named: the stream-load lane's process tree differs from the idle lane's by
construction (verification extension + extension host join the tree), so this multiple
bounds the whole-tree footprint under load, not transcript retention alone — retained
transcript bytes are <=~0.005% of the measured growth, and churn above retained state is
owned by the timing ledgers.

TypeScript stream-load is disclosed as a lane degradation in the artifact
(`harness.laneDegradations`): the reference build (.references/pi `4e4949299`) accepts the <!-- historical witness -->
submitted prompt but never streams offline — an upstream reference regression, not a
pi-oxidized measurement.

**Disposition: OPEN (fail-closed) resolved by this recorded distribution; both hot rows
sit far above 2x floor in bytes with the dominant term named; per this ledger's Phase-6
rule, graduation transfers to PERF-T14 (#100) cold grading in the resource currency. This
unit carries no wall-clock claim.**
