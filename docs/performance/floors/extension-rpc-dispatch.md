# Floor ledger: extension RPC dispatch (frame path, correlation, host loop)

Owning R2 hot rows (lanes 5/6): *JSONL frame encode/decode*, *Request correlation (id
matching)*, *Host loop dispatch*, *Widget callback + UI-slot traffic*. State: **OPEN
(fail-closed — no trusted or paired measured cost).**

## Contract (from call sites, tests, signatures — never internals)

- `serve_io(reader, writer, extension, config) -> Result<(), ServerError>` (crates/pi-ext/src/server.rs:966) drives the whole server: per outbound frame the writer task owes `write_all` + `flush` after `encode_frame` (server.rs:983-1108); per inbound frame the drive loop (:1308) decodes via `FrameDecoder` and routes exactly one allowlisted method, returning exactly one terminal res/error frame (:1550-1691, res_frame :2365 / error_frame :2375 / fault_frame :2394).
- Wire contract: one JSONL line per frame; `encode_frame` = validate + serialize + newline, cap MAX_FRAME_BYTES 8 MiB (protocol.rs:1904-1910, :263); Req/Res require nonzero ids (`requires_nonzero_id` protocol.rs:356); PROTOCOL_VERSION handshake asserted, compatibility version deliberately ignored (validate_hello server.rs:1370, tests hello_answers_with_compiled_constants :4044, hello_rejects_protocol_mismatch :4085).
- Terminal-input budget: 4 ms handler budget mirrored TS/Rust (EXTENSION_INPUT_TIMEOUT_MS host.ts:116; NATIVE_TERMINAL_INPUT_BUDGET server.rs:66) with a correlated non-retryable timeout error — a real-time obligation on the dispatch path.
- The TS host loop owes the same per-event shape (bench-extension-scaling.ts measureTerminalInput :161-185, measureFrameCpu :205-220; host.ts terminalInput dispatch :778, 4 ms race :1456).

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

## Measured cost — unproven, with named prerequisites

Rust: the serve_io scaling suite is correctness-only (no timed distributions; R2 lane
6). TS: all seven distributions fail the noise gate (medians 0.017-0.028 ms, rs
29-116%, R2 lane 5). No paired lane exists. **Multiple unproven; OPEN by the
fail-closed rule.**

Prerequisites recorded for Phase-5 entry: (a) a timed serve_io lane (the PERF-T6
correctness suite already replays the identical corpus — adding distributions is the
named unblock; D8 registry entry "Rust-vs-upstream extension-host scaling"), and/or
(b) TS-side noise remediation (enlarge per-request work or widen samples per the R2
ladder).

## Decomposition status

No trusted lane total exists to decompose; no attributed categories are asserted. The
4 ms terminal-input budget is a *constraint* on any rebuild (a slower dispatch fails
the budget contract), recorded here because Phase-5 candidates must respect it.
