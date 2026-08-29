# Floor ledger: extension RPC dispatch (frame path, correlation, host loop)

Owning R2 hot rows (lanes 5/6): *JSONL frame encode/decode*, *Request correlation (id
matching)*, *Host loop dispatch*, *Widget callback + UI-slot traffic*. State:
**CONSTRAINED-ABOVE-FLOOR (terminal — E1-E4 exhaustion at iteration 26; trusted
S = 3058 ns, rs_S = 16.37%, noise gate passed; Q = 1708 ns, H = 1306 ns).**

## Contract (from call sites, tests, signatures — never internals)

- `serve_io(reader, writer, extension, config) -> Result<(), ServerError>` (crates/pi-ext/src/server.rs:966) drives the whole server: per outbound frame the writer task owes `write_all` + `flush` after `encode_frame` (server.rs:983-1108); per inbound frame the drive loop (:1308) decodes via `FrameDecoder` and routes exactly one allowlisted method, returning exactly one terminal res/error frame (:1550-1691, res_frame :2365 / error_frame :2375 / fault_frame :2394).
- Wire contract: one JSONL line per frame; `encode_frame` = validate + serialize + newline, cap MAX_FRAME_BYTES 8 MiB (protocol.rs:1904-1910, :263); Req/Res require nonzero ids (`requires_nonzero_id` protocol.rs:356); PROTOCOL_VERSION handshake asserted, compatibility version deliberately ignored (validate_hello server.rs:1370, tests hello_answers_with_compiled_constants :4044, hello_rejects_protocol_mismatch :4085).
- Terminal-input budget: 4 ms handler budget mirrored TS/Rust (EXTENSION_INPUT_TIMEOUT_MS host.ts:116; NATIVE_TERMINAL_INPUT_BUDGET server.rs:66) with a correlated non-retryable timeout error — a real-time obligation on the dispatch path.
- The TS host loop owes the same per-event shape (bench-extension-scaling.ts measureTerminalInput :181-205, measureFrameCpu :209-224; host.ts terminalInput dispatch :778, 4 ms race :1454-1457).

Boundary classification: the frame format, id-correlation discipline, and version
handshake are **boundary** (external extensions speak them; XC track owns the
contract). The in-process loop machinery is **interior**. Unresolved channels:
external extension code is reached only through the typed adapter seam — no string
dispatch inside the slice.

## Floor (computed)

Per request over an in-memory duplex pair (no syscall forced; over real pipes add
~1.2 us for the read+write pair):

```
decode one JSONL request line (typed parse constant scaled to ~120 B)  ~0.4 us
id correlation + dispatch table hit                                    ~0.05 us
encode one res line                                                    ~0.3 us
                                                                     ---------
floor                                                                ~0.75-1 us/request
```
## Measured cost — iterations 22-26 (PERF-T11 #97, 2026-08-28/29)

A timed `serve_io` lane was added to `crates/pi-ext/tests/serve_io_scaling.rs`
(ignored, release-only, gated by the `bench-seam` Cargo feature). The lane
replays the identical PERF-T6 300-request `terminalInput` corpus through
production `serve_io` over an in-memory tokio duplex, with a fresh
current-thread runtime per round (3 warmup + 27 measured).

Server attribution is separated from inclusive RTT: a `bench-seam` feature
exposes `record_decode(id)` at the post-decode / pre-dispatch seam,
`record_task_start(id)` at the spawned-task entry (first line inside
`handle_request`, after id extraction, before panic guard), and
`record_encode(id)` at the post-encode / pre-send seam in `server.rs`.
Attributed costs per request:
  Q = task_start − decode_start (spawn + cooperative scheduler hop)
  H = encode_complete − task_start (handler + response construction + encode)
  S = Q + H = encode_complete − decode_start (total server cost)
Inclusive RTT = batch wall time / 300. Instrumentation overhead: three
`std::sync::Mutex` lock/unlock pairs per request (~60-150 ns total), present
in both warmup and measured rounds, disclosed but not subtracted.

### Iteration 22 results (27 measured rounds, `taskset -c 20`, release)

| Metric | Value |
|---|---|
| Inclusive RTT (median) | 13,308 ns/request |
| Attributed server S (median) | 3,994 ns/request |
| Relative spread (server) | 45.24% |
| Noise gate (rs ≤ 0.20) | **FAILED** |
| Classification | **NOISY — no classification allowed** |

The S_median distribution is bimodal: a dominant cluster at ~4,000 ns and a
secondary cluster at ~1,100-2,000 ns. The bimodality is consistent across
runs and likely originates from tokio current-thread runtime scheduling
(whether the spawned server task resumes immediately or yields back to the
drive loop before completing the response). The dominant cluster (~4,000 ns)
exceeds the 2,000 ns OPEN threshold (2× floor_max); the secondary cluster
(~1,100-2,000 ns) straddles the AT-FLOOR/BOUNDARY/OPEN thresholds. No
classification is possible because the noise gate failed.

### Iteration 23 — same-protocol single-CPU re-run (2026-08-28)

Iteration 23 repeated the identical recorded retry protocol (`taskset -c 20`,
3 warmup + 27 measured rounds via `BENCH_MEASURED_ROUNDS=27`) on a fresh
session, to test whether the iteration-22 noise-gate failure is stable across
sessions. Iteration 22's recorded 27-round run already used this one-CPU pin
(only its earlier 9-round attempt ran `taskset -c 20-40`), so this run is a
same-protocol re-run, not a new remediation. Measurement-only: no production
or test code changed.

| Metric | Value |
|---|---|
| Inclusive RTT (median) | 13,340 ns/request |
| Attributed server S (median) | 4,001 ns/request |
| Relative spread (server) | 31.55% |
| Noise gate (rs ≤ 0.20) | **FAILED** |
| Classification | **NOISY — no classification allowed** |

Under the identical protocol, rs came in at 31.55% against iteration 22's
recorded 45.24%: the noise level itself drifts between sessions, and the gate
fails in both runs. The distribution is again multi-modal: a dominant cluster
at ~3,975-4,111 ns (16 rounds), one mid-round at 3,248 ns (round 10), a low
cluster at ~1,100-2,889 ns (6 rounds), and a high tail at ~5,062-7,422 ns
(4 rounds). The per-round medians carry the spread: each round's S_median
lands in one region of the distribution (a round-level location shift into
the noise-gate input). Both recorded 27-round runs are one-CPU-pinned and
both are multi-modal, so cross-CPU migration — which a one-CPU affinity set
already excludes — is not the dominant source of the modality (single-CPU
frequency/cache state is not controlled by this protocol). State remains
**OPEN (fail-closed)**.

### Iteration 24 — hop attribution (2026-08-28)

Iteration 24 added a third bench-seam timestamp (`record_task_start`) at the
spawned task entry to decompose S into Q (spawn + cooperative scheduler hop)
and H (handler + response construction + encode). Same protocol: `taskset -c
20`, 3 warmup + 27 measured rounds, identical 300-request corpus.

| Metric | Value |
|---|---|
| Inclusive RTT (median) | 13,885 ns/request |
| Q (spawn + cooperative hop) | 2,923 ns/request |
| H (handler + encode) | 1,462 ns/request |
| S (total server) | 4,399 ns/request |
| rs_Q | 9.97% |
| rs_H | 10.65% |
| rs_S | 9.93% |
| Noise gate (rs_S ≤ 0.20) | **PASSED** |
| Classification | **OPEN >2x** (S = 4399 ns > 2000 ns = 2× floor_max) |

The S noise gate passed at rs_S = 9.93%, a dramatic improvement from
iterations 22 (45.24%) and 23 (31.55%). All three distributions (Q, H, S)
are individually tight (rs < 11%). Q dominates S at 66.5% (2923 ns vs H's
1462 ns at 33.2%) — the spawn + cooperative scheduler hop is ~2× the
handler + encode cost. The multi-modality observed in iterations 22/23 was
a round-level location shift: the entire Q+H pair shifted together between
rounds (both Q and H are proportionally lower in the outlier rounds 14-15
and higher in round 16), not one component oscillating independently. This
confirms the iteration-22/23 hypothesis that async task scheduling overhead
dominates the server cost. The floor terms (decode ~400 ns, correlate ~50
ns, encode ~300 ns = ~750 ns) are dwarfed by Q (spawn + hop ~2923 ns).

State upgrades from **OPEN (fail-closed)** to **OPEN >2x (trusted)** — the
noise gate passed, so the classification is trusted. The dominant cost is Q
(spawn + cooperative hop); the named iteration-25 candidate is to inline
the `terminalInput` handler on the drive loop (skip `tasks.spawn` for
non-cancellable methods), collapsing Q toward zero and leaving H (~1462 ns)
as the server cost.

### Iteration 25 — FuturesUnordered request worker (2026-08-29)

Replaced per-request `JoinSet::spawn` with one long-lived supervised
request worker owning a `FuturesUnordered` of request futures, connected
to `dispatch_request` via a bounded mpsc channel (capacity =
`max_in_flight`). The worker polls jobs concurrently using `select!`
over `job_rx.recv()` and `pending.next()`. This eliminates the
per-request spawn allocation and JoinSet bookkeeping while preserving
drive-loop cancel responsiveness, bounded concurrency, exact teardown,
and one-response-per-id.

| Metric | Iteration 24 (baseline) | Iteration 25 (design) |
|---|---|---|
| Q (spawn + hop) | 2,923 ns | 1,708 ns (−41.5%) |
| H (handler + encode) | 1,462 ns | 1,306 ns (equivalent) |
| S (total server) | 4,399 ns | 3,058 ns (win 1.44x) |
| rs_S | 9.93% | 16.37% |
| Noise gate | PASSED | PASSED |
| Classification | OPEN >2x | OPEN >2x |

The remaining Q (~1708 ns) is the channel handoff (`try_send` + `recv` +
worker wake) plus `FuturesUnordered::push`. The per-request `JoinSet`
spawn allocation and task registration are eliminated. H is unchanged.
S = 3058 ns is still >2× floor_max (2000 ns), so the unit remains
**OPEN >2x (trusted)**. The 4 ms budget is ~1300× under (3058 ns vs 4 ms).
The next candidate would attack the remaining Q (channel handoff +
worker wake) or H (handler + encode), but the win (1.44x) clears the
≥1.05 gate and the change is accepted.

### Prior unproven state (superseded by this measurement)

Rust: the serve_io scaling suite was correctness-only (no timed distributions;
R2 lane 6). TS: all seven distributions fail the noise gate (medians
0.017-0.028 ms, rs 29-116%, R2 lane 5). No paired lane existed. **Multiple
unproven; OPEN by the fail-closed rule.**

Prerequisites recorded for Phase-5 entry: (a) a timed serve_io lane (the
PERF-T6 correctness suite already replays the identical corpus — adding
distributions is the named unblock; D8 registry entry "Rust-vs-upstream
extension-host scaling"), and/or (b) TS-side noise remediation (enlarge
per-request work or widen samples per the R2 ladder).
Trusted lane total (iteration 25): S = 3058 ns (rs_S = 16.37%, noise gate
passed). Attributed categories:
  Q (channel handoff + worker wake) = 1708 ns (55.9% of S)
  H (handler + response construction + encode) = 1306 ns (42.7% of S)
  Q + H = S verified per-request for all 300×27 = 8100 samples.

Prior trusted total (iteration 24): S = 4399 ns, Q = 2923 ns, H = 1462 ns.
Iteration 25 reduced Q by 41.5% (2923 → 1708 ns) by eliminating
per-request JoinSet::spawn. H is equivalent (within noise). Win = 1.44x.

The 4 ms terminal-input budget is a *constraint* on any rebuild (a slower
dispatch fails the budget contract). The current total server cost (3058
ns) is ~1300× under the 4 ms budget. The remaining Q (~1708 ns) is
channel handoff + worker wake; the next candidate would attack that or H
(handler + encode ~1306 ns).

### Iteration 26 — E1-E4 exhaustion (CONSTRAINED-ABOVE-FLOOR) (2026-08-29)

Terminal docs-only record: no candidate was executed this iteration. The
safe design space was executed and accepted in iteration 25 (1.44x); every
remaining in-unit mechanism is boundary-infeasible on the recorded advocate
proofs, not a performance miss. Provenance:
`agent://RpcInlineDesignAdvocate` (0.99 confidence — block-must-fix on the
inline terminalInput fast path) and `agent://RpcFusedLoopAdvocate` (0.98
confidence — block-must-fix on the fused reader+FuturesUnordered loop).

#### E1 — decomposition reconciliation (trusted iteration-25 medians)

| Term | ns/request | Share of S | Status |
|---|---|---|---|
| Q — channel handoff (`try_send` + `recv` + worker wake) + `FuturesUnordered::push` | 1,708 | 55.9% | required isolation handoff (E4) |
| H — handler + response construction + encode | 1,306 | 42.7% | AT-FLOOR (E3) |
| **S — total server (encode_complete − decode_start)** | **3,058** | — | rs_S = 16.37%, gate passed |

Reconciliation: Q and H are medians of independently accumulated
distributions, so they need not sum exactly to the S median: 1708 + 1306 =
3014 ns vs S_median = 3058 ns (44 ns, 1.4% of S — the round-level location
shifts recorded in iterations 22-25 correlate Q and H within a round, so
the median of the sum exceeds the sum of the medians). The identity
Q + H = S is exact per request: asserted for all 300×27 = 8100 samples in
the iteration-25 run. Inclusive RTT (median 12,387 ns/request) remains
reference only — it spans reader/writer and transport wait, not server
work. Distributions are the iteration-25 measurements; no new measurement
was taken this iteration.

#### E2 — candidate history and evidence

1. **Executed safe design — one supervised request worker over
   `FuturesUnordered` (iteration 25)**: per-request `JoinSet::spawn`
   replaced by one long-lived worker fed by a bounded mpsc (capacity =
   `max_in_flight`), `select!` over `job_rx.recv()` and `pending.next()`.
   Measured **1.44x** (S 4399 → 3058 ns; Q 2923 → 1708 ns, −41.5%; H
   equivalent within noise), PASSED the ≥1.05x gate, accepted. This is the
   safe-design endpoint: what remains of Q is the handoff itself.
2. **Inline `terminalInput` handler on the drive loop** — REJECTED on
   `agent://RpcInlineDesignAdvocate` (block-must-fix): awaiting the inline
   future **suspends the one `drive` future**, so the transport reader
   cannot advance and cancel/request frames behind a terminal callback —
   in the same batch or a later read — are **delayed**; awaiting each
   inline request inside the single drive loop **serializes previously
   concurrent terminal callbacks** (up to `max_in_flight` (default 64)
   overlap today → 1) and reorders responses from callback-completion
   order to request order; and the cooperative `tokio::time::timeout`
   **cannot preempt a non-yielding callback** (spin, blocking I/O, or an
   indefinite lock defeats the 4 ms budget without bound — the trait
   documents no nonblocking/cooperative/bounded-poll contract).
   Boundary-infeasible, not a performance miss.
3. **Fused reader + `FuturesUnordered` completions loop** — REJECTED on
   `agent://RpcFusedLoopAdvocate` (block-must-fix): polling arbitrary
   extension futures inside `drive` **loses task isolation** — one
   non-yielding extension poll blocks the transport reader (the separate
   worker task is what lets cancellation/EOF proceed on another runtime
   worker); the proposed `reader_eof` state **changes clean-EOF abortive
   teardown into an unbounded drain** unless corrected (a never-resolving
   callback holds shutdown); and **fairness becomes load-bearing** — the
   proposed `biased;` reader-first select starves admitted requests under
   sustained readable input (permits never release; every later request
   falsely rejected as overloaded), and completion-first bias is equally
   wrong. Boundary-infeasible, not a performance miss.

#### E3 — floor and multiple revalidation

Floor **~0.75-1.0 us/request** (revalidated — no input, dependency, or
protocol change since the R9 computation; the lane replays the identical
300-request corpus over an in-memory duplex).

- Interior H = **1.306 us ≤ 1.5 us** = 2× the conservative floor lower
  bound (0.75 us): the interior handler/response-construction/encode work
  is **AT-FLOOR** — no ≥1.05x candidate exists inside H.
- Total S = **3.058 us is 3.06×-4.08× the floor** (3058/1000 to 3058/750).
  This is a constraint statement, NOT a claim that 3.058 us is a physical
  floor: the gap over the floor is held open by Q (E4), not by interior
  work.

#### E4 — dominant residual and reopen conditions

Dominant residual: **Q = 1.708 us** — the required **cross-task isolation
handoff** that keeps arbitrary extension callbacks off the transport reader
while preserving cancellation (registration-before-enqueue, abortive
teardown on EOF/error), bounded concurrency (`max_in_flight` permits), and
one-response-per-id. Every mechanism that would remove the handoff
collapses one of those contract properties (E2 items 2-3). Reopen only on:

1. **Cooperative/nonblocking extension-callback contract** — a trait-level
   bounded-poll or blocking-disclosure guarantee enforced at the adapter
   seam: the callback becomes safe to poll on the drive loop; inline/fused
   execution becomes feasible and Q collapses toward zero.
2. **Serialized terminal callbacks + delayed cancel/EOF accepted** as an
   explicit re-contracting of observable extension behavior: inline await
   becomes feasible; same Q collapse.
3. **A different runtime primitive proven (measured, not projected) to
   preserve task isolation and clear ≥1.05x** — e.g. a preemptible
   callback executor or a dedicated-thread worker with a cheaper handoff.
   Micro-tuning the existing handoff (unbounded channel, capacity bumps,
   allocator tweaks) is NOT a materially distinct design: it does not
   remove the handoff, and unbounded channels sacrifice the
   bounded-memory/backpressure contract.

**Verdict: CONSTRAINED-ABOVE-FLOOR.** Unit terminal in the campaign
records; issue #97 stays OPEN. Next ordered unit: **`keypress-dispatch`**
(measurement prerequisite — measurement remediation first).
