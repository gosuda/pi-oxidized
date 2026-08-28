# Floor ledger: stream frame pipeline (decode / state reduction / visible update)

Owning R2 hot rows (lane 3): *Provider frame decode*, *Assistant state reduction*,
*Incremental visible content update*. State: **architecture-floor (terminal — residual classification recorded at iteration 13; drain 2426 ns vs ~200 ns decode/forward floor, channel/scheduler cost + one contract-forced source-side materialization per frame, no materially distinct design inside the unit boundary).**

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
decode/forward per frame: one mpsc send + watch store          ~0.2 us
   (channel send ~100-200 ns class; no syscall forced)
state reduction per frame: fold + one event emission           ~0.15 us
visible update per frame: append delta to view buffer          ~0.1 us
```

No per-frame syscall is forced on any of the three legs (paint is coalesced and owned
by terminal-paint.md; persistence is per message end).

## Measured cost — and why the multiple is unproven

The trusted lane-3 baseline (R2: 1.133 ms CPU per provider frame, process-tree) cannot
currently be decomposed onto these units: (a) the process tree includes the Bun
extension-host subprocess (the verification provider runs inside it) and the artifact
records no per-process CPU split; (b) a whole-process callgrind run of one stream turn
(166.6 M Ir) is dominated by startup (~first-frame scale, see first-frame-init.md) —
the turn-phase delta sits below the run-to-run startup variance, so per-function
attribution of the turn is not separable from the evidence in hand.

Decomposition status: attributed categories — amortized paint block <=26.5 us/frame
(subtraction, terminal-paint.md), amortized persistence ~0.07 us/frame
(session-append.md); **unattributed residual** ~= 1.133 ms - 26.5 us - 0.07 us - host
share, with the host share unmeasured. The residual is named, not estimated.

Measurement prerequisites (recorded for Phase-5 entry): per-process CPU sampling of
the pi child vs the extension host in the runner artifact, or a turn-phase-triggered
callgrind toggle (`--toggle-collect` on the drain entry points). Until one lands, the
three units hold **OPEN by the fail-closed rule**: an unproven multiple can never
declare AT-FLOOR, and each unit's ceiling share of the rank-1 lane keeps it a rebuild
candidate.

## Blind-derivation classification for Phase 5

Consumers interior; nothing published. The watch/mpsc topology is itself contract
(lossless loop leg + lossy presentation leg) — a rebuild must preserve both legs'
fidelity guarantees, sourced from drain.rs:105-118 and the coalescer bound.
