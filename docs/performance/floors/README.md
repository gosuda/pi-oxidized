# PERF-R9: Per-hot-unit floor ledgers, cost decompositions, and blind-derivation contracts

> Resolves [issue #94](https://github.com/metaphorics/pi-oxidized/issues/94) (PERF-R9).
> Inputs: [PERF-R2 workload ranking](../../PERF-R2-workload-surface-ranking.md),
> [PERF-R8 paired baselines](../../PERF-R8-paired-baselines.md), the issue-#22 settled
> decision (Phase 4), and the extremely-optimize method (floor from contract, blind
> derivation, 2x floor rebuild threshold).
> Measurement date for all fresh constants: 2026-08-27, Xeon Gold 6138, zfs root,
> `taskset -c 20-40` unless noted. Method words: *subtraction* = cross-lane or
> cross-shape arithmetic over measured medians; *instrumented counters* = committed
> runner/bench artifacts; *profiler attribution* = callgrind Ir shares (perf events
> unavailable: `perf_event_paranoid=4`) and `strace -c/-T` censuses.

## Ledger index

| Ledger | Owning R2/R8 hot rows | Lane | Measured cost (trust) | Floor | Multiple | State |
|---|---|---|---|---|---|---|
| [session-append.md](session-append.md) | JSONL entry serialization; file append | 9 | 18.34 us/entry (fresh, rs 5.3%) | 3.73 us | 4.91x | OPEN |
| [session-reopen.md](session-reopen.md) | JSONL scan/parse on reopen; state reconstruction | 9 | 5.95 us/entry (fresh, rs 5.7%) | 0.76 us | 7.79x | OPEN |
| [render-churn-recomposition.md](render-churn-recomposition.md) | Component tree recomposition; layout calculation | 7 | 212 us/frame editor (R8 + fresh) | ~1.5 us | ~141x | OPEN |
| [terminal-paint.md](terminal-paint.md) | Terminal diff/encode/write (paint); NullTerminal write; first synchronized paint | 2, 3, 4, 7 | 26.5 us/frame amortized (subtraction) | 0.64 us | ~7.5x paint-only (block 41x) | OPEN |
| [stream-frame-pipeline.md](stream-frame-pipeline.md) | Provider frame decode; assistant state reduction; incremental visible content update | 3 | not separable (see ledger) | 0.15-0.2 us/frame each | unproven | OPEN (fail-closed) |
| [tool-dispatch-slice.md](tool-dispatch-slice.md) | Argument validation; tool start/update/end events; result construction + append | 8 | 24.12 us/call wall (R8 artifact) | 4.29 us | 5.62x | OPEN |
| [startup-version-path.md](startup-version-path.md) | CLI argument parsing; version lookup; one output write + clean exit | 1 | 0.37 ms in-process CPU (callgrind) | 0.15 us | ~2480x (CPU) | OPEN |
| [first-frame-init.md](first-frame-init.md) | Argument parsing + config construction; model/provider construction; TUI construction + layout | 2 | 243.61 ms lane (R2, trusted) | ~1.50 ms | ~162.4x | OPEN |
| [extension-rpc-dispatch.md](extension-rpc-dispatch.md) | JSONL frame encode/decode; request correlation; host loop dispatch; widget callback + UI-slot traffic | 5, 6 | none trusted (Rust untimed; TS noisy) | ~1 us/req | unproven | OPEN (fail-closed) |
| [keypress-dispatch.md](keypress-dispatch.md) | PTY key write; input dispatch to state mutation | 4 | 1.935 ms median (NOISE FAIL, rs 27%) | ~13 us | unproven | OPEN (fail-closed) |
| [memory-resource-units.md](memory-resource-units.md) | Terminal state; stream-load memory growth | 10 | none (artifact incomplete, R8) | bytes-class | unproven | OPEN (fail-closed) |

## Coverage — no residue

Every hot row of the PERF-R2 lane tables (as amended by PERF-R8) maps to exactly one
ledger above. Cold rows (process creation, runtime loading, page-cache effects,
extension registration, V8 profiler setup) are excluded by the R2 split and are graded
in the Phase-6 cold pass, not here. No hot row is unmapped; no unit carries a third
state.

## Ordered OPEN list (rebuild targets, descending time share)

1. `render-churn-recomposition` — rank-3 lane, per-frame during all rendering, ~141x floor; the single largest sustained multiple.
2. `terminal-paint` — component of the rank-1 stream lane, ~7.5x paint-only (its recompose+paint block ~41x over the write floor).
3. `stream-frame-pipeline` (decode / reduce / visible-update) — rank-1 lane components; multiples unproven, measurement prerequisite first (per-process CPU split pi vs extension host, turn-phase attribution).
4. `tool-dispatch-slice` — rank-4 lane, 5.62x.
5. `session-append` — rank-6 lane, 4.91x per entry (and O(n^2) scan term growing with session length).
6. `session-reopen` — rank-6 lane, 7.79x per entry.
7. `first-frame-init` — rank-7 lane, ~162.4x; one-time per session.
8. `extension-rpc-dispatch` — rank-5 lane; measurement prerequisite (timed `serve_io` lane).
9. `keypress-dispatch` — rank-2 interactive lane; measurement remediation (noise gate) prerequisite.
10. `startup-version-path` — rank-8 lane; enormous CPU multiple, de-minimis absolute time; addressable category is runtime construction on the fast-exit path.
11. `memory-resource-units` — resource currency; measurement prerequisite (memory lane artifact).

## AT-FLOOR list

**Empty.** No hot unit's trusted measured cost sits at or under 2x its computed floor
on 2026-08-27. Units whose measurement is missing or noise-rejected are recorded OPEN
by the fail-closed rule (an untrusted measurement can never prove AT-FLOOR), each with
its named measurement prerequisite.

## Blind-derivation contract (binding Phase 5)

For every unit above, the *Contract* section is sourced from call sites, tests, and
signatures only — never from the unit's internals — and each floor term cites its
contract source and measured constant. Phase-5 rebuild candidates MUST derive their
replacement from the ledger's contract + floor sections alone, before reading the
replaced body, and MUST file the branch-by-branch divergence audit named in the
ledger's classification section. Boundary surfaces recorded as `boundary` carry the
issue-#22 rule: no published surface (session JSONL v3 wire format, synchronized-
output framing, extension RPC frames) is touched without a recorded explicit consent.

## Measured constants used by the floors (2026-08-27, this machine)

| Constant | Value | Method |
|---|---|---|
| write syscall, 170 B, /dev/null | 122.7 ns | floorkit micro-bench (median of 31 windows) |
| zfs open(O_APPEND)+write 170 B+close | 5426 ns | floorkit |
| zfs held-open write 170 B | 3363 ns | floorkit |
| tmpfs open+write 170 B+close | 1593 ns | floorkit |
| pipe write 170 B (reader draining) | 598 ns | floorkit |
| pipe write 2 KiB | 1141 ns | floorkit |
| zfs fsync after 2 KiB write | 1081 ns | floorkit |
| zfs warm page-cache readback | 10 ns/entry (170 B) | floorkit |
| format!-build 170 B JSON line | 42.8 ns | floorkit |
| sonic-rs typed parse, 170 B line, owned fields | 683.8 ns | /tmp micro-bench (achievable-parse floor constant) |
| sonic-rs typed serialize entry | 276.2 ns | /tmp micro-bench |
| minimal Rust binary (write+exit), direct exec | 1.9 ms | hyperfine -N 30 runs |
| `pi --version` direct exec | 15.1 ms mean (User 29.6 / Sys 29.0 ms) | hyperfine -N 30 runs |
| `pi --version` syscall census | 2814 syscalls: 80 clone3, 735 futex, 188 munmap | strace -c -f |
| append 5000 entries | 91.686 ms => 18.337 us/entry (rs 5.32%) | fresh session-timing run |
| reopen 5000 entries | 29.758 ms => 5.952 us/entry (rs 5.65%) | fresh session-timing run |
| append syscalls per entry | 1 openat + 1 write + 1 close | strace -c census |
| render churn | static 0.209 / editor 0.214 ms/frame; 25.6/28.3 KiB alloc; 35/39 B written per frame | fresh attrib-build run |
| Ir/wall calibration | ~10.6 kIr/us (1.348 G Ir over 126.7 ms native churn) | callgrind vs native wall |
| dispatch | 5.274 G Ir / 21k calls = 251.1 kIr/call ~= 24.1 us at calibration | callgrind |

Trusted lane baselines quoted by the ledgers (R2/R8 artifacts): stream 1.133 ms
CPU/frame, first frame 243.61 ms cold / 248.36 ms warm, `--version` 40.07 ms cold /
40.93 ms warm (PTY), tool dispatch 24.12 us/call wall, churn editor 0.212 ms/frame,
append-5k 116.42 ms warm (R8; fresh re-run above used for per-entry arithmetic).

## Evidence provenance

- `target/bench/performance-comparison.json`, `session-timing.json`, `tool-dispatch.json` (committed artifacts cited by R2/R8).
- Callgrind profiles (this run, attribution build with symbols at `target/attrib/`, worktree 48696b5): churn, append, reopen, dispatch, version, first-frame, stream.
- Citation provenance: contract line anchors were written against worktree 48696b5 and re-anchored to the integration tree by PERF-G10 (drift table in [PERF-G10-floor-ledger-audit.md](../PERF-G10-floor-ledger-audit.md)); measurement provenance above still refers to 48696b5.
- `strace -c/-T` censuses: version, append, first-frame waits.
- floorkit + sonic-rs micro-benches (throwaway, /tmp, not committed; commands recorded in the ledger that uses each constant).
