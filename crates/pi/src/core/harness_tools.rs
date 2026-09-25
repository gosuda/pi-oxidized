//! Adapters for using the native agent-loop tools in a durable harness.
//!
//! `AgentTool` predates the harness boundary and receives a map, a
//! cancellation token, and legacy update handle.  `HarnessTool` receives a
//! JSON value plus invocation/context capabilities.  The adapter keeps those
//! contracts separate: the harness owns argument preparation and invocation
//! state, while the wrapped tool remains responsible for its own execution and
//! validation details.

use std::sync::Arc;

use futures::future::{BoxFuture, FutureExt};
use pi_agent::harness::tool::{
    HarnessTool, ToolContextValue, ToolInvocation, ToolUpdateSink,
};
use pi_agent::{AgentTool, AgentToolResult, ToolError, ToolExecutionMode, ToolUpdates};
use pi_agent::context::Context;
use pi_ai::ConstrainedSampling;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Bridges one legacy [`AgentTool`] into the harness-native tool contract.
pub struct HarnessToolAdapter {
    tool: Arc<dyn AgentTool>,
}

impl HarnessToolAdapter {
    /// Wraps an existing agent-loop tool for use by a harness tool registry.
    #[must_use]
    pub fn new(tool: Arc<dyn AgentTool>) -> Arc<dyn HarnessTool> {
        Arc::new(Self { tool })
    }
}

impl HarnessTool for HarnessToolAdapter {
    fn name(&self) -> &str {
        self.tool.name()
    }

    fn description(&self) -> &str {
        self.tool.description()
    }

    fn parameters(&self) -> &Value {
        self.tool.parameters()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.tool.execution_mode().unwrap_or_default()
    }

    fn constrained_sampling(&self) -> Option<ConstrainedSampling> {
        self.tool.constrained_sampling()
    }

    fn execute<'a>(
        &'a self,
        tool_call_id: &'a str,
        params: Value,
        on_update: &'a ToolUpdateSink,
        _tool_context: Option<&'a ToolContextValue>,
        _invocation: &'a dyn ToolInvocation,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<AgentToolResult, ToolError>> {
        let Value::Object(args) = params else {
            return async {
                Err(ToolError::new(
                    "Harness tool parameters must be a JSON object",
                ))
            }
            .boxed();
        };

        let tool = Arc::clone(&self.tool);
        let cancel = cx
            .token()
            .cloned()
            .unwrap_or_else(CancellationToken::new);
        let sink = on_update.clone();
        async move {
            let updates = ToolUpdates::new(move |partial| sink.send(partial, false));
            tool.execute(tool_call_id, args, cancel, updates).await
        }
        .boxed()
    }
}

