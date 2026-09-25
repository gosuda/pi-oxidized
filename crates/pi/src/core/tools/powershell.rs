//! Windows PowerShell tool.

#![cfg(windows)]

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use pi_agent::{AgentTool, AgentToolResult, ToolError, ToolUpdates};
use pi_ai::ToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Per-stream capture bound, mirroring the Bash tool's default byte budget.
const MAX_CAPTURE_BYTES: usize = 50 * 1024;
/// Bound on the post-exit pipe drain; mirrors the Bash tool's finalize window.
const DRAIN_WINDOW: Duration = Duration::from_millis(200);

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct PowerShellToolInput {
    /// PowerShell command to execute.
    pub command: String,
}

#[derive(Clone, Debug)]
pub struct PowerShellTool {
    cwd: PathBuf,
    parameters: Value,
    description: String,
}

impl PowerShellTool {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            parameters: json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false
            }),
            description: "Execute a PowerShell command on Windows.".to_owned(),
        }
    }

    fn parse_input(args: &Map<String, Value>) -> Result<PowerShellToolInput, ToolError> {
        serde_json::from_value(Value::Object(args.clone()))
            .map_err(|error| ToolError::new(format!("PowerShell tool input is invalid. {error}")))
    }
}

impl AgentTool for PowerShellTool {
    fn name(&self) -> &'static str {
        "powershell"
    }
    fn label(&self) -> &'static str {
        "powershell"
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
        _updates: ToolUpdates,
    ) -> BoxFuture<'static, Result<AgentToolResult, ToolError>> {
        let cwd = self.cwd.clone();
        async move {
            if cancel.is_cancelled() {
                return Err(ToolError::new("Operation cancelled"));
            }
            let input = Self::parse_input(&args)?;
            let mut child = spawn_powershell(cwd, input.command)?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| ToolError::new("PowerShell stdout pipe is unavailable"))?;
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| ToolError::new("PowerShell stderr pipe is unavailable"))?;
            let stdout_capture = Arc::new(Mutex::new(BoundedCapture::default()));
            let stderr_capture = Arc::new(Mutex::new(BoundedCapture::default()));
            // Concurrent readers keep both pipes drained so the child can
            // never block on a full pipe while the other stream is captured.
            let readers = (
                tokio::spawn(drain_bounded(stdout, Arc::clone(&stdout_capture))),
                tokio::spawn(drain_bounded(stderr, Arc::clone(&stderr_capture))),
            );
            // kill_on_drop plus this race: when the scheduler drops the future
            // on cancellation, or the token fires, the child dies with the
            // tool call instead of outliving it.
            let status = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    let _ = child.start_kill();
                    return Err(ToolError::new("Operation cancelled"));
                }
                status = child.wait() => status,
            }
            .map_err(|error| ToolError::new(format!("PowerShell failed: {error}")))?;
            // Bounded tail drain: a grandchild holding the pipe handles must
            // not pin the tool past the child's exit.
            let _ = tokio::time::timeout(DRAIN_WINDOW, async {
                let _ = readers.0.await;
                let _ = readers.1.await;
            })
            .await;
            let stdout_capture = lock_capture(&stdout_capture);
            let stderr_capture = lock_capture(&stderr_capture);
            let truncated = stdout_capture.truncated || stderr_capture.truncated;
            let mut text = String::from_utf8_lossy(&stdout_capture.bytes).into_owned();
            let stderr_text = String::from_utf8_lossy(&stderr_capture.bytes);
            if !stderr_text.is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&stderr_text);
            }
            if truncated {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str("[PowerShell output truncated]");
            }
            let exit_code = status.code().unwrap_or(-1);
            Ok(AgentToolResult {
                content: vec![ToolResultContent::Text(pi_ai::TextContent::new(text))],
                details: json!({"exitCode": exit_code}),
                terminate: None,
                ..Default::default()
            })
        }
        .boxed()
    }
}

fn spawn_powershell(cwd: PathBuf, command: String) -> Result<tokio::process::Child, ToolError> {
    Command::new("powershell.exe")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!(
            "try {{ [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 }} catch {{}}; {command}"
        ))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The tool future owns the child: if the future is dropped after the
        // process started, the child is killed instead of orphaned.
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ToolError::new(format!("PowerShell failed: {error}")))
}

/// Head-keeping bounded accumulator: bytes beyond the cap are dropped and
/// flagged instead of buffered without bound.
#[derive(Default)]
struct BoundedCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

impl BoundedCapture {
    fn push(&mut self, chunk: &[u8]) {
        let room = MAX_CAPTURE_BYTES.saturating_sub(self.bytes.len());
        let keep = room.min(chunk.len());
        self.bytes.extend_from_slice(&chunk[..keep]);
        self.truncated |= keep < chunk.len();
    }
}

fn lock_capture(capture: &Mutex<BoundedCapture>) -> MutexGuard<'_, BoundedCapture> {
    capture.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Drains `pipe` into `capture` until EOF, dropping bytes past the cap. The
/// guard is scoped inside the loop iteration; it is never held across `.await`.
async fn drain_bounded(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    capture: Arc<Mutex<BoundedCapture>>,
) {
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => lock_capture(&capture).push(&chunk[..read]),
        }
    }
}
