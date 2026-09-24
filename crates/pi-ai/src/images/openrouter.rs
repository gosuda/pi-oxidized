//! `OpenRouter` image-generation adapter.
//!
//! Ports `packages/ai/src/api/openrouter-images.ts` (generation over the
//! OpenAI-compatible Chat Completions shape with image modalities) and the
//! `providers/openrouter-images.ts` provider factory. The HTTP semantics the
//! frozen adapter inherits from `openai@6.40.0` — status error message
//! composition, connection/timeout messages, `x-should-retry` classification,
//! and `retry-after` handling per `utils/provider-retry.ts` — are reproduced
//! against the vendored SDK source in
//! `.references/pi-2.0/node_modules/openai`.
//!
//! The frozen lazy-import wrapper (`providers/images/register-builtins.ts`)
//! turns dynamic-module load failures into error results; native dispatch is
//! statically linked, so no equivalent failure mode exists.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::api_registry::ImagesApiDispatch;
use super::provider::{CreateImagesProviderOptions, ImagesProvider, create_images_provider};
use super::types::{
    AssistantImages, ImagesContext, ImagesInputContent, ImagesModel, ImagesOptions,
    ImagesOutputContent, ImagesStopReason,
};
use crate::auth::default_provider_auth;
use crate::provider::{ProviderResponse, now_millis};
use crate::providers::transport::{HttpTransport, TransportError};
use crate::types::{ImageContent, ModelCostRates, TextContent, Usage, UsageCost};

/// Shared HTTP client standing in for the frozen adapter's per-call SDK
/// client over `globalThis.fetch`.
static IMAGE_HTTP_CLIENT: LazyLock<Client> = LazyLock::new(Client::new);

/// Failure classes the adapter reports, with the frozen SDK message shapes.
#[derive(Debug)]
enum HttpFailure {
    /// Connection-level failure (DNS, connect, send, body read, JSON parse).
    Connection {
        /// The failure was a timeout, not an outright connection error.
        timed_out: bool,
    },
    /// Non-2xx response.
    Http {
        /// Response status.
        status: u16,
        /// Response headers, kept for retry classification and delay reads.
        headers: HeaderMap,
        /// Parsed body when JSON and truthy in the JavaScript sense.
        parsed: Option<Value>,
        /// `openai@6.40.0` `APIError.makeMessage` result.
        message: String,
    },
    /// Plain failure with no HTTP identity (missing key, callback error,
    /// retry-delay cap).
    Message(String),
}

impl HttpFailure {
    /// The error message `formatProviderError` surfaces for this failure.
    fn message(&self) -> String {
        match self {
            Self::Message(message) => message.clone(),
            Self::Connection { timed_out } => {
                if *timed_out {
                    "Request timed out.".to_owned()
                } else {
                    "Connection error.".to_owned()
                }
            }
            Self::Http {
                status,
                parsed,
                message,
                ..
            } => {
                // normalizeProviderError extracts the body from the SDK error
                // object (`error.error`): a plain, non-empty object.
                let body = parsed
                    .as_ref()
                    .and_then(|parsed| parsed.get("error"))
                    .filter(|error| is_plain_non_empty_object(error))
                    .map(ToString::to_string);
                match body {
                    // messageCarriesBody: the SDK message already embeds the
                    // exact serialized body when it has no `message` member.
                    Some(body) if !message.contains(&body) => format!("{status}: {body}"),
                    _ => message.clone(),
                }
            }
        }
    }
}

/// Terminal failure of one generation request.
enum FinalFailure {
    /// The request signal cancelled the request.
    Aborted,
    /// An HTTP-level or plain failure with its message under construction.
    Failure(HttpFailure),
}

/// Parsed pieces of a successful chat completion.
struct SuccessData {
    response_id: Option<String>,
    usage: Option<Usage>,
    content: Vec<ImagesOutputContent>,
}

/// API dispatcher for `openrouter-images`.
pub struct OpenRouterImagesApi;

impl ImagesApiDispatch for OpenRouterImagesApi {
    fn generate_images(
        &self,
        model: &ImagesModel,
        context: ImagesContext,
        options: ImagesOptions,
    ) -> BoxFuture<'static, AssistantImages> {
        let model = model.clone();
        Box::pin(async move { generate_images_openrouter(&model, &context, &options).await })
    }
}

/// Create the shared dispatcher instance registered for
/// [`OPENROUTER_IMAGES_API`](crate::images::types::OPENROUTER_IMAGES_API).
#[must_use]
pub fn openrouter_images_api() -> Arc<dyn ImagesApiDispatch> {
    Arc::new(OpenRouterImagesApi)
}

/// Create the built-in `openrouter` image provider.
///
/// Auth matches the frozen factory: ambient `OPENROUTER_API_KEY` plus the
/// built-in `OpenRouter` OAuth handlers, and the generated image catalog as
/// the static model list.
#[must_use]
pub fn openrouter_images_provider() -> Arc<dyn ImagesProvider> {
    create_images_provider(CreateImagesProviderOptions {
        id: "openrouter".to_owned(),
        name: Some("OpenRouter".to_owned()),
        auth: default_provider_auth("openrouter", None),
        models: super::get_image_models("openrouter"),
        refresh_models: None,
        api: openrouter_images_api(),
    })
}

/// Generate images through `OpenRouter`'s image-capable chat models.
///
/// Errors never panic or reject: every failure path returns an
/// [`AssistantImages`] with an `Error` or `Aborted` stop reason, matching the
/// frozen adapter.
async fn generate_images_openrouter(
    model: &ImagesModel,
    context: &ImagesContext,
    options: &ImagesOptions,
) -> AssistantImages {
    let mut output = AssistantImages::new(model, now_millis());

    let Some(api_key) = options.api_key.as_deref().filter(|key| !key.is_empty()) else {
        output.fail(
            ImagesStopReason::Error,
            format!("No API key for provider: {}", model.provider),
        );
        return output;
    };

    let mut params = build_params(model, context);
    if let Some(on_payload) = options.on_payload.as_ref()
        && let Err(error) = on_payload(&mut params, model).await
    {
        output.fail(ImagesStopReason::Error, error.message());
        return output;
    }

    match run_with_retries(model, options, api_key, params).await {
        Ok(data) => {
            output.response_id = data.response_id;
            output.usage = data.usage;
            output.output = data.content;
            output
        }
        Err(FinalFailure::Aborted) => {
            output.fail(ImagesStopReason::Aborted, "Request aborted");
            output
        }
        Err(FinalFailure::Failure(failure)) => {
            // The frozen catch block classifies every caught error by the
            // live signal state first: a failure arriving on a cancelled
            // request reports aborted, not error.
            if options.is_aborted() {
                output.fail(ImagesStopReason::Aborted, "Request aborted");
            } else {
                output.fail(ImagesStopReason::Error, failure.message());
            }
            output
        }
    }
}

/// Send the request with the shared `retryProviderRequest` policy.
#[allow(
    clippy::result_large_err,
    reason = "the FinalFailure carrier is matched by value at every call site"
)]
async fn run_with_retries(
    model: &ImagesModel,
    options: &ImagesOptions,
    api_key: &str,
    params: Value,
) -> Result<SuccessData, FinalFailure> {
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let headers = request_headers(model, options, api_key);
    let body = serde_json::to_vec(&params)
        .map_err(|error| FinalFailure::Failure(HttpFailure::Message(error.to_string())))?;
    let timeout = options.timeout_ms.map(Duration::from_millis);
    let signal = options.signal.as_ref();
    let max_retries = options.max_retries.unwrap_or(0);
    let mut retries_remaining = max_retries;

    loop {
        // One deadline spans header and body processing: each phase draws
        // its remaining budget from this instant, so a single attempt can
        // never run for twice the configured timeout.
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let failure = match attempt_send(&url, &headers, &body, deadline, signal).await {
            Ok(response) => {
                match consume_response(model, options, response, signal, deadline).await {
                    Ok(data) => return Ok(data),
                    Err(AttemptFailure::Aborted) => return Err(FinalFailure::Aborted),
                    Err(AttemptFailure::Http(failure)) => failure,
                }
            }
            Err(AttemptFailure::Aborted) => return Err(FinalFailure::Aborted),
            Err(AttemptFailure::Http(failure)) => failure,
        };

        // The shared retry helper rewrites every error into an abort once the
        // signal has fired, before any retry decision.
        if options.is_aborted() {
            return Err(FinalFailure::Aborted);
        }

        let retryable = match &failure {
            HttpFailure::Message(_) => false,
            HttpFailure::Connection { .. } => true,
            HttpFailure::Http {
                status, headers, ..
            } => retryable_status(*status, headers),
        };
        if retries_remaining == 0 || !retryable {
            return Err(FinalFailure::Failure(failure));
        }

        let retry_index = max_retries - retries_remaining;
        retries_remaining -= 1;
        let delay = retry_delay_ms(retry_index, &failure, options.max_retry_delay_ms)
            .map_err(FinalFailure::Failure)?;
        if abortable_sleep(delay, signal).await.is_err() {
            return Err(FinalFailure::Aborted);
        }
    }
}

/// One request transmission up to response headers.
#[allow(
    clippy::result_large_err,
    reason = "the AttemptFailure carrier is matched by value at every call site"
)]
async fn attempt_send(
    url: &str,
    headers: &HeaderMap,
    body: &[u8],
    deadline: Option<Instant>,
    signal: Option<&CancellationToken>,
) -> Result<reqwest::Response, AttemptFailure> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(AttemptFailure::Aborted);
    }
    // Draw the send budget from the attempt deadline: an already-exhausted
    // budget surfaces the SDK connection-timeout error without any I/O.
    if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
        return Err(AttemptFailure::Http(HttpFailure::Connection {
            timed_out: true,
        }));
    }
    let timeout = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));

    let request = IMAGE_HTTP_CLIENT
        .post(url)
        .headers(headers.clone())
        .body(body.to_vec())
        .send();

    let sent = match (timeout, signal) {
        (Some(timeout), Some(signal)) => {
            tokio::select! {
                () = signal.cancelled() => return Err(AttemptFailure::Aborted),
                sent = tokio::time::timeout(timeout, request) => sent,
            }
        }
        (Some(timeout), None) => tokio::time::timeout(timeout, request).await,
        (None, Some(signal)) => {
            tokio::select! {
                () = signal.cancelled() => return Err(AttemptFailure::Aborted),
                sent = request => Ok(sent),
            }
        }
        (None, None) => Ok(request.await),
    };

    match sent {
        // Explicit timeout elapsed: the SDK raises its connection-timeout
        // error, which the retry policy classifies as retryable.
        Err(_elapsed) => Err(AttemptFailure::Http(HttpFailure::Connection {
            timed_out: true,
        })),
        Ok(Err(error)) => Err(AttemptFailure::Http(HttpFailure::Connection {
            timed_out: error.is_timeout(),
        })),
        Ok(Ok(response)) => Ok(response),
    }
}

enum AttemptFailure {
    Aborted,
    Http(HttpFailure),
}

/// Turn one received response into success data or a failure.
///
/// The frozen `create(...).withResponse()` resolves only after the full
/// non-streaming body has been parsed, so the response callback observes the
/// complete response before its data is consumed.
#[allow(
    clippy::result_large_err,
    reason = "the AttemptFailure carrier is matched by value at every call site"
)]
async fn consume_response(
    model: &ImagesModel,
    options: &ImagesOptions,
    response: reqwest::Response,
    signal: Option<&CancellationToken>,
    deadline: Option<Instant>,
) -> Result<SuccessData, AttemptFailure> {
    // The body phase draws its remaining budget from the same attempt
    // deadline the send phase used: one timeout per attempt, not per phase.
    if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
        return Err(AttemptFailure::Http(HttpFailure::Connection {
            timed_out: true,
        }));
    }
    let timeout = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
    let status = response.status();
    let response_headers = headers_to_record(response.headers());
    if !status.is_success() {
        return Err(AttemptFailure::Http(
            consume_error_response(response, signal, timeout).await,
        ));
    }

    let bytes = match signal {
        Some(signal) => {
            tokio::select! {
                () = signal.cancelled() => return Err(AttemptFailure::Aborted),
                bytes = read_body_bytes(response, timeout) => bytes,
            }
        }
        None => read_body_bytes(response, timeout).await,
    };
    let bytes = bytes?;
    let parsed: Value = serde_json::from_slice(&bytes)
        .map_err(|_| AttemptFailure::Http(HttpFailure::Connection { timed_out: false }))?;

    let provider_response = ProviderResponse {
        status: status.as_u16(),
        headers: response_headers,
    };
    if let Some(on_response) = options.on_response.as_ref()
        && let Err(error) = on_response(&provider_response, model).await
    {
        return Err(AttemptFailure::Http(HttpFailure::Message(
            error.message().to_owned(),
        )));
    }

    Ok(record_output(model, &parsed))
}

/// Read a full response body under the request timeout.
///
/// Elapsed timeout surfaces as a timed-out connection failure (retryable),
/// matching the frozen SDK's connection-timeout error; other read failures
/// stay plain connection errors.
#[allow(
    clippy::result_large_err,
    reason = "the AttemptFailure carrier is matched by value at every call site"
)]
async fn read_body_bytes(
    response: reqwest::Response,
    timeout: Option<Duration>,
) -> Result<Vec<u8>, AttemptFailure> {
    let read = response.bytes();
    let bytes = match timeout {
        Some(timeout) => match tokio::time::timeout(timeout, read).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return Err(AttemptFailure::Http(HttpFailure::Connection {
                    timed_out: true,
                }));
            }
        },
        None => read.await,
    };
    bytes
        .map(|bytes| bytes.to_vec())
        .map_err(|_| AttemptFailure::Http(HttpFailure::Connection { timed_out: false }))
}

/// Read a non-2xx body and compose the SDK status error.
async fn consume_error_response(
    response: reqwest::Response,
    signal: Option<&CancellationToken>,
    timeout: Option<Duration>,
) -> HttpFailure {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let read = HttpTransport::read_error_body(response, signal);
    let result = match timeout {
        Some(timeout) => match tokio::time::timeout(timeout, read).await {
            Ok(result) => result,
            Err(_) => {
                return HttpFailure::Connection { timed_out: true };
            }
        },
        None => read.await,
    };
    let raw = match result {
        Ok(text) => text,
        Err(TransportError::Cancelled | TransportError::Callback(_)) => {
            return HttpFailure::Message("Request aborted".to_owned());
        }
        // JS reads the error text with `.catch(err => err.message)`, so a
        // body-read failure degrades into that message as the raw body.
        Err(TransportError::Body(error) | TransportError::Request(error)) => error.to_string(),
    };

    // safeJSON: parse, and treat falsy JavaScript values as absent.
    let parsed = serde_json::from_str::<Value>(&raw)
        .ok()
        .filter(js_truthy_filter);
    let raw = if parsed.is_none() { Some(raw) } else { None };
    let message = sdk_status_message(status, parsed.as_ref(), raw.as_deref());
    HttpFailure::Http {
        status,
        headers,
        parsed,
        message,
    }
}

/// Extract response id, usage, and content blocks from the parsed completion.
///
/// Usage cost accounting uses the model's catalog rates, exactly like the
/// frozen `parseUsage(rawUsage, model)`.
fn record_output(model: &ImagesModel, parsed: &Value) -> SuccessData {
    let response_id = parsed.get("id").and_then(Value::as_str).map(str::to_owned);

    let rates = ModelCostRates {
        input: model.cost.input,
        output: model.cost.output,
        cache_read: model.cost.cache_read,
        cache_write: model.cost.cache_write,
    };
    let usage = parsed
        .get("usage")
        .filter(|usage| js_truthy(usage))
        .map(|usage| parse_usage(usage, &rates));

    let mut blocks = Vec::new();
    if let Some(message) = parsed
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
    {
        // A string message content becomes one text block.
        if let Some(text) = message.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            blocks.push(ImagesOutputContent::Text(TextContent::new(text)));
        }
        // `message.images[].image_url` carries data URLs of generated images.
        if let Some(images) = message.get("images").and_then(Value::as_array) {
            for image in images {
                let url = match image.get("image_url") {
                    Some(Value::String(url)) => Some(url.clone()),
                    Some(image_url) => image_url
                        .get("url")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    None => None,
                };
                let Some(url) = url.filter(|url| url.starts_with("data:")) else {
                    continue;
                };
                let Some((mime_type, data)) = parse_data_url(&url) else {
                    continue;
                };
                blocks.push(ImagesOutputContent::Image(ImageContent::new(
                    data, mime_type,
                )));
            }
        }
    }

    SuccessData {
        response_id,
        usage,
        content: blocks,
    }
}

/// Build the Chat Completions payload for one image request.
fn build_params(model: &ImagesModel, context: &ImagesContext) -> Value {
    let parts: Vec<Value> = context
        .input
        .iter()
        .map(|item| match item {
            ImagesInputContent::Text(text) => serde_json::json!({
                "type": "text",
                "text": text.text,
            }),
            ImagesInputContent::Image(image) => serde_json::json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{}", image.mime_type, image.data),
                },
            }),
        })
        .collect();

    let modalities: Vec<&str> = if model.outputs_text() {
        vec!["image", "text"]
    } else {
        vec!["image"]
    };

    serde_json::json!({
        "model": model.id,
        "messages": [{ "role": "user", "content": parts }],
        "stream": false,
        "modalities": modalities,
    })
}

/// Merge model defaults with request headers, then apply bearer auth.
///
/// The frozen adapter builds `providerHeadersToRecord({...model.headers,
/// ...optionsHeaders})` — nulls stripped before an exact-key merge — and the
/// SDK's bearer application wins over any merged `Authorization`.
fn request_headers(model: &ImagesModel, options: &ImagesOptions, api_key: &str) -> HeaderMap {
    let mut record: Vec<(String, String)> = Vec::new();
    if let Some(model_headers) = model.headers.as_ref() {
        for (name, value) in model_headers {
            record.push((name.clone(), value.clone()));
        }
    }
    if let Some(option_headers) = options.headers.as_ref() {
        for (name, value) in option_headers {
            // JS object spread replaces exact (case-sensitive) keys; a null
            // suppresses the model default with the same name.
            record.retain(|(existing, _)| existing != name);
            if let Some(value) = value {
                record.push((name.clone(), value.clone()));
            }
        }
    }

    let mut headers = HeaderMap::new();
    for (name, value) in record {
        let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) else {
            continue;
        };
        headers.insert(name, value);
    }

    for name in headers.keys().cloned().collect::<Vec<_>>() {
        if name.as_str().eq_ignore_ascii_case("authorization") {
            headers.remove(name);
        }
    }
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {api_key}")) {
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    headers
}

/// Mirror `headersToRecord` for the response callback: repeated values are
/// joined with `", "`.
fn headers_to_record(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut record = BTreeMap::new();
    for (name, value) in headers {
        let Ok(value) = value.to_str() else {
            continue;
        };
        record
            .entry(name.as_str().to_owned())
            .and_modify(|existing: &mut String| {
                existing.push_str(", ");
                existing.push_str(value);
            })
            .or_insert_with(|| value.to_owned());
    }
    record
}

/// The frozen status-error classification: `x-should-retry` wins, then
/// 408/409/429/5xx (missing status — connection errors — retry upstream).
fn retryable_status(status: u16, headers: &HeaderMap) -> bool {
    match headers
        .get("x-should-retry")
        .and_then(|value| value.to_str().ok())
    {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    status == 408 || status == 409 || status == 429 || status >= 500
}

/// Compute one retry delay per `getRetryDelayMs`, failing on a server delay
/// above the configured cap.
#[allow(
    clippy::result_large_err,
    clippy::cast_precision_loss,
    reason = "the HttpFailure carrier is matched by value at every call site; epoch-millis values sit far below 2^53"
)]
fn retry_delay_ms(
    retry_index: u32,
    failure: &HttpFailure,
    max_retry_delay_ms: Option<u64>,
) -> Result<f64, HttpFailure> {
    let HttpFailure::Http {
        headers, message, ..
    } = failure
    else {
        return Ok(exponential_delay_ms(retry_index));
    };

    if let Some(value) = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_float_prefix)
    {
        return validate_retry_delay(value, max_retry_delay_ms, message);
    }

    if let Some(value) = headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
    {
        let delay = if let Some(seconds) = parse_float_prefix(value) {
            seconds * 1000.0
        } else {
            parse_http_date_ms(value).map_or_else(
                || exponential_delay_ms(retry_index),
                |date_ms| {
                    // HTTP-date delays are relative to now; a stale date
                    // sleeps zero through the clamped sleep.
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as f64;
                    date_ms - now
                },
            )
        };
        return validate_retry_delay(delay, max_retry_delay_ms, message);
    }

    Ok(exponential_delay_ms(retry_index))
}

/// Fail immediately when a server-requested delay exceeds the cap.
#[allow(
    clippy::result_large_err,
    clippy::cast_precision_loss,
    reason = "millisecond caps sit far below 2^53, so the widening cast is exact"
)]
fn validate_retry_delay(
    delay_ms: f64,
    max_retry_delay_ms: Option<u64>,
    provider_error_message: &str,
) -> Result<f64, HttpFailure> {
    // Millisecond caps are far below 2^53, so this widening cast is exact.
    let max = max_retry_delay_ms.unwrap_or(60_000) as f64;
    if max > 0.0 && delay_ms > max {
        return Err(HttpFailure::Message(format!(
            "Server requested {}s retry delay (max: {}s). {provider_error_message}",
            (delay_ms / 1000.0).ceil(),
            (max / 1000.0).ceil(),
        )));
    }
    Ok(delay_ms)
}

/// `min(0.5 * 2^retryIndex, 8) * 1000`, jittered down by up to 25%.
fn exponential_delay_ms(retry_index: u32) -> f64 {
    let exponent = i32::try_from(retry_index).unwrap_or(i32::MAX);
    let base = (0.5 * (2.0_f64).powi(exponent)).min(8.0);
    base * 1000.0 * (1.0 - jitter_fraction() * 0.25)
}

/// One uniform random fraction in `[0, 1)` per call, seeded from the OS
/// entropy pool with a time fallback.
#[allow(
    clippy::cast_precision_loss,
    reason = "the shift keeps the top 53 bits, so the f64 fraction is exact"
)]
fn jitter_fraction() -> f64 {
    let mut seed = [0_u8; 8];
    if getrandom::fill(&mut seed).is_err() {
        // The sub-second nanos are the only fresh entropy available; widen
        // their four bytes into the low half of the eight-byte seed.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
            .to_le_bytes();
        seed[..4].copy_from_slice(&nanos);
    }
    let mut state = u64::from_le_bytes(seed) | 1;
    state ^= state >> 12;
    state ^= state << 25;
    state ^= state >> 27;
    // Widening shift keeps the top 53 bits: the fraction is exact in f64.
    (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1_u64 << 53) as f64
}

/// JavaScript-style lenient `parseFloat`: parse the leading numeric prefix,
/// ignoring trailing garbage; `None` mirrors `NaN`.
fn parse_float_prefix(text: &str) -> Option<f64> {
    let text = text.trim_start();
    let bytes = text.as_bytes();
    let mut end = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let mut mantissa_digits = 0;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
        mantissa_digits += 1;
    }
    if bytes.get(end) == Some(&b'.') {
        let mut fraction_digits = 0;
        while bytes
            .get(end + 1 + fraction_digits)
            .is_some_and(u8::is_ascii_digit)
        {
            fraction_digits += 1;
        }
        if fraction_digits > 0 {
            end += 1 + fraction_digits;
            mantissa_digits += fraction_digits;
        }
    }
    if mantissa_digits == 0 {
        return None;
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let mut exponent_end =
            end + 1 + usize::from(matches!(bytes.get(end + 1), Some(b'+' | b'-')));
        let mut exponent_digits = 0;
        while bytes.get(exponent_end).is_some_and(u8::is_ascii_digit) {
            exponent_end += 1;
            exponent_digits += 1;
        }
        if exponent_digits > 0 {
            end = exponent_end;
        }
    }
    text[..end].parse::<f64>().ok()
}

/// Parse an IMF-fixdate `retry-after` value to epoch milliseconds.
///
/// JavaScript's `Date.parse` also accepts the obsolete RFC 850 and asctime
/// forms; servers emit IMF-fixdate, so only that form is recognized and any
/// other value falls back to the exponential delay.
#[allow(
    clippy::cast_precision_loss,
    reason = "second-range epoch values stay exact in f64 widening"
)]
fn parse_http_date_ms(text: &str) -> Option<f64> {
    let (day_name, rest) = text.trim().split_once(", ")?;
    if !matches!(
        day_name,
        "Mon" | "Tue" | "Wed" | "Thu" | "Fri" | "Sat" | "Sun"
    ) {
        return None;
    }
    let mut parts = rest.split_ascii_whitespace();
    let day: u32 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    if parts.next() != Some("GMT") {
        return None;
    }
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    // Widening i64→f64 for second-range epoch values stays exact.
    Some((days * 86_400 + hour * 3_600 + minute * 60 + second) as f64 * 1000.0)
}

/// Days from 1970-01-01 to `year-month-day` (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year.rem_euclid(400);
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Sleep for `delay_ms` (clamped at zero) while honoring cancellation.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "negative delays sleep zero and large delays saturate; both only lengthen the wait"
)]
async fn abortable_sleep(delay_ms: f64, signal: Option<&CancellationToken>) -> Result<(), ()> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return Err(());
    }
    // Negative delays (stale HTTP dates) sleep zero; large delays saturate
    // rather than wrap, which only lengthens the wait.
    let delay = Duration::from_millis(delay_ms.max(0.0) as u64);
    if let Some(signal) = signal {
        tokio::select! {
            () = signal.cancelled() => Err(()),
            () = tokio::time::sleep(delay) => Ok(()),
        }
    } else {
        tokio::time::sleep(delay).await;
        Ok(())
    }
}

/// Whether a JSON value is a plain, non-empty object in the
/// `isPlainNonEmptyObject` sense.
fn is_plain_non_empty_object(value: &Value) -> bool {
    value.as_object().is_some_and(|object| !object.is_empty())
}

/// Filter adapter for [`js_truthy`].
fn js_truthy_filter(value: &Value) -> bool {
    js_truthy(value)
}

/// JavaScript truthiness for `safeJSON` results.
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// The `openai@6.40.0` `APIError.makeMessage` composition for status errors.
///
/// `msg` probes `parsed["error"]` with JavaScript truthiness: a string
/// `message` member wins, then the serialized member, then the raw body.
fn sdk_status_message(status: u16, parsed: Option<&Value>, raw: Option<&str>) -> String {
    let error_member = parsed
        .and_then(|parsed| parsed.get("error"))
        .filter(|error| js_truthy(error));
    let message = match error_member {
        Some(Value::Object(map)) => match map.get("message") {
            Some(Value::String(message)) => Some(message.clone()),
            Some(other) => Some(other.to_string()),
            None => Some(Value::Object(map.clone()).to_string()),
        },
        Some(other) => Some(other.to_string()),
        None => raw.map(str::to_owned),
    };
    match message {
        Some(message) => format!("{status} {message}"),
        None => format!("{status} status code (no body)"),
    }
}

/// Parse one `data:` URL per the frozen `^data:([^;]+);base64,(.+)$` match.
///
/// The regex requires at least one character in both the MIME type and the
/// payload, so `data:mime;base64,` with an empty payload does not match.
fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (mime_type, data) = rest.split_once(';')?;
    if mime_type.is_empty() {
        return None;
    }
    let data = data.strip_prefix("base64,")?;
    if data.is_empty() {
        return None;
    }
    Some((mime_type.to_owned(), data.to_owned()))
}

/// Parse `usage` with the frozen arithmetic (`|| 0` fallbacks, cache
/// read/write split, per-million cost rates).
///
// Rates thread through the caller so the parser stays pure; the dispatch
// entry passes the model's catalog rates.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "provider token counters are non-negative integers; the saturating cast mirrors the frozen Usage domain"
)]
fn parse_usage(raw: &Value, rates: &ModelCostRates) -> Usage {
    let prompt_tokens = js_number_or_zero(raw.get("prompt_tokens"));
    let details = raw.get("prompt_tokens_details");
    let reported_cached =
        js_number_or_zero(details.and_then(|details| details.get("cached_tokens")));
    let cache_write =
        js_number_or_zero(details.and_then(|details| details.get("cache_write_tokens")));
    let cache_read = if cache_write > 0.0 {
        (reported_cached - cache_write).max(0.0)
    } else {
        reported_cached
    };
    let input = (prompt_tokens - cache_read - cache_write).max(0.0);
    let output = js_number_or_zero(raw.get("completion_tokens"));

    let mut cost = UsageCost {
        input: rates.input / 1_000_000.0 * input,
        output: rates.output / 1_000_000.0 * output,
        cache_read: rates.cache_read / 1_000_000.0 * cache_read,
        cache_write: rates.cache_write / 1_000_000.0 * cache_write,
        total: 0.0,
    };
    cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;

    Usage {
        // Provider counters are non-negative integers; the saturating cast
        // mirrors the integer token domain of the frozen `Usage`.
        input: input as u64,
        output: output as u64,
        cache_read: cache_read as u64,
        cache_write: cache_write as u64,
        cache_write1h: None,
        reasoning: None,
        total_tokens: (input + output + cache_read + cache_write) as u64,
        cost,
    }
}

/// `value || 0`: missing, null, zero, and non-numeric values read zero.
fn js_number_or_zero(value: Option<&Value>) -> f64 {
    value
        .and_then(Value::as_f64)
        .filter(|value| *value != 0.0)
        .unwrap_or(0.0)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::float_cmp,
    clippy::cast_precision_loss,
    reason = "unit tests use contextual failure messages and compare deterministic exact floats"
)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::images::types::{ImagesOutput, OPENROUTER_IMAGES_API};
    use crate::types::ModelInput;

    fn model() -> ImagesModel {
        ImagesModel::new(
            "test-image",
            "Test Image",
            OPENROUTER_IMAGES_API,
            "openrouter",
            "https://image.example.test/api/v1",
            vec![ModelInput::Text, ModelInput::Image],
            vec![ImagesOutput::Image],
        )
    }

    #[test]
    fn params_carry_modalities_and_data_urls() {
        let mut model = model();
        let context = ImagesContext::new(vec![
            ImagesInputContent::from("draw a cat"),
            ImagesInputContent::from(ImageContent::new("AA==", "image/png")),
        ]);
        let params = build_params(&model, &context);
        assert_eq!(params["model"], json!("test-image"));
        assert_eq!(params["stream"], json!(false));
        assert_eq!(params["modalities"], json!(["image"]));
        assert_eq!(params["messages"][0]["role"], json!("user"));
        assert_eq!(
            params["messages"][0]["content"][0],
            json!({"type": "text", "text": "draw a cat"})
        );
        assert_eq!(
            params["messages"][0]["content"][1]["image_url"]["url"],
            json!("data:image/png;base64,AA==")
        );

        model.output.push(ImagesOutput::Text);
        let params = build_params(&model, &context);
        assert_eq!(params["modalities"], json!(["image", "text"]));
    }

    #[test]
    fn data_urls_require_mime_and_base64_marker() {
        assert_eq!(
            parse_data_url("data:image/png;base64,AA=="),
            Some(("image/png".to_owned(), "AA==".to_owned()))
        );
        assert!(parse_data_url("data:;base64,AA==").is_none());
        assert!(parse_data_url("data:image/png;base64,").is_none());
        assert!(parse_data_url("data:image/png;charset=1;base64,AA==").is_none());
        assert!(parse_data_url("https://example.test/image.png").is_none());
    }

    #[test]
    fn sdk_status_message_matches_openai_composition() {
        let parsed = json!({"error": {"message": "Invalid key", "code": 401}});
        assert_eq!(
            sdk_status_message(401, Some(&parsed), None),
            "401 Invalid key"
        );

        let no_message = json!({"error": {"code": 5}});
        assert_eq!(
            sdk_status_message(401, Some(&no_message), None),
            "401 {\"code\":5}"
        );

        let no_error_member = json!({"foo": 1});
        assert_eq!(
            sdk_status_message(402, Some(&no_error_member), None),
            "402 status code (no body)"
        );

        assert_eq!(
            sdk_status_message(502, None, Some("bad gateway")),
            "502 bad gateway"
        );

        let null_error = json!({"error": null});
        assert_eq!(
            sdk_status_message(500, Some(&null_error), Some("{\"error\":null}")),
            "500 {\"error\":null}"
        );
    }

    #[test]
    fn failure_message_surfaces_status_and_body() {
        let failure = HttpFailure::Http {
            status: 401,
            headers: HeaderMap::new(),
            parsed: Some(json!({"error": {"message": "Invalid key"}})),
            message: "401 Invalid key".to_owned(),
        };
        assert_eq!(failure.message(), "401: {\"message\":\"Invalid key\"}");

        // When the SDK message already embeds the serialized body, it wins.
        let carrying = HttpFailure::Http {
            status: 402,
            headers: HeaderMap::new(),
            parsed: Some(json!({"error": {"code": 7}})),
            message: "402 {\"code\":7}".to_owned(),
        };
        assert_eq!(carrying.message(), "402 {\"code\":7}");
    }

    #[test]
    fn connection_messages_match_sdk_constants() {
        assert_eq!(
            HttpFailure::Connection { timed_out: true }.message(),
            "Request timed out."
        );
        assert_eq!(
            HttpFailure::Connection { timed_out: false }.message(),
            "Connection error."
        );
    }

    #[test]
    fn retry_classification_honors_should_retry_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-should-retry", HeaderValue::from_static("true"));
        assert!(retryable_status(400, &headers));
        headers.insert("x-should-retry", HeaderValue::from_static("false"));
        assert!(!retryable_status(500, &headers));
        headers.clear();
        assert!(retryable_status(429, &headers));
        assert!(retryable_status(503, &headers));
        assert!(retryable_status(408, &headers));
        assert!(retryable_status(409, &headers));
        assert!(!retryable_status(400, &headers));
        assert!(!retryable_status(401, &headers));
    }

    #[test]
    fn retry_delay_reads_ms_seconds_and_dates() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after-ms", HeaderValue::from_static("250"));
        let failure = http_failure_with(headers.clone());
        assert_eq!(
            retry_delay_ms(0, &failure, None).expect("delay parses"),
            250.0
        );

        headers.clear();
        headers.insert("retry-after", HeaderValue::from_static("2"));
        let failure = http_failure_with(headers.clone());
        assert_eq!(
            retry_delay_ms(0, &failure, None).expect("delay parses"),
            2000.0
        );

        headers.clear();
        headers.insert(
            "retry-after",
            HeaderValue::from_static("Sun, 06 Nov 1994 08:49:37 GMT"),
        );
        let failure = http_failure_with(headers.clone());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as f64;
        let date_ms = parse_http_date_ms("Sun, 06 Nov 1994 08:49:37 GMT").expect("date parses");
        assert!(
            date_ms + 1.0 < now,
            "1994 is in the past, delay goes negative"
        );
        // A stale HTTP date yields a negative relative delay; the clamped
        // sleep turns that into an immediate retry.
        assert!(
            retry_delay_ms(0, &failure, None).expect("delay parses") < 0.0,
            "stale date delay is negative"
        );

        // No delay headers: exponential backoff with jitter stays bounded.
        headers.clear();
        let failure = http_failure_with(headers);
        let delay = retry_delay_ms(4, &failure, None).expect("delay parses");
        assert!((500.0..=8000.0).contains(&delay));
    }

    #[test]
    fn retry_delay_cap_fails_with_server_message() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after-ms", HeaderValue::from_static("120000"));
        let failure = http_failure_with(headers);
        let error = retry_delay_ms(0, &failure, Some(60_000)).expect_err("cap exceeded");
        assert_eq!(
            error.message(),
            "Server requested 120s retry delay (max: 60s). 429 slow down"
        );

        // Zero disables the cap.
        assert_eq!(
            retry_delay_ms(0, &failure, Some(0)).expect("no cap"),
            120_000.0
        );
    }

    fn http_failure_with(headers: HeaderMap) -> HttpFailure {
        HttpFailure::Http {
            status: 429,
            headers,
            parsed: None,
            message: "429 slow down".to_owned(),
        }
    }

    #[test]
    fn float_prefix_parses_leading_numbers_only() {
        assert_eq!(parse_float_prefix("2"), Some(2.0));
        assert_eq!(parse_float_prefix(" 1.5s"), Some(1.5));
        assert_eq!(parse_float_prefix("1e2ms"), Some(100.0));
        assert_eq!(parse_float_prefix("-3"), Some(-3.0));
        assert_eq!(parse_float_prefix("abc"), None);
        assert_eq!(parse_float_prefix(""), None);
        assert_eq!(parse_float_prefix("."), None);
    }

    #[test]
    fn http_date_parses_imf_fixdate() {
        assert_eq!(
            parse_http_date_ms("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777_000.0)
        );
        assert_eq!(parse_http_date_ms("nonsense"), None);
        assert_eq!(parse_http_date_ms("Sun, 06 Nov 1994 08:49:37 PST"), None);
    }

    #[test]
    fn usage_arithmetic_matches_frozen_math() {
        let rates = ModelCostRates {
            input: 2.0,
            output: 8.0,
            cache_read: 0.2,
            cache_write: 0.4,
        };
        let raw = json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "prompt_tokens_details": {
                "cached_tokens": 30,
                "cache_write_tokens": 10
            }
        });
        let usage = parse_usage(&raw, &rates);
        // cacheRead = 30 - 10, input = 100 - 20 - 10.
        assert_eq!(usage.input, 70);
        assert_eq!(usage.output, 50);
        assert_eq!(usage.cache_read, 20);
        assert_eq!(usage.cache_write, 10);
        assert_eq!(usage.total_tokens, 150);
        assert!((usage.cost.input - 70.0 * 2.0 / 1_000_000.0).abs() < 1e-12);
        assert!((usage.cost.total - 0.000_548).abs() < 1e-12);
    }

    #[test]
    fn request_headers_merge_then_apply_bearer() {
        let mut model = model();
        model.headers = Some(BTreeMap::from([
            ("X-Title".to_owned(), "pi".to_owned()),
            ("X-Session".to_owned(), "affinity".to_owned()),
            ("Authorization".to_owned(), "Bearer stale".to_owned()),
        ]));
        let options = ImagesOptions {
            headers: Some(BTreeMap::from([
                ("X-Title".to_owned(), Some("override".to_owned())),
                ("X-Drop".to_owned(), None),
                ("X-Session".to_owned(), None),
                ("Authorization".to_owned(), Some("Bearer option".to_owned())),
            ])),
            ..ImagesOptions::default()
        };

        let headers = request_headers(&model, &options, "sk-live");
        let title = headers
            .get("X-Title")
            .expect("title header")
            .to_str()
            .expect("ascii");
        assert_eq!(title, "override");
        // Auth application wins over merged bearer entries.
        let authorization = headers
            .get("authorization")
            .expect("bearer header")
            .to_str()
            .expect("ascii");
        assert_eq!(authorization, "Bearer sk-live");
        assert!(headers.get("X-Drop").is_none());
        // A None option value suppresses the model default with the same name.
        assert!(headers.get("X-Session").is_none());
    }

    #[test]
    fn record_output_extracts_text_images_usage_and_id() {
        let parsed = json!({
            "id": "gen-1",
            "choices": [{
                "message": {
                    "content": "here is your image",
                    "images": [
                        {"image_url": "data:image/png;base64,AAE="},
                        {"image_url": {"url": "data:image/jpeg;base64,ABM="}},
                        {"image_url": "https://example.test/nope.png"},
                        {"image_url": "data:image/png;base64,"}
                    ]
                }
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        });
        let data = record_output(&model(), &parsed);
        assert_eq!(data.response_id.as_deref(), Some("gen-1"));
        assert_eq!(data.content.len(), 3);
        assert!(
            matches!(&data.content[0], ImagesOutputContent::Text(text) if text.text == "here is your image")
        );
        let ImagesOutputContent::Image(first) = &data.content[1] else {
            unreachable!("second block is an image");
        };
        assert_eq!(first.mime_type, "image/png");
        assert_eq!(first.data, "AAE=");
        let ImagesOutputContent::Image(second) = &data.content[2] else {
            unreachable!("third block is an image");
        };
        assert_eq!(second.mime_type, "image/jpeg");
        assert_eq!(second.data, "ABM=");
        assert_eq!(data.usage.expect("usage present").input, 10);
    }

    #[test]
    fn image_only_models_omit_text_modality() {
        let model = model();
        assert!(!model.outputs_text());
    }

    #[tokio::test]
    async fn exhausted_deadline_fails_send_without_io() {
        let deadline = Instant::now() - Duration::from_secs(1);
        let result = attempt_send(
            "https://example.test/dead",
            &HeaderMap::new(),
            b"{}",
            Some(deadline),
            None,
        )
        .await;
        assert!(
            matches!(
                result,
                Err(AttemptFailure::Http(HttpFailure::Connection {
                    timed_out: true
                }))
            ),
            "an exhausted attempt deadline must surface the SDK timeout before any I/O"
        );
    }
}
