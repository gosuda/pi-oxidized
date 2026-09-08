//! Ordered, typed harness hooks and their aggregation rules.
//!
//! Hook registration is a process-local concern: handlers are never persisted
//! and a run receives a snapshot of the registrations that existed when its
//! aggregate started.  Every handler receives an owned [`Context`] carrying
//! the drive gate token, so cancellation remains the same context mechanism as
//! the rest of the agent crate.

use std::any::Any;
use std::collections::BTreeMap;
use std::error::Error;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use super::api::HarnessResources;
use super::event::{HandlerErrorKind, HarnessEvent, HarnessEventPayload};
use super::gate::{Gate, GateRejection};
use super::result::{HarnessError, HarnessFault};
use super::stream::{
    HarnessStreamOptionsPatch, apply_stream_options_patch, create_stream_options_patch,
};
use crate::context::{Context, telemetry_context, with_telemetry_context};
use crate::message::AgentMessage;
use crate::session::{
    CompactionReason, DurableStructuralPreparation, EntryId, HarnessStreamOptions, LaneName,
    OperationKind, SettledAssistantMessage,
};
use crate::telemetry::{
    AttributeValue, SpanAttributes, SpanOptions, SpanStatus, TelemetryContext, TelemetrySpan,
    set_attributes_contained, set_status_contained, start_span_contained,
};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A handler failure accepted by the hook boundary.
pub type HookError = Box<dyn Error + Send + Sync + 'static>;
/// Failure of a hook aggregate after gate admission.
///
/// A gate rejection describes the drive lifecycle.  A handler failure is kept
/// separate so callers do not mistake an ordinary hook error for a closed
/// drive.  The handler source is retained as [`HarnessFault::cause`].
#[derive(Debug, thiserror::Error)]
pub enum HookRunError {
    /// The drive was aborted or closed at effect admission.
    #[error(transparent)]
    Gate(#[from] GateRejection),
    /// A fail-closed hook handler failed.
    #[error("hook handler failed: {0}")]
    Handler(#[source] HarnessFault),
}

/// Result returned by a gate-admitted hook aggregate.
pub type HookRunResult<T> = Result<T, HookRunError>;

/// A future returned by one hook handler.
pub type HookFuture<T> = BoxFuture<'static, Result<T, HookError>>;

/// Callback used to publish isolated `handler_error` events.
///
/// The registry never routes this callback back through itself.  This prevents
/// a broken error listener from recursively generating more hook errors.
pub type HookErrorReporter =
    Arc<dyn Fn(HarnessEvent, Context) -> BoxFuture<'static, ()> + Send + Sync>;

/// A local error used when a structural hook returns incompatible decisions.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct HookMessageError(String);
/// A typed asynchronous hook handler.
pub trait HookHandler<E, R>: Send + Sync + 'static {
    /// Runs the handler for one invocation.
    fn call(&self, event: E, context: Context) -> HookFuture<R>;
}

impl<F, E, R, FResult, ErrorType> HookHandler<E, R> for F
where
    F: Fn(E, Context) -> FResult + Send + Sync + 'static,
    E: Send + 'static,
    R: Send + 'static,
    FResult: Future<Output = Result<R, ErrorType>> + Send + 'static,
    ErrorType: Into<HookError>,
{
    fn call(&self, event: E, context: Context) -> HookFuture<R> {
        let future = (self)(event, context);
        Box::pin(async move { future.await.map_err(Into::into) })
    }
}

impl<E, R> HookHandler<E, R> for Arc<dyn HookHandler<E, R>>
where
    E: Send + 'static,
    R: Send + 'static,
{
    fn call(&self, event: E, context: Context) -> HookFuture<R> {
        (**self).call(event, context)
    }
}

/// Shared handler object used by each built-in hook marker.
///
/// Each `*Hook` alias pairs one event with its per-handler result shape, and a
/// handler returns `Ok(None)` to contribute nothing.  Only `before_drive` is
/// fail-closed: its first handler error aborts the aggregate with
/// [`HookRunError::Handler`].  Every other aggregate turns a handler error
/// into a [`HookErrorReporter`] `handler_error` event instead of aborting.
/// Each `*Hook` alias records its own fold rule.
pub type HookFn<E, R> = Arc<dyn HookHandler<E, R>>;

/// The eleven stable hook names.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookName {
    /// Invoked before a run's prompt is admitted.
    BeforeRun,
    /// Invoked before any drive work starts.
    BeforeDrive,
    /// Invoked when a run reaches its end boundary.
    BeforeRunEnd,
    /// Transforms the effective context before a provider request.
    TransformContext,
    /// Transforms per-request stream options.
    BeforeRequest,
    /// Transforms a provider payload before transport.
    BeforePayload,
    /// Transforms a settled assistant response.
    AfterResponse,
    /// Guards and transforms a tool call before execution.
    BeforeTool,
    /// Transforms a tool result after execution.
    AfterTool,
    /// Supplies or declines a compaction result.
    BeforeCompaction,
    /// Supplies or declines a branch-navigation summary.
    BeforeNavigation,
}

impl HookName {
    /// Returns the wire spelling used in `handler_error` events.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BeforeRun => "before_run",
            Self::BeforeDrive => "before_drive",
            Self::BeforeRunEnd => "before_run_end",
            Self::TransformContext => "transform_context",
            Self::BeforeRequest => "before_request",
            Self::BeforePayload => "before_payload",
            Self::AfterResponse => "after_response",
            Self::BeforeTool => "before_tool",
            Self::AfterTool => "after_tool",
            Self::BeforeCompaction => "before_compaction",
            Self::BeforeNavigation => "before_navigation",
        }
    }
}

impl std::fmt::Display for HookName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Envelope carried by every hook event.
pub trait HookEvent: Clone + Send + Sync + 'static {
    /// Lane receiving the hook invocation.
    fn lane(&self) -> &LaneName;
    /// Logical run/operation identifier receiving the hook invocation.
    fn run_id(&self) -> &str;
}

/// Generated compaction result that a `before_compaction` hook may provide.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactResult {
    /// Summary replacing the compacted history.
    pub summary: String,
    /// Estimated context tokens before compaction.
    pub tokens_before: u64,
    /// Usage consumed by summary generation, when known.
    pub usage: Option<pi_ai::Usage>,
    /// Recent messages retained outside the summary.
    pub retained_tail: Vec<AgentMessage>,
    /// Implementation-specific details for the durable entry.
    pub details: Option<Value>,
}

/// Generated branch summary result that a `before_navigation` hook may provide.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchSummaryResult {
    /// Summary text for the branch being left.
    pub summary: String,
    /// Usage consumed by summary generation, when known.
    pub usage: Option<pi_ai::Usage>,
    /// Paths read while preparing the summary.
    pub read_files: Vec<String>,
    /// Paths modified while preparing the summary.
    pub modified_files: Vec<String>,
}

/// `before_run` invocation.
#[derive(Clone, Debug)]
pub struct BeforeRunEvent {
    /// Hook envelope lane.
    pub lane: LaneName,
    /// Hook envelope run identifier.
    pub run_id: String,
    /// Prompt messages before this handler runs.
    pub prompt: Vec<AgentMessage>,
    /// Configured skills and prompt templates.
    pub resources: HarnessResources,
}

/// Messages injected by one `before_run` aggregate.
#[derive(Clone, Debug)]
pub struct BeforeRunResult {
    /// Messages appended to the prompt.
    pub messages: Option<Vec<AgentMessage>>,
}

/// `before_drive` invocation.
#[derive(Clone, Debug)]
pub struct BeforeDriveEvent {
    /// Hook envelope lane.
    pub lane: LaneName,
    /// Hook envelope run identifier.
    pub run_id: String,
    /// Operation about to be driven.
    pub operation: OperationKind,
}

/// `before_run_end` invocation.
#[derive(Clone, Debug)]
pub struct BeforeRunEndEvent {
    /// Hook envelope lane.
    pub lane: LaneName,
    /// Hook envelope run identifier.
    pub run_id: String,
    /// Messages that ended the run.
    pub messages: Vec<AgentMessage>,
}

/// Follow-up supplied by a `before_run_end` handler.
#[derive(Clone, Debug)]
pub struct BeforeRunEndResult {
    /// Follow-up prompt text.
    pub follow_up: Option<String>,
}

/// `transform_context` invocation.
#[derive(Clone, Debug)]
pub struct TransformContextEvent {
    /// Hook envelope lane.
    pub lane: LaneName,
    /// Hook envelope run identifier.
    pub run_id: String,
    /// Current context messages.
    pub messages: Vec<AgentMessage>,
    /// Current system prompt.
    pub system_prompt: String,
}

/// Context replacements returned by a `transform_context` handler.
#[derive(Clone, Debug)]
pub struct TransformContextResult {
    /// Replacement messages, when supplied.
    pub messages: Option<Vec<AgentMessage>>,
    /// Replacement system prompt, when supplied.
    pub system_prompt: Option<String>,
}

/// Provider request phase exposed to `before_request` handlers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BeforeRequestStep {
    /// Normal assistant generation.
    Assistant,
    /// Deferred-response polling/generation.
    Deferred,
    /// Compaction summary generation.
    Compaction,
    /// Branch-summary generation.
    BranchSummary,
}

/// `before_request` invocation.
#[derive(Clone, Debug)]
pub struct BeforeRequestEvent {
    /// Hook envelope lane.
    pub lane: LaneName,
    /// Hook envelope run identifier.
    pub run_id: String,
    /// Model selected for this request.
    pub model: pi_ai::Model,
    /// Request phase.
    pub step: BeforeRequestStep,
    /// One-based request attempt.
    pub attempt: u64,
    /// Stream options before this handler runs.
    pub stream_options: HarnessStreamOptions,
}

/// Stream-option patch returned by a `before_request` aggregate.
#[derive(Clone, Debug)]
pub struct BeforeRequestResult {
    /// Diff from the original request options.
    pub stream_options: Option<HarnessStreamOptionsPatch>,
}

/// `before_payload` invocation.
#[derive(Clone, Debug)]
pub struct BeforePayloadEvent {
    /// Hook envelope lane.
    pub lane: LaneName,
    /// Hook envelope run identifier.
    pub run_id: String,
    /// Model selected for this request.
    pub model: pi_ai::Model,
    /// Provider payload at the current fold point.
    pub payload: Value,
}

/// Payload replacement returned by a `before_payload` aggregate.
#[derive(Clone, Debug)]
pub struct BeforePayloadResult {
    /// Provider payload after the fold.
    pub payload: Value,
}

/// `after_response` invocation.
#[derive(Clone, Debug)]
pub struct AfterResponseEvent {
    /// Hook envelope lane.
    pub lane: LaneName,
    /// Hook envelope run identifier.
    pub run_id: String,
    /// HTTP response status captured before body consumption.
    pub status: Option<u16>,
    /// HTTP response headers captured before body consumption.
    pub headers: Option<BTreeMap<String, String>>,
    /// Settled message at the current fold point.
    pub message: SettledAssistantMessage,
}

/// Assistant-message replacement returned by an `after_response` aggregate.
#[derive(Clone, Debug)]
pub struct AfterResponseResult {
    /// Replacement settled message.
    pub message: Option<SettledAssistantMessage>,
}

/// `before_tool` invocation.
#[derive(Clone, Debug)]
pub struct BeforeToolEvent {
    /// Hook envelope lane.
    pub lane: LaneName,
    /// Hook envelope run identifier.
    pub run_id: String,
    /// Provider tool-call identifier.
    pub tool_call_id: String,
    /// Registered tool name.
    pub tool_name: String,
    /// Arguments at the current fold point.
    pub args: Map<String, Value>,
}

/// A hook-provided tool block decision.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolBlock {
    /// Human-readable block reason.
    pub reason: String,
    /// Whether the operation should terminate after blocking.
    pub terminate: Option<bool>,
}

/// Argument replacement or block returned by a `before_tool` aggregate.
#[derive(Clone, Debug)]
pub struct BeforeToolResult {
    /// Replacement arguments.
    pub args: Option<Map<String, Value>>,
    /// First block decision, if any.
    pub block: Option<ToolBlock>,
}

/// `after_tool` invocation.
#[derive(Clone, Debug)]
pub struct AfterToolEvent {
    /// Hook envelope lane.
    pub lane: LaneName,
    /// Hook envelope run identifier.
    pub run_id: String,
    /// Provider tool-call identifier.
    pub tool_call_id: String,
    /// Registered tool name.
    pub tool_name: String,
    /// Arguments used by the admitted tool effect.
    pub args: Map<String, Value>,
    /// Tool content at the current fold point.
    pub content: Vec<pi_ai::ToolResultContent>,
    /// Tool details at the current fold point.
    pub details: Option<Value>,
    /// Whether the current result is an error.
    pub is_error: bool,
    /// Usage attached to the current result, when known.
    pub usage: Option<pi_ai::Usage>,
}

/// Fold result returned by an `after_tool` handler.
#[derive(Clone, Debug)]
pub struct AfterToolResult {
    /// Replacement content.
    pub content: Option<Vec<pi_ai::ToolResultContent>>,
    /// Replacement details.
    pub details: Option<Value>,
    /// Replacement error marker.
    pub is_error: Option<bool>,
    /// Replacement usage.
    pub usage: Option<pi_ai::Usage>,
    /// Whether the operation should terminate.
    pub terminate: Option<bool>,
}

/// `before_compaction` invocation.
#[derive(Clone, Debug)]
pub struct BeforeCompactionEvent {
    /// Hook envelope lane.
    pub lane: LaneName,
    /// Hook envelope run identifier.
    pub run_id: String,
    /// Compaction trigger.
    pub reason: CompactionReason,
    /// Persisted preparation selected for this operation.
    pub preparation: DurableStructuralPreparation,
    /// Optional caller instructions.
    pub custom_instructions: Option<String>,
}

/// Compaction decision returned by a handler.
#[derive(Clone, Debug)]
pub struct BeforeCompactionResult {
    /// Decline the operation rather than generating a summary.
    pub decline: Option<bool>,
    /// Hook-provided summary result.
    pub compaction: Option<CompactResult>,
}

/// `before_navigation` invocation.
#[derive(Clone, Debug)]
pub struct BeforeNavigationEvent {
    /// Hook envelope lane.
    pub lane: LaneName,
    /// Hook envelope run identifier.
    pub run_id: String,
    /// Branch target entry.
    pub target_id: EntryId,
    /// Persisted preparation selected for this operation.
    pub preparation: DurableStructuralPreparation,
    /// Optional caller instructions.
    pub custom_instructions: Option<String>,
}

/// Navigation decision returned by a handler.
#[derive(Clone, Debug)]
pub struct BeforeNavigationResult {
    /// Decline the operation rather than changing the branch.
    pub decline: Option<bool>,
    /// Hook-provided branch summary.
    pub summary: Option<BranchSummaryResult>,
}

macro_rules! impl_hook_event {
    ($event:ty) => {
        impl HookEvent for $event {
            fn lane(&self) -> &LaneName {
                &self.lane
            }

            fn run_id(&self) -> &str {
                self.run_id.as_str()
            }
        }
    };
}

impl_hook_event!(BeforeRunEvent);
impl_hook_event!(BeforeDriveEvent);
impl_hook_event!(BeforeRunEndEvent);
impl_hook_event!(TransformContextEvent);
impl_hook_event!(BeforeRequestEvent);
impl_hook_event!(BeforePayloadEvent);
impl_hook_event!(AfterResponseEvent);
impl_hook_event!(BeforeToolEvent);
impl_hook_event!(AfterToolEvent);
impl_hook_event!(BeforeCompactionEvent);
impl_hook_event!(BeforeNavigationEvent);

/// Built-in hook marker and typed aggregate contract.
pub trait Hook: Send + Sync + 'static {
    /// Event delivered to handlers.
    type Event: HookEvent;
    /// Aggregate result and per-handler result shape.
    type Result: Clone + Send + Sync + 'static;
    /// Handler object accepted by [`HookRegistry::on`].
    type Handler: HookHandler<Self::Event, Self::Result> + Send + Sync + 'static;

    /// Stable hook name.
    const NAME: HookName;

    /// Runs this marker's aggregate implementation.
    #[doc(hidden)]
    fn aggregate(
        registry: &HookRegistry,
        event: Self::Event,
        context: Context,
    ) -> BoxFuture<'_, HookRunResult<Self::Result>>;
}

/// Handler for `before_run`, the object [`HookRegistry::on`] accepts for the
/// [`BeforeRun`] marker.  Injected messages from successive handlers are
/// concatenated, and the aggregate reports injections only when at least one
/// handler supplied them.
pub type BeforeRunHook = HookFn<BeforeRunEvent, Option<BeforeRunResult>>;
/// Handler for `before_drive`.  The result carries no value; only the
/// fail-closed error path is observable.
pub type BeforeDriveHook = HookFn<BeforeDriveEvent, ()>;
/// Handler for `before_run_end`.  The last supplied follow-up wins.
pub type BeforeRunEndHook = HookFn<BeforeRunEndEvent, Option<BeforeRunEndResult>>;
/// Handler for `transform_context`.  Each handler sees the replacements the
/// earlier ones made, and the aggregate always returns the final message list
/// and system prompt.
pub type TransformContextHook = HookFn<TransformContextEvent, Option<TransformContextResult>>;
/// Handler for `before_request`.  Patches are applied in registration order
/// and the aggregate returns a single diff from the original options, present
/// only when something changed.
pub type BeforeRequestHook = HookFn<BeforeRequestEvent, Option<BeforeRequestResult>>;
/// Handler for `before_payload`.  Each returned payload replaces the previous
/// one, so the aggregate always returns a payload.
pub type BeforePayloadHook = HookFn<BeforePayloadEvent, Option<BeforePayloadResult>>;
/// Handler for `after_response`.  Each returned message replaces the previous
/// one, so the aggregate always returns a message.
pub type AfterResponseHook = HookFn<AfterResponseEvent, Option<AfterResponseResult>>;
/// Handler for `before_tool`.  The first block decision stops the chain, and a
/// handler error is folded into a block decision carrying that error text.
pub type BeforeToolHook = HookFn<BeforeToolEvent, Option<BeforeToolResult>>;
/// Handler for `after_tool`.  Replacements are folded field by field, and the
/// aggregate reports only the fields some handler changed.
pub type AfterToolHook = HookFn<AfterToolEvent, Option<AfterToolResult>>;
/// Handler for `before_compaction`.  The first handler that declines or
/// supplies a summary decides the outcome; a result with both is reported and
/// skipped.
pub type BeforeCompactionHook = HookFn<BeforeCompactionEvent, Option<BeforeCompactionResult>>;
/// Handler for `before_navigation`.  The first handler that declines or
/// supplies a branch summary decides the outcome; a result with both is
/// reported and skipped.
pub type BeforeNavigationHook = HookFn<BeforeNavigationEvent, Option<BeforeNavigationResult>>;

/// Hook marker for `before_run`.
pub struct BeforeRun;
impl Hook for BeforeRun {
    type Event = BeforeRunEvent;
    type Result = Option<BeforeRunResult>;
    type Handler = BeforeRunHook;
    const NAME: HookName = HookName::BeforeRun;

    fn aggregate(
        registry: &HookRegistry,
        event: Self::Event,
        context: Context,
    ) -> BoxFuture<'_, HookRunResult<Self::Result>> {
        Box::pin(registry.before_run(event, context))
    }
}

/// Hook marker for `before_drive`.
pub struct BeforeDrive;
impl Hook for BeforeDrive {
    type Event = BeforeDriveEvent;
    type Result = ();
    type Handler = BeforeDriveHook;
    const NAME: HookName = HookName::BeforeDrive;

    fn aggregate(
        registry: &HookRegistry,
        event: Self::Event,
        context: Context,
    ) -> BoxFuture<'_, HookRunResult<Self::Result>> {
        Box::pin(registry.before_drive(event, context))
    }
}

/// Hook marker for `before_run_end`.
pub struct BeforeRunEnd;
impl Hook for BeforeRunEnd {
    type Event = BeforeRunEndEvent;
    type Result = Option<BeforeRunEndResult>;
    type Handler = BeforeRunEndHook;
    const NAME: HookName = HookName::BeforeRunEnd;

    fn aggregate(
        registry: &HookRegistry,
        event: Self::Event,
        context: Context,
    ) -> BoxFuture<'_, HookRunResult<Self::Result>> {
        Box::pin(registry.before_run_end(event, context))
    }
}

/// Hook marker for `transform_context`.
pub struct TransformContext;
impl Hook for TransformContext {
    type Event = TransformContextEvent;
    type Result = Option<TransformContextResult>;
    type Handler = TransformContextHook;
    const NAME: HookName = HookName::TransformContext;

    fn aggregate(
        registry: &HookRegistry,
        event: Self::Event,
        context: Context,
    ) -> BoxFuture<'_, HookRunResult<Self::Result>> {
        Box::pin(registry.transform_context(event, context))
    }
}

/// Hook marker for `before_request`.
pub struct BeforeRequest;
impl Hook for BeforeRequest {
    type Event = BeforeRequestEvent;
    type Result = Option<BeforeRequestResult>;
    type Handler = BeforeRequestHook;
    const NAME: HookName = HookName::BeforeRequest;

    fn aggregate(
        registry: &HookRegistry,
        event: Self::Event,
        context: Context,
    ) -> BoxFuture<'_, HookRunResult<Self::Result>> {
        Box::pin(registry.before_request(event, context))
    }
}

/// Hook marker for `before_payload`.
pub struct BeforePayload;
impl Hook for BeforePayload {
    type Event = BeforePayloadEvent;
    type Result = Option<BeforePayloadResult>;
    type Handler = BeforePayloadHook;
    const NAME: HookName = HookName::BeforePayload;

    fn aggregate(
        registry: &HookRegistry,
        event: Self::Event,
        context: Context,
    ) -> BoxFuture<'_, HookRunResult<Self::Result>> {
        Box::pin(registry.before_payload(event, context))
    }
}

/// Hook marker for `after_response`.
pub struct AfterResponse;
impl Hook for AfterResponse {
    type Event = AfterResponseEvent;
    type Result = Option<AfterResponseResult>;
    type Handler = AfterResponseHook;
    const NAME: HookName = HookName::AfterResponse;

    fn aggregate(
        registry: &HookRegistry,
        event: Self::Event,
        context: Context,
    ) -> BoxFuture<'_, HookRunResult<Self::Result>> {
        Box::pin(registry.after_response(event, context))
    }
}

/// Hook marker for `before_tool`.
pub struct BeforeTool;
impl Hook for BeforeTool {
    type Event = BeforeToolEvent;
    type Result = Option<BeforeToolResult>;
    type Handler = BeforeToolHook;
    const NAME: HookName = HookName::BeforeTool;

    fn aggregate(
        registry: &HookRegistry,
        event: Self::Event,
        context: Context,
    ) -> BoxFuture<'_, HookRunResult<Self::Result>> {
        Box::pin(registry.before_tool(event, context))
    }
}

/// Hook marker for `after_tool`.
pub struct AfterTool;
impl Hook for AfterTool {
    type Event = AfterToolEvent;
    type Result = Option<AfterToolResult>;
    type Handler = AfterToolHook;
    const NAME: HookName = HookName::AfterTool;

    fn aggregate(
        registry: &HookRegistry,
        event: Self::Event,
        context: Context,
    ) -> BoxFuture<'_, HookRunResult<Self::Result>> {
        Box::pin(registry.after_tool(event, context))
    }
}

/// Hook marker for `before_compaction`.
pub struct BeforeCompaction;
impl Hook for BeforeCompaction {
    type Event = BeforeCompactionEvent;
    type Result = Option<BeforeCompactionResult>;
    type Handler = BeforeCompactionHook;
    const NAME: HookName = HookName::BeforeCompaction;

    fn aggregate(
        registry: &HookRegistry,
        event: Self::Event,
        context: Context,
    ) -> BoxFuture<'_, HookRunResult<Self::Result>> {
        Box::pin(registry.before_compaction(event, context))
    }
}

/// Hook marker for `before_navigation`.
pub struct BeforeNavigation;
impl Hook for BeforeNavigation {
    type Event = BeforeNavigationEvent;
    type Result = Option<BeforeNavigationResult>;
    type Handler = BeforeNavigationHook;
    const NAME: HookName = HookName::BeforeNavigation;

    fn aggregate(
        registry: &HookRegistry,
        event: Self::Event,
        context: Context,
    ) -> BoxFuture<'_, HookRunResult<Self::Result>> {
        Box::pin(registry.before_navigation(event, context))
    }
}

trait ErasedHookHandler: Send + Sync {
    fn invoke(
        &self,
        event: Box<dyn Any + Send + Sync>,
        context: Context,
    ) -> BoxFuture<'static, Result<Box<dyn Any + Send + Sync>, HookError>>;
}

struct TypedHookHandler<E, R, H> {
    handler: H,
    _event: std::marker::PhantomData<fn(E) -> R>,
}

impl<E, R, H> ErasedHookHandler for TypedHookHandler<E, R, H>
where
    E: HookEvent,
    R: Clone + Send + Sync + 'static,
    H: HookHandler<E, R> + Send + Sync + 'static,
{
    fn invoke(
        &self,
        event: Box<dyn Any + Send + Sync>,
        context: Context,
    ) -> BoxFuture<'static, Result<Box<dyn Any + Send + Sync>, HookError>> {
        let event = match event.downcast::<E>() {
            Ok(event) => *event,
            Err(_) => return Box::pin(async { Err(Box::new(HookTypeMismatch) as HookError) }),
        };
        let future = self.handler.call(event, context);
        Box::pin(async move {
            future
                .await
                .map(|result| Box::new(result) as Box<dyn Any + Send + Sync>)
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("hook event type mismatch")]
struct HookTypeMismatch;

struct Registration {
    identity: Arc<()>,
    id: Option<String>,
    handler: Arc<dyn ErasedHookHandler>,
}

struct HookState {
    registrations: BTreeMap<HookName, Vec<Arc<Registration>>>,
    closed: Option<Arc<HarnessFault>>,
}

struct HookRegistryShared {
    state: Mutex<HookState>,
    reporter: HookErrorReporter,
}

impl HookRegistryShared {
    fn lock_state(&self) -> MutexGuard<'_, HookState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn remove(&self, name: HookName, identity: &Arc<()>) {
        let mut state = self.lock_state();
        let remove_bucket = state
            .registrations
            .get_mut(&name)
            .is_some_and(|registrations| {
                registrations.retain(|registration| !Arc::ptr_eq(&registration.identity, identity));
                registrations.is_empty()
            });
        if remove_bucket {
            state.registrations.remove(&name);
        }
    }
}

/// Unsubscribes one hook registration when dropped or explicitly invoked.
pub struct Unsubscribe {
    registry: Weak<HookRegistryShared>,
    name: HookName,
    identity: Arc<()>,
}

impl Unsubscribe {
    /// Removes this registration immediately.
    pub fn unsubscribe(&self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.remove(self.name, &self.identity);
        }
    }
}

impl Drop for Unsubscribe {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

/// Ordered harness hook registry and aggregate runner.
#[derive(Clone)]
pub struct HookRegistry {
    shared: Arc<HookRegistryShared>,
}

impl HookRegistry {
    /// Creates a registry with the required isolated-error reporter.
    pub fn new<F>(reporter: F) -> Self
    where
        F: Fn(HarnessEvent, Context) -> BoxFuture<'static, ()> + Send + Sync + 'static,
    {
        Self {
            shared: Arc::new(HookRegistryShared {
                state: Mutex::new(HookState {
                    registrations: BTreeMap::new(),
                    closed: None,
                }),
                reporter: Arc::new(reporter),
            }),
        }
    }

    /// Adds one handler at the end of the stable registration order.
    ///
    /// # Errors
    ///
    /// Returns [`HarnessError::Closed`] when the registry is closed.
    pub fn on<H: Hook>(
        &self,
        handler: H::Handler,
        id: Option<String>,
    ) -> Result<Unsubscribe, HarnessError> {
        let identity = Arc::new(());
        let erased: Arc<dyn ErasedHookHandler> =
            Arc::new(TypedHookHandler::<H::Event, H::Result, H::Handler> {
                handler,
                _event: std::marker::PhantomData,
            });
        let mut state = self.shared.lock_state();
        if let Some(error) = state.closed.as_ref() {
            return Err(HarnessError::Closed {
                message: error.message.clone(),
            });
        }
        state
            .registrations
            .entry(H::NAME)
            .or_default()
            .push(Arc::new(Registration {
                identity: Arc::clone(&identity),
                id,
                handler: erased,
            }));
        drop(state);
        Ok(Unsubscribe {
            registry: Arc::downgrade(&self.shared),
            name: H::NAME,
            identity,
        })
    }

    /// Returns whether at least one handler is registered for this hook.
    #[must_use]
    pub fn has<H: Hook>(&self) -> bool {
        let state = self.shared.lock_state();
        state
            .registrations
            .get(&H::NAME)
            .is_some_and(|registrations| !registrations.is_empty())
    }

    /// Runs one typed aggregate after synchronously passing its effect gate.
    ///
    /// # Errors
    ///
    /// Returns [`HookRunError::Gate`] when the gate rejects admission or the
    /// registry is closed, and [`HookRunError::Handler`] when a fail-closed
    /// `before_drive` handler fails.
    pub async fn run_with_gate<H: Hook>(
        &self,
        event: H::Event,
        gate: &Gate,
        cx: &Context,
    ) -> HookRunResult<H::Result> {
        let admitted_context = cx.with_cancellation(gate.token().clone());
        let aggregate = gate.admit(|| H::aggregate(self, event, admitted_context))?;
        aggregate.await
    }

    /// Closes the registry.  Existing aggregate snapshots may finish; new
    /// registrations and new aggregate starts receive the close fault.
    pub fn close(&self, error: Arc<HarnessFault>) {
        let mut state = self.shared.lock_state();
        state.closed.get_or_insert(error);
    }
    fn registrations<H: Hook>(&self) -> HookRunResult<Vec<Arc<Registration>>> {
        let state = self.shared.lock_state();
        if let Some(error) = state.closed.as_ref() {
            return Err(HookRunError::Gate(GateRejection::Closed(Arc::clone(error))));
        }
        Ok(state
            .registrations
            .get(&H::NAME)
            .cloned()
            .unwrap_or_default())
    }

    async fn invoke<E, R>(
        &self,
        registration: &Registration,
        event: E,
        context: &Context,
    ) -> Result<R, HookError>
    where
        E: HookEvent,
        R: Clone + Send + Sync + 'static,
    {
        let value = registration
            .handler
            .invoke(Box::new(event), context.clone())
            .await?;
        value
            .downcast::<R>()
            .map(|result| *result)
            .map_err(|_| Box::new(HookTypeMismatch) as HookError)
    }
    fn invoke_with_telemetry<'a, E, R, O>(
        &'a self,
        hook: HookName,
        registration: &'a Registration,
        event: E,
        context: &'a Context,
        outcome: O,
    ) -> BoxFuture<'a, Result<R, HookError>>
    where
        E: HookEvent,
        R: Clone + Send + Sync + 'static,
        O: Fn(&R) -> &'static str + Send + Sync + 'a,
    {
        let mut attributes = SpanAttributes::new();
        attributes.insert(
            "pi.lane.name".to_owned(),
            AttributeValue::Str(event.lane().as_str().to_owned()),
        );
        attributes.insert(
            "pi.operation.id".to_owned(),
            AttributeValue::Str(event.run_id().to_owned()),
        );
        attributes.insert(
            "pi.hook.name".to_owned(),
            AttributeValue::Str(hook.as_str().to_owned()),
        );
        if let Some(id) = registration.id.as_ref() {
            attributes.insert(
                "pi.hook.registration_id".to_owned(),
                AttributeValue::Str(id.clone()),
            );
        }
        let parent = telemetry_context(context);
        let span: Arc<dyn TelemetrySpan> = Arc::from(start_span_contained(
            parent.as_ref(),
            SpanOptions {
                name: "pi.harness.hook".to_owned(),
                attributes,
            },
        ));
        let span_context: Arc<dyn TelemetryContext> = span.clone();
        let handler_context = with_telemetry_context(span_context, context);
        Box::pin(async move {
            match self.invoke(registration, event, &handler_context).await {
                Ok(result) => {
                    let mut terminal = SpanAttributes::new();
                    terminal.insert(
                        "pi.hook.outcome".to_owned(),
                        AttributeValue::Str(outcome(&result).to_owned()),
                    );
                    set_attributes_contained(span.as_ref(), terminal);
                    Ok(result)
                }
                Err(error) => {
                    let mut terminal = SpanAttributes::new();
                    terminal.insert(
                        "pi.hook.outcome".to_owned(),
                        AttributeValue::Str("failed".to_owned()),
                    );
                    set_attributes_contained(span.as_ref(), terminal);
                    set_status_contained(
                        span.as_ref(),
                        SpanStatus::Error {
                            name: Some("hook_error".to_owned()),
                            message: Some(error.to_string()),
                        },
                    );
                    Err(error)
                }
            }
        })
    }

    async fn report<E: HookEvent>(
        &self,
        hook: HookName,
        event: &E,
        context: &Context,
        error: &HookError,
    ) {
        let event = HarnessEvent::lane(
            event.lane().clone(),
            HarnessEventPayload::HandlerError {
                kind: HandlerErrorKind::Hook {
                    hook: hook.as_str().to_owned(),
                },
                error: error.to_string(),
                stack: None,
            },
        );
        (self.shared.reporter)(event, context.clone()).await;
    }

    async fn before_run(
        &self,
        event: BeforeRunEvent,
        context: Context,
    ) -> HookRunResult<Option<BeforeRunResult>> {
        let registrations = self.registrations::<BeforeRun>()?;
        let mut prompt = event.prompt.clone();
        let mut injected = Vec::new();
        for registration in registrations {
            let handler_event = BeforeRunEvent {
                lane: event.lane.clone(),
                run_id: event.run_id.clone(),
                prompt: prompt.clone(),
                resources: event.resources.clone(),
            };
            match self
                .invoke::<_, Option<BeforeRunResult>>(&registration, handler_event, &context)
                .await
            {
                Ok(Some(result)) => {
                    if let Some(messages) = result.messages {
                        prompt.extend(messages.iter().cloned());
                        injected.extend(messages);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.report(HookName::BeforeRun, &event, &context, &error)
                        .await
                }
            }
        }
        Ok((!injected.is_empty()).then_some(BeforeRunResult {
            messages: Some(injected),
        }))
    }

    async fn before_drive(&self, event: BeforeDriveEvent, context: Context) -> HookRunResult<()> {
        let registrations = self.registrations::<BeforeDrive>()?;
        for registration in registrations {
            match self
                .invoke::<_, ()>(&registration, event.clone(), &context)
                .await
            {
                Ok(()) => {}
                Err(error) => {
                    let message = error.to_string();
                    self.report(HookName::BeforeDrive, &event, &context, &error)
                        .await;
                    return Err(HookRunError::Handler(HarnessFault {
                        message,
                        cause: error,
                    }));
                }
            }
        }
        Ok(())
    }

    async fn before_run_end(
        &self,
        event: BeforeRunEndEvent,
        context: Context,
    ) -> HookRunResult<Option<BeforeRunEndResult>> {
        let registrations = self.registrations::<BeforeRunEnd>()?;
        let mut follow_up = None;
        for registration in registrations {
            match self
                .invoke::<_, Option<BeforeRunEndResult>>(&registration, event.clone(), &context)
                .await
            {
                Ok(Some(result)) => {
                    if result.follow_up.is_some() {
                        follow_up = result.follow_up;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.report(HookName::BeforeRunEnd, &event, &context, &error)
                        .await
                }
            }
        }
        Ok(follow_up.map(|follow_up| BeforeRunEndResult {
            follow_up: Some(follow_up),
        }))
    }

    async fn transform_context(
        &self,
        event: TransformContextEvent,
        context: Context,
    ) -> HookRunResult<Option<TransformContextResult>> {
        let registrations = self.registrations::<TransformContext>()?;
        let mut messages = event.messages.clone();
        let mut system_prompt = event.system_prompt.clone();
        for registration in registrations {
            let handler_event = TransformContextEvent {
                lane: event.lane.clone(),
                run_id: event.run_id.clone(),
                messages: messages.clone(),
                system_prompt: system_prompt.clone(),
            };
            match self
                .invoke::<_, Option<TransformContextResult>>(&registration, handler_event, &context)
                .await
            {
                Ok(Some(result)) => {
                    if let Some(next_messages) = result.messages {
                        messages = next_messages;
                    }
                    if let Some(next_system_prompt) = result.system_prompt {
                        system_prompt = next_system_prompt;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.report(HookName::TransformContext, &event, &context, &error)
                        .await
                }
            }
        }
        Ok(Some(TransformContextResult {
            messages: Some(messages),
            system_prompt: Some(system_prompt),
        }))
    }

    async fn before_request(
        &self,
        event: BeforeRequestEvent,
        context: Context,
    ) -> HookRunResult<Option<BeforeRequestResult>> {
        let registrations = self.registrations::<BeforeRequest>()?;
        let original = event.stream_options.clone();
        let mut stream_options = original.clone();
        let mut changed = false;
        for registration in registrations {
            let handler_event = BeforeRequestEvent {
                lane: event.lane.clone(),
                run_id: event.run_id.clone(),
                model: event.model.clone(),
                step: event.step,
                attempt: event.attempt,
                stream_options: stream_options.clone(),
            };
            match self
                .invoke::<_, Option<BeforeRequestResult>>(&registration, handler_event, &context)
                .await
            {
                Ok(Some(result)) => {
                    if let Some(patch) = result.stream_options {
                        stream_options = apply_stream_options_patch(&stream_options, &patch);
                        changed = true;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.report(HookName::BeforeRequest, &event, &context, &error)
                        .await
                }
            }
        }
        Ok(changed.then_some(BeforeRequestResult {
            stream_options: Some(create_stream_options_patch(&original, &stream_options)),
        }))
    }

    async fn before_payload(
        &self,
        event: BeforePayloadEvent,
        context: Context,
    ) -> HookRunResult<Option<BeforePayloadResult>> {
        let registrations = self.registrations::<BeforePayload>()?;
        let mut payload = event.payload.clone();
        for registration in registrations {
            let handler_event = BeforePayloadEvent {
                lane: event.lane.clone(),
                run_id: event.run_id.clone(),
                model: event.model.clone(),
                payload: payload.clone(),
            };
            match self
                .invoke::<_, Option<BeforePayloadResult>>(&registration, handler_event, &context)
                .await
            {
                Ok(Some(result)) => payload = result.payload,
                Ok(None) => {}
                Err(error) => {
                    self.report(HookName::BeforePayload, &event, &context, &error)
                        .await
                }
            }
        }
        Ok(Some(BeforePayloadResult { payload }))
    }

    async fn after_response(
        &self,
        event: AfterResponseEvent,
        context: Context,
    ) -> HookRunResult<Option<AfterResponseResult>> {
        let registrations = self.registrations::<AfterResponse>()?;
        let mut message = event.message.clone();
        for registration in registrations {
            let handler_event = AfterResponseEvent {
                lane: event.lane.clone(),
                run_id: event.run_id.clone(),
                status: event.status,
                headers: event.headers.clone(),
                message: message.clone(),
            };
            match self
                .invoke::<_, Option<AfterResponseResult>>(&registration, handler_event, &context)
                .await
            {
                Ok(Some(result)) => {
                    if let Some(next_message) = result.message {
                        message = next_message;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.report(HookName::AfterResponse, &event, &context, &error)
                        .await
                }
            }
        }
        Ok(Some(AfterResponseResult {
            message: Some(message),
        }))
    }

    async fn before_tool(
        &self,
        event: BeforeToolEvent,
        context: Context,
    ) -> HookRunResult<Option<BeforeToolResult>> {
        let registrations = self.registrations::<BeforeTool>()?;
        let mut args = event.args.clone();
        let mut block = None;
        for registration in registrations {
            let handler_event = BeforeToolEvent {
                lane: event.lane.clone(),
                run_id: event.run_id.clone(),
                tool_call_id: event.tool_call_id.clone(),
                tool_name: event.tool_name.clone(),
                args: args.clone(),
            };
            match self
                .invoke_with_telemetry::<_, Option<BeforeToolResult>, _>(
                    HookName::BeforeTool,
                    &registration,
                    handler_event,
                    &context,
                    |result: &Option<BeforeToolResult>| {
                        if result.as_ref().is_some_and(|value| value.block.is_some()) {
                            "blocked"
                        } else {
                            "completed"
                        }
                    },
                )
                .await
            {
                Ok(Some(result)) => {
                    if let Some(next_args) = result.args {
                        args = next_args;
                    }
                    if result.block.is_some() {
                        block = result.block;
                        break;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.report(HookName::BeforeTool, &event, &context, &error)
                        .await;
                    block = Some(ToolBlock {
                        reason: error.to_string(),
                        terminate: None,
                    });
                    break;
                }
            }
        }
        Ok(Some(BeforeToolResult {
            args: (args != event.args).then_some(args),
            block,
        }))
    }

    async fn after_tool(
        &self,
        event: AfterToolEvent,
        context: Context,
    ) -> HookRunResult<Option<AfterToolResult>> {
        let registrations = self.registrations::<AfterTool>()?;
        let mut current_content = event.content.clone();
        let mut details = event.details.clone();
        let mut is_error = event.is_error;
        let mut usage = event.usage.clone();
        let mut aggregate = AfterToolResult {
            content: None,
            details: None,
            is_error: None,
            usage: None,
            terminate: None,
        };
        let mut changed = false;
        for registration in registrations {
            let handler_event = AfterToolEvent {
                lane: event.lane.clone(),
                run_id: event.run_id.clone(),
                tool_call_id: event.tool_call_id.clone(),
                tool_name: event.tool_name.clone(),
                args: event.args.clone(),
                content: current_content.clone(),
                details: details.clone(),
                is_error,
                usage: usage.clone(),
            };
            match self
                .invoke_with_telemetry::<_, Option<AfterToolResult>, _>(
                    HookName::AfterTool,
                    &registration,
                    handler_event,
                    &context,
                    |_result: &Option<AfterToolResult>| "completed",
                )
                .await
            {
                Ok(Some(result)) => {
                    if result.content.is_some() {
                        current_content = result.content.clone().unwrap_or_default();
                        aggregate.content = result.content;
                        changed = true;
                    }
                    if result.details.is_some() {
                        details = result.details.clone();
                        aggregate.details = result.details;
                        changed = true;
                    }
                    if result.is_error.is_some() {
                        if let Some(next_is_error) = result.is_error {
                            is_error = next_is_error;
                            aggregate.is_error = Some(next_is_error);
                        }
                        changed = true;
                    }
                    if result.usage.is_some() {
                        usage = result.usage.clone();
                        aggregate.usage = result.usage;
                        changed = true;
                    }
                    if result.terminate.is_some() {
                        aggregate.terminate = result.terminate;
                        changed = true;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.report(HookName::AfterTool, &event, &context, &error)
                        .await
                }
            }
        }
        Ok(changed.then_some(aggregate))
    }

    async fn before_compaction(
        &self,
        event: BeforeCompactionEvent,
        context: Context,
    ) -> HookRunResult<Option<BeforeCompactionResult>> {
        let registrations = self.registrations::<BeforeCompaction>()?;
        for registration in registrations {
            match self
                .invoke::<_, Option<BeforeCompactionResult>>(&registration, event.clone(), &context)
                .await
            {
                Ok(Some(result)) => {
                    if result.decline == Some(true) && result.compaction.is_some() {
                        let error: HookError = Box::new(HookMessageError(
                            "before_compaction hook cannot return both decline and compaction"
                                .to_owned(),
                        ));
                        self.report(HookName::BeforeCompaction, &event, &context, &error)
                            .await;
                        continue;
                    }
                    if result.decline == Some(true) || result.compaction.is_some() {
                        return Ok(Some(result));
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.report(HookName::BeforeCompaction, &event, &context, &error)
                        .await
                }
            }
        }
        Ok(None)
    }

    async fn before_navigation(
        &self,
        event: BeforeNavigationEvent,
        context: Context,
    ) -> HookRunResult<Option<BeforeNavigationResult>> {
        let registrations = self.registrations::<BeforeNavigation>()?;
        for registration in registrations {
            match self
                .invoke::<_, Option<BeforeNavigationResult>>(&registration, event.clone(), &context)
                .await
            {
                Ok(Some(result)) => {
                    if result.decline == Some(true) && result.summary.is_some() {
                        let error: HookError = Box::new(HookMessageError(
                            "before_navigation hook cannot return both decline and summary"
                                .to_owned(),
                        ));
                        self.report(HookName::BeforeNavigation, &event, &context, &error)
                            .await;
                        continue;
                    }
                    if result.decline == Some(true) || result.summary.is_some() {
                        return Ok(Some(result));
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    self.report(HookName::BeforeNavigation, &event, &context, &error)
                        .await
                }
            }
        }
        Ok(None)
    }
}

impl std::fmt::Debug for HookRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HookRegistry")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::user_text;
    use std::sync::Mutex;

    type TestResult = Result<(), String>;

    #[derive(Debug, thiserror::Error)]
    #[error("before drive failed")]
    struct TestHandlerError;

    #[tokio::test]
    async fn before_run_preserves_order_and_reports_errors() -> TestResult {
        let expected_first = user_text("first", []);
        let expected_third = user_text("third", []);
        let reports = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
        let report_sink = Arc::clone(&reports);
        let registry = HookRegistry::new(move |event, _context| -> BoxFuture<'static, ()> {
            let report_sink = Arc::clone(&report_sink);
            Box::pin(async move {
                if let Ok(mut events) = report_sink.lock() {
                    events.push(event);
                }
            })
        });

        let seen_prompt_lengths = Arc::new(Mutex::new(Vec::<usize>::new()));
        let first = {
            let seen_prompt_lengths = Arc::clone(&seen_prompt_lengths);
            let message = expected_first.clone();
            let handler: BeforeRunHook =
                Arc::new(move |event: BeforeRunEvent, _context: Context| {
                    let seen_prompt_lengths = Arc::clone(&seen_prompt_lengths);
                    let message = message.clone();
                    Box::pin(async move {
                        if let Ok(mut lengths) = seen_prompt_lengths.lock() {
                            lengths.push(event.prompt.len());
                        }
                        Ok::<_, HookError>(Some(BeforeRunResult {
                            messages: Some(vec![message]),
                        }))
                    })
                });
            registry.on::<BeforeRun>(handler, Some("first".to_owned()))
        };
        let _first = first.map_err(|error| format!("first hook registration failed: {error}"))?;
        let failing: BeforeRunHook = Arc::new(|_event: BeforeRunEvent, _context: Context| {
            Box::pin(async {
                Err::<Option<BeforeRunResult>, _>(HookMessageError("hook failed".to_owned()))
            })
        });
        let _failing = registry
            .on::<BeforeRun>(failing, Some("failing".to_owned()))
            .map_err(|error| format!("failing hook registration failed: {error}"))?;
        let third = {
            let seen_prompt_lengths = Arc::clone(&seen_prompt_lengths);
            let message = expected_third.clone();
            let handler: BeforeRunHook =
                Arc::new(move |event: BeforeRunEvent, _context: Context| {
                    let seen_prompt_lengths = Arc::clone(&seen_prompt_lengths);
                    let message = message.clone();
                    Box::pin(async move {
                        if let Ok(mut lengths) = seen_prompt_lengths.lock() {
                            lengths.push(event.prompt.len());
                        }
                        Ok::<_, HookError>(Some(BeforeRunResult {
                            messages: Some(vec![message]),
                        }))
                    })
                });
            registry.on::<BeforeRun>(handler, Some("third".to_owned()))
        };
        let _third = third.map_err(|error| format!("third hook registration failed: {error}"))?;

        let (gate, _control) = super::super::gate::create_gate();
        let event = BeforeRunEvent {
            lane: LaneName::from("lane"),
            run_id: "run".to_owned(),
            prompt: Vec::new(),
            resources: HarnessResources::default(),
        };
        let result = registry
            .run_with_gate::<BeforeRun>(event, &gate, &Context::background())
            .await;

        let aggregate = result
            .map_err(|error| format!("before_run aggregate failed: {error}"))?
            .ok_or_else(|| "before_run aggregate unexpectedly omitted its result".to_owned())?;
        let messages = aggregate
            .messages
            .ok_or_else(|| "before_run aggregate omitted injected messages".to_owned())?;
        assert_eq!(messages, vec![expected_first, expected_third]);
        assert_eq!(
            seen_prompt_lengths
                .lock()
                .map_or_else(|_| Vec::new(), |lengths| lengths.clone()),
            vec![0, 1]
        );
        assert_eq!(reports.lock().map_or_else(|_| 0, |events| events.len()), 1);
        Ok(())
    }

    #[tokio::test]
    async fn before_drive_handler_failure_preserves_fault_and_open_gate() -> TestResult {
        let registry =
            HookRegistry::new(|_event, _context| -> BoxFuture<'static, ()> { Box::pin(async {}) });
        let handler: BeforeDriveHook = Arc::new(|_event: BeforeDriveEvent, _context: Context| {
            Box::pin(async { Err::<(), _>(TestHandlerError) })
        });
        let _subscription = registry
            .on::<BeforeDrive>(handler, None)
            .map_err(|error| format!("before_drive hook registration failed: {error}"))?;
        let (gate, _control) = super::super::gate::create_gate();
        let event = BeforeDriveEvent {
            lane: LaneName::from("lane"),
            run_id: "run".to_owned(),
            operation: OperationKind::Run,
        };

        let result = registry
            .run_with_gate::<BeforeDrive>(event, &gate, &Context::background())
            .await;
        let fault = match result {
            Err(HookRunError::Handler(fault)) => fault,
            Err(error) => return Err(format!("before_drive returned the wrong error: {error}")),
            Ok(()) => return Err("before_drive unexpectedly succeeded".to_owned()),
        };
        assert_eq!(fault.message, "before drive failed");
        assert_eq!(fault.cause.to_string(), "before drive failed");
        assert!(fault.cause.downcast_ref::<TestHandlerError>().is_some());
        assert!(matches!(gate.admit(|| ()), Ok(())));
        Ok(())
    }
}
