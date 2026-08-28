# Floor ledger: extension RPC dispatch (frame path, correlation, host loop)

Owning R2 hot rows (lanes 5/6): *JSONL frame encode/decode*, *Request correlation (id
matching)*, *Host loop dispatch*, *Widget callback + UI-slot traffic*. State: **OPEN
(fail-closed — no trusted or paired measured cost).**

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

## Measured cost — iteration 22 (PERF-T11 #97, 2026-08-28)

A timed `serve_io` lane was added to `crates/pi-ext/tests/serve_io_scaling.rs`
(ignored, release-only, gated by the `bench-seam` Cargo feature). The lane
replays the identical PERF-T6 300-request `terminalInput` corpus through
production `serve_io` over an in-memory tokio duplex, with a fresh
current-thread runtime per round (3 warmup + 9 measured, retried at 27).

Server attribution is separated from inclusive RTT: a `bench-seam` feature
exposes `record_decode(id)` at the post-decode / pre-dispatch seam and
`record_encode(id)` at the post-encode / pre-send seam in `server.rs`.
Attributed server cost S_i = encode_complete − decode_start per request;
inclusive RTT = batch wall time / 300. Instrumentation overhead: two
`std::sync::Mutex` lock/unlock pairs per request (~40-100 ns total), present
in both warmup and measured rounds, disclosed but not subtracted.

### Results (27 measured rounds, `taskset -c 20`, release)

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

## Decomposition status

No trusted lane total exists to decompose (noise gate failed); no attributed
categories are asserted. The 4 ms terminal-input budget is a *constraint* on
any rebuild (a slower dispatch fails the budget contract), recorded here
because Phase-5 candidates must respect it. The bimodal S distribution
suggests the dominant cost is async task scheduling overhead (spawn →
semaphore acquire → handler → out_tx.send → writer task wake), not the
decode/correlate/encode floor terms. A next blind candidate would target the
task-spawn-per-request overhead, but no optimization is attempted in this
iteration per the campaign contract.
