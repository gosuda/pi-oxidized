//! Native provider streaming for the durable harness.
//!
//! This module owns the ephemeral request adapter.  Persisted options remain
//! the data-only [`crate::session::configuration::HarnessStreamOptions`]
//! record; callbacks, providers, gates, and observers are intentionally kept
//! out of that record.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use futures::stream::StreamExt;
use pi_ai::provider::{OnPayloadFn, OnResponseFn, ProviderResponse};
use pi_ai::{AssistantMessage, AssistantMessageEvent, Model, ModelThinkingLevel, ProviderError};
use serde_json::Value;

use crate::context::Context;
use crate::message::AgentMessage;
use crate::session::configuration::HarnessStreamOptions;
use crate::session::{SessionError, SettledAssistantMessage};

use super::api::HarnessModels;
use super::gate::{Gate, GateRejection};
use super::result::{HarnessError, HarnessFault};

/// A request context before conversion to provider messages.
#[derive(Clone, Debug)]
pub struct HarnessRequestContext {
    /// Transcript messages selected for this request.
    pub messages: Vec<AgentMessage>,
    /// System prompt selected for this request.
    pub system_prompt: String,
}

/// Optional context transformation performed before provider-message mapping.
pub type TransformRequestContext = Arc<
    dyn Fn(
            HarnessRequestContext,
            Context,
        ) -> BoxFuture<'static, Result<HarnessRequestContext, GateRejection>>
        + Send
        + Sync,
>;

/// Optional post-response transformation hook.
pub type HarnessAfterResponse = Arc<
    dyn Fn(
            SettledAssistantMessage,
            AssistantResponseMetadata,
            Context,
        ) -> BoxFuture<'static, Result<SettledAssistantMessage, GateRejection>>
        + Send
        + Sync,
>;

/// Configuration for one already-admitted assistant stream.
pub struct HarnessAssistantStreamConfig {
    /// Shared product model runtime used for provider streaming.
    pub models: Arc<dyn HarnessModels>,
    /// Captured model snapshot for this request.
    pub model: pi_ai::Model,
    /// Lane-scoped session id passed to the native provider.
    pub session_id: String,
    /// System instruction sent to the provider.
    pub system_prompt: String,
    /// Provider-facing tools, including constrained-sampling metadata.
    pub tools: Vec<pi_ai::Tool>,
    /// Requested model thinking level.
    pub thinking_level: ModelThinkingLevel,
    /// Persisted options captured for this request.
    pub stream_options: HarnessStreamOptions,
    /// Optional hook that changes messages/system prompt before conversion.
    pub transform_context: Option<TransformRequestContext>,
    /// Required transcript-to-provider message conversion.
    pub to_provider_messages: ToProviderMessages,
    /// Existing provider payload callback to preserve while adapting the request.
    pub on_payload: Option<OnPayloadFn>,
    /// Existing provider response callback to preserve while adapting metadata.
    pub on_response: Option<OnResponseFn>,
    /// Optional post-response hook.
    pub after_response: Option<HarnessAfterResponse>,
    /// Process-local observer for stream lifecycle events.
    pub observer: Arc<dyn AssistantStreamObserver>,
}
/// Configuration for polling one provider-owned deferred response.
///
/// Deferred polling reuses the request stored by the provider-side handle, so
/// it does not need transcript messages, a system prompt, tools, or a context
/// transformation. These fields are limited to polling options and lifecycle
/// callbacks.
pub(crate) struct HarnessDeferredStreamConfig {
    /// Shared product model runtime used for provider polling.
    pub models: Arc<dyn HarnessModels>,
    /// Captured model snapshot for this poll.
    pub model: pi_ai::Model,
    /// Lane-scoped session id passed to the native provider.
    pub session_id: String,
    /// Requested model thinking level.
    pub thinking_level: ModelThinkingLevel,
    /// Persisted options captured for this poll.
    pub stream_options: HarnessStreamOptions,
    /// Existing provider payload callback.
    pub on_payload: Option<OnPayloadFn>,
    /// Existing provider response callback.
    pub on_response: Option<OnResponseFn>,
    /// Optional post-response hook.
    pub after_response: Option<HarnessAfterResponse>,
    /// Process-local observer for stream lifecycle events.
    pub observer: Arc<dyn AssistantStreamObserver>,
}

/// Convert owned transcript messages to provider messages.
pub type ToProviderMessages = Arc<
    dyn Fn(
            Vec<AgentMessage>,
            Context,
        ) -> BoxFuture<'static, Result<Vec<pi_ai::Message>, HarnessError>>
        + Send
        + Sync,
>;

/// HTTP metadata captured before the provider response body is consumed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AssistantResponseMetadata {
    /// Response status code, when the provider callback ran.
    pub status: Option<u16>,
    /// Normalized response headers, when the provider callback ran.
    pub headers: Option<BTreeMap<String, String>>,
}

/// Lifecycle observer for one assistant stream.
pub trait AssistantStreamObserver: Send + Sync {
    /// Observe the first assistant snapshot.
    fn start<'a>(
        &'a self,
        message: &'a AssistantMessage,
        event: &'a AssistantMessageEvent,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessFault>>;
    /// Observe one non-terminal assistant event.
    fn update<'a>(
        &'a self,
        message: &'a AssistantMessage,
        event: &'a AssistantMessageEvent,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessFault>>;
    /// Observe the final settled assistant message.
    fn end<'a>(
        &'a self,
        message: &'a SettledAssistantMessage,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<(), HarnessFault>>;
}

/// Failure classes at the native assistant-stream boundary.
#[derive(Debug, thiserror::Error)]
pub enum HarnessStreamError {
    /// Admission or post-response gate rejection.
    #[error(transparent)]
    Gate(#[from] GateRejection),
    /// Observer or stream-protocol infrastructure fault.
    #[error(transparent)]
    Fault(#[from] HarnessFault),
    /// Undeliverable provider stream infrastructure failure.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// Expected request-preparation failure.
    #[error(transparent)]
    Preparation(#[from] HarnessError),
}

/// A request-local metadata cell shared with the native `on_response` callback.
type ResponseMetadataCell = Arc<Mutex<AssistantResponseMetadata>>;

/// Stream one assistant response without mutating the caller's messages.
///
/// Semantic provider failures arrive as `AssistantMessageEvent::Error` and
/// still settle the response.  An infrastructure `ProviderError`, a missing
/// terminal event, or an invalid event ordering is returned as a typed failure;
/// no successful fallback message is synthesized.
///
/// # Errors
///
/// Returns [`HarnessStreamError::Gate`] for admission or post-response gate
/// rejection, [`HarnessStreamError::Fault`] for observer or protocol faults,
/// [`HarnessStreamError::Provider`] for an undeliverable provider stream, and
/// [`HarnessStreamError::Preparation`] for request-preparation failures.
pub async fn stream_harness_assistant(
    messages: &[AgentMessage],
    config: &HarnessAssistantStreamConfig,
    gate: &Gate,
    cx: &Context,
) -> Result<SettledAssistantMessage, HarnessStreamError> {
    let admitted_context = cx.with_cancellation(gate.token().clone());

    let mut request_context = HarnessRequestContext {
        messages: messages.to_vec(),
        system_prompt: config.system_prompt.clone(),
    };
    if let Some(transform) = &config.transform_context {
        let transformed = gate.admit(|| transform(request_context, admitted_context.clone()))?;
        request_context = transformed.await?;
    }

    let HarnessRequestContext {
        messages,
        system_prompt,
    } = request_context;
    let provider_messages = gate
        .admit(|| (config.to_provider_messages)(messages, admitted_context.clone()))?
        .await?;
    let provider_context = pi_ai::Context {
        system_prompt: Some(system_prompt),
        messages: provider_messages,
        tools: Some(config.tools.clone()),
    };

    let (stream, metadata) = gate.admit(|| {
        let metadata = Arc::new(Mutex::new(AssistantResponseMetadata::default()));
        let on_response = response_callback(Arc::clone(&metadata), config.on_response.clone());
        let native_options = native_stream_options(
            &config.stream_options,
            config.thinking_level,
            config.session_id.clone(),
            admitted_context.token().cloned(),
            config.on_payload.clone(),
            on_response,
        );
        let stream = config
            .models
            .stream(&config.model, provider_context, native_options);
        (stream, metadata)
    })?;
    consume_stream(
        stream,
        config.observer.as_ref(),
        config.after_response.as_ref(),
        gate,
        &admitted_context,
        metadata,
    )
    .await
}
/// Poll one already-admitted deferred response exactly once.
///
/// Deferred polling reuses the provider-owned request handle and shares option
/// construction, gate admission, and observer lifecycle with a normal
/// assistant stream. It dispatches `fetch_deferred` with `wait = 0` and never
/// starts a fresh generation or rebuilds the request context.
pub(crate) async fn stream_harness_deferred(
    config: &HarnessDeferredStreamConfig,
    handle: pi_ai::DeferredHandle,
    gate: &Gate,
    cx: &Context,
) -> Result<SettledAssistantMessage, HarnessStreamError> {
    let admitted_context = cx.with_cancellation(gate.token().clone());

    let (stream, metadata) = gate.admit(|| {
        let metadata = Arc::new(Mutex::new(AssistantResponseMetadata::default()));
        let on_response = response_callback(Arc::clone(&metadata), config.on_response.clone());
        let mut native_options = native_stream_options(
            &config.stream_options,
            config.thinking_level,
            config.session_id.clone(),
            admitted_context.token().cloned(),
            config.on_payload.clone(),
            on_response,
        );
        native_options
            .insert_extra_if_absent_with(pi_ai::StreamOptionKey::WAIT, || Value::from(0_u64));
        let stream = config
            .models
            .fetch_deferred(&config.model, handle, native_options);
        (stream, metadata)
    })?;
    consume_stream(
        stream,
        config.observer.as_ref(),
        config.after_response.as_ref(),
        gate,
        &admitted_context,
        metadata,
    )
    .await
}

/// Consume provider events and run the terminal lifecycle in order.
async fn consume_stream(
    mut stream: futures::stream::BoxStream<'static, Result<AssistantMessageEvent, ProviderError>>,
    observer: &dyn AssistantStreamObserver,
    after_response: Option<&HarnessAfterResponse>,
    gate: &Gate,
    cx: &Context,
    metadata: ResponseMetadataCell,
) -> Result<SettledAssistantMessage, HarnessStreamError> {
    let mut started = false;
    let mut terminal: Option<AssistantMessage> = None;

    while let Some(item) = stream.next().await {
        let event = item?;
        if terminal.is_some() {
            return Err(protocol_fault(
                "assistant message stream emitted an event after its terminal event",
            )
            .into());
        }

        match &event {
            AssistantMessageEvent::Start { partial } => {
                if started {
                    return Err(protocol_fault(
                        "assistant message stream emitted more than one start event",
                    )
                    .into());
                }
                started = true;
                gate.admit(|| observer.start(partial.as_ref(), &event, cx))?
                    .await?;
            }
            AssistantMessageEvent::Done { message, .. } => {
                if !started {
                    return Err(protocol_fault(
                        "assistant message stream emitted done before start",
                    )
                    .into());
                }
                terminal = Some(message.clone());
            }
            AssistantMessageEvent::Error { error, .. } => {
                // A provider may fail before it can emit a start snapshot.  The
                // semantic error message is still the authoritative terminal.
                terminal = Some(error.clone());
            }
            _ => {
                let Some(partial) = event_partial(&event) else {
                    return Err(protocol_fault(
                        "assistant message stream emitted an unknown non-terminal event",
                    )
                    .into());
                };
                if !started {
                    return Err(protocol_fault(format!(
                        "assistant message stream emitted {} before start",
                        event_name(&event),
                    ))
                    .into());
                }
                gate.admit(|| observer.update(partial, &event, cx))?.await?;
            }
        }
    }

    let terminal_message = terminal.ok_or_else(|| {
        HarnessStreamError::Fault(protocol_fault(
            "assistant message stream ended without a terminal event",
        ))
    })?;
    let settled = SettledAssistantMessage::new(terminal_message)
        .map_err(|error| protocol_fault(error.to_string()))?;
    let final_message = apply_after_response(settled, after_response, gate, cx, metadata).await?;
    observe_end(observer, gate, &final_message, cx).await?;
    Ok(final_message)
}

/// Deliver the terminal snapshot even when cancellation closes the gate.
async fn observe_end(
    observer: &dyn AssistantStreamObserver,
    gate: &Gate,
    message: &SettledAssistantMessage,
    cx: &Context,
) -> Result<(), HarnessStreamError> {
    let end = gate.admit(|| observer.end(message, cx));
    match end {
        Ok(future) => future.await?,
        Err(GateRejection::Aborted(abort)) => {
            abort.wait().await;
            observer.end(message, cx).await?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

async fn apply_after_response(
    settled: SettledAssistantMessage,
    after_response: Option<&HarnessAfterResponse>,
    gate: &Gate,
    cx: &Context,
    metadata: ResponseMetadataCell,
) -> Result<SettledAssistantMessage, HarnessStreamError> {
    let Some(after_response) = after_response else {
        return Ok(settled);
    };
    let original = settled.clone();
    let response_metadata = metadata_snapshot(&metadata);
    let future = gate.admit(|| after_response(settled, response_metadata, cx.clone()));
    let future = match future {
        Ok(future) => future,
        Err(GateRejection::Aborted(abort)) => {
            abort.wait().await;
            return Ok(original);
        }
        Err(error) => return Err(error.into()),
    };
    match future.await {
        Ok(message) => Ok(message),
        Err(GateRejection::Aborted(abort)) => {
            abort.wait().await;
            Ok(original)
        }
        Err(error) => Err(error.into()),
    }
}

/// Build native options while preserving absence and explicit empty maps.
pub(crate) fn native_stream_options(
    options: &HarnessStreamOptions,
    thinking_level: ModelThinkingLevel,
    session_id: String,
    signal: Option<tokio_util::sync::CancellationToken>,
    on_payload: Option<OnPayloadFn>,
    on_response: OnResponseFn,
) -> pi_ai::StreamOptions {
    let mut native = pi_ai::StreamOptions {
        signal,
        transport: options.transport,
        cache_retention: options.cache_retention,
        session_id: Some(session_id),
        on_payload,
        on_response: Some(on_response),
        headers: options.headers.as_ref().map(|headers| {
            headers
                .iter()
                .map(|(key, value)| (key.clone(), Some(value.clone())))
                .collect()
        }),
        timeout_ms: options.timeout_ms,
        max_retries: options.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        metadata: options.metadata.as_ref().map(|metadata| {
            metadata
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        }),
        ..pi_ai::StreamOptions::default()
    };

    // Native pi-ai uses a provider-neutral reasoning extra.  Do not overwrite
    // an explicit value inserted by a provider-local callback.
    if let Some(reasoning) = thinking_level_name(thinking_level) {
        native.insert_extra_if_absent_with(pi_ai::StreamOptionKey::REASONING, || {
            Value::String(reasoning.to_owned())
        });
    }
    native
}

/// Capture response metadata and then preserve an existing callback's effect.
fn response_callback(
    metadata: ResponseMetadataCell,
    existing: Option<OnResponseFn>,
) -> OnResponseFn {
    Arc::new(move |response: &ProviderResponse, model: &Model| {
        let snapshot = AssistantResponseMetadata {
            status: Some(response.status),
            headers: Some(response.headers.clone()),
        };
        *metadata
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
        let existing = existing.clone();
        Box::pin(async move {
            if let Some(callback) = existing {
                callback(response, model).await
            } else {
                Ok(())
            }
        }) as BoxFuture<'_, Result<(), ProviderError>>
    })
}

fn metadata_snapshot(metadata: &ResponseMetadataCell) -> AssistantResponseMetadata {
    metadata
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn event_partial(event: &AssistantMessageEvent) -> Option<&AssistantMessage> {
    match event {
        AssistantMessageEvent::TextStart { partial, .. }
        | AssistantMessageEvent::TextDelta { partial, .. }
        | AssistantMessageEvent::TextEnd { partial, .. }
        | AssistantMessageEvent::ThinkingStart { partial, .. }
        | AssistantMessageEvent::ThinkingDelta { partial, .. }
        | AssistantMessageEvent::ThinkingEnd { partial, .. }
        | AssistantMessageEvent::ToolCallStart { partial, .. }
        | AssistantMessageEvent::ToolCallDelta { partial, .. }
        | AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial.as_ref()),
        AssistantMessageEvent::Start { .. }
        | AssistantMessageEvent::Done { .. }
        | AssistantMessageEvent::Error { .. } => None,
    }
}

fn event_name(event: &AssistantMessageEvent) -> &'static str {
    match event {
        AssistantMessageEvent::Start { .. } => "start",
        AssistantMessageEvent::TextStart { .. } => "text_start",
        AssistantMessageEvent::TextDelta { .. } => "text_delta",
        AssistantMessageEvent::TextEnd { .. } => "text_end",
        AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
        AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
        AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
        AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
        AssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
        AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
        AssistantMessageEvent::Done { .. } => "done",
        AssistantMessageEvent::Error { .. } => "error",
    }
}

fn thinking_level_name(level: ModelThinkingLevel) -> Option<&'static str> {
    match level {
        ModelThinkingLevel::Off => None,
        ModelThinkingLevel::Minimal => Some("minimal"),
        ModelThinkingLevel::Low => Some("low"),
        ModelThinkingLevel::Medium => Some("medium"),
        ModelThinkingLevel::High => Some("high"),
        ModelThinkingLevel::Xhigh => Some("xhigh"),
        ModelThinkingLevel::Max => Some("max"),
    }
}

fn protocol_fault(message: impl Into<String>) -> HarnessFault {
    let message = message.into();
    HarnessFault {
        message: message.clone(),
        cause: Box::new(StreamProtocolError(message)),
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct StreamProtocolError(String);

/// Apply one per-request patch to canonical persisted options.
#[must_use]
pub fn apply_stream_options_patch(
    base: &HarnessStreamOptions,
    patch: &HarnessStreamOptionsPatch,
) -> HarnessStreamOptions {
    let mut next = base.clone();
    if let Some(value) = &patch.transport {
        next.transport.clone_from(value);
    }
    if let Some(value) = &patch.timeout_ms {
        next.timeout_ms.clone_from(value);
    }
    if let Some(value) = &patch.max_retries {
        next.max_retries.clone_from(value);
    }
    if let Some(value) = &patch.max_retry_delay_ms {
        next.max_retry_delay_ms.clone_from(value);
    }
    if let Some(value) = &patch.cache_retention {
        next.cache_retention.clone_from(value);
    }
    if let Some(value) = &patch.deferred {
        next.deferred.clone_from(value);
    }
    apply_map_patch(&mut next.headers, &patch.headers);
    apply_map_patch(&mut next.metadata, &patch.metadata);
    next
}

/// Return only fields that changed between two canonical option snapshots.
#[must_use]
pub fn create_stream_options_patch(
    before: &HarnessStreamOptions,
    after: &HarnessStreamOptions,
) -> HarnessStreamOptionsPatch {
    HarnessStreamOptionsPatch {
        transport: (before.transport != after.transport).then_some(after.transport),
        timeout_ms: (before.timeout_ms != after.timeout_ms).then_some(after.timeout_ms),
        max_retries: (before.max_retries != after.max_retries).then_some(after.max_retries),
        max_retry_delay_ms: (before.max_retry_delay_ms != after.max_retry_delay_ms)
            .then_some(after.max_retry_delay_ms),
        headers: diff_map(before.headers.as_ref(), after.headers.as_ref()),
        metadata: diff_map(before.metadata.as_ref(), after.metadata.as_ref()),
        cache_retention: (before.cache_retention != after.cache_retention)
            .then_some(after.cache_retention),
        deferred: (before.deferred != after.deferred).then_some(after.deferred.clone()),
    }
}

/// Per-request stream option patch.  `None` inside a scalar option deletes it;
/// map merge values of `None` delete one key, while `Clear` removes the map.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HarnessStreamOptionsPatch {
    /// Transport patch.
    pub transport: Option<Option<pi_ai::Transport>>,
    /// Timeout patch.
    pub timeout_ms: Option<Option<u64>>,
    /// Native provider retry count patch.
    pub max_retries: Option<Option<u32>>,
    /// Native provider retry-delay cap patch.
    pub max_retry_delay_ms: Option<Option<u64>>,
    /// Header map patch.
    pub headers: MapPatch<String>,
    /// Metadata map patch.
    pub metadata: MapPatch<Value>,
    /// Cache-retention patch.
    pub cache_retention: Option<Option<pi_ai::CacheRetention>>,
    /// Deferred-request patch.
    pub deferred: Option<Option<crate::session::configuration::DeferredRequest>>,
}

/// Patch operation for one map-valued option.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum MapPatch<V> {
    /// Leave the map unchanged.
    #[default]
    Unchanged,
    /// Remove the whole map (absence, not an explicit empty map).
    Clear,
    /// Merge values; `None` removes one key.
    Merge(BTreeMap<String, Option<V>>),
}

fn apply_map_patch<V: Clone>(slot: &mut Option<BTreeMap<String, V>>, patch: &MapPatch<V>) {
    match patch {
        MapPatch::Unchanged => {}
        MapPatch::Clear => *slot = None,
        MapPatch::Merge(changes) => {
            if slot.is_none() && !changes.is_empty() && !changes.values().any(Option::is_some) {
                return;
            }
            let map = slot.get_or_insert_with(BTreeMap::new);
            for (key, value) in changes {
                if let Some(value) = value {
                    map.insert(key.clone(), value.clone());
                } else {
                    map.remove(key);
                }
            }
        }
    }
}

fn diff_map<V: Clone + PartialEq>(
    before: Option<&BTreeMap<String, V>>,
    after: Option<&BTreeMap<String, V>>,
) -> MapPatch<V> {
    match (before, after) {
        (None, None) => MapPatch::Unchanged,
        (Some(_), None) => MapPatch::Clear,
        (None, Some(after)) => MapPatch::Merge(
            after
                .iter()
                .map(|(key, value)| (key.clone(), Some(value.clone())))
                .collect(),
        ),
        (Some(before), Some(after)) => {
            let mut changes = BTreeMap::new();
            for key in before.keys() {
                if !after.contains_key(key) {
                    changes.insert(key.clone(), None);
                }
            }
            for (key, value) in after {
                if before.get(key) != Some(value) {
                    changes.insert(key.clone(), Some(value.clone()));
                }
            }
            if changes.is_empty() {
                MapPatch::Unchanged
            } else {
                MapPatch::Merge(changes)
            }
        }
    }
}

/// Reduce persisted progress frames into an owned partial assistant message.
///
/// The result intentionally permits `StopReason::Pending`: frame records do
/// not contain terminal provider metadata.  The runtime combines this partial
/// with its durable terminal record before constructing a settled message.
///
/// # Errors
///
/// Returns [`SessionError::Invariant`] when the frames cannot be reduced or
/// contain no start frame.
pub fn reduce_persisted_frames(
    frames: &[pi_ai::AssistantMessageFrame],
) -> Result<AssistantMessage, SessionError> {
    let reduced = pi_ai::reduce_assistant_message_frames(frames.iter())
        .map_err(|error| SessionError::Invariant(error.to_string()))?;
    reduced.ok_or_else(|| {
        SessionError::Invariant("assistant message frames contain no start frame".to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn options() -> HarnessStreamOptions {
        HarnessStreamOptions {
            headers: Some(BTreeMap::from([("x-one".to_owned(), "1".to_owned())])),
            metadata: Some(BTreeMap::from([("a".to_owned(), json!(1))])),
            ..HarnessStreamOptions::default()
        }
    }

    #[test]
    fn frame_reduction_preserves_pending_without_inventing_terminal_state()
    -> Result<(), SessionError> {
        let mut partial = AssistantMessage::new("api", "provider", "model", 1);
        partial.stop_reason = pi_ai::StopReason::Pending;
        let frames = [pi_ai::AssistantMessageFrame::Start {
            partial: Box::new(partial),
        }];

        let reduced = reduce_persisted_frames(&frames)?;
        assert_eq!(reduced.stop_reason, pi_ai::StopReason::Pending);
        Ok(())
    }

    #[test]
    fn map_patch_distinguishes_delete_merge_and_clear() {
        let patch = HarnessStreamOptionsPatch {
            headers: MapPatch::Merge(BTreeMap::from([
                ("x-one".to_owned(), None),
                ("x-two".to_owned(), Some("2".to_owned())),
            ])),
            ..HarnessStreamOptionsPatch::default()
        };
        let merged = apply_stream_options_patch(&options(), &patch);
        assert_eq!(
            merged.headers,
            Some(BTreeMap::from([(String::from("x-two"), String::from("2"))]))
        );

        let cleared = apply_stream_options_patch(
            &merged,
            &HarnessStreamOptionsPatch {
                headers: MapPatch::Clear,
                ..HarnessStreamOptionsPatch::default()
            },
        );
        assert_eq!(cleared.headers, None);
    }

    #[test]
    fn deleting_from_an_absent_map_preserves_absence() {
        let patch = HarnessStreamOptionsPatch {
            headers: MapPatch::Merge(BTreeMap::from([("missing".to_owned(), None)])),
            ..HarnessStreamOptionsPatch::default()
        };

        let result = apply_stream_options_patch(&HarnessStreamOptions::default(), &patch);
        assert_eq!(result.headers, None);
    }

    #[test]
    fn diff_round_trips_absent_and_explicit_empty_maps() {
        let before = HarnessStreamOptions::default();
        let after = HarnessStreamOptions {
            headers: Some(BTreeMap::new()),
            metadata: Some(BTreeMap::new()),
            ..HarnessStreamOptions::default()
        };
        let patch = create_stream_options_patch(&before, &after);
        assert_eq!(apply_stream_options_patch(&before, &patch), after);
    }

    #[test]
    fn deferred_false_and_empty_options_are_not_collapsed() {
        use crate::session::configuration::{DeferredRequest, DeferredWindow};

        let before = HarnessStreamOptions::default();
        let after = HarnessStreamOptions {
            deferred: Some(DeferredRequest::Flag(false)),
            ..HarnessStreamOptions::default()
        };
        assert_eq!(
            apply_stream_options_patch(&before, &create_stream_options_patch(&before, &after)),
            after
        );

        let object = HarnessStreamOptions {
            deferred: Some(DeferredRequest::Options { window: None }),
            ..HarnessStreamOptions::default()
        };
        assert_eq!(
            apply_stream_options_patch(&before, &create_stream_options_patch(&before, &object)),
            object
        );
        let windowed = HarnessStreamOptions {
            deferred: Some(DeferredRequest::Options {
                window: Some(DeferredWindow::FifteenMinutes),
            }),
            ..HarnessStreamOptions::default()
        };
        assert_eq!(
            apply_stream_options_patch(&object, &create_stream_options_patch(&object, &windowed)),
            windowed
        );
    }
}
