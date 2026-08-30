# PERF-CLOSE: Final performance acceptance evidence for issue #22

> Resolves [issue #105](https://github.com/metaphorics/pi-oxidized/issues/105) (PERF-CLOSE).
> Closing acceptance for [issue #22](https://github.com/metaphorics/pi-oxidized/issues/22)
> ("Define performance superiority acceptance").

Tree: `feat/ver-align-canonical-pin` at `c3dfcb7` (tree `cf34a1f`). Machine: Xeon Gold 6138, 80 cores. Linux 7.0.0-30-generic. Run date: 2026-08-29.

## Closing acceptance run

Three verification lanes ran on a drained box with zero external shard workers. Pinning: `taskset -c 37,77` (performance), `taskset -c 41-55` (extension scaling), and `taskset -c 60-79` (e2e). Cargo used `CARGO_BUILD_BUILD_DIR=/home/alpha/.cargo/ws/pi-oxidized-build/{workspace-path-hash}` and `CARGO_TARGET_DIR=/home/alpha/harness/pi-oxidized/target`. `TMPDIR` used the home filesystem, and `RUSTC_WRAPPER` was empty.

### Lane 1: verify:performance -> performance-comparison.json

Artifact: `target/bench/performance-comparison.json` (check 9, 2026-08-29T11:59:28Z).

| Release minimum | Predicate | Recorded measurement | Result |
|---|---|---|---|
| Version cold >= 3x | `versionSpeedups.cold >= 3` | 29.884x (Rust 19.75 ms / TS 590.11 ms, rs 5.79% / 4.03%) | PASS |
| Version warm >= 3x | `versionSpeedups.warm >= 3` | 27.029x (Rust 23.11 ms / TS 624.55 ms, rs 19.76% / 15.99%) | PASS |
| First-frame cold >= 3x | `firstFrameSpeedups.cold >= 3` | 8.117x (Rust 82.88 ms / TS 672.76 ms, rs 16.35% / 8.07%) | PASS |
| First-frame warm >= 3x | `firstFrameSpeedups.warm >= 3` | 7.104x (Rust 96.86 ms / TS 688.13 ms, rs 8.58% / 3.80%) | PASS |
| Stream thresholdValid | `thresholdValid = true` | true (zero Rust and zero TypeScript starvation samples; 20 samples per implementation) | PASS |
| Stream speedup >= 2x | `streamSpeedup >= 2` | 2.229x (Rust 0.938 ms/frame / TS 2.090 ms/frame, rs 5.35% / 5.63%) | PASS |
| Keypress p99 < 5 ms | `keypressSummary.p99 < 5` | 0.516 ms (median 0.275 ms, 5400 synchronized samples, 10,307.811 ms wall, 0 invalid frames) | PASS |
| Keypress wall >= 1 s | `collectionWallMs >= 1000` | 10,307.811 ms | PASS |

All eight release minimum predicates pass. Every round-median noise distribution has `relativeSpread <= 20%`. The artifact records `pass = true` and `blockers = []`.

### Lane 2: verify:extension-scaling -> extension-scaling.json

Artifact: `target/bench/extension-scaling.json` (check 8, 2026-08-29T12:05:32Z).

| Gate | Threshold | Recorded value | Result |
|---|---|---|---|
| idle100 within 10% of zero | `|idle - zero| / zero <= 0.10` | idle100 keypress median 0.008 ms vs zero 0.008 ms (idle100 kp p99 0.024 ms, zero kp p99 0.025 ms; idle100 frame p99 0.025 ms, zero frame p99 0.024 ms) | PASS |
| FastTerminalInput p99 | `< 5 ms` | 0.033 ms (median 0.011 ms) | PASS |
| slowTerminalInput timeout/locality | first < 4 s, second < 5 ms, 1 active handler | first 58.789 ms, second 0.110 ms, 1 active handler | PASS |

All extension-scaling gates pass (`failures = []`). The artifact records `pass = true`. Rust provenance entrypoint: `pi_ext::server::serve_io`. The 27-round-median noise estimators satisfy the 20% spread limit. The pooled operation populations remain unfiltered because they supply the p99 behavior gates; their raw spread records real tail latency and does not estimate run-to-run noise. Measured keypress and frame populations:
- Zero extensions: p99 keypress 0.025 ms, p99 frame 0.024 ms (median keypress 0.008 ms, median frame 0.008 ms).
- 100 idle extensions: p99 keypress 0.024 ms, p99 frame 0.025 ms (median keypress 0.008 ms, median frame 0.008 ms).
- 20 active extensions: p99 keypress 0.025 ms, p99 frame 0.040 ms (median keypress 0.010 ms, median frame 0.010 ms).
- Fast terminal input: p99 0.033 ms, median 0.011 ms.
- Slow terminal input: first 58.789 ms, second 0.110 ms, one active handler.

### Lane 3: verify:e2e -> e2e evidence

Artifact: `target/verification/e2e/run-5WTjS6` (check 11, 2026-08-29T12:05:41Z – 2026-08-29T12:06:08Z).

| Step | Status | Evidence |
|---|---|---|
| 1. rust-interactive-tools-steering-compaction | PASS | session started, tools/steering executed (14 entries, sha256 c73da34bbaf72de716b82ce949d2d5a6d9cb53204fe14d9c5ff2e17720536a6f) |
| 2. rust-extension-flag-session-start | PASS | compatibility marker seq 1, profile = rust-compatibility-profile |
| 3. rust-extension-shortcut-dispatch | PASS | compatibility marker seq 3, sequence = CSI 120;6u (ctrl+shift+x dispatched) |
| 4. rust-extension-dialogs | PASS | compatibility markers seq 5-15, operationId = verification-dialogs-v1 (select/confirm/input/editor all returned correct values) |
| 5. rust-extension-custom-ui | PASS | compatibility marker seq 21, state: updated (Rust custom UI state updated) |
| 6. rust-fork | PASS | session forked (14 entries, sha256 4bfb93483dda876c475e7c76aa5fb769bb9522ffdaf09d47cdc559806c5c58ee) |
| 7. rust-resume-reload | PASS | session resumed, loadGeneration 3 -> 4, sha256 preserved (4bfb93483dda876c475e7c76aa5fb769bb9522ffdaf09d47cdc559806c5c58ee) |
| 8. rust-extension-reload-flag-preservation | PASS | extension reloaded, replacementInstance 2995821:1788005163883, sessionStartMarkerIndex 27, observationMarkerIndex 29 |
| 9. typescript-real-session-reopen | PASS | TypeScript real-session reopen without mutation (380,252 bytes, sha256 4bfb93483dda876c475e7c76aa5fb769bb9522ffdaf09d47cdc559806c5c58ee preserved, loadGeneration 5) |
| 10. typescript-extension-flag-session-start | PASS | compatibility marker seq 31, profile = typescript-compatibility-profile |

All 10 e2e steps pass. The run root is `run-5WTjS6` with status `pass` and 32 compatibility markers verified. Rust custom UI state updated successfully. Fork, resume, and reload verified. TypeScript real session reopened without mutation.

## Per-lane table: 11 hot units

| # | Unit | Lane | Achieved multiple | Verdict | Unresolved-gap record (above-2x lanes) |
|---|---|---|---|---|---|
| 1 | render-churn-recomposition | 7 | ~1.25–1.35x (terminal) | AT-FLOOR | (at floor) |
| 2 | terminal-paint | 2,3,4,7 | 1.97x / 1.50x / 1.38x (terminal) | AT-FLOOR | (at floor) |
| 3 | session-append | 9 | 1.41x (terminal) | AT-FLOOR | (at floor) |
| 4 | session-reopen | 9 | 1.97x (terminal) | AT-FLOOR | (at floor) |
| 5 | keypress-dispatch | 4 | 1.05–1.15x owned (terminal) | AT-FLOOR | (at floor) |
| 6 | stream-frame-pipeline | 3 | 6.91x (drain 1,382 ns/frame, rs 8.91%) | CONSTRAINED-ABOVE-FLOOR | Dominant residual: contract-forced one full-snapshot materialization per frame (extensions consume the serialized partial; the watch leg needs a complete latest-wins snapshot). Attempted designs: E1–E4 exhausted (iteration 33); source-materialization scenario measured 441 ns (31.9%), drain scenario 1,382 ns (8.91% - PASS). Floor revalidation: the stream floor remains ~0.2 µs/frame; fresh trusted drain 1,382 ns/frame. Contract constraint: the per-frame full-snapshot materialization is load-bearing for the extension watch leg; removing it crosses the extension-host boundary (published surface). |
| 7 | tool-dispatch-slice | 8 | 5.62x (terminal) | CONSTRAINED-ABOVE-FLOOR | Dominant residual: argument validation + tool start/update/end events + result construction + append. Attempted designs: E1–E4 exhausted (iteration 20); the 4.29 µs floor is the owned-code floor (validation + event construction + append). Floor revalidation: floor revalidated from contract (call-site derived). Contract constraint: the event sequence (start/update/end) is a published extension-host surface; the append is the session JSONL v3 wire format (boundary surface). |
| 8 | startup-version-path | 1 | ~2480x CPU (terminal; iteration-27 win banked 1.59x wall / 3.99x IR; residual ~591x de-minimis) | CONSTRAINED-ABOVE-FLOOR | Dominant residual: ELF dynamic relocation + symbol binding of the 27.7 MB dynamically linked executable artifact (~771 kIr, 81.6% of post-fix 945,385 IR). Attempted designs: E1–E4 exhausted (iteration 28); the residual is the loader, outside any in-process lever. Floor revalidation: 0.15 µs in-process floor; the 0.37 ms wall is dominated by execve + page-in (kernel floor). Contract constraint: static linking / RELR / prelink are consent-gated artifact-shape levers outside the dependency boundary. |
| 9 | first-frame-init | 2 | 4.66x cold / 5.06x warm (superseded load-degraded iteration-34 re-attestation, post-`9ead528`); 8.117x cold / 7.104x warm (current closing run) | CONSTRAINED-ABOVE-FLOOR | Dominant residual: the 25 ms `PROBE_FIRST_BYTE_TIMEOUT` first-byte window on silent terminals (the yield-armed probe collector serializes ahead of the first paint after `9ead528`). Attempted designs: E1–E4 exhausted (iteration 34); the fix buys a mutation-verified correctness property (EventStream owns stdin from the first frame; bracketed pastes no longer corrupt). Floor revalidation: ~1.50 ms floor; post-fix operating point ~118.6 ms on this box. Contract constraint: any latency recovery trades against the correctness fix's stdin-ownership guarantee. Regression accepted by owner disposition …
| 10 | extension-rpc-dispatch | 5,6 | <= 3,058 ns trusted (terminal) | CONSTRAINED-ABOVE-FLOOR | Dominant residual: the cross-task handoff (request -> extension host -> response -> reply channel). Attempted designs: E1–E4 exhausted (iteration 26); the hop attribution isolated the Q producer-fed column damage as the non-redundant capture point. Floor revalidation: ~1 µs/req floor; fresh trusted <= 3,058 ns. Contract constraint: removing the cross-task handoff crosses the extension-host boundary (published RPC surface); the Q producer-fed column damage is the only non-redundant capture point, and fail-open keeps under-recording impossible on un-instrumented paths. |
| 11 | memory-resource-units | 10 | ~352x / ~1,363x (bytes, idle tree RSS) | GRADED (bytes currency) | Dominant retained term: process/runtime baseline (binary text/data, runtime heaps, allocator arenas); the unit-own retained state is upper-bounded at <= ~100 KiB ≈ <= ~1.4x its 72,000 B floor. Addressable only via consent-gated artifact-shape levers (static linking, RELR, prelink). Bytes currency; never a wall-clock claim. |

## Cold grading summary: 12 rows

| # | Lane | Cold row | Verdict |
|---|---|---|---|
| 1 | 1 | OS process creation | AT-FLOOR |
| 2 | 1 | Executable/runtime loading | LEFT |
| 3 | 2 | Process creation + runtime loading | LEFT |
| 4 | 3 | Process startup | LEFT |
| 5 | 5 | Extension fan-out/registration | LEFT |
| 6 | 5 | Timeout/locality correctness | LEFT |
| 7 | 7 | V8 heap profiler sampling | LEFT |
| 8 | 9 | Page-cache warm/cold | LEFT |
| 9 | 10 | Resident executable/runtime pages | LEFT (bytes) |
| 10 | 10 | Allocator baseline | LEFT (bytes) |
| 11 | 10 | Model/config state | AT-FLOOR (bytes) |
| 12 | 10 | Task/event-loop stacks | LEFT (bytes) |

2 AT-FLOOR, 10 LEFT, 0 FIXED. Zero outstanding. No cold fix was landed and none was rejected mid-flight: every campaign win is a hot-unit cost booked to its timing ledger, and no doc-derivable cold fix exists that is not a branch/cache/config-knob trade, which the cold rules reject.

## Memory bytes-currency grades

| Row | Recorded measurement | Bytes floor | Dominant retained term | Verdict |
|---|---|---|---|---|
| Terminal state (idle tree) | rust RSS 25,309,184 B (~352x); TS RSS 98,099,200 B (~1,363x) | 72,000 B (100×30×24, transcript empty at idle) | process/runtime baseline; grid + empty transcript <= ~100 KiB | LEFT (bytes): multiple's content is the process baseline, owned by cold rows 9–10 (consent-gated); unit-own retained state <= ~1.4x floor |
| Stream-load growth (one turn) | rust load-window RSS 121,507,840 B; TS load-window RSS 114,245,632 B | 6,314–49,664 B | whole-tree footprint under load; retained transcript bytes <= ~6 KiB ≈ <= ~1x floor | LEFT (bytes): multiple bounds tree footprint, not transcript retention; retention itself at the ledger's retained-once floor |

The ~1,737x TypeScript multiple in the floor index records the earlier PERF-T14
run. The check-9 closing artifact supplies the current per-run values above.

## Environmental hazards disclosed

1. **Physical core isolation for performance benchmarking**: The performance benchmark suite requires a quiet SMT sibling pair such as `taskset -c 37,77`. A run pinned to one logical CPU still encountered shared-core contention through its sibling. The quiet sibling pair produced stable round medians across repeated fresh processes.

2. **TypeScript first-frame /quit timeout escalations**: The TypeScript reference implementation does not terminate within the 10 s `/quit` window during first-frame measurements, resulting in 55 quit-timeout escalations recorded in `harness.quitTimeouts`. The harness escalates process termination to proceed with subsequent benchmark rounds. These escalations occur strictly after first-frame arrival timestamps are captured and do not affect the measured frame arrival or Rust performance numbers.

## Campaign ledger completeness

- The prior campaign contains 33 numbered iterations and one residual-classification record. This historical count is context only; it does not back canonical closure.
- 11 floor ledgers in `docs/performance/floors/`; every State column synced to terminal verdicts.
- `docs/performance/PERF-T14-cold-grading.md`: 12 cold rows graded (2 AT-FLOOR, 10 LEFT, 0 FIXED).
- All four audit blockers CLOSED: #92 (G13), #96 (G12), #99 (G16), #101 (G15).
- G13 findings resolved: iteration 33 (stream-frame-pipeline CONSTRAINED 6.91x, complete E1–E4); iteration 34 (first-frame re-attestation, regression accepted by owner disposition, minimum 4.66x/5.06x vs >= 3x).

## Closure verdict

All performance, extension-scaling, and end-to-end acceptance criteria are satisfied. All eight release minimum predicates pass. Every round-median noise estimator satisfies the 20% spread limit, while the unfiltered operation populations preserve the measured tails used by the p99 gates. The extension-scaling gates pass with zero failures. All 10 end-to-end compatibility steps pass with 32 verified markers. Issue #105 and parent issue #22 acceptance requirements are met.
