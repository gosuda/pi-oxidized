//! Tool contract used by the agent loop.

use std::sync::{Arc, Condvar, Mutex};

use futures::future::BoxFuture;
use pi_ai::{TextContent, Tool, ToolResultContent};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;

/// How tool calls from a single assistant message are scheduled.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecutionMode {
    /// Execute tool calls one by one in assistant source order.
    Sequential,
    /// Preflight sequentially, then execute allowed tools concurrently.
    #[default]
    Parallel,
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

/// Final or partial result produced by a tool.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolResult {
    /// Text or image content returned to the model.
    pub content: Vec<ToolResultContent>,
    /// Arbitrary structured details for logs or UI rendering.
    #[serde(default = "empty_object")]
    pub details: Value,
    /// Tool names introduced by this result and available afterward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_tool_names: Option<Vec<String>>,
    /// Hint that the agent should stop after the current tool batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

impl Default for AgentToolResult {
    fn default() -> Self {
        Self {
            content: Vec::new(),
            details: empty_object(),
            added_tool_names: None,
            terminate: None,
        }
    }
}

/// Builds an error tool result whose content is the provided message.
#[must_use]
pub fn error_tool_result(message: impl Into<String>) -> AgentToolResult {
    AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent::new(message))],
        details: empty_object(),
        added_tool_names: None,
        terminate: None,
    }
}

impl From<ToolError> for AgentToolResult {
    fn from(error: ToolError) -> Self {
        error_tool_result(error.message())
    }
}

type UpdateSink = Arc<dyn Fn(AgentToolResult) + Send + Sync>;

struct ToolUpdatesState {
    accepting: bool,
    in_flight: usize,
    sink: Option<UpdateSink>,
}

/// Streaming update handle scoped to one `execute` invocation.
///
/// Calls made after [`ToolUpdates::stop_accepting`] returns are ignored.
/// `stop_accepting` waits for any already-accepted callback to finish so a late
/// update cannot publish after the execution lifecycle has settled.
#[derive(Clone)]
pub struct ToolUpdates {
    state: Arc<(Mutex<ToolUpdatesState>, Condvar)>,
}

impl Default for ToolUpdates {
    fn default() -> Self {
        Self::noop()
    }
}

impl ToolUpdates {
    /// Creates an update handle that invokes `sink` while accepting updates.
    #[must_use]
    pub fn new(sink: impl Fn(AgentToolResult) + Send + Sync + 'static) -> Self {
        Self {
            state: Arc::new((
                Mutex::new(ToolUpdatesState {
                    accepting: true,
                    in_flight: 0,
                    sink: Some(Arc::new(sink)),
                }),
                Condvar::new(),
            )),
        }
    }

    /// Creates a no-op update handle.
    #[must_use]
    pub fn noop() -> Self {
        Self {
            state: Arc::new((
                Mutex::new(ToolUpdatesState {
                    accepting: true,
                    in_flight: 0,
                    sink: None,
                }),
                Condvar::new(),
            )),
        }
    }

    /// Emits a partial tool result while this handle still accepts updates.
    pub fn send(&self, partial_result: AgentToolResult) {
        let (lock, cvar) = &*self.state;
        let sink = {
            let mut guard = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !guard.accepting {
                return;
            }
            let Some(sink) = guard.sink.clone() else {
                return;
            };
            guard.in_flight = guard.in_flight.saturating_add(1);
            sink
        };

        sink(partial_result);

        let mut guard = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.in_flight = guard.in_flight.saturating_sub(1);
        if guard.in_flight == 0 {
            cvar.notify_all();
        }
    }

    /// Stops accepting further updates after execute settles.
    ///
    /// Returns only after any already-accepted callback has finished.
    pub fn stop_accepting(&self) {
        let (lock, cvar) = &*self.state;
        let mut guard = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.accepting = false;
        guard.sink = None;
        while guard.in_flight > 0 {
            guard = cvar
                .wait(guard)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Returns whether this handle still accepts updates.
    #[must_use]
    pub fn is_accepting(&self) -> bool {
        let (lock, _) = &*self.state;
        let guard = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.accepting
    }
}

/// Tool definition used by the agent runtime.
///
/// Object-safe and independent of `async-trait`/`jsonschema`. Concrete tools
/// own their own argument validation (for example via schemars in Phase 3).
pub trait AgentTool: Send + Sync {
    /// Unique tool name.
    fn name(&self) -> &str;

    /// Human-readable label for UI display.
    fn label(&self) -> &str;

    /// Human-readable tool description.
    fn description(&self) -> &str;

    /// JSON Schema for tool arguments.
    fn parameters(&self) -> &Value;

    /// Optional per-tool execution mode override.
    ///
    /// When any tool in a batch returns [`ToolExecutionMode::Sequential`], the
    /// whole batch executes sequentially.
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }

    /// Optional compatibility shim for raw tool-call arguments before validation.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when raw arguments cannot be prepared for validation.
    fn prepare_arguments(&self, raw: &Map<String, Value>) -> Result<Map<String, Value>, ToolError> {
        Ok(raw.clone())
    }

    /// Validates prepared arguments for this tool.
    ///
    /// On success returns the arguments that will be passed to [`AgentTool::execute`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when arguments fail tool-specific validation.
    fn validate_arguments(
        &self,
        args: &Map<String, Value>,
    ) -> Result<Map<String, Value>, ToolError>;

    /// Executes the tool call.
    ///
    /// Failures must be returned as [`ToolError`]; the loop converts them into
    /// error tool results. The returned future is `'static` so implementations
    /// must clone any needed state instead of borrowing `self`.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when tool execution fails.
    fn execute(
        &self,
        tool_call_id: &str,
        args: Map<String, Value>,
        cancel: CancellationToken,
        updates: ToolUpdates,
    ) -> BoxFuture<'static, Result<AgentToolResult, ToolError>>;
}

/// Converts an [`AgentTool`] into a provider-facing [`Tool`] definition.
#[must_use]
pub fn to_pi_tool(tool: &dyn AgentTool) -> Tool {
    Tool {
        name: tool.name().to_owned(),
        description: tool.description().to_owned(),
        parameters: tool.parameters().clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct StrictTool {
        name: String,
        label: String,
        description: String,
        parameters: Value,
        mode: Option<ToolExecutionMode>,
    }

    impl AgentTool for StrictTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn label(&self) -> &str {
            &self.label
        }

        fn description(&self) -> &str {
            &self.description
        }

        fn parameters(&self) -> &Value {
            &self.parameters
        }

        fn execution_mode(&self) -> Option<ToolExecutionMode> {
            self.mode
        }

        fn prepare_arguments(
            &self,
            raw: &Map<String, Value>,
        ) -> Result<Map<String, Value>, ToolError> {
            let mut prepared = raw.clone();
            if let Some(Value::String(path)) = prepared.get("path").cloned() {
                prepared.insert("path".to_owned(), Value::String(path.trim().to_owned()));
            }
            Ok(prepared)
        }

        fn validate_arguments(
            &self,
            args: &Map<String, Value>,
        ) -> Result<Map<String, Value>, ToolError> {
            match args.get("path") {
                Some(Value::String(path)) if !path.is_empty() => Ok(args.clone()),
                _ => Err(ToolError::new("path is required")),
            }
        }

        fn execute(
            &self,
            _tool_call_id: &str,
            args: Map<String, Value>,
            _cancel: CancellationToken,
            updates: ToolUpdates,
        ) -> BoxFuture<'static, Result<AgentToolResult, ToolError>> {
            Box::pin(async move {
                updates.send(AgentToolResult {
                    content: vec![ToolResultContent::Text(TextContent::new("partial"))],
                    details: json!({ "stage": "partial" }),
                    added_tool_names: None,
                    terminate: None,
                });
                updates.stop_accepting();
                updates.send(AgentToolResult {
                    content: vec![ToolResultContent::Text(TextContent::new("late"))],
                    details: json!({ "stage": "late" }),
                    added_tool_names: None,
                    terminate: None,
                });
                Ok(AgentToolResult {
                    content: vec![ToolResultContent::Text(TextContent::new("ok"))],
                    details: Value::Object(args),
                    added_tool_names: None,
                    terminate: None,
                })
            })
        }
    }

    #[test]
    fn tool_execution_mode_serde_is_lowercase() -> Result<(), serde_json::Error> {
        assert_eq!(
            serde_json::to_value(ToolExecutionMode::Sequential)?,
            json!("sequential")
        );
        assert_eq!(
            serde_json::to_value(ToolExecutionMode::Parallel)?,
            json!("parallel")
        );
        let sequential: ToolExecutionMode = serde_json::from_value(json!("sequential"))?;
        assert_eq!(sequential, ToolExecutionMode::Sequential);
        Ok(())
    }

    #[test]
    fn sequential_mode_and_validation_contracts_are_observable() -> Result<(), ToolError> {
        let tool = StrictTool {
            name: "strict".to_owned(),
            label: "Strict".to_owned(),
            description: "requires path".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            mode: Some(ToolExecutionMode::Sequential),
        };

        assert_eq!(tool.execution_mode(), Some(ToolExecutionMode::Sequential));

        let prepared =
            tool.prepare_arguments(&Map::from_iter([("path".to_owned(), json!("  a.rs  "))]))?;
        assert_eq!(prepared.get("path"), Some(&json!("a.rs")));

        let validated = tool.validate_arguments(&prepared)?;
        assert_eq!(validated.get("path"), Some(&json!("a.rs")));

        let missing = tool.validate_arguments(&Map::new());
        assert!(matches!(&missing, Err(error) if error.message() == "path is required"));

        let pi_tool = to_pi_tool(&tool);
        assert_eq!(pi_tool.name, "strict");
        assert_eq!(pi_tool.description, "requires path");
        assert_eq!(pi_tool.parameters, tool.parameters);
        Ok(())
    }

    #[test]
    fn tool_result_and_error_conversion_round_trip() -> Result<(), serde_json::Error> {
        let result = AgentToolResult {
            content: vec![ToolResultContent::Text(TextContent::new("hello"))],
            details: json!({ "n": 1 }),
            added_tool_names: Some(vec!["extra".to_owned()]),
            terminate: Some(true),
        };
        let encoded = serde_json::to_value(&result)?;
        assert_eq!(
            encoded,
            json!({
                "content": [{ "type": "text", "text": "hello" }],
                "details": { "n": 1 },
                "addedToolNames": ["extra"],
                "terminate": true
            })
        );

        let error_result = AgentToolResult::from(ToolError::new("nope"));
        assert_eq!(
            serde_json::to_value(&error_result)?,
            json!({
                "content": [{ "type": "text", "text": "nope" }],
                "details": {}
            })
        );
        Ok(())
    }

    #[test]
    fn tool_updates_ignore_sends_after_stop() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_cb = Arc::clone(&seen);
        let updates = ToolUpdates::new(move |partial| {
            let mut values = seen_cb
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            values.push(partial.content.first().map(|content| match content {
                ToolResultContent::Text(text) => text.text.clone(),
                ToolResultContent::Image(_) => "image".to_owned(),
            }));
        });

        updates.send(error_tool_result("one"));
        updates.stop_accepting();
        updates.send(error_tool_result("two"));

        let values = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(values.as_slice(), &[Some("one".to_owned())]);
        assert!(!updates.is_accepting());
        assert!(ToolUpdates::default().is_accepting());
    }
}
