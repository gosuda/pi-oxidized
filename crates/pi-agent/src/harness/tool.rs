//! Harness-native tool contracts and invocation-scoped progress.
//!
//! The harness tool surface is deliberately separate from the low-level agent
//! loop's [`crate::tool::AgentTool`].  A harness tool receives durable
//! invocation identity and a typed context; the runtime owns the invocation
//! implementation and decides when an update can be published.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, ThreadId};

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
    /// Threads with at least one in-flight callback admission on this sink.
    active_threads: HashMap<ThreadId, usize>,
}

/// Retires one admitted callback when the `send` that admitted it finishes:
/// drops the in-flight count and the admitting thread's tracking even on
/// unwind.
struct InFlightGuard {
    state: Arc<(Mutex<ToolUpdateState>, Condvar)>,
    admitting_thread: ThreadId,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.state;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight = state.in_flight.saturating_sub(1);
        if let Some(count) = state.active_threads.get_mut(&self.admitting_thread) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.active_threads.remove(&self.admitting_thread);
            }
        }
        if state.in_flight == 0 {
            cvar.notify_all();
        }
    }
}

/// Full-snapshot progress sink for one tool invocation.
///
/// Updates are accepted in admission order and delivered as complete snapshots.
/// Once [`Self::stop_accepting`] returns on the non-reentrant path, no callback
/// can still be running, so a late tool callback cannot publish after its
/// durable result has settled.
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
                    active_threads: HashMap::new(),
                    callback: Some(callback),
                }),
                Condvar::new(),
            )),
        }
    }

    /// Sends a full replacement snapshot and its checkpoint request.
    pub fn send(&self, partial: AgentToolResult, checkpoint: bool) {
        let (callback, admitting_thread) = {
            let (lock, _) = &*self.state;
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
            let admitting_thread = thread::current().id();
            *state.active_threads.entry(admitting_thread).or_insert(0) += 1;
            (callback, admitting_thread)
        };

        // The callback runs without the lock; the guard retires this admission
        // even when the callback unwinds.
        let _admission = InFlightGuard {
            state: Arc::clone(&self.state),
            admitting_thread,
        };
        callback(partial, checkpoint);
    }

    /// Stops accepting updates and waits for already-admitted callbacks.
    ///
    /// Safe to call reentrantly from within this sink's own callback: the
    /// reentrant call marks the sink stopped and returns without blocking, and
    /// the enclosing [`Self::send`] retires its admission to complete the
    /// handshake. Only the non-reentrant path waits for callback quiescence.
    pub fn stop_accepting(&self) {
        let (lock, cvar) = &*self.state;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.accepting = false;
        state.callback = None;
        if state.active_threads.contains_key(&thread::current().id()) {
            // Blocking here would wait on the in-flight admission that only
            // this thread's enclosing `send` can retire: deadlock.
            return;
        }
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
pub type ToolContextSource = Arc<
    dyn Fn(Context) -> BoxFuture<'static, Result<ToolContextValue, HarnessError>> + Send + Sync,
>;

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
#[allow(clippy::panic, reason = "unwind-safety regression needs an exploding callback")]
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

    #[test]
    fn reentrant_stop_accepting_from_callback_does_not_deadlock() {
        let sink_cell = Arc::new(Mutex::new(None::<ToolUpdateSink>));
        let sink_for_callback = Arc::clone(&sink_cell);
        let sink = ToolUpdateSink::new(move |_result, _checkpoint| {
            let sink = sink_for_callback
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(sink) = sink {
                // Must return instead of waiting on this very callback's
                // in-flight admission.
                sink.stop_accepting();
                assert!(!sink.is_accepting());
            }
        });
        *sink_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink.clone());

        // Run on a thread so a regression deadlocks that thread and fails the
        // recv_timeout instead of hanging the whole suite.
        let (done, done_rx) = std::sync::mpsc::channel();
        let sink_for_send = sink.clone();
        let sender = thread::spawn(move || {
            sink_for_send.send(
                AgentToolResult {
                    details: json!({"step": 1}),
                    ..AgentToolResult::default()
                },
                true,
            );
            let _ = done.send(());
        });
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .is_ok(),
            "reentrant stop_accepting deadlocked inside send"
        );
        assert!(sender.join().is_ok(), "sender thread panicked");

        // The reentrant stop took effect: later sends are dropped.
        sink.send(
            AgentToolResult {
                details: json!({"step": 2}),
                ..AgentToolResult::default()
            },
            false,
        );
        assert!(!sink.is_accepting());
    }

    #[test]
    fn reentrant_stop_accepting_with_concurrent_sends_does_not_deadlock() {
        // Two threads admitted concurrently: a reentrant stop on one must not
        // wait on the other's still-running callback.
        let sink_cell = Arc::new(Mutex::new(None::<ToolUpdateSink>));
        let both_in_flight = Arc::new(std::sync::Barrier::new(2));
        let stop_done = Arc::new(std::sync::Barrier::new(2));

        let sink_for_callback = Arc::clone(&sink_cell);
        let entered = Arc::clone(&both_in_flight);
        let stopped = Arc::clone(&stop_done);
        let sink = ToolUpdateSink::new(move |_result, checkpoint| {
            entered.wait();
            if checkpoint {
                // Callback A: stop reentrantly while B's callback is still
                // running on another thread.
                let sink = sink_for_callback
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if let Some(sink) = sink {
                    sink.stop_accepting();
                }
            }
            stopped.wait();
        });
        *sink_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink.clone());

        let (done, done_rx) = std::sync::mpsc::channel();
        for checkpoint in [true, false] {
            let sink = sink.clone();
            let done = done.clone();
            thread::spawn(move || {
                sink.send(
                    AgentToolResult {
                        details: json!({"step": 1}),
                        ..AgentToolResult::default()
                    },
                    checkpoint,
                );
                let _ = done.send(());
            });
        }
        for _ in 0..2 {
            assert!(
                done_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .is_ok(),
                "reentrant stop_accepting deadlocked under concurrent sends"
            );
        }
        assert!(!sink.is_accepting());
    }

    #[test]
    fn panicking_callback_retires_admission_and_unwinds() {
        let sink = ToolUpdateSink::new(|_result, _checkpoint| panic!("callback exploded"));

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sink.send(
                AgentToolResult {
                    details: json!({"step": 1}),
                    ..AgentToolResult::default()
                },
                true,
            );
        }));
        assert!(
            panicked.is_err(),
            "callback panic must propagate out of send"
        );

        // The panicked admission is already retired: stop completes instead of
        // waiting on a phantom in-flight callback.
        sink.stop_accepting();
        assert!(!sink.is_accepting());

        // Post-stop sends stay dropped.
        sink.send(
            AgentToolResult {
                details: json!({"step": 2}),
                ..AgentToolResult::default()
            },
            false,
        );
    }
}
