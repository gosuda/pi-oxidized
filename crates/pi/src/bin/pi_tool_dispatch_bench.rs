//! PERF-T5: dispatch-only tool benchmark with a no-op deterministic tool.
//!
//! Times per-call tool dispatch on the production `pi_agent::execute_tool_calls`
//! path with a no-op tool, exercising exactly the units the ticket names:
//! argument validation (prepare + validate), tool start/update/end events,
//! result construction, and session append. The real `read`/`edit`/`bash`
//! tools stay out of this bench on purpose — they are covered end to end by
//! `scripts/verification/e2e-smoke.ts` and their filesystem/shell work is cold
//! (varies independently of dispatch).
//!
//! Matched boundary with the TypeScript worker in
//! `scripts/bench-tool-dispatch.ts` (which drives upstream `runAgentLoop`
//! from `.references/pi-2.0`): the timed slice starts when the event sink
//! receives `tool_execution_start` and ends when the sink has appended the
//! tool-result message to a real `SessionManager` JSONL file. Loop/stream
//! overhead sits outside the slice on both implementations.
//!
//! Per call the bench also appends the assistant message carrying the tool
//! call to the session (before the slice), mirroring what the product does
//! per tool call; the append is identical work on both implementations.
//!
//! Run: `cargo run -p pi --release --bin pi_tool_dispatch_bench -- \
//!   --calls 3000 --warmup 300 --blocks 1 --session-dir <dir> [--arguments invalid]`

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::process::ExitCode;
use std::time::Instant;

use pi::core::sessions::SessionManager;
use pi_agent::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentTool, AgentToolResult,
    EmitAgentEvent, ToolError, ToolUpdates, execute_tool_calls, now_millis,
};
use pi_ai::{
    AssistantContent, AssistantMessage, Message, Model, ModelCost, ModelInput, StopReason,
    TextContent, ToolCall, ToolResultContent,
};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

/// JSON Schema for the noop tool, identical to the TypeScript worker's.
fn noop_parameters() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "minLength": 1 },
            "count": { "type": "integer", "minimum": 1, "maximum": 64 }
        },
        "required": ["path"],
        "additionalProperties": false
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NoopInput {
    path: String,
    count: Option<u8>,
}

fn parse_noop_input(args: &Map<String, Value>) -> Result<NoopInput, ToolError> {
    let input: NoopInput = serde_json::from_value(Value::Object(args.clone()))
        .map_err(|error| ToolError::new(format!("noop tool input is invalid. {error}")))?;
    if input.path.is_empty() {
        return Err(ToolError::new(
            "noop tool input is invalid. path must be non-empty",
        ));
    }
    if let Some(count) = input.count
        && !(1..=64).contains(&count)
    {
        return Err(ToolError::new(
            "noop tool input is invalid. count must be between 1 and 64",
        ));
    }
    Ok(input)
}

/// No-op deterministic tool. `execute` re-parses the arguments (like the real
/// tools do), emits exactly one partial update, and returns a fixed result.
struct NoopTool {
    parameters: Value,
}

impl AgentTool for NoopTool {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn label(&self) -> &'static str {
        "noop"
    }

    fn description(&self) -> &'static str {
        "Benchmark no-op tool; validates arguments, emits one update, returns a fixed result."
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn validate_arguments(
        &self,
        args: &Map<String, Value>,
    ) -> Result<Map<String, Value>, ToolError> {
        parse_noop_input(args)?;
        Ok(args.clone())
    }

    fn execute(
        &self,
        _tool_call_id: &str,
        args: Map<String, Value>,
        _cancel: CancellationToken,
        updates: ToolUpdates,
    ) -> futures::future::BoxFuture<'static, Result<AgentToolResult, ToolError>> {
        Box::pin(async move {
            let input = parse_noop_input(&args)?;
            let count = input.count.unwrap_or(1);
            updates.send(AgentToolResult {
                content: vec![ToolResultContent::Text(TextContent::new("noop progress"))],
                details: serde_json::json!({ "kind": "noop-progress", "count": count }),
                ..AgentToolResult::default()
            });
            Ok(AgentToolResult {
                content: vec![ToolResultContent::Text(TextContent::new(format!(
                    "noop ok: {} x{count}",
                    input.path
                )))],
                details: serde_json::json!({
                    "kind": "noop",
                    "path": input.path,
                    "count": count
                }),
                ..AgentToolResult::default()
            })
        })
    }
}

/// Sink state: receives agent events, appends messages to the session, and
/// times the per-call dispatch slice (start event → tool-result append done).
struct Sink {
    session: RefCell<SessionManager>,
    t0: Cell<Option<Instant>>,
    slices: RefCell<Vec<u64>>,
    starts: Cell<u64>,
    updates: Cell<u64>,
    ends: Cell<u64>,
    error_results: Cell<u64>,
    appends: Cell<u64>,
    failure: RefCell<Option<String>>,
}

impl Sink {
    fn new(session: SessionManager) -> Self {
        Self {
            session: RefCell::new(session),
            t0: Cell::new(None),
            slices: RefCell::new(Vec::new()),
            starts: Cell::new(0),
            updates: Cell::new(0),
            ends: Cell::new(0),
            error_results: Cell::new(0),
            appends: Cell::new(0),
            failure: RefCell::new(None),
        }
    }

    /// Appends the assistant message that carries the tool call (before the
    /// timed slice), mirroring the TypeScript worker's sink on assistant
    /// `message_end`.
    fn append_assistant(&self, message: &AgentMessage) {
        if let Err(error) = self.session.borrow_mut().append_message(message) {
            *self.failure.borrow_mut() = Some(format!("assistant append failed: {error}"));
        }
        self.appends.set(self.appends.get() + 1);
    }

    fn session_bytes(&self) -> u64 {
        self.session
            .borrow()
            .get_session_file()
            .and_then(|path| std::fs::metadata(path).ok())
            .map_or(0, |meta| meta.len())
    }

    fn session_file(&self) -> Option<String> {
        self.session.borrow().get_session_file().map(str::to_owned)
    }
}

impl EmitAgentEvent for Sink {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::ToolExecutionStart { .. } => {
                self.t0.set(Some(Instant::now()));
                self.starts.set(self.starts.get() + 1);
            }
            AgentEvent::ToolExecutionUpdate { .. } => {
                self.updates.set(self.updates.get() + 1);
            }
            AgentEvent::ToolExecutionEnd { is_error, .. } => {
                self.ends.set(self.ends.get() + 1);
                if is_error {
                    self.error_results.set(self.error_results.get() + 1);
                }
            }
            AgentEvent::MessageEnd { message } => {
                if let AgentMessage::Llm(inner) = &message
                    && matches!(inner.as_ref(), Message::ToolResult(_))
                {
                    if let Err(error) = self.session.borrow_mut().append_message(&message) {
                        *self.failure.borrow_mut() =
                            Some(format!("tool-result append failed: {error}"));
                    }
                    self.appends.set(self.appends.get() + 1);
                    if let Some(t0) = self.t0.take() {
                        #[expect(
                            clippy::cast_possible_truncation,
                            reason = "Instant::elapsed nanos fit in u64 for any realistic bench duration"
                        )]
                        self.slices
                            .borrow_mut()
                            .push(t0.elapsed().as_nanos() as u64);
                    }
                }
            }
            AgentEvent::AgentStart
            | AgentEvent::AgentEnd { .. }
            | AgentEvent::TurnStart
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::MessageStart { .. }
            | AgentEvent::MessageUpdate { .. } => {}
        }
    }
}

// ── Process CPU time (µs) via /proc/self/stat — Linux, no unsafe ──────────

#[cfg(target_os = "linux")]
mod cpu {
    /// User + system CPU of the whole process in microseconds.
    ///
    /// `clk_tck` is the kernel clock-tick rate (`getconf CLK_TCK`, `USER_HZ`);
    /// the orchestrator passes it in. Granularity is one tick (typically
    /// 10 ms), so measured blocks must be large enough to average it out.
    pub fn cpu_micros(clk_tck: u64) -> Option<u128> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        // Skip "pid (comm)"; comm may contain spaces.
        let after_comm = stat.rfind(')')? + 2;
        let fields: Vec<&str> = stat[after_comm..].split_whitespace().collect();
        let utime: u128 = fields.get(11)?.parse().ok()?;
        let stime: u128 = fields.get(12)?.parse().ok()?;
        Some((utime + stime) * 1_000_000 / u128::from(clk_tck.max(1)))
    }
}

#[cfg(not(target_os = "linux"))]
mod cpu {
    /// CPU attribution is Linux-only in this bench, mirroring the /proc-based
    /// lanes in `scripts/verification/performance.ts`.
    pub fn cpu_micros(_clk_tck: u64) -> Option<u128> {
        None
    }
}

fn sample_model() -> Model {
    Model {
        id: "noop-bench".to_owned(),
        name: "noop-bench".to_owned(),
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
    AgentLoopConfig::base(sample_model())
}

fn valid_arguments() -> Map<String, Value> {
    let mut args = Map::new();
    args.insert(
        "path".to_owned(),
        Value::String("bench/noop/input.txt".to_owned()),
    );
    args.insert("count".to_owned(), Value::from(3u64));
    args
}

fn invalid_arguments() -> Map<String, Value> {
    let mut args = Map::new();
    // count 999 exceeds the schema maximum, so both implementations reject
    // the payload during argument validation (upstream coerces mistyped
    // primitives instead of rejecting them, so the shared rejection case
    // must violate a range constraint).
    args.insert(
        "path".to_owned(),
        Value::String("bench/noop/input.txt".to_owned()),
    );
    args.insert("count".to_owned(), Value::from(999u64));
    args
}

struct Args {
    clk_tck: u64,
    calls: usize,
    warmup: usize,
    blocks: usize,
    session_dir: String,
    invalid: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        clk_tck: 100,
        calls: 10_000,
        warmup: 1_000,
        blocks: 1,
        session_dir: std::env::temp_dir()
            .join("pi-tool-dispatch-bench")
            .to_string_lossy()
            .into_owned(),
        invalid: false,
    };
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < raw.len() {
        let flag = raw[index].clone();
        index += 1;
        let mut value = || -> Result<String, String> {
            let value = raw.get(index).cloned();
            index += 1;
            value.ok_or_else(|| format!("missing value for {flag}"))
        };
        match flag.as_str() {
            "--clk-tck" => {
                args.clk_tck = value()?.parse().map_err(|e| format!("--clk-tck: {e}"))?;
            }
            "--calls" => args.calls = value()?.parse().map_err(|e| format!("--calls: {e}"))?,
            "--warmup" => args.warmup = value()?.parse().map_err(|e| format!("--warmup: {e}"))?,
            "--blocks" => args.blocks = value()?.parse().map_err(|e| format!("--blocks: {e}"))?,
            "--session-dir" => args.session_dir = value()?,
            "--arguments" => {
                let mode = value()?;
                match mode.as_str() {
                    "valid" => args.invalid = false,
                    "invalid" => args.invalid = true,
                    other => {
                        return Err(format!("--arguments must be valid|invalid, got {other}"));
                    }
                }
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if args.calls == 0 || args.blocks == 0 {
        return Err("--calls and --blocks must be positive".to_owned());
    }
    Ok(args)
}

fn tool_call_message(index: usize, args: &Map<String, Value>) -> AssistantMessage {
    let tool_call = ToolCall::new(format!("call-{index}"), "noop", args.clone());
    let mut message =
        AssistantMessage::new("openai-completions", "openai", "noop-bench", now_millis());
    message.content.push(AssistantContent::ToolCall(tool_call));
    message.stop_reason = StopReason::ToolUse;
    message
}

struct BlockResult {
    index: usize,
    calls: usize,
    wall_ms_per_call: f64,
    wall_median_ns: u64,
    wall_min_ns: u64,
    wall_max_ns: u64,
    cpu_ms_per_call: Option<f64>,
}

#[expect(
    clippy::cast_precision_loss,
    reason = "bench binary: u128/usize to f64 for timing statistics; f64 precision is sufficient for benchmark reporting"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "bench harness: all parameters are used directly in the timed dispatch loop; grouping would obscure the benchmark's flat structure"
)]
fn run_block(
    sink: &Sink,
    context: &AgentContext,
    config: &AgentLoopConfig,
    args: &Map<String, Value>,
    calls: usize,
    block_index: usize,
    cancel: &CancellationToken,
    runtime: &tokio::runtime::Runtime,
    clk_tck: u64,
) -> Result<BlockResult, String> {
    sink.slices.borrow_mut().clear();
    if calls == 0 {
        // Degenerate block (e.g. --warmup 0): nothing to time or index.
        return Ok(BlockResult {
            index: block_index,
            calls,
            wall_ms_per_call: 0.0,
            wall_median_ns: 0,
            wall_min_ns: 0,
            wall_max_ns: 0,
            cpu_ms_per_call: cpu::cpu_micros(clk_tck).map(|_| 0.0),
        });
    }
    let cpu_before = cpu::cpu_micros(clk_tck);
    for index in 0..calls {
        let message = tool_call_message(index, args);
        sink.append_assistant(&AgentMessage::Llm(Box::new(Message::Assistant(
            message.clone(),
        ))));
        let batch = runtime
            .block_on(execute_tool_calls(context, &message, config, cancel, sink))
            .map_err(|error| format!("execute_tool_calls failed: {error}"))?;
        if batch.messages.len() != 1 {
            return Err(format!(
                "expected 1 tool-result message per call, got {}",
                batch.messages.len()
            ));
        }
        if let Some(failure) = sink.failure.borrow().as_ref() {
            return Err(failure.clone());
        }
    }
    let cpu_after = cpu::cpu_micros(clk_tck);
    let slices = sink.slices.borrow();
    if slices.len() != calls {
        return Err(format!(
            "expected {calls} timed slices, got {}",
            slices.len()
        ));
    }
    let total_nanos: u128 = slices.iter().map(|n| u128::from(*n)).sum();
    let mut sorted = slices.clone();
    sorted.sort_unstable();
    let cpu_ms_per_call = match (cpu_before, cpu_after) {
        (Some(before), Some(after)) => Some((after - before) as f64 / calls as f64 / 1_000.0),
        (None, None) => None,
        _ => return Err("getrusage became unavailable mid-block".to_owned()),
    };
    Ok(BlockResult {
        index: block_index,
        calls,
        wall_ms_per_call: total_nanos as f64 / calls as f64 / 1_000_000.0,
        wall_median_ns: sorted[calls / 2],
        wall_min_ns: sorted[0],
        wall_max_ns: sorted[calls - 1],
        cpu_ms_per_call,
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "bench binary main: linear setup-to-report flow; extracting sub-functions would obscure the sequential benchmark lifecycle"
)]
fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("pi_tool_dispatch_bench: {error}");
            return ExitCode::FAILURE;
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("pi_tool_dispatch_bench: failed to build runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    let session = match SessionManager::create(".", Some(&args.session_dir), None) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("pi_tool_dispatch_bench: failed to create session: {error}");
            return ExitCode::FAILURE;
        }
    };
    let sink = Sink::new(session);

    let tool = std::sync::Arc::new(NoopTool {
        parameters: noop_parameters(),
    });
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![tool],
    };
    let config = bench_config();
    let cancel = CancellationToken::new();
    let call_args = if args.invalid {
        invalid_arguments()
    } else {
        valid_arguments()
    };

    // Warmup: exercise the same path without recording slices. Counter
    // snapshots are taken after warmup so totals cover measured blocks only.
    if let Err(error) = run_block(
        &sink,
        &context,
        &config,
        &call_args,
        args.warmup,
        0,
        &cancel,
        &runtime,
        args.clk_tck,
    ) {
        eprintln!("pi_tool_dispatch_bench: warmup failed: {error}");
        return ExitCode::FAILURE;
    }
    let (warmup_starts, warmup_updates, warmup_ends, warmup_errors, warmup_appends) = (
        sink.starts.get(),
        sink.updates.get(),
        sink.ends.get(),
        sink.error_results.get(),
        sink.appends.get(),
    );
    let bytes_after_warmup = sink.session_bytes();

    let mut blocks = Vec::with_capacity(args.blocks);
    for block_index in 0..args.blocks {
        match run_block(
            &sink,
            &context,
            &config,
            &call_args,
            args.calls,
            block_index,
            &cancel,
            &runtime,
            args.clk_tck,
        ) {
            Ok(result) => blocks.push(result),
            Err(error) => {
                eprintln!("pi_tool_dispatch_bench: block {block_index} failed: {error}");
                return ExitCode::FAILURE;
            }
        }
    }

    let bytes_after = sink.session_bytes();
    let starts = sink.starts.get() - warmup_starts;
    let updates = sink.updates.get() - warmup_updates;
    let ends = sink.ends.get() - warmup_ends;
    let error_results = sink.error_results.get() - warmup_errors;
    let appends = sink.appends.get() - warmup_appends;
    let measured_calls = args.calls * args.blocks;

    // Protocol verification: identical event contract for every measured call.
    let expected_updates = if args.invalid { 0 } else { measured_calls };
    let expected_errors = if args.invalid { measured_calls } else { 0 };
    if starts != measured_calls as u64
        || ends != measured_calls as u64
        || updates != expected_updates as u64
        || error_results != expected_errors as u64
        || appends != (measured_calls * 2) as u64
    {
        eprintln!(
            "pi_tool_dispatch_bench: protocol mismatch: starts={starts} ends={ends} \
             updates={updates} (expected {expected_updates}) error_results={error_results} \
             (expected {expected_errors}) appends={appends} (expected {}) calls={measured_calls}",
            measured_calls * 2
        );
        return ExitCode::FAILURE;
    }

    let blocks_json: Vec<Value> = blocks
        .iter()
        .map(|block| {
            serde_json::json!({
                "index": block.index,
                "calls": block.calls,
                "wallMsPerCall": block.wall_ms_per_call,
                "wallMedianNs": block.wall_median_ns,
                "wallMinNs": block.wall_min_ns,
                "wallMaxNs": block.wall_max_ns,
                "cpuMsPerCall": block.cpu_ms_per_call,
            })
        })
        .collect();

    let report = serde_json::json!({
        "implementation": "rust",
        "argumentsMode": if args.invalid { "invalid" } else { "valid" },
        "warmupCalls": args.warmup,
        "callsPerBlock": args.calls,
        "blocks": blocks_json,
        "events": {
            "start": starts,
            "update": updates,
            "end": ends,
            "errorResults": error_results
        },
        "appends": appends,
        "session": {
            "file": sink.session_file().map_or_else(|| Value::Null, Value::String),
            "bytesDelta": bytes_after.saturating_sub(bytes_after_warmup),
            "headerEntries": 1
        },
        "ok": true,
        "failure": null
    });

    println!("{report}");
    eprintln!(
        "pi_tool_dispatch_bench: calls={} blocks={} wallMsPerCall={:.6} cpuMsPerCall={}",
        args.calls,
        args.blocks,
        blocks[0].wall_ms_per_call,
        blocks[0]
            .cpu_ms_per_call
            .map_or_else(|| "n/a".to_owned(), |v| format!("{v:.6}"))
    );
    ExitCode::SUCCESS
}
