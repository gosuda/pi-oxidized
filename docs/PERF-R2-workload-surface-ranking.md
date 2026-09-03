# PERF-R2: Repo-wide performance workload surface ranking with trusted baselines

> **Historical regression witness**: This document records historical baseline measurements collected against previous reference checkouts. All legacy paths, runner paths, commands, and commit references herein are preserved historical witnesses and are excluded from canonical closure metrics.
>
> Resolves [issue #90](https://github.com/metaphorics/pi-oxidized/issues/90).
> Artifact: `target/bench/performance-comparison.json` (generated 2026-08-26).
> Extension-scaling artifact: `target/bench/extension-scaling.json` (generated 2026-08-26).
> Upstream baseline: `.references/pi` at `8fa7eebd235355522c8104166b4f1f959b4e2f10`. <!-- historical witness -->

## Trusted-baseline criteria

A lane has a **trusted baseline** when all three hold:

1. **Noise gate**: median distribution with relative spread (stddev/median) < 20%, enforced by `scripts/statistics.ts` `requireQuiet` at `NOISE_RELATIVE_SPREAD_LIMIT = 0.2`.
2. **Wall clock >= 1 s**: total collection wall clock for the lane exceeds 1 second, so the distribution is not dominated by timer resolution or scheduler granularity.
3. **Alternating order**: implementations are interleaved per sample to cancel sequential drift. `implementationOrder(index)` in `performance.ts` alternates `[rust, typescript]` / `[typescript, rust]`.

Lanes that fail any criterion carry no trusted baseline and are marked accordingly.

## Claim classes

| Class | Definition |
|---|---|
| **Paired comparative** | Both Rust and TypeScript execute the same workload with alternating order; the TypeScript/Rust median ratio is a valid "materially better" claim. |
| **Single-implementation regression floor** | Only one implementation is measured; the distribution is a regression floor for that implementation, not a comparative win. |
| **D8-blocked** | Functional evidence exists but no symmetric timed boundary is available; the lane is an explicit non-claim until a sibling task (PERF-T3/T4/T5/T6/T7) builds the missing peer or instrumentation. "D8" refers to decision D8 in [issue #22](https://github.com/metaphorics/pi-oxidized/issues/22): the unsupported-claims registry that lists idle memory, isolated persistence, tool dispatch, layout/recomposition, pure paint, total footprint, and extension-host scaling as non-claims until named instrumentation lands. |

## Workload lane inventory

Eleven lanes span the five crates. Lanes 1 through 4 are measured by `scripts/verification/performance.ts` (check 9). Lane 5 is measured by `scripts/bench-extension-scaling.ts` (check 8). Lane 6 is a Rust-only correctness suite. Lane 8 is measured by `scripts/bench-tool-dispatch.ts` (PERF-T5). Lanes 7, 9, and 10 are D8-blocked pending sibling tasks. Lane 11 is an artifact-size comparison.

### Lane 1: Startup `--version` (paired comparative)

| Field | Value |
|---|---|
| Crates | `pi` |
| Script | `scripts/verification/performance.ts` measurements.version |
| Command (Rust) | `target/release/pi --version` |
| Command (TS) | `.references/pi/packages/coding-agent/dist/pi --version` | <!-- historical witness -->
| Samples | 20 cold, 10 warmups + 50 warm per implementation |
| Alternating order | Yes |
| Unit | milliseconds wall time |

Trusted baseline (from artifact):

| Distribution | Median (ms) | Rel. spread | Noise gate | Collection wall |
|---|---|---|---|---|
| Rust cold | 40.07 | 6.72% | pass | ~0.8 s (borderline) |
| TS cold | 540.42 | 4.61% | pass | ~10.8 s |
| Rust warm | 40.93 | 3.86% | pass | ~2.5 s |
| TS warm | 535.01 | 8.84% | pass | ~32.1 s |

Ranked time share: one-time startup cost. Negligible in long sessions; dominant in short `--version`-class invocations. Ranked 8/11 by session time share (startup is a fixed cost, not a sustained loop).

Hot/cold unit split:

| Unit | Hot/cold | Rationale |
|---|---|---|
| OS process creation | Cold | One-time per invocation; not input-scaling |
| Executable/runtime loading | Cold | File-cache-dependent; `posix_fadvise(DONTNEED)` forces cold |
| CLI argument parsing | Hot | Executes on every invocation regardless of cache |
| Version lookup | Hot | In-memory constant-time path |
| One output write + clean exit | Hot | Executes on every invocation |

Claim class: paired comparative. Speedup TS/Rust: cold 13.49x, warm 13.07x.

### Lane 2: Extension-free first frame (paired comparative)

| Field | Value |
|---|---|
| Crates | `pi` (entry), `pi-tui` (layout/paint), `pi-ai` (model/config) |
| Script | `scripts/verification/performance.ts` measurements.extensionFreeFirstFrame |
| Command | `pi --provider anthropic --model claude-sonnet-4-5 --api-key verification-no-network --no-extensions --no-session --offline --no-context-files --no-skills --no-prompt-templates --no-themes --approve` |
| Samples | 20 cold, 5 warmups + 30 warm per implementation |
| Alternating order | Yes |
| Unit | milliseconds wall time |
| Boundary | First complete DEC synchronized-output transaction (row-local CSI fallback recorded) |

Trusted baseline (from artifact):

| Distribution | Median (ms) | Rel. spread | Noise gate | Collection wall |
|---|---|---|---|---|
| Rust cold | 243.61 | 2.21% | pass | ~4.9 s |
| TS cold | 552.65 | 5.36% | pass | ~11.1 s |
| Rust warm | 248.36 | 2.53% | pass | ~8.7 s |
| TS warm | 600.24 | 6.43% | pass | ~21.0 s |

Ranked time share: one-time per session. For a 30-minute coding session, ~0.4 s (Rust) is <0.02% of wall time. For a 5-second short session, it is ~5%. Ranked 7/11 by session time share.

Hot/cold unit split:

| Unit | Hot/cold | Rationale |
|---|---|---|
| Process creation + runtime loading | Cold | One-time; cache-dependent |
| Argument parsing + config construction | Hot | Every invocation |
| Model/provider construction | Hot | Every invocation (offline stub) |
| TUI construction + layout | Hot | Every invocation; scales with component tree |
| First terminal paint (synchronized output) | Hot | Every invocation; I/O-bound |

Claim class: paired comparative. Speedup TS/Rust: cold 2.27x, warm 2.42x.

### Lane 3: Deterministic streaming tail-frame CPU (paired comparative)

| Field | Value |
|---|---|
| Crates | `pi` (entry), `pi-tui` (render), `pi-ext` (extension protocol), `pi-ai` (event reduction), `pi-agent` (agent loop) |
| Script | `scripts/verification/performance.ts` measurements.streamingTailFrameCpu |
| Fixture | `scripts/verification/extension.ts` emits 256 deterministic text-delta frames at 2 ms spacing |
| Samples | 3 whole-process warmups + 20 measured fresh-process samples per implementation |
| Alternating order | Yes |
| Unit | process-tree CPU milliseconds per deterministic provider frame |
| Boundary | CPU from submit Enter through final marker, divided by 256 frames |

Trusted baseline (from artifact):

| Distribution | Median (CPU ms/frame) | Rel. spread | Noise gate | Collection wall |
|---|---|---|---|---|
| Rust | 1.133 | 5.13% | pass | ~11.8 s (23 x 512 ms minimum) |
| TS | 2.441 | 3.59% | pass | ~11.8 s |

Threshold validity confirmed: zero starvation samples in either implementation (`thresholdValid: true`).

Ranked time share: sustained dominant workload during active model interaction. During a streaming turn, this is approximately 60 to 80% of wall time. Ranked 1/11 by session time share.

Hot/cold unit split:

| Unit | Hot/cold | Rationale |
|---|---|---|
| Provider frame decode | Hot | Per-frame (256x); input-scaling |
| Assistant state reduction | Hot | Per-frame; input-scaling |
| Incremental visible content update | Hot | Per-frame; input-scaling |
| Terminal diff/encode/write (paint) | Hot | Per-frame; input-scaling |
| Session JSONL append (persistence) | Hot | Per-frame; input-scaling |
| Process startup | Cold | One-time per sample |

The 512 ms injected delay (256 x 2 ms) is wall time but not CPU, so CPU/frame is the clean implementation metric.

Claim class: paired comparative. Speedup TS/Rust: 2.16x.

### Lane 4: Rust native keypress-to-paint (single-implementation regression floor)

| Field | Value |
|---|---|
| Crates | `pi` (entry), `pi-tui` (paint) |
| Script | `scripts/verification/performance.ts` measurements.rustNativeKeypressToPaint |
| Samples | 20 warmups + 200 measured |
| Alternating order | N/A (Rust-only) |
| Unit | milliseconds wall time |
| Boundary | PTY key write to first complete synchronized output paint |

Trusted baseline: none. The distribution fails the noise gate: median 1.935 ms, relative spread 26.98% (> 20% threshold). Collection wall ~0.44 s (< 1 s). No TypeScript peer exists.

Ranked time share: per-keystroke interactive loop. During active typing, this is the dominant latency path. Ranked 2/11 by interactive time share (dominant during typing, idle during streaming).

Hot/cold unit split:

| Unit | Hot/cold | Rationale |
|---|---|---|
| PTY key write | Hot | Per-keystroke; input-scaling |
| Input dispatch to state mutation | Hot | Per-keystroke |
| Terminal diff/encode/write (paint) | Hot | Per-keystroke |

No cold units (process is already warm; this is a steady-state interactive measurement).

Claim class: single-implementation regression floor. The p99 threshold is 5 ms (current p99: 2.59 ms, passes the regression floor). No comparative claim is supportable because no TypeScript peer exists. The noise-gate failure means the current distribution cannot feed a verdict; remediation requires pinning CPU governor, isolating the process, widening sample counts, or enlarging the input.

### Lane 5: Extension-host JSONL/RPC scaling (single-implementation regression floor)

| Field | Value |
|---|---|
| Crates | `pi-ext` (TypeScript `ExtensionHost` directly) |
| Script | `scripts/bench-extension-scaling.ts` (check 8) |
| Scenarios | zero extensions, 100 idle, 20 active widgets, 300-request fast stream, slow/fast queue locality |
| Samples | 30 warmups + 100 measured per scenario |
| Alternating order | N/A (single-implementation) |
| Unit | milliseconds wall time (request-to-response, frame CPU) |

Trusted baseline: none. All seven measured timed distributions fail the noise gate:

| Distribution | Median (ms) | Rel. spread | Noise gate |
|---|---|---|---|
| zero keypress | 0.026 | 116.0% | FAIL |
| idle100 keypress | 0.019 | 28.9% | FAIL |
| zero frame | 0.028 | 94.0% | FAIL |
| idle100 frame | 0.018 | 37.9% | FAIL |
| active20 keypress | 0.017 | 48.7% | FAIL |
| active20 frame | 0.018 | 34.4% | FAIL |
| fast terminalInput | 0.024 | 46.5% | FAIL |

The artifact is rejected as noise (the `requireQuiet` gating set covers five distributions; the active20 keypress and frame distributions are reported in the artifact but not gated by the script). Sub-millisecond medians with high jitter are inherent to the current sample size and input granularity.

Ranked time share: per-input-event overhead. During active interaction with extensions, this is per-keystroke overhead. Ranked 5/11 by session time share (amortized over the extension fan-out).

Hot/cold unit split:

| Unit | Hot/cold | Rationale |
|---|---|---|
| JSONL frame encode/decode | Hot | Per-event; input-scaling |
| Request correlation (id matching) | Hot | Per-event |
| Host loop dispatch | Hot | Per-event |
| Extension fan-out/registration | Cold | Startup only (100 idle factories registered once) |
| Widget callback + UI-slot traffic | Hot | Per-frame when active |
| Timeout/locality correctness | Cold | One-time correctness assertion (slow path) |

Claim class: single-implementation regression floor (TypeScript `ExtensionHost` only). No Rust peer executes the same frames. The Rust `serve_io` scaling suite (lane 6) covers the production server path but does not produce timed distributions.

### Lane 6: Rust `serve_io` scaling (D8-blocked)

| Field | Value |
|---|---|
| Crates | `pi-ext` |
| Test | `crates/pi-ext/tests/serve_io_scaling.rs` (PERF-T6) |
| Scenarios | zero/idle100/active20, fast/slow terminalInput, same frame corpus as lane 5 |
| Assertions | Protocol version handshake, id correlation, timeout locality, non-retryable errors |

Trusted baseline: none. The test contains correctness assertions only, no timed distributions are recorded. It uses the production `serve_io`, `encode_frame`, and `decode_frame_str` entry points with a deterministic `NativeExtension` adapter.

Ranked time share: same as lane 5 (production extension RPC path). Ranked 5/11 (tied with lane 5 conceptually).

Hot/cold unit split: same as lane 5, the production server path exercises the same units.

Claim class: D8-blocked. Functional correctness is proven through the production server, but no timed performance comparison is available. The sibling task PERF-T6 (#88) is open; until it adds timing instrumentation, this lane is an explicit non-claim.

### Lane 7: Layout/recomposition render churn (D8-blocked)

| Field | Value |
|---|---|
| Crates | `pi-tui` |
| Upstream | `.references/pi/packages/tui/test/render-churn-bench.ts` | <!-- historical witness -->
| Parameters | 100x30 viewport, 150-line transcript/dock tree, 20 warmups, 300 frames, static + editor scenarios, NullTerminal |
| Rust | None (PERF-T3 #89 is open) |

Trusted baseline: none. Upstream-only; no Rust peer benchmark exists. The upstream script reports elapsed ms/frame and sampled allocated KiB/frame per scenario.

Ranked time share: per-frame during rendering. Subsumed by lanes 2 and 3 in the end-to-end CLI workload. Ranked 3/11 by render time share (dominant during scroll/recomposition, subsumed by streaming).

Hot/cold unit split:

| Unit | Hot/cold | Rationale |
|---|---|---|
| Component tree recomposition | Hot | Per-frame; input-scaling (editor appends 1 char/frame) |
| Layout calculation | Hot | Per-frame |
| Terminal diff/encode | Hot | Per-frame |
| NullTerminal write (discard) | Hot | Per-frame (counts bytes only) |
| V8 heap profiler sampling | Cold | One-time setup per scenario |

The static scenario measures pure recomposition churn; the editor scenario adds one text mutation per frame.

Claim class: D8-blocked. Upstream-only; no symmetric Rust benchmark. PERF-T3 (#89) will build the Rust peer with the same tree, viewport, warmups, frame count, and scenarios. Until then, this lane is an explicit non-claim.

### Lane 8: Tool dispatch (paired comparative)

| Field | Value |
|---|---|
| Crates | `pi` (entry), `pi-agent` (dispatch), `pi-ai` (tool schema) |
| Script | `scripts/bench-tool-dispatch.ts` (PERF-T5) |
| Rust worker | `target/release/pi_tool_dispatch_bench` driving `pi_agent::execute_tool_calls` in-process |
| TypeScript worker | `scripts/bench-tool-dispatch.ts --worker` driving upstream `runAgentLoop` (`.references/pi`, `executeToolCalls` is module-private) | <!-- historical witness -->
| Tool | `noop`: JSON Schema `{path: string minLength 1, count: integer 1..64}`, required `path`, no additional properties; one partial update per call |
| Samples | 10 per implementation, fresh process each, alternating order (`implementationOrder`) |
| Unit | milliseconds per tool call, slice = `tool_execution_start` event → tool-result message session append |
| Boundary | Argument validation, tool start/update/end events, result construction, and session append (assistant-with-tool-call append pre-slice, tool-result append in-slice on both implementations); loop/stream overhead sits outside the slice |
| Real tools | `read`/`edit`/`bash` dispatch stays with `scripts/verification/e2e-smoke.ts` (separate end-to-end confirmation) |

Trusted baseline (from `target/bench/tool-dispatch.json`, 2026-08-27, Xeon Gold 6138):

| Distribution | Median (ms/call) | Rel. spread | Noise gate |
|---|---|---|---|
| Rust wall | 0.024123 | 5.13% | pass |
| TypeScript wall | 0.018652 | 5.71% | pass |
| Rust CPU | 0.05 | 5.52% | pass |
| TypeScript CPU | 0.102231 | 4.05% | pass |

Claim class: paired comparative. Wall ratio TS/Rust 0.77x (TypeScript's slice is faster in wall time); CPU ratio TS/Rust 2.04x (Rust uses half the CPU per call). Recorded as lane data — the Rust dispatch slice pays tokio task-spawn scheduling for the production parallel batch path, while upstream executes the batch on promises. Both implementations reject the shared invalid payload (`count: 999`) during argument validation with `update=0` and an error result per call; upstream additionally coerces mistyped primitives (TypeBox `Value.Convert`) where Rust rejects them — a recorded validation divergence, kept out of the timed payload.

### Lane 9: Session persistence/reopen (D8-blocked)

| Field | Value |
|---|---|
| Crates | `pi` (entry), `pi-ai` (session JSONL backend) |
| Existing | Composite evidence in stream workload (lane 3) + `e2e-smoke.ts` (fork/resume/reload) |
| Rust | No isolated append/reopen timing (PERF-T4 #86 is open) |
| TS | Reopens Rust v3 JSONL sessions (interop only, not timed) |

Trusted baseline: none. Stream CPU (lane 3) includes model-event processing, rendering, and persistence, not an isolated persistence measurement. The e2e-smoke script proves fork/resume/reload correctness, not performance.

Ranked time share: per-turn append + per-session reopen. Ranked 6/11 by session time share (amortized over turns, spikes on reopen).

Hot/cold unit split:

| Unit | Hot/cold | Rationale |
|---|---|---|
| JSONL entry serialization | Hot | Per-entry; input-scaling |
| File append (bytes written) | Hot | Per-entry; I/O-bound |
| JSONL scan/parse on reopen | Hot | Per-session; input-scaling with entry count |
| Session/tree state reconstruction | Hot | Per-session; input-scaling |
| Page-cache warm/cold | Cold | Cache-dependent; requires explicit warm/cold lanes |

The cold-cache lane (page-cache miss on reopen) is a cold unit that requires explicit instrumentation.

Claim class: D8-blocked. Composite evidence exists; no isolated timed boundary. PERF-T4 (#86) will add isolated session append and reopen timing lanes. Until then, this lane is an explicit non-claim.

### Lane 10: Idle/stream process-tree memory (D8-blocked)

| Field | Value |
|---|---|
| Crates | `pi` (process), `pi-tui` (terminal state), `pi-ai` (model/config state) |
| Script | `scripts/verification/performance.ts` measurements.idleProcessTreeMemory, measurements.streamProcessTreeMemory |
| Instrumentation | `/proc/<pid>/smaps_rollup` Rss/Pss + `/proc/<pid>/status` VmHWM (added by PERF-T1 #85) |
| Samples | 5 idle samples (500 ms stabilization, 1000 ms window), 5 stream-load samples (1000 ms window) |

Trusted baseline: none. The memory lanes run post-verdict (non-gating) and are not present in the current artifact (`target/bench/performance-comparison.json` has no `idleProcessTreeMemory` or `streamProcessTreeMemory` keys; the artifact was written before the memory collectors completed). The instrumentation exists but no trusted baseline distribution has been recorded.

Ranked time share: memory is a resource-share metric, not a time-share metric. It is a sustained overhead during the entire process lifetime. Ranked N/A by time share; ranked 9/11 by resource significance.

Hot/cold unit split:

| Unit | Hot/cold | Rationale |
|---|---|---|
| Resident executable/runtime pages | Cold | One-time at startup |
| Allocator baseline | Cold | One-time after initialization |
| Model/config state | Cold | One-time after construction |
| Terminal state | Hot | Scales with viewport/content changes |
| Task/event-loop stacks | Cold | One-time after startup |
| Stream-load memory growth | Hot | Scales with streaming duration |

Claim class: D8-blocked. Instrumentation was added by PERF-T1 (#85, closed), but no trusted baseline distribution has been recorded in the current artifact. The lanes are non-gating measurements, not claims. PERF-R8 (#95) will measure paired baselines on the newly symmetric lanes including memory.

### Lane 11: CLI launcher artifact size (paired comparative, carefully named)

| Field | Value |
|---|---|
| Crates | `pi` |
| Script | `scripts/verification/performance.ts` build.artifacts |
| Artifacts | `target/release/pi` (Rust ELF) vs `.references/pi/packages/coding-agent/dist/pi` (Bun-compiled executable) | <!-- historical witness -->

Trusted baseline (from artifact):

| Artifact | Bytes | SHA-256 |
|---|---|---|
| Rust `pi` | 27,414,272 | `9ce6b9d8...` |
| TypeScript `pi` | 93,402,312 | `5079aec7...` |
| Rust extension host | 90,379,464 | `57ab8d6d...` |

Not a timing lane, no noise gate or alternating order applies. The comparison is valid as launcher-file bytes only.

Ranked time share: N/A (artifact size, not a time-share metric). Ranked 11/11 (not a runtime workload).

Hot/cold unit split: N/A (static artifact property).

Claim class: paired comparative, carefully named. The comparison supports "Rust launcher is smaller" (27.4 MB vs 93.4 MB, 3.41x). It does not by itself support "installed package size" or "distribution size" claims: launcher bytes are one accounting class, not an installed footprint. The D8 registry entry for **total installed/distribution footprint** is no longer blocked: PERF-T7 (#91) landed the symmetric install-footprint accounting ([docs/PERF-T7-install-footprint-accounting.md](PERF-T7-install-footprint-accounting.md); runner `verify:footprint`, artifact `target/bench/install-footprint.json`) — launcher, runtime payload, shipped dependencies, and external interpreter prerequisite measured side by side under one contract, with no size threshold applied. Trusted baseline: recorded per run by that lane's artifact; numbers are quotable only under that contract's naming.

## Ranked time-share summary

| Rank | Lane | Claim class | Time share | Trusted baseline |
|---|---|---|---|---|
| 1 | Streaming tail-frame CPU | Paired comparative | ~60 to 80% during active streaming | Yes (Rust 1.133 ms/frame, TS 2.441 ms/frame) |
| 2 | Rust keypress-to-paint | Regression floor | Dominant during typing | No (noise gate fail, <1 s wall) |
| 3 | Layout/recomposition | D8-blocked | Subsumed by streaming/first-frame | No (upstream-only) |
| 4 | Tool dispatch | D8-blocked | ~5 to 15% during tool-heavy turns | No (no timed boundary) |
| 5 | Extension RPC scaling | Regression floor | Per-input-event overhead | No (all distributions noisy) |
| 5 | Rust `serve_io` scaling | D8-blocked | Same as lane 5 (production path) | No (correctness only) |
| 6 | Session persistence/reopen | D8-blocked | Per-turn append, per-session reopen | No (no isolated timing) |
| 7 | First frame | Paired comparative | One-time startup (~0.4 s Rust) | Yes (Rust 243.6 ms, TS 552.6 ms) |
| 8 | Startup `--version` | Paired comparative | One-time startup | Yes (Rust 40.1 ms, TS 540.4 ms) |
| 9 | Idle/stream memory | D8-blocked | Sustained resource overhead | No (no recorded distribution) |
| 11 | Launcher artifact size | Paired comparative | N/A (static) | N/A (size, not timing) |

## Lanes with trusted baselines

| Lane | Rust median | TS median | TS/Rust ratio | Noise gate | Alternating | >=1 s wall |
|---|---|---|---|---|---|---|
| Startup `--version` (cold) | 40.07 ms | 540.42 ms | 13.49x | pass | yes | yes (TS) |
| Startup `--version` (warm) | 40.93 ms | 535.01 ms | 13.07x | pass | yes | yes |
| First frame (cold) | 243.61 ms | 552.65 ms | 2.27x | pass | yes | yes |
| First frame (warm) | 248.36 ms | 600.24 ms | 2.42x | pass | yes | yes |
| Streaming CPU/frame | 1.133 ms | 2.441 ms | 2.16x | pass | yes | yes |
| Launcher size | 27.4 MB | 93.4 MB | 3.41x | N/A | N/A | N/A |

## Lanes without trusted baselines

| Lane | Reason | Claim class | Blocking task |
|---|---|---|---|
| Rust keypress-to-paint | Noise gate fail (rs=27.0%), <1 s wall | Regression floor | Remediation ladder (pin governor, widen samples, enlarge input) |
| Extension RPC scaling | All distributions noisy (rs 29 to 116%) | Regression floor | Remediation ladder (enlarge input, widen samples) |
| Rust `serve_io` scaling | No timed distributions | D8-blocked | PERF-T6 (#88) open |
| Layout/recomposition | No Rust peer benchmark | D8-blocked | PERF-T3 (#89) open |
| Tool dispatch | No timed dispatch-only boundary | D8-blocked | PERF-T5 (#93) open |
| Session persistence/reopen | No isolated timing | D8-blocked | PERF-T4 (#86) open |
| Idle/stream memory | No recorded distribution | D8-blocked | PERF-R8 (#95) open (depends on T3/T4/T5/T6/T7) |

## Crate coverage

| Crate | Lanes | Paired comparative | Regression floor | D8-blocked |
|---|---|---|---|---|
| `pi` | 1, 2, 3, 4, 8, 9, 10, 11 | 1, 2, 3, 11 | 4 | 8, 9, 10 |
| `pi-tui` | 2, 3, 4, 7, 10 | 2, 3 | 4 | 7, 10 |
| `pi-ext` | 3, 5, 6 | 3 | 5 | 6 |
| `pi-ai` | 2, 3, 8, 9, 10 | 2, 3 | none | 8, 9, 10 |
| `pi-agent` | 3, 8 | 3 | none | 8 |

## Evidence provenance

- `target/bench/performance-comparison.json`: check 9 artifact, generated 2026-08-26T14:22:29Z on this machine.
- `target/bench/extension-scaling.json`: check 8 artifact, generated 2026-08-26T14:18:43Z on this machine.
- `scripts/verification/performance.ts`: workload definitions, sample counts, noise gate integration, alternating order (`implementationOrder`), memory instrumentation (`observeProcessTreeMemory`, `sampleProcessTreeMemoryWindow`).
- `scripts/bench-extension-scaling.ts`: extension-host scaling scenarios, noise gate integration.
- `scripts/bench-tool-dispatch.ts`: paired dispatch-only tool benchmark (lane 8), noise gate integration, artifact `target/bench/tool-dispatch.json`.
- `scripts/statistics.ts`: `NOISE_RELATIVE_SPREAD_LIMIT = 0.2`, `requireQuiet`, `REMEDIATION_LADDER`.
- `crates/pi-ext/tests/serve_io_scaling.rs`: Rust production `serve_io` scaling correctness suite.
- `.references/pi/packages/tui/test/render-churn-bench.ts`: upstream render-churn parameters (100x30, 20 warmups, 300 frames, static + editor). <!-- historical witness -->
- Issue #13 comment: ranked comparative workload and floor plan (10 lanes with dispositions).
- Issue #22 comment: performance superiority acceptance decisions D1 through D9, including D8 unsupported-claims registry.
- Issue #85 (PERF-T1, closed): noise gate and process memory instrumentation.
- Issues #86, #88, #89, #91, #93, #95: open sibling tasks that unblock D8-blocked lanes.
