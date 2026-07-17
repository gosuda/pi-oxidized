//! Provider stream contract and the options that control it.
//!
//! A [`Provider`] turns a [`Model`], [`Context`], and [`StreamOptions`] into a
//! `'static`, self-owned stream of [`AssistantMessageEvent`] values. Only
//! undeliverable stream infrastructure failures surface as
//! [`ProviderError`]; ordinary provider failures, retries, and cancellations
//! are encoded as `AssistantMessageEvent::Error` events.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::types::{AssistantMessageEvent, CacheRetention, Context, Model, Transport};

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

/// Options that control how a provider streams a response.
///
/// All optional scalar and map fields use `Option`. This type is not `Debug`
/// or serde because it contains async callbacks.
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
    pub timeout_ms: Option<u64>,

    /// WebSocket connect handshake timeout in milliseconds.
    pub websocket_connect_timeout_ms: Option<u64>,

    /// Maximum retry attempts for clients that support client-side retries.
    pub max_retries: Option<u32>,

    /// Maximum delay in milliseconds to honor when a server requests a long
    /// retry wait.
    pub max_retry_delay_ms: Option<u64>,

    /// Optional metadata to include in API requests.
    pub metadata: Option<Map<String, Value>>,

    /// Provider-scoped environment overrides.
    pub env: Option<BTreeMap<String, String>>,

    /// Extra provider-specific options not covered by the common fields.
    pub extra: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantMessage, ErrorReason, StopReason};
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
}
