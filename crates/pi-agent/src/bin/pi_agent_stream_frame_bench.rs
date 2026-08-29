//! Stream frame pipeline benchmark (PERF-T11, `stream-frame-pipeline` unit).
//!
//! Pinned workload = the R2 lane-3 verification stream shape
//! (`scripts/verification/extension.ts` `streamVerification` with
//! `PI_VERIFICATION_CHUNK_COUNT=256`): `Start`, `TextStart`, 256 x
//! `TextDelta` (25-byte `verification-chunk-NNNN\n` chunks, full message
//! snapshot per frame), `TextEnd`, `Done`. This is the workload behind the
//! ledger's 1.133 ms-CPU-per-provider-frame baseline.
//!
//! Scenarios (in-process, current-thread runtime, counting event sink):
//!
//! - `funnel` — `run_agent_loop`: provider decode (source snapshot clone,
//!   wrapped in a fresh `Arc`) + drain (lossy watch + lossless mpsc forward)
//!   + reduce (fold + one `MessageUpdate` per frame).
//! - `drain` — `ProviderDrain::spawn` over the same rematerialized stream:
//!   the two channel legs without the reduce leg.
//! - `source` — stream poll + per-frame rematerialization in the drain
//!   loop's shape; both channel legs disabled (E1 materialization term).
//! - `source-watch` — source + lossy watch leg only (mpsc disabled).
//! - `source-forward` — source + lossless mpsc leg only (watch disabled).
//!
//! E1 stage attribution derives each term by stage disabling: watch leg =
//! `source-watch − source`; mpsc leg = `source-forward − source`; cross-task
//! interaction = `drain − source-watch − (source-forward − source)`. The
//! four terms sum to the measured drain median by construction (remainder
//! form); interaction absorbs task-placement and contention effects.
//!
//! `reduce` is disclosed as `funnel - drain` (arithmetic, not separately
//! timed): `consume_drain_items` is private and the funnel-minus-drain delta
//! isolates its per-frame fold/emit share. Because both `funnel` and `drain`
//! use the same `rematerialize` map, the provider's per-frame source cost
//! (one `AssistantMessage` clone wrapped in `Arc::new`) is present in both
//! scenarios and cancels out of the `funnel - drain` difference.
//!
//! Rounds are interleaved (funnel then drain per round) to decorrelate box
//! noise; medians are reported in ns/frame. Allocation counting needs the
//! pi-bench-alloc crate, which cannot be added here under the frozen
//! Cargo.lock; clone-count accounting is used for the allocation disclosure.
//!
//! Run:
//!   cargo run -p pi-agent --release --bin `pi_agent_stream_frame_bench`
//!
//! `STREAM_BENCH_MEASURED_ROUNDS` raises the measured-round count (default 9)
//! for the trusted-distribution protocol; per-round samples and rs (sample
//! stddev / mean) are printed for the noise gate.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use futures::stream::{self, BoxStream, StreamExt};
use pi_agent::config::{
    AgentContext, AgentLoopConfig, default_convert_to_llm_hook, text_user_message,
};
use pi_agent::tool::ToolExecutionMode;
use pi_agent::{EventSink, ProviderDrain, RunIo, run_agent_loop};
use pi_ai::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, DoneReason, Model,
    ModelCost, ModelInput, Provider, ProviderError, StopReason, StreamOptions, TextContent,
};
use serde_json::Map;
use std::collections::BTreeMap;
use tokio::runtime::Builder;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

// -- Pinned workload ----------------------------------------------------------

const FRAMES: usize = 256;
const WARMUP_ROUNDS: usize = 3;
const MEASURED_ROUNDS: usize = 9;

/// One provider frame: the verification chunk literal.
fn chunk(index: usize) -> String {
    format!("verification-chunk-{:04}\n", index + 1)
}

fn base_message() -> AssistantMessage {
    AssistantMessage::new("openai-completions", "openai", "m", 1)
}

/// Event-kind payload for one scripted frame, decoupled from the event object
/// so the same source can be rematerialized for every round.
#[derive(Clone)]
enum FrameKind {
    Start,
    TextStart,
    TextDelta { delta: String },
    TextEnd { content: String },
    Done { reason: DoneReason },
}

/// Canonical snapshot template for one scripted frame. `inner` is a shared
/// `Arc<AssistantMessage>`; the consumer (`FrameProvider` or the drain stream)
/// rematerializes the `AssistantMessageEvent` by cloning the inner message and
/// wrapping it in a fresh `Arc`, mirroring `AssistantState::snapshot`'s cost.
#[derive(Clone)]
struct FrameTemplate {
    content_index: u64,
    inner: Arc<AssistantMessage>,
    kind: FrameKind,
}

/// Builds the verification stream script: shared canonical `Arc<AssistantMessage>`
/// snapshots grow per frame exactly as `AssistantState` materializes them.
fn frame_script() -> Vec<FrameTemplate> {
    let mut script = Vec::with_capacity(FRAMES + 4);

    let base = base_message();
    script.push(FrameTemplate {
        content_index: 0,
        inner: Arc::new(base.clone()),
        kind: FrameKind::Start,
    });

    let mut partial = base;
    partial
        .content
        .push(AssistantContent::Text(TextContent::new("")));
    script.push(FrameTemplate {
        content_index: 0,
        inner: Arc::new(partial.clone()),
        kind: FrameKind::TextStart,
    });

    let mut text = String::new();
    for index in 0..FRAMES {
        let delta = chunk(index);
        text.push_str(&delta);
        if let AssistantContent::Text(block) = &mut partial.content[0] {
            block.text.push_str(&delta);
        }
        script.push(FrameTemplate {
            content_index: 0,
            inner: Arc::new(partial.clone()),
            kind: FrameKind::TextDelta { delta },
        });
    }

    script.push(FrameTemplate {
        content_index: 0,
        inner: Arc::new(partial.clone()),
        kind: FrameKind::TextEnd {
            content: text.clone(),
        },
    });

    let mut final_message = partial;
    final_message.stop_reason = StopReason::Stop;
    script.push(FrameTemplate {
        content_index: 0,
        inner: Arc::new(final_message),
        kind: FrameKind::Done {
            reason: DoneReason::Stop,
        },
    });

    script
}

/// Rematerialize a provider stream from the shared script. Each per-frame
/// `partial` is produced by `Arc::new(inner.clone())`, preserving the source
/// cost class of `AssistantState::snapshot()`.
fn rematerialize(
    script: Arc<Vec<FrameTemplate>>,
) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
    let len = script.len();
    stream::iter((0..len).map(move |i| script[i].clone()).map(|frame| {
        let inner = frame.inner.as_ref();
        let partial = Arc::new(inner.clone());
        Ok(match frame.kind {
            FrameKind::Start => AssistantMessageEvent::Start { partial },
            FrameKind::TextStart => AssistantMessageEvent::TextStart {
                content_index: frame.content_index,
                partial,
            },
            FrameKind::TextDelta { delta } => AssistantMessageEvent::TextDelta {
                content_index: frame.content_index,
                delta,
                partial,
            },
            FrameKind::TextEnd { content } => AssistantMessageEvent::TextEnd {
                content_index: frame.content_index,
                content,
                partial,
            },
            FrameKind::Done { reason } => AssistantMessageEvent::Done {
                reason,
                message: frame.inner.as_ref().clone(),
            },
        })
    }))
    .boxed()
}

/// Replays the script as a provider stream. Each yield rematerializes the event
/// with `Arc::new(inner.clone())`, mirroring `AssistantState::snapshot`'s
/// source cost: one canonical message clone wrapped in a fresh `Arc` per frame.
#[derive(Clone)]
struct FrameProvider {
    script: Arc<Vec<FrameTemplate>>,
}

impl Provider for FrameProvider {
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
        rematerialize(Arc::clone(&self.script))
    }
}

// -- Measurement support -------------------------------------------------------

/// Synchronous sink that counts the events the reduce leg emits and drops
/// them (the bench has no UI attached).
struct CountingSink {
    updates: AtomicU64,
    ends: AtomicU64,
}

impl CountingSink {
    fn new() -> Self {
        Self {
            updates: AtomicU64::new(0),
            ends: AtomicU64::new(0),
        }
    }
}

impl EventSink for CountingSink {
    fn emit(&self, event: pi_agent::AgentEvent) {
        match event {
            pi_agent::AgentEvent::MessageUpdate { .. } => {
                self.updates.fetch_add(1, Ordering::Relaxed);
            }
            pi_agent::AgentEvent::MessageEnd { .. } => {
                self.ends.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

fn bench_model() -> Model {
    Model {
        id: "m".to_owned(),
        name: "m".to_owned(),
        api: "openai-completions".to_owned(),
        provider: "openai".to_owned(),
        base_url: "https://example.test".to_owned(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 8_192,
        max_tokens: 1_024,
        headers: None,
        compat: None,
        extra: BTreeMap::new(),
    }
}

fn bench_config() -> AgentLoopConfig {
    AgentLoopConfig {
        model: bench_model(),
        reasoning: None,
        temperature: None,
        max_tokens: None,
        session_id: None,
        transport: None,
        cache_retention: None,
        thinking_budgets: None,
        max_retry_delay_ms: None,
        metadata: None,
        headers: None,
        env: None,
        stream_extra: Map::new(),
        tool_execution: ToolExecutionMode::Parallel,
        convert_to_llm: default_convert_to_llm_hook(),
        transform_context: None,
        get_api_key: None,
        should_stop_after_turn: None,
        prepare_next_turn: None,
        get_steering_messages: None,
        get_follow_up_messages: None,
        before_tool_call: None,
        after_tool_call: None,
        on_payload: None,
        on_response: None,
        telemetry: pi_agent::telemetry::noop_context(),
    }
}

/// One measured funnel round: provider -> drain -> reduce, end to end.
/// Returns `(elapsed_ns, message_update_count)`.
fn bench_funnel(script: &Arc<Vec<FrameTemplate>>) -> (u64, u64) {
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("bench runtime: {error}"));
    let provider = FrameProvider {
        script: Arc::clone(script),
    };
    let sink = CountingSink::new();
    let config = bench_config();
    let (partial_tx, partial_rx) = watch::channel(None);

    let start = Instant::now();
    let result = rt.block_on(async {
        // Presentation leg consumer: mirrors the interactive runtime polling
        // the lossy watch (latest-wins) while the stream runs.
        let watcher = tokio::spawn(async move {
            let mut partial_rx = partial_rx;
            loop {
                if partial_rx.changed().await.is_err() {
                    break;
                }
                std::hint::black_box(partial_rx.borrow_and_update());
            }
        });

        let io = RunIo {
            sink: &sink,
            provider: &provider,
            partial: partial_tx,
        };
        let messages = run_agent_loop(
            vec![text_user_message("bench")],
            AgentContext {
                system_prompt: String::new(),
                messages: Vec::new(),
                tools: Vec::new(),
            },
            config,
            io,
            CancellationToken::new(),
        )
        .await
        .expect("bench funnel run");
        watcher.abort();
        messages
    });
    let elapsed = start.elapsed();
    drop(result);
    (
        u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
        sink.updates.load(Ordering::Relaxed),
    )
}

/// One measured drain round: the two channel legs alone (lossy watch +
/// lossless mpsc), no reduce leg. Uses the same `rematerialize` map as
/// `FrameProvider` so the `funnel - drain` delta is exactly the reduce leg.
/// Returns `(elapsed_ns, delivered_items)`.
fn bench_drain(script: &Arc<Vec<FrameTemplate>>) -> (u64, u64) {
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("bench runtime: {error}"));
    let start = Instant::now();
    let delivered = rt.block_on(async {
        let (event_tx, mut event_rx) =
            mpsc::channel::<pi_agent::DrainItem>(pi_agent::DRAIN_EVENT_CAPACITY);
        let (partial_tx, mut partial_rx) = watch::channel(None);
        let watcher = tokio::spawn(async move {
            loop {
                if partial_rx.changed().await.is_err() {
                    break;
                }
                std::hint::black_box(partial_rx.borrow_and_update());
            }
        });
        let cancel = CancellationToken::new();
        let stream = rematerialize(Arc::clone(script));
        let drain = ProviderDrain::spawn(stream, partial_tx, event_tx, cancel);

        let mut count = 0u64;
        while let Some(item) = event_rx.recv().await {
            std::hint::black_box(&item);
            count += 1;
        }
        let _ = drain.await;
        watcher.abort();
        count
    });
    let elapsed = start.elapsed();
    (
        u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
        delivered,
    )
}

/// `AssistantMessageEvent` partial accessor, identical to the private
/// `drain.rs::event_partial` (copied because it is private; the pinned
/// workload exercises the Start/TextStart/TextDelta/TextEnd/Done subset).
fn event_partial(event: &AssistantMessageEvent) -> Option<&Arc<AssistantMessage>> {
    match event {
        AssistantMessageEvent::Start { partial }
        | AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial),
        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => None,
    }
}

/// One measured source round: the drain loop's stream poll + per-frame
/// rematerialization (`Arc::new(inner.clone())`) with both channel legs
/// disabled. Returns `(elapsed_ns, events_pulled)`.
fn bench_source(script: &Arc<Vec<FrameTemplate>>) -> (u64, u64) {
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("bench runtime: {error}"));
    let start = Instant::now();
    let count = rt.block_on(async {
        let cancel = CancellationToken::new();
        let mut stream = rematerialize(Arc::clone(script));
        let mut count = 0u64;
        loop {
            let next = tokio::select! {
                () = cancel.cancelled() => break,
                item = stream.next() => item,
            };
            let Some(Ok(event)) = next else { break };
            std::hint::black_box(&event);
            count += 1;
        }
        count
    });
    let elapsed = start.elapsed();
    (u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX), count)
}

/// One measured source+watch round: the source leg plus the lossy watch leg
/// (refcount-only publish identical to `drain.rs::publish_partial`, watcher
/// task identical to `bench_drain`'s) with the mpsc forward disabled.
/// Returns `(elapsed_ns, events_pulled)`.
fn bench_source_watch(script: &Arc<Vec<FrameTemplate>>) -> (u64, u64) {
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("bench runtime: {error}"));
    let start = Instant::now();
    let count = rt.block_on(async {
        let (partial_tx, mut partial_rx) = watch::channel(None);
        let watcher = tokio::spawn(async move {
            loop {
                if partial_rx.changed().await.is_err() {
                    break;
                }
                std::hint::black_box(partial_rx.borrow_and_update());
            }
        });
        let cancel = CancellationToken::new();
        let mut stream = rematerialize(Arc::clone(script));
        let mut count = 0u64;
        loop {
            let next = tokio::select! {
                () = cancel.cancelled() => break,
                item = stream.next() => item,
            };
            let Some(Ok(event)) = next else { break };
            if let Some(partial) = event_partial(&event) {
                let _ = partial_tx.send(Some(Arc::clone(partial)));
            }
            std::hint::black_box(&event);
            count += 1;
        }
        watcher.abort();
        count
    });
    let elapsed = start.elapsed();
    (u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX), count)
}

/// One measured source+forward round: the source leg inside a spawned task
/// (as `ProviderDrain::spawn` positions it) plus the lossless mpsc leg
/// (boxed `DrainItem` forward under the `send_item`-shaped cancellation
/// select, receiver loop identical to `bench_drain`'s) with the watch leg
/// disabled. Returns `(elapsed_ns, delivered_items)`.
fn bench_source_forward(script: &Arc<Vec<FrameTemplate>>) -> (u64, u64) {
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("bench runtime: {error}"));
    let start = Instant::now();
    let delivered = rt.block_on(async {
        let (event_tx, mut event_rx) =
            mpsc::channel::<pi_agent::DrainItem>(pi_agent::DRAIN_EVENT_CAPACITY);
        let cancel = CancellationToken::new();
        let stream = rematerialize(Arc::clone(script));
        let forwarder = tokio::spawn(async move {
            let mut stream = stream;
            loop {
                let next = tokio::select! {
                    () = cancel.cancelled() => return,
                    item = stream.next() => item,
                };
                let Some(Ok(event)) = next else {
                    return;
                };
                tokio::select! {
                    () = cancel.cancelled() => return,
                    result = event_tx
                        .send(pi_agent::DrainItem::Event(Box::new(event))) =>
                    {
                        if result.is_err() {
                            return;
                        }
                    }
                }
            }
        });
        let mut count = 0u64;
        while let Some(item) = event_rx.recv().await {
            std::hint::black_box(&item);
            count += 1;
        }
        let _ = forwarder.await;
        count
    });
    let elapsed = start.elapsed();
    (
        u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
        delivered,
    )
}

/// Measured-round count: `STREAM_BENCH_MEASURED_ROUNDS` override (>= 9),
/// else the 9-round default.
fn measured_rounds() -> usize {
    std::env::var("STREAM_BENCH_MEASURED_ROUNDS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|&rounds| rounds >= MEASURED_ROUNDS)
        .unwrap_or(MEASURED_ROUNDS)
}

/// Warmup-round count: `STREAM_BENCH_WARMUP_ROUNDS` override, else 3.
fn warmup_rounds() -> usize {
    std::env::var("STREAM_BENCH_WARMUP_ROUNDS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(WARMUP_ROUNDS)
}

/// rs: sample standard deviation / mean, in percent (noise-gate input).
fn rs_pct(values: &[u64]) -> f64 {
    let n = values.len();
    if n < 2 {
        return 0.0;
    }
    let mean = values.iter().map(|&v| v as f64).sum::<f64>() / n as f64;
    if mean == 0.0 {
        return 0.0;
    }
    let variance = values
        .iter()
        .map(|&v| {
            let delta = v as f64 - mean;
            delta * delta
        })
        .sum::<f64>()
        / (n - 1) as f64;
    (variance.sqrt() / mean) * 100.0
}

fn median(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

fn main() {
    let script = Arc::new(frame_script());
    let final_len = script.last().map_or(0, |frame| match &frame.kind {
        FrameKind::Done { .. } => frame.inner.as_ref().content.first().map_or(0, |c| match c {
            AssistantContent::Text(block) => block.text.len(),
            _ => 0,
        }),
        _ => 0,
    });
    let rounds = measured_rounds();
    let warmups = warmup_rounds();

    let mut funnel_ns = Vec::with_capacity(rounds);
    let mut drain_ns = Vec::with_capacity(rounds);
    let mut source_ns = Vec::with_capacity(rounds);
    let mut source_watch_ns = Vec::with_capacity(rounds);
    let mut source_forward_ns = Vec::with_capacity(rounds);

    for round in 0..warmups + rounds {
        let measured = round >= warmups;

        let (ns, updates) = bench_funnel(&script);
        assert_eq!(
            updates,
            (FRAMES + 2) as u64,
            "funnel must emit one MessageUpdate per non-start partial event"
        );
        if measured {
            funnel_ns.push(ns / FRAMES as u64);
        }

        let (ns, items) = bench_drain(&script);
        assert_eq!(
            items,
            (FRAMES + 4) as u64,
            "drain must forward every scripted event losslessly"
        );
        if measured {
            drain_ns.push(ns / FRAMES as u64);
        }

        let (ns, events) = bench_source(&script);
        assert_eq!(
            events,
            (FRAMES + 4) as u64,
            "source must pull every scripted event"
        );
        if measured {
            source_ns.push(ns / FRAMES as u64);
        }

        let (ns, events) = bench_source_watch(&script);
        assert_eq!(
            events,
            (FRAMES + 4) as u64,
            "source-watch must pull every scripted event"
        );
        if measured {
            source_watch_ns.push(ns / FRAMES as u64);
        }

        let (ns, items) = bench_source_forward(&script);
        assert_eq!(
            items,
            (FRAMES + 4) as u64,
            "source-forward must deliver every scripted event"
        );
        if measured {
            source_forward_ns.push(ns / FRAMES as u64);
        }
    }

    let funnel_rs = rs_pct(&funnel_ns);
    let drain_rs = rs_pct(&drain_ns);
    let source_rs = rs_pct(&source_ns);
    let source_watch_rs = rs_pct(&source_watch_ns);
    let source_forward_rs = rs_pct(&source_forward_ns);
    let drain_samples: Vec<u64> = drain_ns.clone();
    let source_samples: Vec<u64> = source_ns.clone();
    let source_watch_samples: Vec<u64> = source_watch_ns.clone();
    let source_forward_samples: Vec<u64> = source_forward_ns.clone();

    let funnel_ns = median(&mut funnel_ns);
    let drain_ns = median(&mut drain_ns);
    let source_ns = median(&mut source_ns);
    let source_watch_ns = median(&mut source_watch_ns);
    let source_forward_ns = median(&mut source_forward_ns);
    let reduce_ns = funnel_ns.saturating_sub(drain_ns);
    let watch_leg = source_watch_ns as i64 - source_ns as i64;
    let forward_leg = source_forward_ns as i64 - source_ns as i64;
    let interaction = drain_ns as i64 - source_watch_ns as i64 - forward_leg;
    let attributed_sum = source_ns as i64 + watch_leg + forward_leg + interaction;

    println!(
        "stream-frame-pipeline bench (pinned: {FRAMES} x verification-chunk frames, final text {final_len} B)"
    );
    println!("protocol: release, medians of {rounds} interleaved rounds after {warmups} warmups");
    println!();
    println!("scenario | median ns/frame | rs");
    println!("funnel (decode+forward+reduce)   | {funnel_ns} | {funnel_rs:.2}%");
    println!("drain (decode+forward only)      | {drain_ns} | {drain_rs:.2}%");
    println!("source (materialization+poll)    | {source_ns} | {source_rs:.2}%");
    println!("source-watch (source+lossy leg)  | {source_watch_ns} | {source_watch_rs:.2}%");
    println!("source-forward (source+lossless) | {source_forward_ns} | {source_forward_rs:.2}%");
    println!("reduce (funnel - drain)          | {reduce_ns}");
    println!();
    println!("E1 stage attribution (stage disabling; sums to the drain median by construction):");
    println!("source materialization+poll      = {source_ns} ns/frame");
    println!("watch leg (refcount publish)     = {watch_leg} ns/frame  = source-watch - source");
    println!(
        "mpsc leg (boxed fwd + handoff)   = {forward_leg} ns/frame  = source-forward - source"
    );
    println!(
        "cross-task interaction           = {interaction} ns/frame  = drain - source-watch - (source-forward - source)"
    );
    println!(
        "attributed sum                   = {attributed_sum} ns/frame (drain median {drain_ns})"
    );
    println!();
    println!("drain samples (ns/frame): {drain_samples:?}");
    println!("source samples (ns/frame): {source_samples:?}");
    println!("source-watch samples (ns/frame): {source_watch_samples:?}");
    println!("source-forward samples (ns/frame): {source_forward_samples:?}");
    println!();
    println!("ledger floor: decode/forward ~200 ns + reduce ~150 ns (per frame)");
}
