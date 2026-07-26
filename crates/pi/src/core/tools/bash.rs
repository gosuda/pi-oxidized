//! Bash tool: execute a shell command with streamed, tail-truncated output.
//!
//! Ports `.references/pi/packages/coding-agent/src/core/tools/bash.ts` plus the
//! local process-tree kill path from `utils/shell.ts`. stdout and stderr are
//! merged by arrival order into an [`OutputAccumulator`] with spill prefix
//! `pi-bash`. Partial tool updates are throttled to 100 ms.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use futures::future::BoxFuture;
use pi_agent::{AgentTool, AgentToolResult, ToolError, ToolUpdates};
use pi_ai::ToolResultContent;
use pi_ai::types::TextContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, OutputAccumulator, OutputAccumulatorError,
    OutputAccumulatorOptions, OutputSnapshot, TruncatedBy, TruncationResult, format_size,
};

/// Maximum timeout duration in milliseconds (TypeScript `MAX_TIMEOUT_MS`).
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;

/// Numeric form of the maximum timeout in seconds.
const MAX_TIMEOUT_SECONDS: f64 = 2_147_483.647;

/// Display form of the maximum timeout in seconds (`MAX_TIMEOUT_MS / 1000`).
const MAX_TIMEOUT_SECONDS_DISPLAY: &str = "2147483.647";

/// Throttle window for streaming tool updates.
const BASH_UPDATE_THROTTLE: Duration = Duration::from_millis(100);

/// Temp-file prefix for bash spill paths (`pi-bash-{16hex}.log`).
const BASH_TEMP_FILE_PREFIX: &str = "pi-bash";

/// TypeBox-compatible bash arguments (fixture `bash.json`).
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct BashToolInput {
    /// Bash command to execute.
    #[schemars(description = "Bash command to execute")]
    pub command: String,
    /// Timeout in seconds (optional, no default timeout).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Timeout in seconds (optional, no default timeout)")]
    pub timeout: Option<f64>,
}

/// Structured details returned when bash output is truncated.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BashToolDetails {
    /// Tail truncation metadata when the stream exceeded limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
    /// Absolute path of the full-output spill file, when one was opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
}

/// Pluggable command execution backend (TypeScript `BashOperations`).
pub trait BashOperations: Send + Sync {
    /// Execute `command` in `cwd`, streaming merged output chunks through
    /// `on_data`. Returns the process exit code (`None` when killed without a
    /// status). Failures use the TypeScript internal markers `"aborted"` and
    /// `"timeout:{secs}"` so the tool wrapper can append status text.
    fn exec(
        &self,
        command: String,
        cwd: PathBuf,
        on_data: Box<dyn FnMut(Vec<u8>) + Send>,
        cancel: CancellationToken,
        timeout: Option<f64>,
        env: HashMap<String, String>,
    ) -> BoxFuture<'static, Result<Option<i32>, ToolError>>;
}

/// Spawn rewrite hook context (TypeScript `BashSpawnContext`).
#[derive(Clone, Debug)]
pub struct BashSpawnContext {
    /// Command string to execute.
    pub command: String,
    /// Working directory for the child process.
    pub cwd: PathBuf,
    /// Environment map for the child process.
    pub env: HashMap<String, String>,
}

/// Optional rewrite applied before local spawn (TypeScript `BashSpawnHook`).
pub type BashSpawnHook = Arc<dyn Fn(BashSpawnContext) -> BashSpawnContext + Send + Sync>;

/// Options for [`BashTool`].
#[derive(Clone)]
pub struct BashToolOptions {
    /// Working directory used when no spawn hook rewrites it.
    pub cwd: PathBuf,
    /// Optional absolute shell path (TypeScript `shellPath`).
    pub shell_path: Option<PathBuf>,
    /// Optional command prefix prepended as `{prefix}\n{command}`.
    pub command_prefix: Option<String>,
    /// Optional spawn rewrite hook.
    pub spawn_hook: Option<BashSpawnHook>,
    /// Custom operations; default is local shell execution.
    pub operations: Option<Arc<dyn BashOperations>>,
}

impl std::fmt::Debug for BashToolOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BashToolOptions")
            .field("cwd", &self.cwd)
            .field("shell_path", &self.shell_path)
            .field("command_prefix", &self.command_prefix)
            .field("spawn_hook", &self.spawn_hook.as_ref().map(|_| "Some(..)"))
            .field("operations", &self.operations.as_ref().map(|_| "Some(..)"))
            .finish()
    }
}

impl BashToolOptions {
    /// Builds options for `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            shell_path: None,
            command_prefix: None,
            spawn_hook: None,
            operations: None,
        }
    }
}

/// Local shell backend matching TypeScript `createLocalBashOperations`.
#[derive(Clone, Debug, Default)]
pub struct LocalBashOperations {
    shell_path: Option<PathBuf>,
}

impl LocalBashOperations {
    /// Creates local operations with optional custom shell path.
    #[must_use]
    pub fn new(shell_path: Option<PathBuf>) -> Self {
        Self { shell_path }
    }
}

impl BashOperations for LocalBashOperations {
    fn exec(
        &self,
        command: String,
        cwd: PathBuf,
        mut on_data: Box<dyn FnMut(Vec<u8>) + Send>,
        cancel: CancellationToken,
        timeout: Option<f64>,
        env: HashMap<String, String>,
    ) -> BoxFuture<'static, Result<Option<i32>, ToolError>> {
        let shell_path = self.shell_path.clone();
        async move {
            let timeout_ms = resolve_timeout_ms(timeout)?;
            if cancel.is_cancelled() {
                return Err(ToolError::new("aborted"));
            }
            if !cwd.is_dir() {
                return Err(ToolError::new(format!(
                    "Working directory does not exist: {}\nCannot execute bash commands.",
                    cwd.display()
                )));
            }

            let shell = resolve_shell_config(shell_path.as_deref())?;
            let mut child = spawn_shell_command(&shell, &command, &cwd, &env)?;
            let pid = child.id();
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();

            let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<Vec<u8>>();
            let readers = spawn_stream_readers(stdout, stderr, chunk_tx);

            let outcome = run_child_loop(
                &mut child,
                pid,
                &mut chunk_rx,
                &mut on_data,
                &cancel,
                timeout_ms,
            )
            .await;

            // Always reap and drain; cancellation wins over timeout/exit.
            let exit_code = finalize_child(
                &mut child,
                pid,
                &mut chunk_rx,
                &mut on_data,
                readers,
                outcome,
            )
            .await?;

            if cancel.is_cancelled() {
                return Err(ToolError::new("aborted"));
            }
            if matches!(outcome, ChildLoopOutcome::TimedOut) {
                return Err(ToolError::new(format!(
                    "timeout:{}",
                    timeout_seconds_label(timeout)
                )));
            }
            Ok(exit_code)
        }
        .boxed()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChildLoopOutcome {
    Exited,
    Cancelled,
    TimedOut,
    WaitFailed,
}

async fn run_child_loop(
    child: &mut Child,
    pid: Option<u32>,
    chunk_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    on_data: &mut (dyn FnMut(Vec<u8>) + Send),
    cancel: &CancellationToken,
    timeout_ms: Option<u64>,
) -> ChildLoopOutcome {
    let timeout_at = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
    let mut exited = false;

    loop {
        if cancel.is_cancelled() {
            kill_process(pid);
            return ChildLoopOutcome::Cancelled;
        }
        if timeout_at.is_some_and(|deadline| Instant::now() >= deadline) {
            kill_process(pid);
            return ChildLoopOutcome::TimedOut;
        }

        let wait_timeout = timeout_at.map(|deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1))
        });

        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                kill_process(pid);
                return ChildLoopOutcome::Cancelled;
            }
            () = tokio::time::sleep(wait_timeout.unwrap_or(Duration::from_hours(8760))),
                if wait_timeout.is_some() =>
            {
                kill_process(pid);
                return ChildLoopOutcome::TimedOut;
            }
            chunk = chunk_rx.recv() => {
                if let Some(bytes) = chunk {
                    on_data(bytes);
                } else {
                    // Both stream readers finished. Wait for process if needed.
                    if !exited {
                        return match child.wait().await {
                            Ok(_) => ChildLoopOutcome::Exited,
                            Err(_) => ChildLoopOutcome::WaitFailed,
                        };
                    }
                    return ChildLoopOutcome::Exited;
                }
            }
            status = child.wait(), if !exited => {
                match status {
                    Ok(_) => {
                        exited = true;
                        // Drain remaining pipe data until readers disconnect.
                    }
                    Err(_) => return ChildLoopOutcome::WaitFailed,
                }
            }
        }

        if exited {
            return drain_exited_output(pid, chunk_rx, on_data, cancel).await;
        }
    }
}

async fn drain_exited_output(
    pid: Option<u32>,
    chunk_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    on_data: &mut (dyn FnMut(Vec<u8>) + Send),
    cancel: &CancellationToken,
) -> ChildLoopOutcome {
    let idle_deadline = Instant::now() + Duration::from_millis(100);
    loop {
        if cancel.is_cancelled() {
            kill_process(pid);
            return ChildLoopOutcome::Cancelled;
        }
        match chunk_rx.try_recv() {
            Ok(bytes) => on_data(bytes),
            Err(mpsc::error::TryRecvError::Empty) => {
                if Instant::now() >= idle_deadline {
                    return ChildLoopOutcome::Exited;
                }
                tokio::select! {
                    () = cancel.cancelled() => {
                        kill_process(pid);
                        return ChildLoopOutcome::Cancelled;
                    }
                    chunk = chunk_rx.recv() => {
                        match chunk {
                            Some(bytes) => on_data(bytes),
                            None => return ChildLoopOutcome::Exited,
                        }
                    }
                    () = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
            }
            Err(mpsc::error::TryRecvError::Disconnected) => {
                return ChildLoopOutcome::Exited;
            }
        }
    }
}

fn kill_process(pid: Option<u32>) {
    if let Some(pid) = pid {
        kill_process_tree(pid);
    }
}

async fn finalize_child(
    child: &mut Child,
    pid: Option<u32>,
    chunk_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    on_data: &mut (dyn FnMut(Vec<u8>) + Send),
    readers: Vec<tokio::task::JoinHandle<()>>,
    outcome: ChildLoopOutcome,
) -> Result<Option<i32>, ToolError> {
    if matches!(
        outcome,
        ChildLoopOutcome::Cancelled | ChildLoopOutcome::TimedOut
    ) {
        kill_process(pid);
    }

    // Drain any remaining output.
    let drain_deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < drain_deadline {
        match chunk_rx.try_recv() {
            Ok(bytes) => on_data(bytes),
            Err(mpsc::error::TryRecvError::Empty) => {
                tokio::task::yield_now().await;
            }
            Err(mpsc::error::TryRecvError::Disconnected) => break,
        }
    }
    while let Ok(bytes) = chunk_rx.try_recv() {
        on_data(bytes);
    }

    for handle in readers {
        let _ = handle.await;
    }

    match child.try_wait() {
        Ok(Some(status)) => Ok(status.code()),
        Ok(None) => {
            if let Some(pid) = pid {
                kill_process_tree(pid);
            }
            match child.wait().await {
                Ok(status) => Ok(status.code()),
                Err(error) => Err(ToolError::new(error.to_string())),
            }
        }
        Err(error) => Err(ToolError::new(error.to_string())),
    }
}

/// Agent tool that runs bash commands in a working directory.
#[derive(Clone)]
pub struct BashTool {
    cwd: PathBuf,
    command_prefix: Option<String>,
    spawn_hook: Option<BashSpawnHook>,
    operations: Arc<dyn BashOperations>,
    parameters: Value,
    description: String,
}

impl std::fmt::Debug for BashTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BashTool")
            .field("cwd", &self.cwd)
            .field("command_prefix", &self.command_prefix)
            .field("spawn_hook", &self.spawn_hook.as_ref().map(|_| "Some(..)"))
            .field("operations", &"<dyn BashOperations>")
            .field("parameters", &self.parameters)
            .field("description", &self.description)
            .finish()
    }
}

impl BashTool {
    /// Creates a bash tool rooted at `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_options(BashToolOptions::new(cwd))
    }

    /// Creates a bash tool from explicit options.
    #[must_use]
    pub fn with_options(options: BashToolOptions) -> Self {
        let operations = options.operations.unwrap_or_else(|| {
            Arc::new(LocalBashOperations::new(options.shell_path.clone()))
                as Arc<dyn BashOperations>
        });
        Self {
            cwd: options.cwd,
            command_prefix: options.command_prefix,
            spawn_hook: options.spawn_hook,
            operations,
            parameters: bash_parameters_schema(),
            description: bash_description(),
        }
    }

    /// Returns the JSON Schema for bash arguments (normalized `TypeBox` shape).
    #[must_use]
    pub fn parameters_schema() -> Value {
        bash_parameters_schema()
    }

    /// Validates raw tool arguments into [`BashToolInput`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when required fields are missing or mistyped, or
    /// when `timeout` fails TypeScript `resolveTimeoutMs` checks.
    pub fn parse_input(args: &Map<String, Value>) -> Result<BashToolInput, ToolError> {
        let input: BashToolInput = serde_json::from_value(Value::Object(args.clone()))
            .map_err(|error| ToolError::new(format!("Bash tool input is invalid. {error}")))?;
        let _ = resolve_timeout_ms(input.timeout)?;
        Ok(input)
    }
}

impl AgentTool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn label(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn validate_arguments(
        &self,
        args: &Map<String, Value>,
    ) -> Result<Map<String, Value>, ToolError> {
        let _ = Self::parse_input(args)?;
        Ok(args.clone())
    }

    fn execute(
        &self,
        _tool_call_id: &str,
        args: Map<String, Value>,
        cancel: CancellationToken,
        updates: ToolUpdates,
    ) -> BoxFuture<'static, Result<AgentToolResult, ToolError>> {
        let cwd = self.cwd.clone();
        let command_prefix = self.command_prefix.clone();
        let spawn_hook = self.spawn_hook.clone();
        let operations = Arc::clone(&self.operations);

        async move {
            let input = BashTool::parse_input(&args)?;
            let resolved_command = match command_prefix {
                Some(prefix) => format!("{prefix}\n{}", input.command),
                None => input.command,
            };
            let spawn_context = resolve_spawn_context(resolved_command, cwd, spawn_hook.as_ref());
            let collected =
                collect_command_output(operations, spawn_context, input.timeout, cancel, updates)
                    .await?;
            command_result(collected)
        }
        .boxed()
    }
}

struct CollectedCommandOutput {
    exec_result: Result<Option<i32>, ToolError>,
    snapshot: OutputSnapshot,
    last_line_bytes: usize,
}

async fn collect_command_output(
    operations: Arc<dyn BashOperations>,
    spawn_context: BashSpawnContext,
    timeout: Option<f64>,
    cancel: CancellationToken,
    updates: ToolUpdates,
) -> Result<CollectedCommandOutput, ToolError> {
    updates.send(AgentToolResult {
        content: Vec::new(),
        details: Value::Null,
        added_tool_names: None,
        terminate: None,
    });
    let output = Arc::new(Mutex::new(OutputAccumulator::new(
        OutputAccumulatorOptions {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
            temp_file_prefix: BASH_TEMP_FILE_PREFIX.to_owned(),
        },
    )));
    let throttle = Arc::new(Mutex::new(UpdateThrottle::new()));
    let (data_tx, mut data_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let pump_output = Arc::clone(&output);
    let pump_updates = updates.clone();
    let pump_throttle = Arc::clone(&throttle);
    let pump = tokio::spawn(async move {
        while let Some(chunk) = data_rx.recv().await {
            pump_output.lock().await.append(&chunk)?;
            schedule_throttled_update(
                pump_updates.clone(),
                Arc::clone(&pump_output),
                Arc::clone(&pump_throttle),
            );
        }
        Ok::<(), OutputAccumulatorError>(())
    });
    let on_data = Box::new(move |bytes: Vec<u8>| {
        let _ = data_tx.send(bytes);
    });
    let exec_result = operations
        .exec(
            spawn_context.command,
            spawn_context.cwd,
            on_data,
            cancel,
            timeout,
            spawn_context.env,
        )
        .await;
    finish_collected_output(exec_result, pump, output, throttle, updates).await
}

async fn finish_collected_output(
    exec_result: Result<Option<i32>, ToolError>,
    pump: tokio::task::JoinHandle<Result<(), OutputAccumulatorError>>,
    output: Arc<Mutex<OutputAccumulator>>,
    throttle: Arc<Mutex<UpdateThrottle>>,
    updates: ToolUpdates,
) -> Result<CollectedCommandOutput, ToolError> {
    match pump.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(accumulator_error(&error)),
        Err(error) => return Err(ToolError::new(format!("bash output pump failed: {error}"))),
    }
    for _ in 0..20 {
        if !throttle.lock().await.timer_armed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut output_guard = output.lock().await;
    output_guard
        .finish()
        .map_err(|error| accumulator_error(&error))?;
    let snapshot = output_guard
        .snapshot(true)
        .map_err(|error| accumulator_error(&error))?;
    let last_line_bytes = output_guard.last_line_bytes();
    let emitted = EmittedSnapshot::from(&snapshot);
    let should_emit = {
        let mut throttle_guard = throttle.lock().await;
        let ordinary_empty_output = throttle_guard.last_emitted.is_none()
            && emitted.content.is_empty()
            && !emitted.truncation.truncated;
        if ordinary_empty_output || throttle_guard.last_emitted.as_ref() == Some(&emitted) {
            false
        } else {
            throttle_guard.last_emitted = Some(emitted);
            true
        }
    };
    output_guard.close_temp_file();
    drop(output_guard);
    if should_emit {
        updates.send(snapshot_to_partial(&snapshot));
    }
    Ok(CollectedCommandOutput {
        exec_result,
        snapshot,
        last_line_bytes,
    })
}

fn command_result(collected: CollectedCommandOutput) -> Result<AgentToolResult, ToolError> {
    let CollectedCommandOutput {
        exec_result,
        snapshot,
        last_line_bytes,
    } = collected;
    match exec_result {
        Ok(exit_code) => {
            let (text, details) = format_output(&snapshot, last_line_bytes, "(no output)");
            if let Some(code) = exit_code
                && code != 0
            {
                return Err(ToolError::new(append_status(
                    &text,
                    &format!("Command exited with code {code}"),
                )));
            }
            Ok(AgentToolResult {
                content: vec![ToolResultContent::Text(TextContent::new(text))],
                details: details_to_value(details),
                added_tool_names: None,
                terminate: None,
            })
        }
        Err(error) => command_error_result(&error, &snapshot, last_line_bytes),
    }
}

fn command_error_result(
    error: &ToolError,
    snapshot: &OutputSnapshot,
    last_line_bytes: usize,
) -> Result<AgentToolResult, ToolError> {
    let message = error.message().to_owned();
    let (text, _) = format_output(snapshot, last_line_bytes, "");
    if message == "aborted" {
        return Err(ToolError::new(append_status(&text, "Command aborted")));
    }
    if let Some(secs) = message.strip_prefix("timeout:") {
        return Err(ToolError::new(append_status(
            &text,
            &format!("Command timed out after {secs} seconds"),
        )));
    }
    Err(ToolError::new(message))
}

/// Builds an [`Arc<dyn AgentTool>`] bash tool for `cwd`.
#[must_use]
pub fn create_bash_tool(cwd: impl Into<PathBuf>) -> Arc<dyn AgentTool> {
    Arc::new(BashTool::new(cwd))
}

fn bash_description() -> String {
    format!(
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
        DEFAULT_MAX_BYTES / 1024
    )
}

fn bash_parameters_schema() -> Value {
    normalize_tool_schema(schemars::schema_for!(BashToolInput))
}

fn normalize_tool_schema(schema: schemars::Schema) -> Value {
    let mut value = serde_json::to_value(schema).unwrap_or_else(|_| Value::Object(Map::new()));
    if let Value::Object(map) = &mut value {
        map.remove("$schema");
        map.remove("title");
        // TypeBox fixtures omit the root struct doc-comment description.
        map.remove("description");
        normalize_schema_node(map);
    }
    value
}

fn normalize_schema_node(map: &mut Map<String, Value>) {
    map.remove("format");
    // schemars represents `Option<number>` as `["number","null"]`; TypeBox uses
    // a plain `"number"` with the key absent from `required`.
    if let Some(Value::Array(types)) = map.get("type").cloned() {
        let non_null: Vec<Value> = types
            .into_iter()
            .filter(|item| item.as_str() != Some("null"))
            .collect();
        if non_null.len() == 1 {
            map.insert("type".to_owned(), non_null[0].clone());
        }
    }
    let keys: Vec<String> = map.keys().cloned().collect();
    for key in keys {
        match map.get_mut(&key) {
            Some(Value::Object(child)) => normalize_schema_node(child),
            Some(Value::Array(items)) => {
                for item in items {
                    if let Value::Object(child) = item {
                        normalize_schema_node(child);
                    }
                }
            }
            _ => {}
        }
    }
}

/// TypeScript `resolveTimeoutMs`.
fn resolve_timeout_ms(timeout: Option<f64>) -> Result<Option<u64>, ToolError> {
    let Some(timeout) = timeout else {
        return Ok(None);
    };
    if !timeout.is_finite() || timeout <= 0.0 {
        return Err(ToolError::new(
            "Invalid timeout: must be a finite number of seconds",
        ));
    }
    if timeout > MAX_TIMEOUT_SECONDS {
        return Err(ToolError::new(format!(
            "Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS_DISPLAY} seconds"
        )));
    }
    let milliseconds = (timeout * 1000.0).floor();
    Ok(Some(
        bounded_integer_f64_to_u64(milliseconds).min(MAX_TIMEOUT_MS),
    ))
}

fn bounded_integer_f64_to_u64(value: f64) -> u64 {
    const FRACTION_BITS: u32 = 52;
    const FRACTION_BITS_I32: i32 = 52;
    const EXPONENT_BIAS: i32 = 1023;

    if value == 0.0 {
        return 0;
    }
    let bits = value.to_bits();
    let exponent_bits = (bits >> FRACTION_BITS) & 0x7ff;
    let fraction = bits & ((1_u64 << FRACTION_BITS) - 1);
    let significand = fraction | (1_u64 << FRACTION_BITS);
    let exponent = i32::try_from(exponent_bits).unwrap_or(0) - EXPONENT_BIAS;
    let denominator_shift = u32::try_from(FRACTION_BITS_I32 - exponent).unwrap_or(FRACTION_BITS);
    significand >> denominator_shift
}

fn timeout_seconds_label(timeout: Option<f64>) -> String {
    timeout.map_or_else(|| "0".to_owned(), |value| value.to_string())
}

struct ShellConfig {
    shell: PathBuf,
    args: Vec<String>,
    command_from_stdin: bool,
}

fn resolve_shell_config(custom: Option<&Path>) -> Result<ShellConfig, ToolError> {
    if let Some(path) = custom {
        if path.exists() {
            return Ok(bash_shell_config(path.to_path_buf()));
        }
        return Err(ToolError::new(format!(
            "Custom shell path not found: {}",
            path.display()
        )));
    }

    #[cfg(windows)]
    {
        let mut candidates = Vec::new();
        if let Ok(program_files) = std::env::var("ProgramFiles") {
            candidates.push(PathBuf::from(program_files).join(r"Git\bin\bash.exe"));
        }
        if let Ok(program_files_x86) = std::env::var("ProgramFiles(x86)") {
            candidates.push(PathBuf::from(program_files_x86).join(r"Git\bin\bash.exe"));
        }
        for path in &candidates {
            if path.exists() {
                return Ok(bash_shell_config(path.clone()));
            }
        }
        if let Some(path) = find_on_path("bash.exe") {
            return Ok(bash_shell_config(path));
        }
        return Err(ToolError::new(
            "No bash shell found. Options:\n  1. Install Git for Windows: https://git-scm.com/download/win\n  2. Add your bash to PATH (Cygwin, MSYS2, etc.)\n  3. Set shellPath in settings.json".to_owned(),
        ));
    }

    #[cfg(not(windows))]
    {
        if Path::new("/bin/bash").exists() {
            return Ok(bash_shell_config(PathBuf::from("/bin/bash")));
        }
        if let Some(path) = find_on_path("bash") {
            return Ok(bash_shell_config(path));
        }
        Ok(ShellConfig {
            shell: PathBuf::from("sh"),
            args: vec!["-c".to_owned()],
            command_from_stdin: false,
        })
    }
}

fn bash_shell_config(shell: PathBuf) -> ShellConfig {
    let legacy_wsl = shell
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
        .contains("/windows/system32/bash.exe");
    if legacy_wsl {
        ShellConfig {
            shell,
            args: vec!["-s".to_owned()],
            command_from_stdin: true,
        }
    } else {
        ShellConfig {
            shell,
            args: vec!["-c".to_owned()],
            command_from_stdin: false,
        }
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn current_env_map() -> HashMap<String, String> {
    std::env::vars().collect()
}

fn resolve_spawn_context(
    command: String,
    cwd: PathBuf,
    spawn_hook: Option<&BashSpawnHook>,
) -> BashSpawnContext {
    let base = BashSpawnContext {
        command,
        cwd,
        env: current_env_map(),
    };
    match spawn_hook {
        Some(hook) => hook(base),
        None => base,
    }
}

fn spawn_shell_command(
    shell: &ShellConfig,
    command: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Result<Child, ToolError> {
    let mut cmd = Command::new(&shell.shell);
    cmd.args(&shell.args);
    if !shell.command_from_stdin {
        cmd.arg(command);
    }
    cmd.current_dir(cwd)
        .stdin(if shell.command_from_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));

    #[cfg(unix)]
    {
        // Detached process group so killpg reaps descendants (Node `detached`).
        cmd.process_group(0);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|error| ToolError::new(format!("Failed to spawn shell: {error}")))?;

    if shell.command_from_stdin
        && let Some(mut stdin) = child.stdin.take()
    {
        let payload = command.to_owned();
        tokio::spawn(async move {
            let _ = stdin.write_all(payload.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }

    Ok(child)
}

fn spawn_stream_readers(
    stdout: Option<impl AsyncRead + Unpin + Send + 'static>,
    stderr: Option<impl AsyncRead + Unpin + Send + 'static>,
    tx: mpsc::UnboundedSender<Vec<u8>>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();
    if let Some(stdout) = stdout {
        let tx = tx.clone();
        handles.push(tokio::spawn(async move {
            pump_reader(stdout, tx).await;
        }));
    }
    if let Some(stderr) = stderr {
        handles.push(tokio::spawn(async move {
            pump_reader(stderr, tx).await;
        }));
    }
    handles
}

async fn pump_reader<R>(mut reader: R, tx: mpsc::UnboundedSender<Vec<u8>>)
where
    R: AsyncRead + Unpin,
{
    let mut buf = vec![0_u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill, killpg};
        use nix::unistd::Pid;

        let Ok(raw) = i32::try_from(pid) else {
            return;
        };
        let group = Pid::from_raw(raw);
        // Node: process.kill(-pid, SIGKILL) then fallback process.kill(pid).
        if killpg(group, Signal::SIGKILL).is_err() {
            let _ = kill(group, Signal::SIGKILL);
        }
    }

    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

#[derive(Clone, PartialEq)]
struct EmittedSnapshot {
    content: String,
    truncation: TruncationResult,
    full_output_path: Option<PathBuf>,
}

impl From<&OutputSnapshot> for EmittedSnapshot {
    fn from(snapshot: &OutputSnapshot) -> Self {
        Self {
            content: snapshot.content.clone(),
            truncation: snapshot.truncation.clone(),
            full_output_path: snapshot.full_output_path.clone(),
        }
    }
}

struct UpdateThrottle {
    last_update_at: Option<Instant>,
    last_emitted: Option<EmittedSnapshot>,
    dirty: bool,
    timer_armed: bool,
}

impl UpdateThrottle {
    fn new() -> Self {
        Self {
            last_update_at: None,
            last_emitted: None,
            dirty: false,
            timer_armed: false,
        }
    }
}

fn schedule_throttled_update(
    updates: ToolUpdates,
    output: Arc<Mutex<OutputAccumulator>>,
    state: Arc<Mutex<UpdateThrottle>>,
) {
    tokio::spawn(async move {
        {
            let mut guard = state.lock().await;
            guard.dirty = true;
            if guard.timer_armed {
                return;
            }
            guard.timer_armed = true;
        }

        loop {
            let delay = {
                let guard = state.lock().await;
                match guard.last_update_at {
                    Some(last) => BASH_UPDATE_THROTTLE.saturating_sub(last.elapsed()),
                    None => Duration::ZERO,
                }
            };
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }

            let should_emit = {
                let mut guard = state.lock().await;
                if guard.dirty {
                    guard.dirty = false;
                    guard.last_update_at = Some(Instant::now());
                    true
                } else {
                    false
                }
            };
            if should_emit {
                let mut output_guard = output.lock().await;
                if let Ok(snapshot) = output_guard.snapshot(true) {
                    let emitted = EmittedSnapshot::from(&snapshot);
                    let should_send = {
                        let mut guard = state.lock().await;
                        if guard.last_emitted.as_ref() == Some(&emitted) {
                            false
                        } else {
                            guard.last_emitted = Some(emitted);
                            true
                        }
                    };
                    if should_send {
                        updates.send(snapshot_to_partial(&snapshot));
                    }
                }
            }

            let mut guard = state.lock().await;
            if guard.dirty {
                continue;
            }
            guard.timer_armed = false;
            break;
        }
    });
}

fn snapshot_to_partial(snapshot: &OutputSnapshot) -> AgentToolResult {
    let mut details = Map::new();
    if snapshot.truncation.truncated
        && let Ok(value) = serde_json::to_value(&snapshot.truncation)
    {
        details.insert("truncation".to_owned(), value);
    }
    if let Some(path) = &snapshot.full_output_path {
        details.insert(
            "fullOutputPath".to_owned(),
            Value::String(path.to_string_lossy().into_owned()),
        );
    }
    AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent::new(
            snapshot.content.clone(),
        ))],
        details: Value::Object(details),
        added_tool_names: None,
        terminate: None,
    }
}

fn format_output(
    snapshot: &OutputSnapshot,
    last_line_bytes: usize,
    empty_text: &str,
) -> (String, Option<BashToolDetails>) {
    let truncation = &snapshot.truncation;
    let mut text = if snapshot.content.is_empty() {
        empty_text.to_owned()
    } else {
        snapshot.content.clone()
    };
    let mut details = None;
    if truncation.truncated {
        let full_output_path = snapshot
            .full_output_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        details = Some(BashToolDetails {
            truncation: Some(truncation.clone()),
            full_output_path: full_output_path.clone(),
        });
        let start_line = truncation
            .total_lines
            .saturating_sub(truncation.output_lines)
            + 1;
        let end_line = truncation.total_lines;
        let path_display = full_output_path.unwrap_or_default();
        if truncation.last_line_partial {
            let last_line_size = format_size(last_line_bytes as u64);
            let _ = write!(
                text,
                "\n\n[Showing last {} of line {end_line} (line is {last_line_size}). Full output: {path_display}]",
                format_size(truncation.output_bytes as u64)
            );
        } else if truncation.truncated_by == Some(TruncatedBy::Lines) {
            let _ = write!(
                text,
                "\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {path_display}]",
                truncation.total_lines
            );
        } else {
            let _ = write!(
                text,
                "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {path_display}]",
                truncation.total_lines,
                format_size(DEFAULT_MAX_BYTES as u64)
            );
        }
    }
    (text, details)
}

fn append_status(text: &str, status: &str) -> String {
    if text.is_empty() {
        status.to_owned()
    } else {
        format!("{text}\n\n{status}")
    }
}

fn details_to_value(details: Option<BashToolDetails>) -> Value {
    match details {
        Some(details) => serde_json::to_value(details).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

fn accumulator_error(error: &OutputAccumulatorError) -> ToolError {
    ToolError::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use proptest::prelude::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn fixture_schema() -> Result<Value, serde_json::Error> {
        let text = include_str!(
            "../../../../../.agent-tasks/pi-rust-rewrite/fixtures/tool-schemas/bash.json"
        );
        serde_json::from_str(text)
    }

    fn json_map(value: Value) -> Result<Map<String, Value>, Box<dyn std::error::Error>> {
        match value {
            Value::Object(map) => Ok(map),
            _ => Err(io::Error::other("test arguments must be a JSON object").into()),
        }
    }

    fn required<T>(
        value: Option<T>,
        message: &'static str,
    ) -> Result<T, Box<dyn std::error::Error>> {
        value.ok_or_else(|| io::Error::other(message).into())
    }

    fn expected_error<T>(
        result: Result<T, ToolError>,
        message: &'static str,
    ) -> Result<ToolError, Box<dyn std::error::Error>> {
        match result {
            Err(error) => Ok(error),
            Ok(_) => Err(io::Error::other(message).into()),
        }
    }

    fn text_of(result: &AgentToolResult) -> String {
        match result.content.first() {
            Some(ToolResultContent::Text(text)) => text.text.clone(),
            _ => String::new(),
        }
    }

    fn text_of_err(error: &ToolError) -> String {
        error.message().to_owned()
    }

    #[derive(Clone)]
    struct ScriptedBashOperations {
        chunks: Vec<Vec<u8>>,
        pause_between_chunks: Option<Duration>,
    }

    impl BashOperations for ScriptedBashOperations {
        fn exec(
            &self,
            _command: String,
            _cwd: PathBuf,
            mut on_data: Box<dyn FnMut(Vec<u8>) + Send>,
            _cancel: CancellationToken,
            _timeout: Option<f64>,
            _env: HashMap<String, String>,
        ) -> BoxFuture<'static, Result<Option<i32>, ToolError>> {
            let chunks = self.chunks.clone();
            let pause_between_chunks = self.pause_between_chunks;
            async move {
                let chunk_count = chunks.len();
                for (index, chunk) in chunks.into_iter().enumerate() {
                    on_data(chunk);
                    if index + 1 < chunk_count
                        && let Some(pause) = pause_between_chunks
                    {
                        tokio::time::sleep(pause).await;
                    }
                }
                Ok(Some(0))
            }
            .boxed()
        }
    }

    async fn execute_scripted(
        chunks: Vec<Vec<u8>>,
        pause_between_chunks: Option<Duration>,
    ) -> Result<(AgentToolResult, Vec<AgentToolResult>), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let mut options = BashToolOptions::new(dir.path());
        options.operations = Some(Arc::new(ScriptedBashOperations {
            chunks,
            pause_between_chunks,
        }));
        let tool = BashTool::with_options(options);
        let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&updates);
        let stream = ToolUpdates::new(move |partial| {
            captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(partial);
        });
        let result = tool
            .execute(
                "1",
                json_map(json!({ "command": "scripted" }))?,
                CancellationToken::new(),
                stream,
            )
            .await?;
        let updates = updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok((result, updates))
    }

    async fn wait_for_marked_pids(marker: &Path) -> Result<Vec<u32>, Box<dyn std::error::Error>> {
        for _ in 0..50 {
            if let Ok(raw) = tokio::fs::read_to_string(marker).await {
                let pids = raw
                    .lines()
                    .filter_map(|line| line.trim().parse().ok())
                    .collect::<Vec<u32>>();
                if !pids.is_empty() {
                    return Ok(pids);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Err(io::Error::other("command did not record a descendant pid").into())
    }

    async fn assert_processes_exit(pids: &[u32]) -> Result<(), Box<dyn std::error::Error>> {
        for &pid in pids {
            for _ in 0..50 {
                if !process_alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            if process_alive(pid) {
                return Err(io::Error::other(format!("descendant {pid} still alive")).into());
            }
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 4, .. ProptestConfig::default() })]
        #[test]
        fn timeout_reaps_generated_process_tree(depth in 1_usize..4, timeout_tenths in 3_u8..8) {
            let result: Result<(), String> = (|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                runtime.block_on(async {
                    let dir = tempdir().map_err(|error| error.to_string())?;
                    let marker = dir.path().join("tree.pid");
                    let marker_str = marker.display();
                    let mut command = format!("sleep 60 & echo $! >> '{marker_str}'; wait");
                    for _ in 1..depth {
                        command = format!("({command}) & echo $! >> '{marker_str}'; wait");
                    }
                    let tool = BashTool::new(dir.path());
                    let timeout = f64::from(timeout_tenths) / 10.0;
                    let err = expected_error(
                        tool.execute(
                            "1",
                            json_map(json!({"command": command, "timeout": timeout})).map_err(|error| error.to_string())?,
                            CancellationToken::new(),
                            ToolUpdates::noop(),
                        ).await,
                        "timed command succeeded",
                    ).map_err(|error| error.to_string())?;
                    if !text_of_err(&err).contains("Command timed out") {
                        return Err(text_of_err(&err));
                    }
                    let pids = wait_for_marked_pids(&marker).await.map_err(|error| error.to_string())?;
                    assert_processes_exit(&pids).await.map_err(|error| error.to_string())
                })
            })();
            let error = result.as_ref().err().map_or("", String::as_str);
            prop_assert!(result.is_ok(), "{error}");
        }

        #[test]
        fn cancellation_aborts_generated_command(cancel_delay_ms in 0_u8..25) {
            let result: Result<(), String> = (|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                runtime.block_on(async {
                    let dir = tempdir().map_err(|error| error.to_string())?;
                    let marker = dir.path().join("cancel.pid");
                    let marker_str = marker.display();
                    let tool = BashTool::new(dir.path());
                    let cancel = CancellationToken::new();
                    let command = format!("sleep 60 & echo $! > '{marker_str}'; wait");
                    let args = json_map(json!({"command": command, "timeout": 30.0})).map_err(|error| error.to_string())?;
                    let task_cancel = cancel.clone();
                    let join = tokio::spawn(async move { tool.execute("1", args, task_cancel, ToolUpdates::noop()).await });
                    let pids = wait_for_marked_pids(&marker).await.map_err(|error| error.to_string())?;
                    tokio::time::sleep(Duration::from_millis(u64::from(cancel_delay_ms))).await;
                    cancel.cancel();
                    let err = expected_error(join.await.map_err(|error| error.to_string())?, "cancelled command succeeded").map_err(|error| error.to_string())?;
                    if !text_of_err(&err).contains("Command aborted") {
                        return Err(text_of_err(&err));
                    }
                    assert_processes_exit(&pids).await.map_err(|error| error.to_string())
                })
            })();
            let error = result.as_ref().err().map_or("", String::as_str);
            prop_assert!(result.is_ok(), "{error}");
        }
    }

    #[test]
    fn schema_matches_typebox_fixture() -> Result<(), Box<dyn std::error::Error>> {
        let schema = BashTool::parameters_schema();
        assert_eq!(schema, fixture_schema()?);
        Ok(())
    }

    #[test]
    fn timeout_validation_rejects_non_positive_and_too_large()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            expected_error(resolve_timeout_ms(Some(0.0)), "zero accepted")?.message(),
            "Invalid timeout: must be a finite number of seconds"
        );
        assert_eq!(
            expected_error(resolve_timeout_ms(Some(-1.0)), "negative accepted")?.message(),
            "Invalid timeout: must be a finite number of seconds"
        );
        assert_eq!(
            expected_error(resolve_timeout_ms(Some(f64::NAN)), "NaN accepted")?.message(),
            "Invalid timeout: must be a finite number of seconds"
        );
        assert_eq!(
            expected_error(resolve_timeout_ms(Some(f64::INFINITY)), "infinity accepted",)?
                .message(),
            "Invalid timeout: must be a finite number of seconds"
        );
        let too_large = MAX_TIMEOUT_SECONDS + 0.001;
        assert_eq!(
            expected_error(resolve_timeout_ms(Some(too_large)), "large accepted")?.message(),
            format!("Invalid timeout: maximum is {MAX_TIMEOUT_SECONDS_DISPLAY} seconds")
        );
        assert_eq!(resolve_timeout_ms(Some(1.0))?, Some(1000));
        assert_eq!(resolve_timeout_ms(Some(0.000_999))?, Some(0));
        assert_eq!(resolve_timeout_ms(Some(0.001))?, Some(1));
        assert_eq!(
            resolve_timeout_ms(Some(MAX_TIMEOUT_SECONDS))?,
            Some(MAX_TIMEOUT_MS)
        );
        assert!(resolve_timeout_ms(None)?.is_none());
        Ok(())
    }

    #[test]
    fn validate_arguments_rejects_bad_timeout() -> Result<(), Box<dyn std::error::Error>> {
        let tool = BashTool::new("/tmp");
        let err = expected_error(
            tool.validate_arguments(&json_map(json!({"command": "true", "timeout": 0}))?),
            "bad timeout accepted",
        )?;
        assert_eq!(
            err.message(),
            "Invalid timeout: must be a finite number of seconds"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_success_returns_no_output() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = BashTool::new(dir.path());
        let result = tool
            .execute(
                "1",
                json_map(json!({"command": "true"}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await?;
        assert_eq!(text_of(&result), "(no output)");
        assert!(result.details.is_null());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nonempty_success_returns_stdout() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = BashTool::new(dir.path());
        let result = tool
            .execute(
                "1",
                json_map(json!({"command": "printf 'hello\n'"}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await?;
        assert_eq!(text_of(&result), "hello\n");
        assert!(result.details.is_null());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nonzero_exit_appends_status() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = BashTool::new(dir.path());
        let err = expected_error(
            tool.execute(
                "1",
                json_map(json!({"command": "printf 'fail\n'; exit 7"}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await,
            "nonzero exit succeeded",
        )?;
        assert_eq!(text_of_err(&err), "fail\n\n\nCommand exited with code 7");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interleaves_stdout_and_stderr_by_arrival() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = BashTool::new(dir.path());
        let result = tool
            .execute(
                "1",
                json_map(json!({
                    "command": "printf 'out1\n'; sleep 0.05; printf 'err1\n' 1>&2; sleep 0.05; printf 'out2\n'"
                }))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await?;
        let text = text_of(&result);
        assert!(text.contains("out1"));
        assert!(text.contains("err1"));
        assert!(text.contains("out2"));
        let out1 = required(text.find("out1"), "out1 missing")?;
        let err1 = required(text.find("err1"), "err1 missing")?;
        let out2 = required(text.find("out2"), "out2 missing")?;
        assert!(out1 < err1 && err1 < out2, "text={text:?}");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_kills_descendants() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let marker = dir.path().join("child.pid");
        let marker_str = marker.display().to_string();
        let tool = BashTool::new(dir.path());
        let command = format!("sleep 60 & echo $! > '{marker_str}'; wait");
        let err = expected_error(
            tool.execute(
                "1",
                json_map(json!({"command": command, "timeout": 0.3}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await,
            "timed command succeeded",
        )?;
        assert!(
            text_of_err(&err).contains("Command timed out after 0.3 seconds"),
            "{}",
            text_of_err(&err)
        );

        let pids = wait_for_marked_pids(&marker).await?;
        assert_processes_exit(&pids).await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_kills_descendants_and_wins_race() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let marker = dir.path().join("cancel.pid");
        let marker_str = marker.display().to_string();
        let tool = BashTool::new(dir.path());
        let cancel = CancellationToken::new();
        let cancel_task = cancel.clone();
        let command = format!("sleep 60 & echo $! > '{marker_str}'; wait");

        let cancel_args = json_map(json!({"command": command, "timeout": 30.0}))?;
        let join = tokio::spawn(async move {
            tool.execute("1", cancel_args, cancel_task, ToolUpdates::noop())
                .await
        });

        let pids = wait_for_marked_pids(&marker).await?;
        cancel.cancel();
        let err = expected_error(join.await?, "cancelled command succeeded")?;
        assert!(
            text_of_err(&err).contains("Command aborted"),
            "{}",
            text_of_err(&err)
        );
        assert_processes_exit(&pids).await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_updates_are_throttled() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = BashTool::new(dir.path());
        let seen = Arc::new(AtomicUsize::new(0));
        let seen_c = Arc::clone(&seen);
        let updates = ToolUpdates::new(move |_partial| {
            seen_c.fetch_add(1, Ordering::SeqCst);
        });
        let _ = tool
            .execute(
                "1",
                json_map(json!({
                    "command": "python3 - <<'PY'\nimport sys,time\nfor i in range(20):\n    sys.stdout.write(f'line-{i}\\n')\n    sys.stdout.flush()\n    time.sleep(0.02)\nPY"
                }))?,
                CancellationToken::new(),
                updates,
            )
            .await?;
        let count = seen.load(Ordering::SeqCst);
        assert!(count >= 2, "expected streaming updates, got {count}");
        assert!(count < 20, "updates were not throttled: {count}");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_stream_updates_match_source_event_shape() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempdir()?;
        let tool = BashTool::new(dir.path());
        let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&updates);
        let stream = ToolUpdates::new(move |partial| {
            captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(partial);
        });

        tool.execute(
            "1",
            json_map(json!({ "command": "printf 'bash-stream\\n'" }))?,
            CancellationToken::new(),
            stream,
        )
        .await?;

        let updates = updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(updates.len(), 2);
        let initial = serde_json::to_value(&updates[0])?;
        let streamed = serde_json::to_value(&updates[1])?;
        assert!(initial.get("details").is_none());
        assert_eq!(streamed.get("details"), Some(&json!({})));
        assert_eq!(text_of(&updates[1]), "bash-stream\n");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn line_truncation_notice_and_spill() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = BashTool::new(dir.path());
        let result = tool
            .execute(
                "1",
                json_map(json!({
                    "command": "python3 - <<'PY'\nfor i in range(2100):\n    print(f'L{i}')\nPY"
                }))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await?;
        let text = text_of(&result);
        assert!(
            text.contains("Showing lines ") && text.contains("Full output: "),
            "{text}"
        );
        let details = required(result.details.as_object(), "details object missing")?;
        let path = required(
            details.get("fullOutputPath").and_then(Value::as_str),
            "spill path missing",
        )?;
        assert!(
            Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("pi-bash-")
                        && Path::new(name)
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("log"))
                }),
            "path={path}"
        );
        assert!(Path::new(path).is_file(), "spill missing: {path}");
        let full = tokio::fs::read_to_string(path).await?;
        assert!(full.lines().count() >= 2100);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_throttle_still_delivers_final_partial()
    -> Result<(), Box<dyn std::error::Error>> {
        let (result, updates) = execute_scripted(
            vec![b"first\n".to_vec(), b"second\n".to_vec()],
            Some(BASH_UPDATE_THROTTLE + Duration::from_millis(20)),
        )
        .await?;
        assert_eq!(
            text_of(
                updates
                    .last()
                    .ok_or_else(|| io::Error::other("missing update"))?
            ),
            text_of(&result)
        );
        assert_eq!(text_of(&result), "first\nsecond\n");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finalized_incomplete_utf8_is_streamed() -> Result<(), Box<dyn std::error::Error>> {
        let (result, updates) =
            execute_scripted(vec![b"ok".to_vec(), vec![0xf0, 0x9f, 0x98]], None).await?;
        let final_partial = updates
            .last()
            .ok_or_else(|| io::Error::other("missing update"))?;
        assert_eq!(text_of(final_partial), "ok\u{fffd}");
        assert_eq!(text_of(final_partial), text_of(&result));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn final_snapshot_matching_stream_does_not_duplicate()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_result, updates) = execute_scripted(vec![b"x\n".to_vec()], None).await?;
        assert_eq!(updates.len(), 2);
        assert_eq!(text_of(&updates[1]), "x\n");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn untruncated_has_no_spill() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let tool = BashTool::new(dir.path());
        let result = tool
            .execute(
                "1",
                json_map(json!({"command": "printf 'small\n'"}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await?;
        assert_eq!(text_of(&result), "small\n");
        assert!(result.details.is_null());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_cwd_errors() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let missing = dir.path().join("nope");
        let tool = BashTool::new(&missing);
        let err = expected_error(
            tool.execute(
                "1",
                json_map(json!({"command": "true"}))?,
                CancellationToken::new(),
                ToolUpdates::noop(),
            )
            .await,
            "missing cwd accepted",
        )?;
        assert!(
            text_of_err(&err).contains("Working directory does not exist:"),
            "{}",
            text_of_err(&err)
        );
        Ok(())
    }

    fn process_alive(pid: u32) -> bool {
        #[cfg(unix)]
        {
            use nix::sys::signal::kill;
            use nix::unistd::Pid;
            i32::try_from(pid).is_ok_and(|raw| kill(Pid::from_raw(raw), None).is_ok())
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            false
        }
    }
}
