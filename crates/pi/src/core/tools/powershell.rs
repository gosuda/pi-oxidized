//! Windows PowerShell tool.

#![cfg(windows)]

use std::path::PathBuf;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use pi_agent::{AgentTool, AgentToolResult, ToolError, ToolUpdates};
use pi_ai::ToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

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
            let output = Command::new("powershell.exe")
                .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
                .arg(format!("try {{ [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 }} catch {{}}; {}", input.command))
                .current_dir(cwd)
                .output()
                .await
                .map_err(|error| ToolError::new(format!("PowerShell failed: {error}")))?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut text = stdout.into_owned();
            if !stderr.is_empty() {
                if !text.is_empty() { text.push('\n'); }
                text.push_str(&stderr);
            }
            let exit_code = output.status.code().unwrap_or(-1);
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
