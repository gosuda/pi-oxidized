# Floor ledger: tool dispatch slice (validation, events, result, append)

Owning R2 hot rows (lane 8): *argument validation*, *tool start/update/end events*,
*result construction + session append*. State: **OPEN**, 5.62x floor.

## Contract (from call sites, tests, signatures — never internals)

- `pi_agent::execute_tool_calls` (crates/pi-agent/src/schedule.rs:105) is the single production dispatch entry; called by `run_agent_loop` (crates/pi-agent/src/run.rs:164) and driven standalone by the bench (crates/pi/src/bin/pi_tool_dispatch_bench.rs) with the timed slice = `ToolExecutionStart` event -> `MessageEnd` bearing the `ToolResult` (bench sink :200-221).
- Per call the protocol owes (tests: crates/pi/tests/tool_dispatch_bench.rs): one start event, >=1 update, one end event, exactly two session appends (assistant-with-tool-call pre-slice, tool-result in-slice), and validation rejection of invalid payloads (`count: 999`) with `update=0` and an error result.
- Validation contract: `AgentTool::prepare_and_validate_arguments` (schedule.rs:601 -> crates/pi-agent/src/tool.rs:234-241); the noop tool validates by typed parse + manual range checks (bench :65-89) — a typed parse of the arguments is forced per call.
- Parallel batch: bounded concurrency MAX_PARALLEL_TOOL_CALLS=8 (schedule.rs:26,291); one worker task per call on the parallel path (JoinSet spawn, schedule.rs:378-429; the sequential path also spawns an inner worker, :680-703).
- Result plumbing: `finalize_executed_tool_call` (schedule.rs:749), end-event emission (:828), `tool_result_message` (:847-866), MessageStart+MessageEnd (:837-848), context append (run.rs:164-172).

Boundary classification: the AgentEvent/AgentMessage shapes and the session append are
**boundary** (session JSONL v3 wire; e2e-smoke pins real read/edit/bash dispatch).
The in-process dispatch machinery is **interior**. Unresolved channels: none — the
scout census found a single synchronous sink; no string dispatch on the slice.

## Floor (computed)

```
typed parse of tool arguments (~60 B)         ~300 ns   (achievable parse constant scaled)
3 event constructions + emissions              ~150 ns
result message construction                    ~200 ns
one session append: serialize 170 B + write    3639.6 ns  (276.2 + 3363.4, session-append constants)
                                            ---------
floor                                       4289.6 ns ~= 4.29 us/call
```

One worker-task spawn is owed by the bounded-parallelism contract but costs no syscall
at floor (task reuse is achievable); it is booked as overhead, not floor.

## Measured cost

Trusted artifact (R8): Rust wall median **24.12 us/call** (rs 5.13%), CPU 49 us/call;
fresh callgrind corroborates 251.1 kIr/call ~= 23.7 us at the 10.6 kIr/us calibration.

**Multiple = 24.12 / 4.29 ~= 5.62x => OPEN.**

## Cost decomposition (sums to 24.12 us/call)

| Category | Cost | Method |
|---|---|---|
| session-append syscalls (openat+write+close per result append) | 5.43 us | floorkit append-shape measurement + strace census (1 openat/1 write/1 close per entry) |
| allocation (malloc/free family, 33.5% of Ir) | 8.10 us | profiler attribution (callgrind, 5.274 G Ir / 21k calls) |
| serde_json Value pipeline (BTreeMap insert/dying_next/drop_glue + ser, ~14.8%) | 3.55 us | profiler attribution |
| memcpy/memcmp payload copying (9.2%) | 2.22 us | profiler attribution |
| tokio spawn/JoinSet + events + validation + residual | 4.82 us | subtraction (residual closes the sum) |

## Addressable-overhead notes for Phase 5

The allocator + Value-pipeline terms (~11.7 us) trace to double serialization and
per-call cloning (ToolCall clone schedule.rs:189-197, PreparedToolCall clone, result
content/details clones :847-866); the append syscalls shrink with a held-open fd (see
session-append.md). Boundary: the two session appends and the event triple are
protocol — the tests pin them; the wall-vs-CPU split (tokio scheduling) is recorded in
R2 lane 8 as a known divergence note.
