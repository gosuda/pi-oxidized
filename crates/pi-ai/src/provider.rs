//! Provider stream contract and the options that control it.
//!
//! A [`Provider`] turns a [`Model`], [`Context`], and [`StreamOptions`] into a
//! `'static`, self-owned stream of [`AssistantMessageEvent`] values. Only
//! undeliverable stream infrastructure failures surface as
//! [`ProviderError`]; ordinary provider failures, retries, and cancellations
//! are encoded as `AssistantMessageEvent::Error` events.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::types::{
    AssistantMessage, AssistantMessageEvent, CacheRetention, Context, DeferredHandle, ErrorReason,
    Model, StopReason, Transport,
};

pub(crate) mod deferred;

pub use deferred::{CancelDeferredFn, DeferredCallbacks, FetchDeferredFn};

/// A provider implementation that materializes an assistant response as an
/// asynchronous stream of events.
///
/// Implementations must be [`Send`] + [`Sync`] because the provider is
/// typically shared across tasks. The returned stream is `'static` and owns
/// all data required to produce events; callers do not need to keep the
/// original arguments alive once [`Provider::stream`] returns.
///
/// The stream's `Err` variant is reserved for infrastructure failures that
/// make event delivery impossible (for example, a protocol-level crash or an
/// unrecoverable transport error). Normal failure modes — retryable errors,
/// API refusals, user cancellation, malformed provider output — must be
/// delivered as `AssistantMessageEvent::Error` inside an `Ok` stream item.
pub trait Provider: Send + Sync {
    /// Start streaming assistant message events for the given request.
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>>;

    /// Deferred-response callbacks registered by this provider, if any.
    ///
    /// Default: `None` — the provider does not support deferred responses.
    /// Capability is derived from the presence of each callback in the
    /// returned record; there is no separate flag to keep in sync. The record
    /// travels inside the provider object, so provider replacement or
    /// removal drops stale callbacks with it.
    fn deferred(&self) -> Option<&DeferredCallbacks> {
        None
    }

    /// Poll a provider-owned deferred response once without waiting.
    ///
    /// This is a single status check (`wait = 0`), never a fresh generation.
    /// The default dispatches to the registered [`DeferredCallbacks::fetch`]
    /// callback; when absent, the returned stream terminates with the
    /// source's unsupported error event
    /// (`Provider {provider} does not support deferred responses`), matching
    /// how `lazyStream` surfaces a thrown `ModelsError`.
    fn fetch_deferred(
        &self,
        model: &Model,
        handle: DeferredHandle,
        options: StreamOptions,
    ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
        match self
            .deferred()
            .and_then(|callbacks| callbacks.fetch.clone())
        {
            Some(fetch) => fetch(model, handle, options),
            None => deferred::unsupported_fetch_stream(model, None),
        }
    }

    /// Best-effort cancellation of a provider-owned deferred response.
    ///
    /// The default dispatches to the registered [`DeferredCallbacks::cancel`]
    /// callback; when absent it fails with the source's unsupported error
    /// (`Provider {provider} does not support deferred responses`).
    fn cancel_deferred(
        &self,
        model: &Model,
        handle: DeferredHandle,
        options: StreamOptions,
    ) -> BoxFuture<'static, Result<(), ProviderError>> {
        match self
            .deferred()
            .and_then(|callbacks| callbacks.cancel.clone())
        {
            Some(cancel) => cancel(model, handle, options),
            None => deferred::unsupported_cancel_future(model, None),
        }
    }
}

/// An undeliverable stream infrastructure failure.
///
/// This error is returned as the `Err` variant of a provider stream only when
/// the stream machinery itself cannot continue. It intentionally carries a
/// single human-readable message and no HTTP-specific taxonomy, retry
/// classification, or adapter details. Higher-level callers may map this to
/// a terminal `AssistantMessageEvent::Error` event if they need to surface it
/// to consumers.
#[derive(Clone, Debug, thiserror::Error)]
#[error("provider error: {message}")]
pub struct ProviderError {
    message: String,
}

impl ProviderError {
    /// Create a new infrastructure-level provider error from any string-like
    /// value.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The error message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// HTTP response metadata exposed to provider callbacks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderResponse {
    /// HTTP status code returned by the provider.
    pub status: u16,

    /// Normalized response headers.
    pub headers: BTreeMap<String, String>,
}

/// Callback invoked before a request payload is sent.
///
/// The callback mutates `payload` in place to replace it. Adapters must convert
/// a returned error into a terminal [`AssistantMessageEvent::Error`] rather
/// than yielding it through the stream's infrastructure error channel.
pub type OnPayloadFn = Arc<
    dyn for<'a> Fn(&'a mut Value, &'a Model) -> BoxFuture<'a, Result<(), ProviderError>>
        + Send
        + Sync,
>;

/// Callback invoked after a provider response is received and before its body
/// stream is consumed.
///
/// Adapters must convert a returned error into a terminal
/// [`AssistantMessageEvent::Error`] before ending the stream.
pub type OnResponseFn = Arc<
    dyn for<'a> Fn(&'a ProviderResponse, &'a Model) -> BoxFuture<'a, Result<(), ProviderError>>
        + Send
        + Sync,
>;

/// A supported provider-specific stream option name.
///
/// The private constructor keeps supported key spellings in this module. Raw
/// entries in [`StreamOptions::extra`] remain available for unknown options.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamOptionKey(&'static str);

impl StreamOptionKey {
    /// Google Vertex API version.
    pub const API_VERSION: Self = Self("apiVersion");
    /// `Azure OpenAI` API version.
    pub const AZURE_API_VERSION: Self = Self("azureApiVersion");
    /// Explicit `Azure OpenAI` endpoint base URL.
    pub const AZURE_BASE_URL: Self = Self("azureBaseUrl");
    /// `Azure OpenAI` deployment name.
    pub const AZURE_DEPLOYMENT_NAME: Self = Self("azureDeploymentName");
    /// `Azure OpenAI` resource name.
    pub const AZURE_RESOURCE_NAME: Self = Self("azureResourceName");
    /// Pi Messages debug mode.
    pub const DEBUG: Self = Self("debug");
    /// Anthropic adaptive-thinking effort.
    pub const EFFORT: Self = Self("effort");
    /// Bedrock interleaved-thinking mode.
    pub const INTERLEAVED_THINKING: Self = Self("interleavedThinking");
    /// Google Vertex location.
    pub const LOCATION: Self = Self("location");
    /// AWS credential profile.
    pub const PROFILE: Self = Self("profile");
    /// Google Cloud project.
    pub const PROJECT: Self = Self("project");
    /// Mistral prompt mode.
    pub const PROMPT_MODE: Self = Self("promptMode");
    /// Google Cloud quota project.
    pub const QUOTA_PROJECT: Self = Self("quotaProject");
    /// Provider-neutral reasoning level.
    pub const REASONING: Self = Self("reasoning");
    /// `OpenAI`-compatible reasoning effort.
    pub const REASONING_EFFORT: Self = Self("reasoningEffort");
    /// `OpenAI`-compatible reasoning summary mode.
    pub const REASONING_SUMMARY: Self = Self("reasoningSummary");
    /// AWS region.
    pub const REGION: Self = Self("region");
    /// Bedrock request metadata.
    pub const REQUEST_METADATA: Self = Self("requestMetadata");
    /// `OpenAI` service tier.
    pub const SERVICE_TIER: Self = Self("serviceTier");
    /// `OpenAI Codex` response-text verbosity.
    pub const TEXT_VERBOSITY: Self = Self("textVerbosity");
    /// Google thinking configuration.
    pub const THINKING: Self = Self("thinking");
    /// Anthropic scalar thinking-token budget.
    pub const THINKING_BUDGET_TOKENS: Self = Self("thinkingBudgetTokens");
    /// Provider thinking budgets by reasoning level.
    pub const THINKING_BUDGETS: Self = Self("thinkingBudgets");
    /// Provider thinking display mode.
    pub const THINKING_DISPLAY: Self = Self("thinkingDisplay");
    /// Anthropic thinking activation.
    pub const THINKING_ENABLED: Self = Self("thinkingEnabled");
    /// Canonical provider tool selection.
    pub const TOOL_CHOICE: Self = Self("toolChoice");
    /// Google snake-case tool-selection fallback.
    pub const TOOL_CHOICE_SNAKE_CASE: Self = Self("tool_choice");
    /// Deferred fetch long-poll bound in milliseconds (`wait`).
    ///
    /// `DeferredFetchOptions.wait` in the TypeScript source. Default `0`
    /// performs one status check; deferred fetch is always a non-waiting
    /// poll, never a fresh generation.
    pub const WAIT: Self = Self("wait");

    /// Return the serialized option name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// Options that control how a provider streams a response.
///
/// All optional scalar and map fields use `Option`. This type is not `Debug`
/// or serde because it contains async callbacks.
///
/// # Shared implicit defaults (TypeScript parity)
///
/// When a field is `None`, adapters apply these defaults where the TypeScript
/// port does the same:
/// - `cache_retention`: `simple_options::DEFAULT_CACHE_RETENTION` (`short`),
///   with `PI_CACHE_RETENTION=long` env override in adapters.
/// - `max_retry_delay_ms`: `simple_options::DEFAULT_MAX_RETRY_DELAY_MS`
///   (`60000`) in adapters that implement client-side retry (e.g. Codex).
/// - [`Self::timeout_ms`] / [`Self::max_retries`]: left unset. TypeScript only
///   forwards these to the OpenAI/Anthropic SDK when provided; SDK-internal
///   defaults (10 minutes / 2 retries) are not reimplemented by pi-ai HTTP
///   adapters, and the create path uses `maxRetries ?? 0`. Do not invent a
///   crate-level timeout/retry default without a golden transcript requiring it.
#[derive(Clone, Default)]
pub struct StreamOptions {
    /// Sampling temperature.
    pub temperature: Option<f64>,

    /// Maximum tokens to generate.
    pub max_tokens: Option<u64>,

    /// Cancellation token for the request.
    pub signal: Option<CancellationToken>,

    /// Explicit API key for this request.
    pub api_key: Option<String>,

    /// Preferred transport for providers that support multiple transports.
    pub transport: Option<Transport>,

    /// Prompt cache retention preference.
    ///
    /// Default when unset: `short` (see module docs).
    pub cache_retention: Option<CacheRetention>,

    /// Optional session identifier for session-aware providers.
    pub session_id: Option<String>,

    /// Optional payload mutation callback.
    pub on_payload: Option<OnPayloadFn>,

    /// Optional response inspection callback.
    pub on_response: Option<OnResponseFn>,

    /// Custom HTTP headers. A `None` value suppresses a default header with
    /// the same name.
    pub headers: Option<BTreeMap<String, Option<String>>>,

    /// Request timeout in milliseconds.
    ///
    /// No crate-level default; see struct docs.
    pub timeout_ms: Option<u64>,

    /// WebSocket connect handshake timeout in milliseconds.
    pub websocket_connect_timeout_ms: Option<u64>,

    /// Maximum retry attempts for clients that support client-side retries.
    ///
    /// No crate-level default; adapters that pass this to SDKs use `0` when
    /// unset (matching TypeScript `maxRetries ?? 0` on the create call).
    pub max_retries: Option<u32>,

    /// Maximum delay in milliseconds to honor when a server requests a long
    /// retry wait.
    ///
    /// Default when unset in retrying adapters: 60000 (see struct docs).
    pub max_retry_delay_ms: Option<u64>,

    /// Optional metadata to include in API requests.
    pub metadata: Option<Map<String, Value>>,

    /// Provider-scoped environment overrides.
    pub env: Option<BTreeMap<String, String>>,

    /// Extra provider-specific options not covered by the common fields.
    ///
    /// In-tree code uses [`StreamOptionKey`] for supported names. The raw map
    /// preserves arbitrary options supplied by extension providers.
    pub extra: Map<String, Value>,
}

impl StreamOptions {
    /// Read a supported provider-specific option.
    #[must_use]
    pub fn extra_value(&self, key: StreamOptionKey) -> Option<&Value> {
        self.extra.get(key.as_str())
    }

    /// Mutably read a supported provider-specific option.
    #[must_use]
    pub fn extra_value_mut(&mut self, key: StreamOptionKey) -> Option<&mut Value> {
        self.extra.get_mut(key.as_str())
    }

    /// Insert or replace a supported provider-specific option.
    pub fn insert_extra(&mut self, key: StreamOptionKey, value: Value) {
        drop(self.extra.insert(key.as_str().to_owned(), value));
    }

    /// Insert a supported option only when the caller did not supply it.
    pub fn insert_extra_if_absent_with(
        &mut self,
        key: StreamOptionKey,
        value: impl FnOnce() -> Value,
    ) {
        if self.extra.contains_key(key.as_str()) {
            return;
        }
        self.insert_extra(key, value());
    }
}

/// A stream that terminates with a single provider-level error event.
///
/// This mirrors how the TypeScript `lazyStream` surfaces a thrown
/// `ModelsError`: one `AssistantMessageEvent::Error` carrying a synthetic
/// error message stamped with the model's provider metadata.
pub(crate) fn error_event_stream(
    model: &Model,
    message: String,
) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
    let mut error = AssistantMessage::new(
        model.api.clone(),
        model.provider.clone(),
        model.id.clone(),
        now_millis(),
    );
    error.stop_reason = StopReason::Error;
    error.error_message = Some(message);
    Box::pin(futures::stream::once(async move {
        Ok(AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error,
        })
    }))
}

pub(crate) fn now_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "unit tests use contextual failure messages"
)]
#[expect(
    clippy::panic,
    reason = "unit tests assert on unexpected dispatch shapes"
)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;
    use crate::types::{DoneReason, ModelCost, ModelInput};
    use futures::stream::StreamExt;

    /// A provider returning an empty stream is enough to prove trait object
    /// safety and that the returned stream is `'static` and `Send`.
    struct NullProvider;

    impl Provider for NullProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            futures::stream::empty().boxed()
        }
    }

    struct DeferredFixtureProvider {
        deferred: DeferredCallbacks,
    }

    impl Provider for DeferredFixtureProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            futures::stream::empty().boxed()
        }

        fn deferred(&self) -> Option<&DeferredCallbacks> {
            Some(&self.deferred)
        }
    }

    fn test_model() -> Model {
        Model {
            id: "test-model".into(),
            name: "Test model".into(),
            api: "custom-api".into(),
            provider: "custom-provider".into(),
            base_url: "https://example.test".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 32_000,
            max_tokens: 4_096,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    /// What a registered fetch callback observed about one dispatch.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RecordedFetch {
        provider: String,
        handle: String,
        has_signal: bool,
        timeout_ms: Option<u64>,
        has_headers: bool,
    }

    fn deferred_handle(id: &str) -> DeferredHandle {
        DeferredHandle {
            provider: "custom-provider".into(),
            model_id: "test-model".into(),
            api: "custom-api".into(),
            id: id.into(),
            expires_at: None,
            poll_after_ms: None,
            data: None,
        }
    }

    fn recording_fetch(calls: Arc<Mutex<Vec<RecordedFetch>>>) -> FetchDeferredFn {
        Arc::new(
            move |model: &Model,
                  handle: DeferredHandle,
                  options: StreamOptions|
                  -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
                if let Ok(mut calls) = calls.lock() {
                    calls.push(RecordedFetch {
                        provider: model.provider.clone(),
                        handle: handle.id,
                        has_signal: options.signal.is_some(),
                        timeout_ms: options.timeout_ms,
                        has_headers: options.headers.is_some(),
                    });
                }
                let model = model.clone();
                futures::stream::once(async move {
                    let mut message = AssistantMessage::new(
                        model.api.clone(),
                        model.provider.clone(),
                        model.id.clone(),
                        0,
                    );
                    message.stop_reason = StopReason::Stop;
                    Ok(AssistantMessageEvent::Done {
                        reason: DoneReason::Stop,
                        message,
                    })
                })
                .boxed()
            },
        )
    }

    #[test]
    fn provider_trait_is_object_safe_and_stream_is_static() {
        fn assert_object_safe(_: &dyn Provider) {}
        fn assert_static_stream<T: Send + 'static>(_: T) {}

        let provider: Box<dyn Provider> = Box::new(NullProvider);
        assert_object_safe(&*provider);

        // The provider contract promises a 'static, Send stream type.
        assert_static_stream::<BoxStream<'static, Result<AssistantMessageEvent, ProviderError>>>(
            futures::stream::empty().boxed(),
        );
    }

    #[test]
    fn semantic_error_event_is_not_provider_error() {
        /// Inspect a result to show the two failure channels are distinct.
        ///
        /// The `Ok(AssistantMessageEvent::Error { .. })` arm is a compile-time
        /// proof that the semantic error event exists and is distinct from the
        /// `Err(ProviderError)` infrastructure channel.
        fn classify(result: &Result<AssistantMessageEvent, ProviderError>) -> &'static str {
            match result {
                Ok(AssistantMessageEvent::Error { .. }) => "semantic error event",
                Ok(_) => "other event",
                Err(_) => "infrastructure provider error",
            }
        }

        let mut assistant = AssistantMessage::new("custom-api", "custom-provider", "model", 1);
        assistant.stop_reason = StopReason::Error;
        assistant.error_message = Some("request rejected".into());
        let semantic_error: Result<_, ProviderError> = Ok(AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: assistant,
        });
        assert_eq!(classify(&semantic_error), "semantic error event");

        let infrastructure_error = Err(ProviderError::new("stream transport failed"));
        assert_eq!(
            classify(&infrastructure_error),
            "infrastructure provider error"
        );
    }

    #[test]
    fn cloned_stream_options_share_cancellation_token() {
        let mut original = StreamOptions::default();
        let token = CancellationToken::new();
        original.signal = Some(token.clone());
        let cloned = original.clone();

        assert!(
            original
                .signal
                .as_ref()
                .is_some_and(|signal| !signal.is_cancelled())
        );
        assert!(
            cloned
                .signal
                .as_ref()
                .is_some_and(|signal| !signal.is_cancelled())
        );

        token.cancel();

        assert!(
            original
                .signal
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        );
        assert!(
            cloned
                .signal
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        );
    }

    #[test]
    fn callback_signature_compiles_without_executor() {
        let payload_cb: OnPayloadFn = Arc::new(|_payload: &mut Value, _model: &Model| {
            Box::pin(std::future::ready(Ok(()))) as BoxFuture<'_, Result<(), ProviderError>>
        });

        let response_cb: OnResponseFn = Arc::new(|_response: &ProviderResponse, _model: &Model| {
            Box::pin(std::future::ready(Ok(()))) as BoxFuture<'_, Result<(), ProviderError>>
        });

        let options = StreamOptions {
            on_payload: Some(payload_cb),
            on_response: Some(response_cb),
            ..StreamOptions::default()
        };

        assert!(options.on_payload.is_some());
        assert!(options.on_response.is_some());
    }

    #[test]
    fn stream_option_vocabulary_has_stable_wire_names() {
        for (key, wire_name) in [
            (StreamOptionKey::API_VERSION, "apiVersion"),
            (StreamOptionKey::AZURE_API_VERSION, "azureApiVersion"),
            (StreamOptionKey::AZURE_BASE_URL, "azureBaseUrl"),
            (
                StreamOptionKey::AZURE_DEPLOYMENT_NAME,
                "azureDeploymentName",
            ),
            (StreamOptionKey::AZURE_RESOURCE_NAME, "azureResourceName"),
            (StreamOptionKey::DEBUG, "debug"),
            (StreamOptionKey::EFFORT, "effort"),
            (StreamOptionKey::INTERLEAVED_THINKING, "interleavedThinking"),
            (StreamOptionKey::LOCATION, "location"),
            (StreamOptionKey::PROFILE, "profile"),
            (StreamOptionKey::PROJECT, "project"),
            (StreamOptionKey::PROMPT_MODE, "promptMode"),
            (StreamOptionKey::QUOTA_PROJECT, "quotaProject"),
            (StreamOptionKey::REASONING, "reasoning"),
            (StreamOptionKey::REASONING_EFFORT, "reasoningEffort"),
            (StreamOptionKey::REASONING_SUMMARY, "reasoningSummary"),
            (StreamOptionKey::REGION, "region"),
            (StreamOptionKey::REQUEST_METADATA, "requestMetadata"),
            (StreamOptionKey::SERVICE_TIER, "serviceTier"),
            (StreamOptionKey::TEXT_VERBOSITY, "textVerbosity"),
            (StreamOptionKey::THINKING, "thinking"),
            (
                StreamOptionKey::THINKING_BUDGET_TOKENS,
                "thinkingBudgetTokens",
            ),
            (StreamOptionKey::THINKING_BUDGETS, "thinkingBudgets"),
            (StreamOptionKey::THINKING_DISPLAY, "thinkingDisplay"),
            (StreamOptionKey::THINKING_ENABLED, "thinkingEnabled"),
            (StreamOptionKey::TOOL_CHOICE, "toolChoice"),
            (StreamOptionKey::TOOL_CHOICE_SNAKE_CASE, "tool_choice"),
            (StreamOptionKey::WAIT, "wait"),
        ] {
            assert_eq!(key.as_str(), wire_name);
        }

        let mut options = StreamOptions::default();
        options.insert_extra(StreamOptionKey::REASONING, Value::String("high".to_owned()));
        options.insert_extra_if_absent_with(StreamOptionKey::REASONING, || {
            Value::String("ignored".to_owned())
        });
        assert_eq!(
            options.extra_value(StreamOptionKey::REASONING),
            Some(&Value::String("high".to_owned()))
        );

        let value = options.extra_value_mut(StreamOptionKey::REASONING);
        assert!(value.is_some());
        if let Some(value) = value {
            *value = Value::String("low".to_owned());
        }
        assert_eq!(
            options.extra_value(StreamOptionKey::REASONING),
            Some(&Value::String("low".to_owned()))
        );
    }
    #[tokio::test]
    async fn absent_deferred_callbacks_are_source_unsupported() {
        let provider = NullProvider;
        let model = test_model();
        assert!(provider.deferred().is_none());

        let events = provider
            .fetch_deferred(
                &model,
                deferred_handle("handle-1"),
                StreamOptions::default(),
            )
            .collect::<Vec<_>>()
            .await;
        match events.as_slice() {
            [Ok(AssistantMessageEvent::Error { reason, error })] => {
                assert_eq!(*reason, ErrorReason::Error);
                assert_eq!(error.stop_reason, StopReason::Error);
                assert_eq!(error.provider, "custom-provider");
                assert_eq!(error.api, "custom-api");
                assert_eq!(
                    error.error_message.as_deref(),
                    Some("Provider custom-provider does not support deferred responses")
                );
            }
            other => panic!("expected unsupported error event, got {other:?}"),
        }

        let error = provider
            .cancel_deferred(
                &model,
                deferred_handle("handle-1"),
                StreamOptions::default(),
            )
            .await
            .expect_err("absent cancel must fail");
        assert_eq!(
            error.message(),
            "Provider custom-provider does not support deferred responses"
        );
    }

    #[tokio::test]
    async fn registered_fetch_receives_handle_and_request_options() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let provider = DeferredFixtureProvider {
            deferred: DeferredCallbacks {
                fetch: Some(recording_fetch(Arc::clone(&calls))),
                cancel: None,
            },
        };
        let deferred = provider.deferred().expect("fixture registers callbacks");
        assert!(deferred.supports_fetch());
        assert!(!deferred.supports_cancel());

        let signal = CancellationToken::new();
        let mut headers = BTreeMap::new();
        headers.insert("x-test".to_owned(), Some("value".to_owned()));
        let options = StreamOptions {
            signal: Some(signal),
            timeout_ms: Some(42),
            headers: Some(headers),
            ..StreamOptions::default()
        };

        let events = provider
            .fetch_deferred(&test_model(), deferred_handle("handle-7"), options)
            .collect::<Vec<_>>()
            .await;

        assert_eq!(
            calls.lock().map(|calls| calls.clone()).unwrap_or_default(),
            vec![RecordedFetch {
                provider: "custom-provider".to_owned(),
                handle: "handle-7".to_owned(),
                has_signal: true,
                timeout_ms: Some(42),
                has_headers: true,
            }]
        );
        assert!(matches!(
            events.as_slice(),
            [Ok(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message,
            })] if message.provider == "custom-provider" && message.api == "custom-api"
        ));

        let error = provider
            .cancel_deferred(
                &test_model(),
                deferred_handle("handle-7"),
                StreamOptions::default(),
            )
            .await
            .expect_err("fetch-only registration must not imply cancel");
        assert_eq!(
            error.message(),
            "Provider custom-provider does not support deferred responses"
        );
    }
}
