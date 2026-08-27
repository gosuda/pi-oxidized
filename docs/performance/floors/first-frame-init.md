# Floor ledger: first-frame init (config/provider construction, TUI construction + layout)

Owning R2 hot rows (lane 2): *Argument parsing + config construction*, *Model/provider
construction (offline stub)*, *TUI construction + layout*. (The first synchronized
paint row is owned by terminal-paint.md.) State: **OPEN**, ~122x floor.

## Contract (from call sites, tests, signatures — never internals)

- `run_interactive_mode` (crates/pi/src/modes/interactive/runtime.rs:6589-6660) owes, before the first frame: terminal guard + raw mode (:6602-6608), a blocking terminal probe (:6610, probe batch without sync wrapper, probe.rs:27-86), theme resolution from probed polarity, `Tui::new` (:6623), `InteractiveRuntime::new` (:6656), then the first `Tui::commit` (Txn::Frame/Settle) through stage3 (writer.rs:522, backend.rs:275-284).
- Provider/model construction on the offline path: `ModelRuntime::create` (crates/pi/src/core/model_runtime.rs:359) builds the compiled-in builtin catalog (:360) plus a 10-adapter reqwest ProviderRegistry (:1641-1668), `rebuild_providers` (:435, compose_models_static :1380-1419) and `refresh(allow_network: false)` (:436-438); the verification api key installs without network (set_runtime_api_key :592-621). PI_OFFLINE=1 becomes bootstrap offline mode (bootstrap.rs:446-454).
- The lane's observable boundary: first complete DEC synchronized-output transaction after spawn (performance.ts :1026-1044); sync-balance and probe-before-sync are test-pinned (pty_no_flicker.rs:54-66, 236-290).

Boundary classification: the first-frame *wire* (probe batch + first synchronized
frame) is **boundary**. The construction machinery (registry, catalog, TUI) is
**interior**. Unresolved channels: none found.

## Floor (computed)

```
one terminal probe round trip (write+read+wakeup)   ~1 ms   (pipe RT proxy + scheduler wakeup class)
config + 10-adapter registry + Tui construction      ~0.5 ms (data-driven struct construction at
                                                        achievable cost; no network on the offline path)
first paint transaction                               ~0.64 us (terminal-paint floor)
                                                    ---------
floor                                               ~1.50 ms
```

No contract term forces TLS-root loading, per-frame registration, or any wait beyond
the probe round trip on this path.

## Measured cost

Trusted lane baseline (R2): **243.61 ms cold / 248.36 ms warm** (the 4.75 ms cold-warm
delta proves the lane is wait-bound, not cache-bound).

**Multiple = 243.61 / 1.50 ~= 162.4x => OPEN.**

## Cost decomposition (sums to 243.6 ms)

| Category | Cost | Method |
|---|---|---|
| 5 ms-cadence epoll polling loop during init (30 x epoll_wait(timeout=5ms) with zero events, crossterm input task tick) | 157.2 ms | instrumented counters: strace -T census, 30 observed 5.15-5.29 ms waits |
| one blocking epoll_wait(EPOLLIN) before first output | 58.3 ms | instrumented counters: strace -T (single 58.332 ms wait, timeout 3.4 s, woken by EPOLLIN) |
| in-process CPU (construction + first render) | 18.9 ms | profiler attribution: callgrind 200.6 M Ir at 10.6 kIr/us |
| — of which rustls cert/public-key decode (rustls-pki-types base64::decode_public + pem) | (1.9 ms) | profiler attribution: 20.2 M Ir = 10.1% |
| — of which allocator | (7.3 ms) | profiler attribution: ~78 M Ir |
| process spawn + loader + page faults + residual | 9.2 ms | subtraction (residual closes the sum) |

## Addressable-overhead notes for Phase 5

~215 ms of the lane is two wait shapes, not compute: the 5 ms poll cadence (30 cycles)
and one 58 ms blocking wait. Identifying and removing/shortening those waits (event-
driven readiness instead of 5 ms ticks; eliminating the long pre-output block) is the
campaign entry — the compute side (18.9 ms, incl. TLS-root decode on an offline path)
is the secondary target. Boundary: probe/first-frame wire behavior is pinned by
pty_no_flicker; only the waits are addressable.
