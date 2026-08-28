# PERF-T14 cold-unit grading: fixed, at-floor, or left

> Resolves [issue #100](https://github.com/metaphorics/pi-oxidized/issues/100) (PERF-T14).
> Grading date 2026-08-29; graded tree = `feat/ver-align-canonical-pin` at `8794486`.
> Cold-list sources: the PERF-R2 hot/cold unit-split tables
> ([docs/PERF-R2-workload-surface-ranking.md](../PERF-R2-workload-surface-ranking.md):
> lane 1 :55-63, lane 2 :94-104, lane 3 :131-142, lane 4 :161-169, lane 5 :200-209,
> lane 6 :226, lane 7 :243-253, lane 9 :297-307, lane 10 :324-333, lane 11 :357) and the
> floor-ledger coverage note ([docs/performance/floors/README.md](floors/README.md),
> "Cold rows ... are graded in the Phase-6 cold pass"). PERF-R8 amended lane baselines
> only and adds no split rows (no hot/cold table in `docs/PERF-R8-paired-baselines.md`).
> Method: every written Cold row enumerated in full; exactly one verdict per row
> (FIXED / AT-FLOOR / LEFT), each anchored to recorded evidence; cold rules binding: a
> fix that adds a branch, cache, or config knob to buy microseconds is rejected and
> reclassified LEFT. Memory-resource-units is graded here in the bytes currency per the
> PERF-T11 iteration-29 transfer; it carries no wall-clock claim.

## Verdict

**Complete.** 12 written cold rows carry exactly one verdict each (2 AT-FLOOR, 10 LEFT,
0 FIXED); zero outstanding. The memory-resource-units graduation (2 rows, bytes
currency) is recorded LEFT with the dominant retained term named. No cold fix was
landed and none was rejected mid-flight: every campaign win is a hot-unit cost booked
to its timing ledger, and no doc-derivable cold fix exists that is not a
branch/cache/config-knob trade, which the cold rules reject.

## Coverage: the written cold lists in full

| Lane | Written cold rows | Split-table anchor |
|---|---|---|
| 1: Startup `--version` | OS process creation; Executable/runtime loading | R2 :55-63 |
| 2: Extension-free first frame | Process creation + runtime loading | R2 :94-104 |
| 3: Streaming tail-frame CPU | Process startup | R2 :131-142 |
| 4: Keypress-to-paint | none ("No cold units", steady-state interactive) | R2 :161-169 |
| 5: Extension-host JSONL/RPC | Extension fan-out/registration; Timeout/locality correctness | R2 :200-209 |
| 6: Rust `serve_io` | inherits lane 5's rows ("same as lane 5"); no new rows | R2 :226 |
| 7: Layout/recomposition churn | V8 heap profiler sampling | R2 :243-253 |
| 8: Tool dispatch | no split table written (baseline-only section, lane added by PERF-T5); see finding F2 | R2 :257-282 |
| 9: Session persistence/reopen | Page-cache warm/cold | R2 :297-307 |
| 10: Idle/stream process-tree memory | Resident executable/runtime pages; Allocator baseline; Model/config state; Task/event-loop stacks | R2 :324-333 |
| 11: Launcher artifact size | N/A ("static artifact property") | R2 :357 |

Twelve cold rows total. The floors-README coverage note named five concepts where these
twelve rows express seven distinct concepts; the note is corrected in this ticket's
commit (finding F1).

## Grading table

| # | Lane | Written cold row | Verdict | Cost named / reason (anchor) |
|---|---|---|---|---|
| 1 | 1 | OS process creation | AT-FLOOR | execve + page-in is the kernel floor of producing the observable. R2 recorded cold≈warm (40.07 vs 40.93 ms, "cache-drop has minimal marginal effect", R2 :65); iteration-28 E1: post-fix wall 3.7 ms with 0.95 ms syscall time dominated by execve, "outside any in-process lever" (t11-iterations.md, iter. 28 E4). |
| 2 | 1 | Executable/runtime loading | LEFT | ELF dynamic relocation + symbol binding of the 27.7 MB dynamically linked launcher: ~771 kIr, 81.6% of the post-fix 945,385 Ir (iter. 28 E1). Addressable only via consent-gated artifact-shape levers (static linking, projected ~109x; RELR; prelink/ld.so cache), all outside the dependency boundary; reopen conditions recorded (iter. 28 E4). |
| 3 | 2 | Process creation + runtime loading | LEFT | Bundle of rows 1-2 on the first-frame lane: creation share at the kernel floor (row 1), loading share consent-gated (row 2, same iter.-28 evidence). First-frame improvements in T11 (243.61 → ~127 ms) were hot-unit construction/paint overlap, not this row. |
| 4 | 3 | Process startup | LEFT | Same physics per streaming sample (exec + runtime load), one-time and amortized over 256 frames; the dominant component is the ELF loader, consent-gated (row 2 evidence). No in-boundary lever. |
| 5 | 5 | Extension fan-out/registration | LEFT | One-time O(100-factory) startup registration (R2 :207). Lane 5 noise-failed wholesale (rs 29-116%), so no trusted distribution isolates registration cost; T11 iterations 22-26 addressed per-request dispatch only. Lazy registration would add a startup branch to buy unmeasured microseconds: rejected under the cold rules. |
| 6 | 5 | Timeout/locality correctness | LEFT | Contract-required one-time correctness slow path (R2 :209). Optimizing it trades correctness instrumentation for nothing observable; one-time, not input-scaling. |
| 7 | 7 | V8 heap profiler sampling | LEFT | Upstream-only measurement-instrument setup (V8 heap-profiler attach, once per scenario in the upstream churn bench, R2 :251). It prices the instrument, not the product; the PERF-T3 Rust peer has no V8 counterpart. Nothing product-owned to fix. |
| 8 | 9 | Page-cache warm/cold | LEFT | Platform page-cache state on reopen (R2 :305). The trusted reopen lane measured the warm path (AT-FLOOR 1.97x, iterations 13/14). A cold-cache lane would add fadvise-style instrumentation to observe kernel-owned page-in and informs no product lever; never landed, per the cold rules. |
| 9 | 10 | Resident executable/runtime pages | LEFT (bytes) | Dominant retained term of the memory lanes: the process baseline (binary text/data, runtime heaps, allocator arenas). Idle tree RSS rust 25,362,432 B / TS 125,042,688 B against the 72,000 B terminal-state floor: ~352x / ~1,737x (iter. 29). Addressable only via artifact-size/dependency consents (row 2 levers). Bytes currency; never a wall-clock claim. |
| 10 | 10 | Allocator baseline | LEFT (bytes) | One-time allocator arenas inside the named process-baseline term (iter. 29 attribution). No recorded decomposition isolates it; an allocator swap or tuning knob buys bytes with a config knob: rejected under the cold rules. Hot-path allocation wins (churn 28.3 KiB/frame, dispatch) are booked to the timing ledgers, not here. |
| 11 | 10 | Model/config state | AT-FLOOR (bytes) | Contract-required state retained once after construction, which is the ledger's own bytes-floor definition ("the turn's text retained once", memory-resource-units.md). No campaign iteration or audit record names duplicate or redundant retention of this state; all recorded allocation findings concern per-frame churn, booked to the timing ledgers. |
| 12 | 10 | Task/event-loop stacks | LEFT (bytes) | Required async-runtime worker stacks + event loops, one-time after startup. Bounded parallelism is contract-load-bearing (cancellation symmetry + force-abort, iter. 20 E2); worker-count tuning is a config knob buying bytes: rejected under the cold rules. Inside the process-baseline term named in row 9. |

## memory-resource-units graduation (bytes currency, iteration-29 distribution)

Per the ledger's Phase-6 rule and T11 iteration 29, the unit's two hot rows are graded
here in the resource (bytes) currency from the recorded distribution
(`docs/performance/floors/memory-resource-units.md`; artifact
`target/bench/performance-comparison.json`, full run, `taskset -c 20-40`). They can
never carry a wall-clock claim.

| Row | Recorded measurement | Bytes floor | Dominant retained term | Verdict |
|---|---|---|---|---|
| Terminal state (idle tree) | rust RSS 25,362,432 B (PSS 16,147,456 B), ~352x; TS RSS 125,042,688 B (PSS 118,487,040 B), ~1,737x | 100x30x24 B = 72,000 B (transcript empty at idle) | process/runtime baseline (binary text/data, runtime heaps, allocator arenas); grid + empty transcript ≤~100 KiB of the total | LEFT (bytes): the multiple's content is the process baseline, owned by cold rows 9-10 (consent-gated); the unit-own retained state is upper-bounded at ≤~100 KiB ≈ ≤~1.4x its floor, i.e. itself at floor; no in-boundary lever on the dominant term |
| Stream-load growth (one turn, 256 x 24 B) | rust load-window RSS 145,068,032 B (PSS 133,730,304 B); growth over idle ~119.7 MB, ~18,959x to ~2,410x; TS n=0 (upstream no-stream regression, disclosed in `harness.laneDegradations`) | 6,314 B (single entry) to 49,664 B (256-frame sensitivity) | whole-tree footprint under load: the stream tree adds the verification extension + extension host by construction; retained transcript bytes ≤~0.005% (~≤6 KiB, ≈ ≤~1x floor: the turn's text retained once) | LEFT (bytes): the multiple bounds tree footprint, not transcript retention; retention itself is at the ledger's retained-once floor; churn above retained state is owned by the timing ledgers |

Ledger state line updated to GRADED in this ticket's commit (the graduation lands here).

## Fixed verdicts

None. Every campaign commit that removed cost removed a hot-unit cost and is booked to
its timing ledger (render-churn-recomposition, terminal-paint, stream-frame-pipeline,
session-reopen, session-append, keypress-dispatch AT-FLOOR; first-frame-init,
tool-dispatch-slice, extension-rpc-dispatch, startup-version-path
CONSTRAINED-ABOVE-FLOOR; #97 trail). No cold row's cost was removed by any landed
commit, and no remaining cold cost admits a fix that is not a branch/cache/config-knob
trade for unmeasured or kernel-owned bytes, which the cold rules reject. Zero fixes
were attempted, landed, or rejected mid-flight. The acceptance clause "each fixed
verdict names the removed cost and its verifier-green commit" therefore holds
vacuously.

## Finding 1 (record, fixed): the floors-README coverage note under-enumerated the cold set

`floors/README.md` named five cold concepts (process creation, runtime loading,
page-cache effects, extension registration, V8 profiler setup) where the R2 split
tables write twelve cold rows expressing seven distinct concepts: the lane-5
timeout/locality row and all four lane-10 memory rows were unnamed. Docs-only and
mechanically derivable from the R2 tables; corrected in this ticket's commit by
completing the enumeration and pointing the note at this document.

## Finding 2 (record, erratum): lane 8 has no written hot/cold split row

Every other measured lane carries a split table; lane 8 (added to the inventory by
PERF-T5) records baseline + boundary only. Erratum: lane 8's timed slice contains no
cold unit, since worker process startup amortizes outside the slice boundary (10 fresh
samples x 10,000 calls; boundary field, R2 :257-282). The R2 record is historical and
is not edited; this erratum completes it.

## Finding 3 (record, fixed): the floors-README ledger index contradicted the terminal campaign verdicts

The ledger-index table in `floors/README.md` still carried the pre-campaign states
(eleven OPEN / fail-closed rows, keypress measured cost quoted from the superseded
1.935 ms noise-failed lane, memory quoted "none (artifact incomplete, R8)") after the
individual ledger headers were terminal-synced at `8794486`. The close comment's
"floor ledger header synchronized" claim covered the ledgers, not the index. Docs-only
and mechanically derivable from the synced headers and the #97 terminal table;
corrected in this ticket's commit by syncing the State column (and the two stale
measured-cost/multiple cells) to the terminal verdicts.

## Provenance

- Cold-list sources: `docs/PERF-R2-workload-surface-ranking.md` split tables (anchors
  above); `docs/performance/floors/README.md` coverage note.
- Evidence: `docs/performance/t11-iterations.md` iterations 5-32 (esp. 28 E1/E4, 29);
  `docs/performance/floors/*.md` (headers synced at `8794486`); issue #97 comment trail
  (campaign stop-condition comment, terminal table); issue #94/#98 close comments.
- Grading discipline: one verdict per row; cold rules binding; no wall-clock claim on
  bytes-currency rows; historical records unedited; corrections as F1 and F3 (README,
  this commit) and F2 (erratum here).

## Audit erratum (PERF-G15, #101)

The "Fixed verdicts" enumeration above lists `stream-frame-pipeline` in the
AT-FLOOR group. Its terminal classification is **architecture-floor**
(terminal, iteration 13 — residual classified, multiples unproven): the
floors-README ledger index ("architecture-floor (terminal, iteration 13)"),
the `stream-frame-pipeline.md` header, and the #97 campaign terminal table
all record that state, and the ledger's own verdict predates this document.
The enumeration's substance — every campaign win is a hot-unit cost booked
to its timing ledger — is unaffected: the iteration-10/12 Arc-at-birth wins
are booked to `stream-frame-pipeline.md` and the unit's terminal record is
iteration 13. Recorded here as an addendum; the original sentence is
historical record and is not rewritten. All other PERF-G15 checks passed
(no cold fix landed — 0 FIXED corroborated by a commit-by-commit sweep of
the campaign's code commits against the cold rows; part (b) holds vacuously;
no CONSTRAINED residual term touched outside Phase-5 discipline).
