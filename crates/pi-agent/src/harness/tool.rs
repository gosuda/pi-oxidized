//! Harness-native tool contracts and invocation-scoped progress.
//!
//! The harness tool surface is deliberately separate from the low-level agent
//! loop's [`crate::tool::AgentTool`].  A harness tool receives durable
//! invocation identity and a typed context; the runtime owns the invocation
//! implementation and decides when an update can be published.

use std::any::Any;
use std::sync::{Arc, Condvar, Mutex};

use futures::future::BoxFuture;
use pi_ai::{ConstrainedSampling, Tool};
use serde_json::Value;

use crate::context::Context;
use crate::error::ToolError;
use crate::session::{EntryId, OperationId, SessionError};
use crate::tool::{AgentToolResult, ToolExecutionMode};

use super::result::HarnessError;

/// A harness-native tool executed during a durable operation.
pub trait HarnessTool: Send + Sync {
    /// Stable model-visible tool name.
    fn name(&self) -> &str;
    /// Model-visible tool description.
    fn description(&self) -> &str;
    /// JSON Schema for the tool's arguments.
    fn parameters(&self) -> &Value;
    /// Scheduling mode for calls to this tool.
    fn execution_mode(&self) -> ToolExecutionMode;
    /// Provider-side constrained sampling requested by this tool.
    ///
    /// `None` leaves the provider decision unchanged.  An explicit
    /// [`ConstrainedSampling::Disabled`] is preserved as `false` when the
    /// runtime builds the provider-facing tool record.
    fn constrained_sampling(&self) -> Option<ConstrainedSampling> {
        None
    }
    /// Execute one already-prepared call.
    fn execute<'a>(
        &'a self,
        tool_call_id: &'a str,
        params: Value,
        on_update: &'a ToolUpdateSink,
        tool_context: Option<&'a ToolContextValue>,
        invocation: &'a dyn ToolInvocation,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<AgentToolResult, ToolError>>;
}

/// Stable identity and durable replay-memo capability for one tool call.
pub trait ToolInvocation: Send + Sync {
    /// Opaque session-unique id equal to the call's reserved result-entry id.
    fn invocation_id(&self) -> &EntryId;
    /// Durable operation owning this invocation.
    fn operation_id(&self) -> &OperationId;
    /// Turn id of the assistant response that requested this call.
    fn turn_id(&self) -> &str;
    /// Read an invocation-scoped memo.
    fn get_memo<'a>(
        &'a self,
        name: &'a str,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<Value>, SessionError>>;
    /// Set or delete an invocation-scoped memo.
    fn set_memo<'a>(
        &'a self,
        name: &'a str,
        value: Option<Value>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), SessionError>>;
}

/// Shared callback used by [`ToolUpdateSink`].
pub type ToolUpdateCallback = Arc<dyn Fn(AgentToolResult, bool) + Send + Sync>;

struct ToolUpdateState {
    accepting: bool,
    in_flight: usize,
    callback: Option<ToolUpdateCallback>,
}

/// Full-snapshot progress sink for one tool invocation.
///
/// Updates are accepted in admission order and delivered as complete snapshots.
/// Once [`Self::stop_accepting`] returns, no callback can still be running, so a
/// late tool callback cannot publish after its durable result has settled.
pub struct ToolUpdateSink {
    state: Arc<(Mutex<ToolUpdateState>, Condvar)>,
}

impl Clone for ToolUpdateSink {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl ToolUpdateSink {
    /// Creates an update sink backed by `callback`.
    #[must_use]
    pub fn new(callback: impl Fn(AgentToolResult, bool) + Send + Sync + 'static) -> Self {
        Self::from_callback(Arc::new(callback))
    }

    /// Creates an update sink from an already shared callback.
    #[must_use]
    pub fn from_callback(callback: ToolUpdateCallback) -> Self {
        Self {
            state: Arc::new((
                Mutex::new(ToolUpdateState {
                    accepting: true,
                    in_flight: 0,
                    callback: Some(callback),
                }),
                Condvar::new(),
            )),
        }
    }

    /// Sends a full replacement snapshot and its checkpoint request.
    pub fn send(&self, partial: AgentToolResult, checkpoint: bool) {
        let (lock, cvar) = &*self.state;
        let callback = {
            let mut state = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !state.accepting {
                return;
            }
            let Some(callback) = state.callback.clone() else {
                return;
            };
            state.in_flight = state.in_flight.saturating_add(1);
            callback
        };

        callback(partial, checkpoint);

        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight = state.in_flight.saturating_sub(1);
        if state.in_flight == 0 {
            cvar.notify_all();
        }
    }

    /// Stops accepting updates and waits for already-admitted callbacks.
    pub fn stop_accepting(&self) {
        let (lock, cvar) = &*self.state;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.accepting = false;
        state.callback = None;
        while state.in_flight != 0 {
            state = cvar
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Returns whether this sink still accepts updates.
    #[must_use]
    pub fn is_accepting(&self) -> bool {
        let (lock, _) = &*self.state;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .accepting
    }
}

/// Source for a turn-scoped typed tool context.
pub type ToolContextSource =
    Arc<dyn Fn(Context) -> BoxFuture<'static, Result<ToolContextValue, HarnessError>> + Send + Sync>;

/// Erased, shareable tool context value.
pub type ToolContextValue = Arc<dyn Any + Send + Sync>;

/// Convert a harness tool to the exact provider-facing tool record.
///
/// In particular, constrained-sampling is copied instead of being dropped;
/// this preserves an explicit `false` opt-out in the `pi_ai` wire type.
#[must_use]
pub fn to_provider_tool(tool: &dyn HarnessTool) -> Tool {
    Tool {
        name: tool.name().to_owned(),
        description: tool.description().to_owned(),
        parameters: tool.parameters().clone(),
        constrained_sampling: tool.constrained_sampling(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn update_sink_stops_late_full_snapshot_delivery() {
        let seen = Arc::new(Mutex::new(Vec::<(Value, bool)>::new()));
        let receiver = Arc::clone(&seen);
        let sink = ToolUpdateSink::new(move |result, checkpoint| {
            receiver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((result.details, checkpoint));
        });

        sink.send(
            AgentToolResult {
                details: json!({"step": 1}),
                ..AgentToolResult::default()
            },
            true,
        );
        sink.stop_accepting();
        sink.send(
            AgentToolResult {
                details: json!({"step": 2}),
                ..AgentToolResult::default()
            },
            false,
        );

        let values = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(values, vec![(json!({"step": 1}), true)]);
    }
}
