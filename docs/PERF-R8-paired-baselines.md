# PERF-R8: Trusted paired baselines on the newly symmetric lanes

> **Historical regression witness**: This document records historical paired baselines collected against previous reference checkouts. All legacy checkout paths, runner paths, and commit references herein are preserved historical witnesses and are excluded from canonical closure metrics.
>
> Resolves [issue #95](https://github.com/metaphorics/pi-oxidized/issues/95).
> Parent: [Wayfinder: Complete pi Rust port](https://github.com/metaphorics/pi-oxidized/issues/12).
> Upstream baseline: `.references/pi` at `8fa7eebd235355522c8104166b4f1f959b4e2f10`. <!-- historical witness -->
> Prior ranking: [PERF-R2](PERF-R2-workload-surface-ranking.md).

## Scope

PERF-R2 ranked eleven workload lanes and recorded trusted baselines for four
(lanes 1, 2, 3, 11). Seven lanes were D8-blocked pending sibling tasks
PERF-T3 through PERF-T7. All five sibling tasks are now closed:

| Ticket | Issue | Lane | Status |
|--------|-------|------|--------|
| PERF-T3 | #89 | Lane 7: render churn | CLOSED |
| PERF-T4 | #86 | Lane 9: session append/reopen | CLOSED |
| PERF-T5 | #93 | Lane 8: tool dispatch | CLOSED |
| PERF-T6 | #88 | Lane 5/6: extension-host scaling | CLOSED |
| PERF-T7 | #91 | Lane 11 footprint: install accounting | CLOSED |

This document records the trusted paired baselines on each newly symmetric
lane, applies the same three-criterion trusted-baseline test from PERF-R2,
and updates the hot list.

## Lane 7: Layout/recomposition render churn (paired comparative)

| Field | Value |
|-------|-------|
| Workload | 100x30 viewport, 150-line transcript/dock tree, 20 warmups, 300 frames, static + editor scenarios, null-sink terminal |
| Rust runner | `target/release/pi_tui_render_churn_bench` (PERF-T3) |
| TypeScript runner | `.references/pi/packages/tui/test/render-churn-bench.ts` | <!-- historical witness -->
| Parameters diff-checked | Yes — viewport, transcript lines, warmup frames, measured frames, scenarios all match |
| Alternating order | N/A — separate processes, 10 runs each, interleaved collection |
| Artifact | Inline (10 runs per implementation, 2026-08-27, Xeon Gold 6138) |

Trusted baseline (wall ms/frame, 10 runs each):

| Distribution | Median (ms/frame) | Rel. spread | Noise gate |
|--------------|-------------------|-------------|------------|
| Rust static | 0.2125 | 4.55% | PASS |
| Rust editor | 0.2120 | 2.76% | PASS |
| TS static | 0.1120 | 18.45% | PASS |
| TS editor | 0.2430 | 15.18% | PASS |

Allocation (KiB/frame):

| Distribution | Median (KiB/frame) | Rel. spread | Notes |
|--------------|--------------------|-------------|-------|
| Rust static | 25.6 | 0% (deterministic) | Counting global allocator |
| Rust editor | 28.3 | 0% (deterministic) | Counting global allocator |
| TS static | 40.2 | 3.11% | V8 sampling heap profiler |
| TS editor | 78.2 | 3.72% | V8 sampling heap profiler |

Claim class: paired comparative. Wall speedup TS/Rust: static 0.53x (TS
faster on static recomposition), editor 1.15x (Rust faster on editor
mutation). Allocation ratio TS/Rust: static 1.57x, editor 2.76x (Rust
allocates less in both scenarios).

The static-scenario result is expected: the Rust TUI uses ratatui's
diff-based rendering which has higher per-frame overhead when nothing changes
(the diff pass itself is work), while the TypeScript TUI caches the entire
frame and skips re-render. The editor scenario — one character appended per
frame — is the production-relevant case, and Rust is 15% faster there while
allocating 2.8x less.

Hot list impact: **adds** per-frame layout/recomposition (hot, >= 5% of
render time during scroll/recomposition). **Removes** nothing — this lane
was already ranked 3/11 by render time share in PERF-R2 but carried no
baseline. It now carries a trusted paired baseline.

## Lane 9: Session persistence append/reopen (paired comparative)

| Field | Value |
|-------|-------|
| Workload | Pre-generated v3 JSONL sessions at 100/1000/5000 entries, append + reopen lanes, warm + cold-cache |
| Rust runner | `target/release/session-timing` (PERF-T4) |
| TypeScript runner | Bun-side SessionManager harness in `scripts/session-timing.ts` |
| SHA-256 prefix | Verified per sample (186 distinct prefixes across 360 samples, 0 missing) |
| Alternating order | No — blocked order per cell (all Rust samples, then all TypeScript samples per entry count); sequential drift is not cancelled |
| Artifact | `target/bench/session-timing.json` (generated 2026-08-27) |

Distributions that pass the noise gate (population stddev / median, matching the artifact's `relativeSpread`):

| Lane | Impl | Cache | Entries | Median (ms) | Rel. spread | Gate | Peak RSS (MB) |
|------|------|-------|---------|-------------|-------------|------|---------------|
| append | rust | cold | 100 | 5.191 | 14.29% | PASS | 2.2 |
| append | rust | cold | 5000 | 135.571 | 6.09% | PASS | 5.7 |
| append | rust | warm | 1000 | 15.023 | 19.45% | PASS | 3.2 |
| append | rust | warm | 5000 | 116.416 | 3.06% | PASS | 6.2 |
| append | ts | cold | 5000 | 154.541 | 6.71% | PASS | 95.1 |
| append | ts | warm | 1000 | 14.413 | 9.18% | PASS | 65.7 |
| append | ts | warm | 5000 | 134.552 | 6.70% | PASS | 86.8 |
| reopen | rust | cold | 1000 | 14.811 | 7.12% | PASS | 4.7 |
| reopen | rust | cold | 5000 | 38.331 | 14.04% | PASS | 15.3 |
| reopen | rust | warm | 1000 | 4.578 | 1.84% | PASS | 4.9 |
| reopen | rust | warm | 5000 | 26.210 | 13.48% | PASS | 16.0 |

Distributions that fail the noise gate (no trusted baseline for these cells):

| Lane | Impl | Cache | Entries | Median (ms) | Rel. spread | Gate |
|------|------|-------|---------|-------------|-------------|------|
| append | rust | cold | 1000 | 29.121 | 21.38% | FAIL |
| append | rust | warm | 100 | 3.344 | 164.35% | FAIL |
| append | ts | cold | 100 | 1.654 | 36.44% | FAIL |
| append | ts | cold | 1000 | 16.307 | 24.59% | FAIL |
| append | ts | warm | 100 | 1.152 | 62.26% | FAIL |
| reopen | rust | cold | 100 | 2.060 | 21.27% | FAIL |
| reopen | rust | warm | 100 | 0.853 | 34.78% | FAIL |
| reopen | ts | cold | 100 | 0.743 | 80.11% | FAIL |
| reopen | ts | cold | 1000 | 3.899 | 20.37% | FAIL |
| reopen | ts | cold | 5000 | 16.257 | 27.24% | FAIL |
| reopen | ts | warm | 100 | 0.391 | 203.44% | FAIL |
| reopen | ts | warm | 1000 | 1.551 | 77.88% | FAIL |
| reopen | ts | warm | 5000 | 7.292 | 25.48% | FAIL |

The 100-entry cells fail because sub-millisecond to low-millisecond medians
have inherently high jitter at the current sample count (20 warm, 10 cold).
The TypeScript reopen lane fails at all entry counts — the Bun-side
SessionManager harness has higher variance in file I/O scheduling. The
5000-entry append cells pass for both implementations and also meet the
collection-wall criterion (>= 1 s per implementation). The 1000-entry warm
append cell also passes the noise gate on both sides, but it collects < 1 s
per implementation. All three are recorded as paired measurements (not fully
trusted baselines) because the harness runs blocked order, not alternating
per sample, so sequential drift is not cancelled.

Because the harness runs blocked order (not alternating), these cells do not
meet all three trusted-baseline criteria from PERF-R2 — they meet the noise
gate and (for the 5000-entry cells) the collection-wall criterion, but not
the alternating-order criterion.

Paired measurements (both implementations pass noise gate; blocked order, not alternating):

| Lane | Cache | Entries | Rust median (ms) | TS median (ms) | TS/Rust ratio | Collection wall (Rust / TS) |
|------|-------|---------|------------------|-----------------|---------------|----------------------------|
| append | cold | 5000 | 135.571 | 154.541 | 1.14x | 1.36 s / 1.55 s (>= 1 s) |
| append | warm | 1000 | 15.023 | 14.413 | 0.96x | 0.30 s / 0.29 s (< 1 s) |
| append | warm | 5000 | 116.416 | 134.552 | 1.16x | 2.33 s / 2.69 s (>= 1 s) |

Claim class: paired measurement with methodology gap (blocked order, not
alternating). At 5000 entries, Rust append is 14-16% faster than TypeScript
in both cold and warm cache. At 1000 warm entries, the two are within 4%
(TS slightly faster). Peak RSS: Rust 2-16 MB vs TypeScript 52-105 MB across
all cells — Rust uses ~7-25x less memory for session persistence.

Hot list impact: **adds** JSONL append (hot, per-turn, >= 5% of time during
append at 1000+ entries) and reopen (hot, per-session, >= 5% of time during
reopen at 1000+ entries). **Removes** nothing — this lane was D8-blocked in
PERF-R2. The 100-entry cells are cold-only noise and do not feed a verdict.
The blocked-order methodology gap means these are paired measurements, not
fully trusted baselines under the R2 criteria; re-running with per-index
alternation would upgrade them.

## Lane 8: Tool dispatch (paired comparative)

| Field | Value |
|-------|-------|
| Workload | 10 samples x 10 000 calls each, 1000 warmup calls, no-op deterministic tool |
| Rust entry | `pi_agent::execute_tool_calls` (in-process) |
| TypeScript entry | `runAgentLoop` (upstream, `executeToolCalls` is module-private) |
| Boundary | `tool_execution_start` event through tool-result message session append |
| Alternating order | Yes — alternated per sample index |
| Artifact | `target/bench/tool-dispatch.json` (generated 2026-08-27) |

Trusted baseline (from artifact):

| Distribution | Median (ms/call) | Rel. spread | Noise gate |
|--------------|------------------|-------------|------------|
| Rust wall | 0.0236 | 7.32% | PASS |
| Rust CPU | 0.0490 | 7.50% | PASS |
| TS wall | 0.0177 | 9.14% | PASS |
| TS CPU | 0.0999 | 4.71% | PASS |

Claim class: paired comparative. Wall ratio TS/Rust 0.75x (TypeScript's
slice is faster in wall time — the Rust dispatch slice pays tokio task-spawn
scheduling for the production parallel batch path). CPU ratio TS/Rust 2.04x
(Rust uses half the CPU per call). Both implementations reject the shared
invalid payload (`count: 999`) during argument validation.

Hot list impact: **no change**. This lane already had a trusted baseline in
PERF-R2 (recorded 2026-08-27). The fresh run confirms the prior numbers
within noise. Tool dispatch remains ranked 4/11 by session time share
(amortized over turns, per-call overhead).

## Lane 5: Extension-host JSONL/RPC scaling (single-implementation, no trusted baseline)

| Field | Value |
|-------|-------|
| Workload | Zero/idle100/active20 scenarios, 300-request fast stream, slow/fast queue locality |
| Runner | `scripts/bench-extension-scaling.ts` (check 8) |
| Rust peer | `crates/pi-ext/tests/serve_io_scaling.rs` — correctness assertions only, no timed distributions |
| Artifact | `target/bench/extension-scaling.json` (generated 2026-08-26) |

Trusted baseline: **none**. All seven measured timed distributions fail the
noise gate:

| Distribution | Median (ms) | Rel. spread | Noise gate |
|--------------|-------------|-------------|------------|
| zero keypress | 0.0260 | 116.03% | FAIL |
| zero frame | 0.0281 | 94.00% | FAIL |
| idle100 keypress | 0.0186 | 28.91% | FAIL |
| idle100 frame | 0.0183 | 37.87% | FAIL |
| active20 keypress | 0.0166 | 48.72% | FAIL |
| active20 frame | 0.0181 | 34.39% | FAIL |
| fastTerminalInput | 0.0236 | 46.48% | FAIL |

Sub-millisecond medians with high jitter are inherent to the current sample
size and input granularity. The Rust `serve_io` scaling suite (PERF-T6,
CLOSED) proves functional correctness through the production server with a
deterministic `NativeExtension` adapter — protocolVersion handshake, id
correlation, timeout locality, non-retryable errors, cooperative
cancellation — but produces no timed distributions. No paired comparative
claim is supportable.

Claim class: single-implementation regression floor (TypeScript
`ExtensionHost` only). The Rust peer is a correctness suite, not a timing
benchmark. Remediation for the noise gate: enlarge the input (heavier
per-request work), widen sample counts, or pin CPU governor.

Hot list impact: **no change**. The extension-host scaling lane remains
without a trusted baseline. It does not add or remove units from the hot
list. The production extension RPC path is exercised correctness-wise but
not timed comparatively.

## Lane 11 footprint: Install-footprint accounting (accounting only, runner blocked)

| Field | Value |
|-------|-------|
| Contract | `docs/PERF-T7-install-footprint-accounting.md` |
| Runner | `scripts/verification/footprint.ts` (`verify:footprint`) |
| Artifact | `target/bench/install-footprint.json` (generated 2026-08-27, incomplete) |

Trusted baseline: **not completed**. The footprint runner failed during the
Rust extension-host release build step. The build failed because
`packages/extension-host` has a pre-existing TypeScript typecheck error in
`tests/endpoint-conformance.test.ts` (TS18046: `mode1` is of type
`unknown`, lines 463-466). This is a pre-existing issue unrelated to PERF-R8
or any concurrent agent work — the file is unmodified in the working tree
(last commit: `5cb29a4 feat(xc-6)`).

The accounting contract is defined and the runner is wired; the blocker is
the extension-host typecheck failure, not the footprint accounting itself.
Once the typecheck error is resolved, `verify:footprint` will produce the
complete artifact. No size threshold is applied — this lane defines
accounting only.

Claim class: accounting only (no threshold, no target). The launcher-bytes
comparison from PERF-R2 (Rust 27.4 MB vs TS 93.4 MB, 3.41x) remains valid
as launcher-file bytes. The full installed-footprint comparison under the
PERF-T7 contract is pending the extension-host build fix.

Hot list impact: **no change**. Install footprint is a static artifact
property, not a runtime workload. It does not add or remove units from the
hot list.

## Lane 10: Idle/stream process-tree memory (non-gating, artifact incomplete)

| Field | Value |
|-------|-------|
| Instrumentation | `observeProcessTreeMemory`, `sampleProcessTreeMemoryWindow` in `scripts/verification/performance.ts` |
| Runner | `scripts/verification/performance.ts` (check 9, post-verdict memory collectors) |
| Artifact | `target/bench/performance-comparison.json` (generated 2026-08-26, memory keys absent) |

Trusted baseline: **not completed**. The memory collectors exist in the
performance runner but the current artifact was generated before the memory
collection completed — `target/bench/performance-comparison.json` has no
`idleProcessTreeMemory` or `streamProcessTreeMemory` keys in its
`measurements` object. Re-running `verify:performance` to capture memory
data is blocked by concurrent compilation errors in `crates/pi-tui` (latex
module, from parallel agent work) that prevent the Rust binary from
building.

The memory lane is a non-gating measurement, not a claim. Once the pi-tui
compilation issue is resolved, a full `verify:performance` run will produce
the memory distributions as post-verdict artifacts.

Hot list impact: **no change**. Memory is a resource-share metric, not a
time-share metric. It does not add or remove units from the hot list.

## Updated hot list

The hot list from PERF-R2 ranked eleven lanes by session time share. PERF-R8
adds paired baselines or paired measurements to two previously D8-blocked
lanes and confirms one existing baseline. No units are removed.

| Rank | Lane | Claim class | Time share | Trusted baseline | Change from R2 |
|------|------|-------------|------------|------------------|----------------|
| 1 | Stream tail-frame CPU | Paired comparative | 60-80% during streaming | Yes (R2) | No change |
| 2 | Keypress-to-paint | Regression floor | Dominant during typing | No (noise gate FAIL) | No change |
| 3 | Render churn | Paired comparative | Per-frame during render | **Yes (R8)** | **Added** |
| 4 | Tool dispatch | Paired comparative | Per-turn amortized | Yes (R2, confirmed R8) | Confirmed |
| 5 | Extension-host scaling | Regression floor | Per-input-event | No (noise gate FAIL) | No change |
| 6 | Session append/reopen | Paired measurement (methodology gap) | Per-turn + per-session | **Partial (R8)** | **Added** |
| 7 | First frame | Paired comparative | One-time per session | Yes (R2) | No change |
| 8 | Startup --version | Paired comparative | One-time startup | Yes (R2) | No change |
| 9 | Idle/stream memory | Non-gating measurement | Resource, not time | No (artifact incomplete) | No change |
| 10 | Extension serve_io | Correctness only | Same as lane 5 | No (no timed distributions) | No change |
| 11 | Artifact size / footprint | Paired comparative | N/A (static) | Yes (R2, launcher only) | Footprint pending |

### Lanes with trusted baselines after R8

Seven lanes now have trusted baselines or paired measurements:

| Lane | Rust median | TS median | TS/Rust ratio | Noise gate | Type |
|------|-------------|-----------|---------------|------------|------|
| 1: Version cold | 40.07 ms | 540.43 ms | 13.49x | PASS | Paired comparative |
| 1: Version warm | 40.94 ms | 535.01 ms | 13.07x | PASS | Paired comparative |
| 2: First frame cold | 243.61 ms | 552.65 ms | 2.27x | PASS | Paired comparative |
| 2: First frame warm | 248.36 ms | 600.24 ms | 2.42x | PASS | Paired comparative |
| 3: Stream CPU | 1.133 ms/frame | 2.441 ms/frame | 2.16x | PASS | Paired comparative |
| 7: Render churn static | 0.213 ms/frame | 0.112 ms/frame | 0.53x | PASS | Paired comparative |
| 7: Render churn editor | 0.212 ms/frame | 0.243 ms/frame | 1.15x | PASS | Paired comparative |
| 8: Tool dispatch wall | 0.024 ms/call | 0.018 ms/call | 0.75x | PASS | Paired comparative |
| 8: Tool dispatch CPU | 0.049 ms/call | 0.100 ms/call | 2.04x | PASS | Paired comparative |
| 9: Append 5k cold | 135.57 ms | 154.54 ms | 1.14x | PASS | Paired measurement (blocked order) |
| 9: Append 5k warm | 116.42 ms | 134.55 ms | 1.16x | PASS | Paired measurement (blocked order) |
| 9: Append 1k warm | 15.02 ms | 14.41 ms | 0.96x | PASS | Paired measurement (blocked order, < 1 s wall) |
| 11: Launcher bytes | 27.4 MB | 93.4 MB | 3.41x | N/A | Paired comparative |

### Lanes without trusted baselines after R8

| Lane | Reason | Claim class |
|------|--------|-------------|
| 4: Keypress | Noise gate FAIL (rel 26.98%) | Regression floor (p99 2.59 ms < 5 ms threshold) |
| 5: Extension scaling | Noise gate FAIL (all 7 distributions) | Regression floor (TS only) |
| 9: Session 100-entry cells | Noise gate FAIL (sub-ms jitter) | No baseline for low-entry cells |
| 9: Session TS reopen | Noise gate FAIL (all entry counts) | No TS reopen baseline |
| 10: Idle/stream memory | Artifact incomplete (memory keys absent) | Non-gating measurement |
| 11: Full footprint | Runner blocked (extension-host typecheck) | Accounting only |

## Evidence provenance

- `target/bench/performance-comparison.json`: check 9 artifact, generated 2026-08-26T14:22:29Z on this machine (Xeon Gold 6138, powersave governor).
- `target/bench/extension-scaling.json`: check 8 artifact, generated 2026-08-26T14:18:43Z.
- `target/bench/session-timing.json`: PERF-T4 artifact, generated 2026-08-27T06:48Z. 360 samples (180 rust, 180 typescript), 186 distinct SHA-256 prefixes, 0 missing.
- `target/bench/tool-dispatch.json`: PERF-T5 artifact, generated 2026-08-27T08:15:45Z. 10 samples x 10 000 calls, alternating order.
- `target/bench/install-footprint.json`: PERF-T7 artifact, generated 2026-08-27T08:22:17Z (incomplete — runner blocked by extension-host typecheck failure).
- Render-churn (lane 7): 10 inline runs per implementation, 2026-08-27, Xeon Gold 6138. Rust binary `target/release/pi_tui_render_churn_bench`, TS script `.references/pi/packages/tui/test/render-churn-bench.ts`. <!-- historical witness -->
- `scripts/verification/performance.ts`: workload definitions, noise gate integration, alternating order (`implementationOrder`), memory instrumentation.
- `scripts/bench-tool-dispatch.ts`: paired dispatch-only tool benchmark (lane 8).
- `scripts/session-timing.ts`: isolated session append/reopen timing (lane 9).
- `scripts/bench-extension-scaling.ts`: extension-host scaling scenarios (lane 5).
- `scripts/verification/footprint.ts`: symmetric install-footprint accounting (lane 11 footprint).
- `scripts/statistics.ts`: `NOISE_RELATIVE_SPREAD_LIMIT = 0.2`, `requireQuiet`, `REMEDIATION_LADDER`.
- `crates/pi-ext/tests/serve_io_scaling.rs`: Rust production `serve_io` scaling correctness suite (lane 6/10).
- `crates/pi-tui/src/bin/pi_tui_render_churn_bench.rs`: Rust render-churn benchmark binary (lane 7).
- `.references/pi/packages/tui/test/render-churn-bench.ts`: upstream render-churn parameters (lane 7). <!-- historical witness -->
- Issues #89, #86, #93, #88, #91: closed sibling tasks (PERF-T3 through PERF-T7).
- Issue #90: PERF-R2 prior ranking.
- Issue #22: performance superiority acceptance decisions D1-D9, including D8 unsupported-claims registry.
