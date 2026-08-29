# Floor ledger: stream frame pipeline (decode / state reduction / visible update)

Owning R2 hot rows (lane 3): *Provider frame decode*, *Assistant state reduction*,
*Incremental visible content update*. State: **CONSTRAINED-ABOVE-FLOOR (terminal —
E1-E4 exhaustion recorded at iteration 33; fresh trusted drain 1,382 ns/frame,
rs 8.91%, vs the ~200 ns decode/forward floor ≈ 6.9x; dominant residual the
contract-forced lossless mpsc leg (boxed forward + cross-task handoff); no
in-boundary candidate projects >=1.05x).**

## Contract (from call sites, tests, signatures — never internals)

- Provider leg: the verification provider emits pre-typed `text_delta` events (scripts/verification/extension.ts `streamVerification` :178, push at :210; chunk count from `PI_VERIFICATION_CHUNK_COUNT`); on real providers the same funnel is `Provider::stream` -> `AssistantState::text_delta` producing `AssistantMessageEvent::TextDelta` (crates/pi-ai/src/providers/stream_state.rs:187-198). The event consumer set is fixed by the drain contract.
- `ProviderDrain::spawn` (crates/pi-agent/src/drain.rs:105-118) owes: every non-terminal event's partial snapshot published to a lossy watch (:220-229) AND every event forwarded losslessly to the capacity-64 mpsc (:146) — state fidelity for the loop, latest-wins for presentation.
- Reduce: `consume_drain_items` folds each item and emits one `AgentEvent::MessageUpdate` per frame (crates/pi-agent/src/run.rs:260-363, emit at :344-347); the session pump republishes `AgentSessionEvent::MessageUpdate` (crates/pi/src/core/agent_session/subscribe.rs:180-260).
- Visible update: `handle_partial_update` replaces the streaming assistant view per partial (crates/pi/src/modes/interactive/runtime.rs:2196-2224) behind a <=16 ms coalescer (runtime.rs:104 BACKGROUND_COALESCE_WINDOW, armed :2193); the terminal write is coalesced, the event loop keeps every frame.
- Persistence is per message end (agent_session/persistence.rs `persist_message_end`), not per frame — the lane-3 "Session JSONL append" row amortizes to ~0.07 us/frame (18.34 us / 256) and is owned by session-append.md.

Boundary classification: the provider event funnel signature and the AgentEvent/
AgentSessionEvent shapes are **interior** (all consumers in-tree); the provider *wire*
(SSE/JSON from real providers) is **boundary** but is not exercised by the measured
verification lane (typed events). Unresolved channels: none on the measured path.

## Floors (computed)

```
decode/forward per frame (drain leg), computed from the contract ops
(uncontended, current-thread):
   Box::new(event) heap alloc                     ~20-30 ns
 + tokio bounded mpsc send (capacity 64)          ~50-100 ns
 + Arc::clone (refcount, Arc-at-birth)            ~1-2 ns
 + watch::send (store + watcher notify)           ~30-50 ns
 = ~101-182 ns  ->  the ~0.2 us class stands
state reduction per frame: fold + one event emission           ~0.15 us
visible update per frame: append delta to view buffer          ~0.1 us
```

No per-frame syscall is forced on any of the three legs (paint is coalesced and owned
by terminal-paint.md; persistence is per message end). Arc-at-birth (iteration 12)
lowered the watch term from a full-snapshot clone to a refcount store; the ~0.2 us
class remains the conservative bound.

## Measured cost (terminal — iteration 33)

The in-process instrument (`pi_agent_stream_frame_bench`, landed at iteration 10)
drives the real funnel on the pinned verification stream shape
(`PI_VERIFICATION_CHUNK_COUNT=256`, full snapshot per event, 6,144 B final text).
Fresh trusted distribution (release, `taskset -c 20-40`, 6 warmup + 27 measured
interleaved rounds; noise gate rs <= 20% on the predeclared median estimator):
drain **1,382 ns/frame** (rs 8.91%, PASS); funnel 1,848; reduce (funnel − drain)
466. Absolute levels drift with box state (the 2026-08-28 iteration-12 protocol
measured 2,426 ns on a contended box; repeat runs this session ranged
1,256-1,469 ns) — every observed level is far above 2x the floor.

E1 stage attribution (reversible stage disabling on the pinned workload):
source materialization + poll 441 ns (the contract-forced one-snapshot-per-frame),
lossy watch leg 272 ns (refcount publish + watcher wake), lossless mpsc leg
697 ns (boxed forward + drain-task→consumer handoff; **dominant**), cross-task
interaction −28 ns at medians (the per-round identity is exact; median
non-additivity disclosed, not clamped). Sum = 1,382 ns = the measured drain
median. Multiple vs the ~0.2 us decode/forward floor ≈ **6.9x**.

## Terminal classification (iteration 33)

**CONSTRAINED-ABOVE-FLOOR.** The watch/mpsc two-leg topology and the
one-snapshot-per-frame source materialization are contract, not slack: the
lossless loop leg forwards every event intact with cancellation at channel
capacity and an abortable lifecycle (drain.rs:90-107, :148, :171-180), the
lossy presentation leg is a per-frame latest-wins refcount publish
(drain.rs:226-232), and extensions consume the serialized partial. The
iteration-12 rebuild (Arc-at-birth snapshot sharing, 1.18x paired win) removed
every redundant O(message-length) copy. The iteration-10 blind candidates
(B pi-agent-only, C delta-carrying funnel, D partial-stripping drain,
E coalesced publish) remain boundary-rejected; the remaining in-boundary
designs miss the gate (unboxed `DrainItem` forward projects ~1.02x < 1.05x;
handoff micro-tuning is not materially distinct; fused single-task drain
crosses the cancellation/lifecycle contract). Full E1-E4 record with exact
reopen consents: t11-iterations.md iteration 33.


