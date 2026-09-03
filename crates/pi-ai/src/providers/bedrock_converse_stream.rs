//! Amazon Bedrock `ConverseStream` provider adapter.
//!
//! The adapter receives an injected [`BedrockClientFactory`]. Production code can use
//! [`DefaultBedrockClientFactory`], which loads the official AWS default credential chain via
//! `aws-config` and optionally overlays profile or static credentials from the request's
//! `StreamOptions.env` snapshot. This module owns request shape, endpoint and region selection,
//! Bedrock event conversion, and stream terminal semantics — never hand-rolled `SigV4` or
//! event-stream framing.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aws_config::BehaviorVersion;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::config::{Credentials, Region};
use aws_sdk_bedrockruntime::primitives::Blob;
use aws_sdk_bedrockruntime::types::{
    AnyToolChoice, AutoToolChoice, CachePointBlock, CachePointType, CacheTtl, ContentBlock,
    ContentBlockDelta, ContentBlockStart, ConversationRole, ConverseStreamMetadataEvent,
    ConverseStreamOutput, ImageBlock, ImageFormat, ImageSource, InferenceConfiguration,
    Message as AwsMessage, ReasoningContentBlock, ReasoningContentBlockDelta, ReasoningTextBlock,
    SpecificToolChoice, SystemContentBlock, Tool as AwsTool, ToolChoice as AwsToolChoice,
    ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock, ToolResultStatus,
    ToolSpecification, ToolUseBlock,
};
use base64::Engine as _;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use serde_json::{Map, Value, json};

use crate::provider::{Provider, ProviderError, ProviderResponse, StreamOptionKey, StreamOptions};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, CacheRetention, Context, DoneReason,
    ErrorReason, Message, Model, ModelThinkingLevel, ThinkingLevel, ToolResultContent, Usage,
    UserContent, UserMessageContent,
};

use super::shared::{calculate_cost, parse_streaming_json, transform_messages};
use super::stream_state::{AssistantState, EventSendError, ProviderEventSender};

const API: &str = "bedrock-converse-stream";
const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";
const DEFAULT_REGION: &str = "us-east-1";
const ABORTED_MESSAGE: &str = "Request was aborted";
const EVENT_CAPACITY: NonZeroUsize = match NonZeroUsize::new(32) {
    Some(capacity) => capacity,
    None => NonZeroUsize::MIN,
};

/// Explicit IAM credentials taken from a request-scoped `StreamOptions.env` overlay.
///
/// Secrets are never written by [`Debug`]; only a redacted access-key marker is visible so logs
/// and test failures cannot leak key material.
#[derive(Clone, Eq, PartialEq)]
pub struct BedrockStaticCredentials {
    /// Access key id.
    pub access_key_id: String,
    /// Secret access key.
    pub secret_access_key: String,
    /// Optional session token for temporary credentials.
    pub session_token: Option<String>,
}

impl fmt::Debug for BedrockStaticCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BedrockStaticCredentials")
            .field(
                "access_key_id",
                &redacted_access_key_id(&self.access_key_id),
            )
            .field("secret_access_key", &"** redacted **")
            .field(
                "session_token",
                &self
                    .session_token
                    .as_ref()
                    .map_or("none", |_| "** redacted **"),
            )
            .finish()
    }
}

/// Configuration the adapter asks a [`BedrockClientFactory`] to apply to an AWS SDK client.
///
/// Region, endpoint, model id, and non-reserved headers always travel with the request. Optional
/// profile and static credential overlays come from the request-scoped environment snapshot when
/// present; otherwise production factories keep the official default credential chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BedrockClientRequest {
    /// Region selected from an inference-profile ARN, explicit option/environment, endpoint, or
    /// the Bedrock default.
    pub region: String,
    /// Explicit endpoint for custom endpoints, or for a standard endpoint whose region must be
    /// pinned because no region was configured independently.
    pub endpoint_url: Option<String>,
    /// Model or inference-profile identifier that will be sent to `ConverseStream`.
    pub model_id: String,
    /// Non-reserved custom headers for the signed Bedrock request.
    pub headers: BTreeMap<String, String>,
    /// Named AWS profile selected from options or the request-scoped environment overlay.
    pub profile: Option<String>,
    /// Explicit IAM credentials from the request-scoped environment overlay.
    ///
    /// When present, production factories must use these instead of the ambient default chain.
    pub static_credentials: Option<BedrockStaticCredentials>,
}

/// Object-safe source of configured official Bedrock Runtime SDK clients.
///
/// Implementations may asynchronously load credentials. Production implementations must return a
/// real [`aws_sdk_bedrockruntime::Client`].
pub trait BedrockClientFactory: Send + Sync {
    /// Build an SDK client for one resolved Bedrock request.
    fn create_client(
        &self,
        request: BedrockClientRequest,
    ) -> BoxFuture<'static, Result<Client, ProviderError>>;
}

/// Production factory that builds official Bedrock Runtime clients through `aws-config`.
///
/// Resolution order for each request:
/// 1. Exact region from [`BedrockClientRequest::region`].
/// 2. Optional endpoint URL from [`BedrockClientRequest::endpoint_url`].
/// 3. Optional named profile from [`BedrockClientRequest::profile`].
/// 4. Explicit static credentials from [`BedrockClientRequest::static_credentials`] when present;
///    otherwise the official default credential provider chain.
///
/// Non-reserved request headers are applied by the adapter through the SDK
/// `customize().map_request` surface so they participate in `SigV4` signing.
#[derive(Clone, Debug, Default)]
pub struct DefaultBedrockClientFactory;

impl DefaultBedrockClientFactory {
    /// Construct the production factory used by the provider registry.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl BedrockClientFactory for DefaultBedrockClientFactory {
    fn create_client(
        &self,
        request: BedrockClientRequest,
    ) -> BoxFuture<'static, Result<Client, ProviderError>> {
        Box::pin(async move {
            let mut loader = aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new(request.region.clone()));
            if let Some(endpoint_url) = request.endpoint_url.as_ref() {
                loader = loader.endpoint_url(endpoint_url.clone());
            }
            if let Some(profile) = request.profile.as_ref() {
                loader = loader.profile_name(profile.clone());
            }
            if let Some(static_credentials) = request.static_credentials {
                loader = loader.credentials_provider(Credentials::new(
                    static_credentials.access_key_id,
                    static_credentials.secret_access_key,
                    static_credentials.session_token,
                    None,
                    "pi-stream-options-env",
                ));
            }
            let sdk_config = loader.load().await;
            Ok(Client::new(&sdk_config))
        })
    }
}

/// Native Amazon Bedrock `ConverseStream` provider.
#[derive(Clone)]
pub struct BedrockConverseStream {
    client_factory: Arc<dyn BedrockClientFactory>,
}

impl BedrockConverseStream {
    /// Create a Bedrock adapter backed by an injected official SDK client factory.
    #[must_use]
    pub fn new(client_factory: Arc<dyn BedrockClientFactory>) -> Self {
        Self { client_factory }
    }

    /// Create a Bedrock adapter backed by [`DefaultBedrockClientFactory`].
    #[must_use]
    pub fn with_default_client_factory() -> Self {
        Self::new(Arc::new(DefaultBedrockClientFactory::new()))
    }

    async fn execute(
        &self,
        sender: &ProviderEventSender,
        model: &Model,
        context: Context,
        options: &StreamOptions,
        assembly: &mut StreamAssembly,
    ) -> Result<(DoneReason, AssistantMessage), AdapterFailure> {
        check_cancelled(options)?;

        let mut payload =
            build_request_payload(model, &context, options).map_err(AdapterFailure::Semantic)?;
        if let Some(callback) = &options.on_payload {
            callback(&mut payload, model)
                .await
                .map_err(|error| AdapterFailure::Semantic(error.message().to_owned()))?;
        }

        let model_id = required_string(&payload, "modelId")?.to_owned();
        let client_request = build_client_request(model, options, model_id);
        let headers = client_request.headers.clone();

        let client_future = self.client_factory.create_client(client_request);
        let client = if let Some(signal) = &options.signal {
            tokio::select! {
                () = signal.cancelled() => return Err(AdapterFailure::Aborted),
                result = client_future => result
                    .map_err(|error| AdapterFailure::Semantic(error.message().to_owned()))?,
            }
        } else {
            client_future
                .await
                .map_err(|error| AdapterFailure::Semantic(error.message().to_owned()))?
        };

        check_cancelled(options)?;
        let send_future = send_sdk_request(&client, &payload, &headers);
        let mut response = if let Some(signal) = &options.signal {
            tokio::select! {
                () = signal.cancelled() => return Err(AdapterFailure::Aborted),
                result = send_future => result.map_err(AdapterFailure::Semantic)?,
            }
        } else {
            send_future.await.map_err(AdapterFailure::Semantic)?
        };

        if let Some(callback) = &options.on_response {
            // A successful ConverseStream operation has passed the SDK's HTTP status validation.
            // The generated output exposes the request ID but not the raw response status or
            // headers, so the portable callback view is the successful status with no headers.
            callback(
                &ProviderResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                },
                model,
            )
            .await
            .map_err(|error| AdapterFailure::Semantic(error.message().to_owned()))?;
        }

        loop {
            let received = if let Some(signal) = &options.signal {
                tokio::select! {
                    () = signal.cancelled() => return Err(AdapterFailure::Aborted),
                    result = response.stream.recv() => result,
                }
            } else {
                response.stream.recv().await
            };

            match received {
                Ok(Some(event)) => {
                    let events = assembly.process(event)?;
                    for event in events {
                        sender
                            .event(event)
                            .await
                            .map_err(AdapterFailure::Undeliverable)?;
                    }
                }
                Ok(None) => break,
                Err(error) => return Err(AdapterFailure::Semantic(format_bedrock_error(&error))),
            }
        }

        check_cancelled(options)?;
        let reason = assembly.required_terminal()?;
        let message = assembly.finish(reason, model, false);
        Ok((reason, message))
    }
}

impl Provider for BedrockConverseStream {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
        let (sender, stream) = ProviderEventSender::channel(EVENT_CAPACITY);
        let adapter = self.clone();
        let model = model.clone();

        tokio::spawn(async move {
            let message =
                AssistantMessage::new(API, model.provider.clone(), model.id.clone(), now_ms());
            let mut assembly = StreamAssembly::new(message);
            if sender.start(assembly.snapshot()).await.is_err() {
                return;
            }

            match adapter
                .execute(&sender, &model, context, &options, &mut assembly)
                .await
            {
                Ok((reason, message)) => {
                    let _result = sender.done(reason, message).await;
                }
                Err(AdapterFailure::Undeliverable(_)) => {}
                Err(AdapterFailure::Aborted) => {
                    let message = assembly.fail(ErrorReason::Aborted, ABORTED_MESSAGE, &model);
                    let _result = sender.error(ErrorReason::Aborted, message).await;
                }
                Err(AdapterFailure::Semantic(message)) => {
                    let message = assembly.fail(ErrorReason::Error, message, &model);
                    let _result = sender.error(ErrorReason::Error, message).await;
                }
            }
        });

        stream
    }
}

#[derive(Debug, thiserror::Error)]
enum AdapterFailure {
    #[error("{0}")]
    Semantic(String),
    #[error("{ABORTED_MESSAGE}")]
    Aborted,
    #[error(transparent)]
    Undeliverable(EventSendError),
}

fn check_cancelled(options: &StreamOptions) -> Result<(), AdapterFailure> {
    if options
        .signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        Err(AdapterFailure::Aborted)
    } else {
        Ok(())
    }
}

fn now_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn format_bedrock_error(error: &impl std::fmt::Display) -> String {
    let message = error.to_string();
    if message.trim().is_empty() {
        "Amazon Bedrock request failed".to_owned()
    } else {
        message
    }
}

fn resolved_headers(model: &Model, options: &StreamOptions) -> BTreeMap<String, String> {
    let mut headers = model.headers.clone().unwrap_or_default();
    if let Some(overrides) = &options.headers {
        for (name, value) in overrides {
            if let Some(value) = value {
                headers.insert(name.clone(), value.clone());
            } else {
                headers.remove(name);
            }
        }
    }
    headers.retain(|name, _| !is_reserved_header(name));
    headers
}

fn is_reserved_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("x-amz-") || matches!(lower.as_str(), "authorization" | "host")
}

fn build_client_request(
    model: &Model,
    options: &StreamOptions,
    model_id: String,
) -> BedrockClientRequest {
    let (region, endpoint_url) = resolve_region_and_endpoint(model, options);
    BedrockClientRequest {
        region,
        endpoint_url,
        model_id,
        headers: resolved_headers(model, options),
        profile: resolve_profile(options),
        static_credentials: resolve_static_credentials(options),
    }
}

fn resolve_profile(options: &StreamOptions) -> Option<String> {
    option_string(options, StreamOptionKey::PROFILE).or_else(|| env_value(options, "AWS_PROFILE"))
}

fn resolve_static_credentials(options: &StreamOptions) -> Option<BedrockStaticCredentials> {
    let access_key_id = env_value(options, "AWS_ACCESS_KEY_ID")?;
    let secret_access_key = env_value(options, "AWS_SECRET_ACCESS_KEY")?;
    Some(BedrockStaticCredentials {
        access_key_id,
        secret_access_key,
        session_token: env_value(options, "AWS_SESSION_TOKEN"),
    })
}

fn redacted_access_key_id(access_key_id: &str) -> String {
    if access_key_id.len() <= 4 {
        "****".to_owned()
    } else {
        format!(
            "****{}",
            &access_key_id[access_key_id.len().saturating_sub(4)..]
        )
    }
}

fn resolve_region_and_endpoint(model: &Model, options: &StreamOptions) -> (String, Option<String>) {
    let configured_region = option_string(options, StreamOptionKey::REGION)
        .or_else(|| env_value(options, "AWS_REGION"))
        .or_else(|| env_value(options, "AWS_DEFAULT_REGION"));
    let endpoint_region = standard_bedrock_endpoint_region(&model.base_url);
    let use_explicit_endpoint = endpoint_region.is_none() || configured_region.is_none();
    let region = arn_bedrock_region(&model.id)
        .or(configured_region)
        .or_else(|| use_explicit_endpoint.then_some(endpoint_region).flatten())
        .unwrap_or_else(|| DEFAULT_REGION.to_owned());
    let endpoint_url = (use_explicit_endpoint && !model.base_url.trim().is_empty())
        .then(|| model.base_url.clone());
    (region, endpoint_url)
}

fn arn_bedrock_region(model_id: &str) -> Option<String> {
    let mut parts = model_id.split(':');
    let arn = parts.next()?;
    let partition = parts.next()?;
    let service = parts.next()?;
    let region = parts.next()?;
    if arn == "arn" && partition.starts_with("aws") && service == "bedrock" && !region.is_empty() {
        Some(region.to_owned())
    } else {
        None
    }
}

fn standard_bedrock_endpoint_region(base_url: &str) -> Option<String> {
    let without_scheme = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))?;
    let host = without_scheme.split('/').next()?.split(':').next()?;
    let suffix = if host.ends_with(".amazonaws.com.cn") {
        ".amazonaws.com.cn"
    } else if host.ends_with(".amazonaws.com") {
        ".amazonaws.com"
    } else {
        return None;
    };
    let stem = host.strip_suffix(suffix)?;
    let region = stem
        .strip_prefix("bedrock-runtime.")
        .or_else(|| stem.strip_prefix("bedrock-runtime-fips."))?;
    (!region.is_empty()).then(|| region.to_owned())
}

fn env_value(options: &StreamOptions, name: &str) -> Option<String> {
    options
        .env
        .as_ref()
        .and_then(|environment| environment.get(name))
        .filter(|value| !value.trim().is_empty())
        .cloned()
}

fn option_string(options: &StreamOptions, key: StreamOptionKey) -> Option<String> {
    options
        .extra_value(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, AdapterFailure> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AdapterFailure::Semantic(format!("Bedrock payload field {field} is required"))
        })
}

fn build_request_payload(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> Result<Value, String> {
    let cache_retention = resolve_cache_retention(options);
    let messages = convert_messages(context, model, cache_retention, options)?;
    let mut payload = Map::new();
    payload.insert("modelId".to_owned(), Value::String(model.id.clone()));
    payload.insert("messages".to_owned(), Value::Array(messages));

    if let Some(system) = build_system_prompt(context, model, cache_retention, options) {
        payload.insert("system".to_owned(), Value::Array(system));
    }

    let mut inference = Map::new();
    let max_tokens = options
        .max_tokens
        .or_else(|| is_anthropic_claude(model).then_some(model.max_tokens));
    if let Some(max_tokens) = max_tokens {
        inference.insert("maxTokens".to_owned(), Value::from(max_tokens));
    }
    if let Some(temperature) = options.temperature {
        if !temperature.is_finite() {
            return Err("Bedrock temperature must be finite".to_owned());
        }
        inference.insert("temperature".to_owned(), Value::from(temperature));
    }
    payload.insert("inferenceConfig".to_owned(), Value::Object(inference));

    if let Some(tool_config) = convert_tool_config(context, options)? {
        payload.insert("toolConfig".to_owned(), tool_config);
    }
    if let Some(additional) = build_additional_model_request_fields(model, options)? {
        payload.insert("additionalModelRequestFields".to_owned(), additional);
    }
    if let Some(metadata) = request_metadata(options)? {
        payload.insert("requestMetadata".to_owned(), Value::Object(metadata));
    }

    Ok(Value::Object(payload))
}

fn resolve_cache_retention(options: &StreamOptions) -> CacheRetention {
    options.cache_retention.unwrap_or_else(|| {
        if env_value(options, "PI_CACHE_RETENTION").as_deref() == Some("long") {
            CacheRetention::Long
        } else {
            CacheRetention::Short
        }
    })
}

fn build_system_prompt(
    context: &Context,
    model: &Model,
    cache_retention: CacheRetention,
    options: &StreamOptions,
) -> Option<Vec<Value>> {
    let prompt = context
        .system_prompt
        .as_deref()
        .filter(|prompt| !prompt.is_empty())?;
    let mut blocks = vec![json!({ "text": prompt })];
    if cache_retention != CacheRetention::None && supports_prompt_caching(model, options) {
        blocks.push(cache_point_value(cache_retention));
    }
    Some(blocks)
}

fn convert_messages(
    conversation: &Context,
    model: &Model,
    cache_retention: CacheRetention,
    options: &StreamOptions,
) -> Result<Vec<Value>, String> {
    let transformed = transform_messages(&conversation.messages, model, |id, _, _| {
        normalize_tool_call_id(id)
    });
    let mut result = Vec::with_capacity(transformed.len());
    let mut index = 0;

    while index < transformed.len() {
        match &transformed[index] {
            Message::User(message) => {
                result.push(convert_user_message(message)?);
            }
            Message::Assistant(message) => {
                if let Some(entry) = convert_assistant_message(message, model) {
                    result.push(entry);
                }
            }
            Message::ToolResult(_) => {
                let (entry, next) = convert_tool_result_run(&transformed, index)?;
                result.push(entry);
                index = next.saturating_sub(1);
            }
        }
        index += 1;
    }

    if cache_retention != CacheRetention::None
        && supports_prompt_caching(model, options)
        && let Some(last) = result.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("user")
        && let Some(parts) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        parts.push(cache_point_value(cache_retention));
    }

    Ok(result)
}

fn convert_user_message(message: &crate::types::UserMessage) -> Result<Value, String> {
    let mut parts = Vec::new();
    match &message.content {
        UserMessageContent::Text(text) => {
            parts.push(json!({ "text": required_text(text) }));
        }
        UserMessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    UserContent::Text(text) => {
                        if let Some(text) = non_blank_text(&text.text) {
                            parts.push(json!({ "text": text }));
                        }
                    }
                    UserContent::Image(image) => {
                        parts.push(image_value(&image.mime_type, &image.data)?);
                    }
                }
            }
            if parts.is_empty() {
                parts.push(json!({ "text": EMPTY_TEXT_PLACEHOLDER }));
            }
        }
    }
    Ok(json!({ "role": "user", "content": parts }))
}

fn convert_assistant_message(message: &AssistantMessage, model: &Model) -> Option<Value> {
    let mut parts = Vec::new();
    for block in &message.content {
        match block {
            AssistantContent::Text(text) => {
                if let Some(text) = non_blank_text(&text.text) {
                    parts.push(json!({ "text": text }));
                }
            }
            AssistantContent::ToolCall(tool_call) => {
                parts.push(json!({
                    "toolUse": {
                        "toolUseId": normalize_tool_call_id(&tool_call.id),
                        "name": tool_call.name,
                        "input": tool_call.arguments,
                    }
                }));
            }
            AssistantContent::Thinking(thinking) => {
                if let Some(part) = convert_thinking_block(thinking, model) {
                    parts.push(part);
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(json!({ "role": "assistant", "content": parts }))
    }
}

fn convert_thinking_block(
    thinking: &crate::types::ThinkingContent,
    model: &Model,
) -> Option<Value> {
    // Encrypted reasoning is opaque: replay the stored payload without
    // requiring display text, mirroring the reference redacted-first order.
    if thinking.redacted == Some(true) {
        let data = thinking
            .thinking_signature
            .as_deref()
            .filter(|signature| !signature.trim().is_empty())?;
        return Some(json!({
            "reasoningContent": { "redactedContent": data }
        }));
    }
    let text = non_blank_text(&thinking.thinking)?;
    if is_anthropic_claude(model) {
        if let Some(signature) = thinking
            .thinking_signature
            .as_deref()
            .filter(|signature| !signature.trim().is_empty())
        {
            Some(json!({
                "reasoningContent": {
                    "reasoningText": { "text": text, "signature": signature }
                }
            }))
        } else {
            Some(json!({ "text": text }))
        }
    } else {
        Some(json!({
            "reasoningContent": { "reasoningText": { "text": text } }
        }))
    }
}

fn convert_tool_result_run(messages: &[Message], start: usize) -> Result<(Value, usize), String> {
    let mut parts = Vec::new();
    let mut next = start;
    while let Some(Message::ToolResult(tool_result)) = messages.get(next) {
        parts.push(json!({
            "toolResult": {
                "toolUseId": normalize_tool_call_id(&tool_result.tool_call_id),
                "content": convert_tool_result_content(&tool_result.content)?,
                "status": if tool_result.is_error { "error" } else { "success" },
            }
        }));
        next += 1;
    }
    Ok((json!({ "role": "user", "content": parts }), next))
}

fn convert_tool_result_content(content: &[ToolResultContent]) -> Result<Vec<Value>, String> {
    let mut result = Vec::with_capacity(content.len());
    for block in content {
        match block {
            ToolResultContent::Text(text) => {
                if let Some(text) = non_blank_text(&text.text) {
                    result.push(json!({ "text": text }));
                }
            }
            ToolResultContent::Image(image) => {
                result.push(image_value(&image.mime_type, &image.data)?);
            }
        }
    }
    if result.is_empty() {
        result.push(json!({ "text": EMPTY_TEXT_PLACEHOLDER }));
    }
    Ok(result)
}

fn image_value(mime_type: &str, data: &str) -> Result<Value, String> {
    let format = match mime_type {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => return Err(format!("Unknown image type: {other}")),
    };
    Ok(json!({ "image": { "format": format, "source": { "bytes": data } } }))
}

fn non_blank_text(text: &str) -> Option<&str> {
    (!text.trim().is_empty()).then_some(text)
}

fn required_text(text: &str) -> &str {
    non_blank_text(text).unwrap_or(EMPTY_TEXT_PLACEHOLDER)
}

fn normalize_tool_call_id(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn cache_point_value(cache_retention: CacheRetention) -> Value {
    if cache_retention == CacheRetention::Long {
        json!({ "cachePoint": { "type": "default", "ttl": "1h" } })
    } else {
        json!({ "cachePoint": { "type": "default" } })
    }
}

fn supports_prompt_caching(model: &Model, options: &StreamOptions) -> bool {
    let candidates = model_match_candidates(model);
    let has_claude = candidates
        .iter()
        .any(|candidate| candidate.contains("claude"));
    if !has_claude {
        return env_value(options, "AWS_BEDROCK_FORCE_CACHE").as_deref() == Some("1");
    }
    candidates.iter().any(|candidate| {
        candidate.contains("fable-5")
            || candidate.contains("sonnet-5")
            || candidate.contains("-4-")
            || candidate.contains("claude-3-7-sonnet")
            || candidate.contains("claude-3-5-haiku")
    })
}

fn is_anthropic_claude(model: &Model) -> bool {
    let id = model.id.to_ascii_lowercase();
    let name = model.name.to_ascii_lowercase();
    id.contains("anthropic.claude")
        || id.contains("anthropic/claude")
        || name.contains("anthropic.claude")
        || name.contains("anthropic/claude")
        || name.contains("claude")
}

fn model_match_candidates(model: &Model) -> Vec<String> {
    [&model.id, &model.name]
        .into_iter()
        .flat_map(|value| {
            let lower = value.to_ascii_lowercase();
            let normalized = normalize_model_name(&lower);
            [lower, normalized]
        })
        .collect()
}

fn normalize_model_name(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut separator = false;
    for character in value.chars() {
        if matches!(character, ' ' | '\t' | '\n' | '\r' | '_' | '.' | ':') {
            if !separator {
                result.push('-');
                separator = true;
            }
        } else {
            result.push(character);
            separator = false;
        }
    }
    result
}

fn convert_tool_config(
    context: &Context,
    options: &StreamOptions,
) -> Result<Option<Value>, String> {
    let Some(tools) = context.tools.as_ref().filter(|tools| !tools.is_empty()) else {
        return Ok(None);
    };
    let choice = options.extra_value(StreamOptionKey::TOOL_CHOICE);
    if choice.and_then(Value::as_str) == Some("none") {
        return Ok(None);
    }

    let tools = tools
        .iter()
        .map(|tool| {
            json!({
                "toolSpec": {
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": { "json": tool.parameters },
                }
            })
        })
        .collect::<Vec<_>>();
    let mut config = Map::new();
    config.insert("tools".to_owned(), Value::Array(tools));

    if let Some(choice) = choice {
        let value = match choice {
            Value::String(choice) if choice == "auto" => json!({ "auto": {} }),
            Value::String(choice) if choice == "any" => json!({ "any": {} }),
            Value::Object(choice) if choice.get("type").and_then(Value::as_str) == Some("tool") => {
                let name = choice
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| "Bedrock tool choice requires a tool name".to_owned())?;
                json!({ "tool": { "name": name } })
            }
            Value::String(choice) if choice == "none" => return Ok(None),
            _ => return Err("Invalid Bedrock toolChoice".to_owned()),
        };
        config.insert("toolChoice".to_owned(), value);
    }

    Ok(Some(Value::Object(config)))
}

fn request_metadata(options: &StreamOptions) -> Result<Option<Map<String, Value>>, String> {
    let object = if let Some(source) = options.extra_value(StreamOptionKey::REQUEST_METADATA) {
        source
            .as_object()
            .ok_or_else(|| "Bedrock requestMetadata must be an object".to_owned())?
            .clone()
    } else if let Some(metadata) = &options.metadata {
        metadata.clone()
    } else {
        return Ok(None);
    };
    for (key, value) in &object {
        if value.as_str().is_none() {
            return Err(format!(
                "Bedrock requestMetadata value for {key} must be a string"
            ));
        }
    }
    Ok(Some(object))
}

fn reasoning_level(options: &StreamOptions) -> Result<Option<ThinkingLevel>, String> {
    options
        .extra_value(StreamOptionKey::REASONING)
        .map(|value| {
            serde_json::from_value(value.clone())
                .map_err(|error| format!("Invalid Bedrock reasoning level: {error}"))
        })
        .transpose()
}

fn build_additional_model_request_fields(
    model: &Model,
    options: &StreamOptions,
) -> Result<Option<Value>, String> {
    let Some(reasoning) = reasoning_level(options)? else {
        return Ok(None);
    };
    if !model.reasoning || !is_anthropic_claude(model) {
        return Ok(None);
    }

    let display = if is_govcloud_target(model, options) {
        None
    } else {
        Some(
            options
                .extra_value(StreamOptionKey::THINKING_DISPLAY)
                .and_then(Value::as_str)
                .unwrap_or("summarized"),
        )
    };
    if display.is_some_and(|display| !matches!(display, "summarized" | "omitted")) {
        return Err("Bedrock thinkingDisplay must be summarized or omitted".to_owned());
    }

    let mut result = Map::new();
    if supports_adaptive_thinking(model) {
        let mut thinking = Map::new();
        thinking.insert("type".to_owned(), Value::String("adaptive".to_owned()));
        if let Some(display) = display {
            thinking.insert("display".to_owned(), Value::String(display.to_owned()));
        }
        result.insert("thinking".to_owned(), Value::Object(thinking));
        result.insert(
            "output_config".to_owned(),
            json!({ "effort": map_thinking_effort(model, reasoning) }),
        );
    } else {
        let mut thinking = Map::new();
        thinking.insert("type".to_owned(), Value::String("enabled".to_owned()));
        thinking.insert(
            "budget_tokens".to_owned(),
            Value::from(thinking_budget(options, reasoning)?),
        );
        if let Some(display) = display {
            thinking.insert("display".to_owned(), Value::String(display.to_owned()));
        }
        result.insert("thinking".to_owned(), Value::Object(thinking));
        if options
            .extra_value(StreamOptionKey::INTERLEAVED_THINKING)
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
            result.insert(
                "anthropic_beta".to_owned(),
                json!(["interleaved-thinking-2025-05-14"]),
            );
        }
    }
    Ok(Some(Value::Object(result)))
}

fn thinking_budget(options: &StreamOptions, reasoning: ThinkingLevel) -> Result<u64, String> {
    let level = match reasoning {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "high",
    };
    if let Some(value) = options
        .extra_value(StreamOptionKey::THINKING_BUDGETS)
        .and_then(Value::as_object)
        .and_then(|budgets| budgets.get(level))
    {
        return value.as_u64().ok_or_else(|| {
            format!("Bedrock thinking budget for {level} must be an unsigned integer")
        });
    }
    Ok(match reasoning {
        ThinkingLevel::Minimal => 1_024,
        ThinkingLevel::Low => 2_048,
        ThinkingLevel::Medium => 8_192,
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => 16_384,
    })
}

fn supports_adaptive_thinking(model: &Model) -> bool {
    model_match_candidates(model).iter().any(|candidate| {
        candidate.contains("opus-4-6")
            || candidate.contains("opus-4-7")
            || candidate.contains("opus-4-8")
            || candidate.contains("sonnet-4-6")
            || candidate.contains("sonnet-5")
            || candidate.contains("fable-5")
    })
}

fn supports_native_xhigh(model: &Model) -> bool {
    model_match_candidates(model).iter().any(|candidate| {
        candidate.contains("opus-4-7")
            || candidate.contains("opus-4-8")
            || candidate.contains("sonnet-5")
            || candidate.contains("fable-5")
    })
}

fn map_thinking_effort(model: &Model, level: ThinkingLevel) -> String {
    if level == ThinkingLevel::Xhigh && supports_native_xhigh(model) {
        return "xhigh".to_owned();
    }
    let mapped_level = match level {
        ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
        ThinkingLevel::Low => ModelThinkingLevel::Low,
        ThinkingLevel::Medium => ModelThinkingLevel::Medium,
        ThinkingLevel::High => ModelThinkingLevel::High,
        ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
        ThinkingLevel::Max => ModelThinkingLevel::Max,
    };
    if let Some(mapped) = model
        .thinking_level_map
        .as_ref()
        .and_then(|mapping| mapping.get(&mapped_level))
        .and_then(Clone::clone)
    {
        return mapped;
    }
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "high",
    }
    .to_owned()
}

fn is_govcloud_target(model: &Model, options: &StreamOptions) -> bool {
    option_string(options, StreamOptionKey::REGION)
        .or_else(|| env_value(options, "AWS_REGION"))
        .or_else(|| env_value(options, "AWS_DEFAULT_REGION"))
        .is_some_and(|region| region.to_ascii_lowercase().starts_with("us-gov-"))
        || model.id.to_ascii_lowercase().starts_with("us-gov.")
        || model.id.to_ascii_lowercase().starts_with("arn:aws-us-gov:")
}

async fn send_sdk_request(
    client: &Client,
    payload: &Value,
    headers: &BTreeMap<String, String>,
) -> Result<aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamOutput, String> {
    let model_id = payload
        .get("modelId")
        .and_then(Value::as_str)
        .filter(|model_id| !model_id.is_empty())
        .ok_or_else(|| "Bedrock payload field modelId is required".to_owned())?;
    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "Bedrock payload field messages must be an array".to_owned())?
        .iter()
        .map(parse_sdk_message)
        .collect::<Result<Vec<_>, _>>()?;
    let system = payload.get("system").map(parse_sdk_system).transpose()?;
    let inference = payload
        .get("inferenceConfig")
        .map(parse_sdk_inference)
        .transpose()?;
    let tools = payload.get("toolConfig").map(parse_sdk_tools).transpose()?;
    let request_metadata = payload
        .get("requestMetadata")
        .map(parse_sdk_metadata)
        .transpose()?;

    let mut request = client
        .converse_stream()
        .model_id(model_id)
        .set_messages(Some(messages))
        .set_system(system)
        .set_inference_config(inference)
        .set_tool_config(tools)
        .set_request_metadata(request_metadata);
    if let Some(additional) = payload.get("additionalModelRequestFields") {
        let schema = document_schema(additional);
        let document = schema.as_json().cloned().unwrap_or_default();
        request = request.additional_model_request_fields(document);
    }

    // Official SDK customization surface: headers are applied before signing so `SigV4` covers them.
    // The generated ConverseStream output still does not expose raw response headers, so the
    // portable on_response callback remains status-only.
    let headers = headers.clone();
    request
        .customize()
        .mutate_request(move |http_request| {
            for (name, value) in &headers {
                // Headers API accepts owned String; borrows from the captured map are not 'static.
                // Invalid header components are skipped rather than panicking in production.
                let _ = http_request
                    .headers_mut()
                    .try_insert(name.clone(), value.clone());
            }
        })
        .send()
        .await
        .map_err(|error| error.to_string())
}

fn parse_sdk_message(value: &Value) -> Result<AwsMessage, String> {
    let role = match value.get("role").and_then(Value::as_str) {
        Some("user") => ConversationRole::User,
        Some("assistant") => ConversationRole::Assistant,
        _ => return Err("Bedrock message role must be user or assistant".to_owned()),
    };
    let content = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| "Bedrock message content must be an array".to_owned())?
        .iter()
        .map(parse_sdk_content_block)
        .collect::<Result<Vec<_>, _>>()?;
    AwsMessage::builder()
        .role(role)
        .set_content(Some(content))
        .build()
        .map_err(|error| error.to_string())
}

fn parse_sdk_content_block(value: &Value) -> Result<ContentBlock, String> {
    if let Some(text) = value.get("text").and_then(Value::as_str) {
        return Ok(ContentBlock::Text(text.to_owned()));
    }
    if let Some(image) = value.get("image") {
        return Ok(ContentBlock::Image(parse_sdk_image(image)?));
    }
    if let Some(tool) = value.get("toolUse") {
        let id = field_string(tool, "toolUseId")?;
        let name = field_string(tool, "name")?;
        let input = tool
            .get("input")
            .ok_or_else(|| "Bedrock toolUse input is required".to_owned())?;
        let schema = document_schema(input);
        let document = schema.as_json().cloned().unwrap_or_default();
        let block = ToolUseBlock::builder()
            .tool_use_id(id)
            .name(name)
            .input(document)
            .build()
            .map_err(|error| error.to_string())?;
        return Ok(ContentBlock::ToolUse(block));
    }
    if let Some(tool_result) = value.get("toolResult") {
        return Ok(ContentBlock::ToolResult(parse_sdk_tool_result(
            tool_result,
        )?));
    }
    if let Some(reasoning) = value.get("reasoningContent") {
        return Ok(ContentBlock::ReasoningContent(parse_sdk_reasoning(
            reasoning,
        )?));
    }
    if let Some(cache_point) = value.get("cachePoint") {
        return Ok(ContentBlock::CachePoint(parse_sdk_cache_point(
            cache_point,
        )?));
    }
    Err("Unsupported Bedrock content block".to_owned())
}

fn parse_sdk_system(value: &Value) -> Result<Vec<SystemContentBlock>, String> {
    value
        .as_array()
        .ok_or_else(|| "Bedrock system must be an array".to_owned())?
        .iter()
        .map(|block| {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                Ok(SystemContentBlock::Text(text.to_owned()))
            } else if let Some(cache_point) = block.get("cachePoint") {
                Ok(SystemContentBlock::CachePoint(parse_sdk_cache_point(
                    cache_point,
                )?))
            } else {
                Err("Unsupported Bedrock system block".to_owned())
            }
        })
        .collect()
}

fn temperature_to_f32(value: f64) -> Result<f32, String> {
    if !value.is_finite() {
        return Err("Bedrock temperature must be finite".to_owned());
    }
    let max = f64::from(f32::MAX);
    let min = f64::from(f32::MIN);
    if value >= max {
        return Ok(f32::MAX);
    }
    if value <= min {
        return Ok(f32::MIN);
    }
    f32_from_checked_f64(value)
}

/// Convert a finite `f64` already proven to lie inside the finite `f32` range.
fn f32_from_checked_f64(value: f64) -> Result<f32, String> {
    // Prefer decimal round-trip over a bare cast so Clippy does not treat the
    // conversion as an unchecked precision loss. Values outside the finite f32
    // range are rejected by the caller before this helper runs.
    value
        .to_string()
        .parse::<f32>()
        .map_err(|error| format!("Bedrock temperature is outside the supported range: {error}"))
        .and_then(|converted| {
            if converted.is_finite() {
                Ok(converted)
            } else {
                Err("Bedrock temperature is outside the supported range".to_owned())
            }
        })
}

fn parse_sdk_inference(value: &Value) -> Result<InferenceConfiguration, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "Bedrock inferenceConfig must be an object".to_owned())?;
    let max_tokens = object
        .get("maxTokens")
        .map(|value| {
            value
                .as_u64()
                .and_then(|tokens| i32::try_from(tokens).ok())
                .ok_or_else(|| "Bedrock maxTokens must fit a signed 32-bit integer".to_owned())
        })
        .transpose()?;
    let temperature = object
        .get("temperature")
        .map(|value| {
            let temperature = value
                .as_f64()
                .ok_or_else(|| "Bedrock temperature must be a number".to_owned())?;
            temperature_to_f32(temperature)
        })
        .transpose()?;
    Ok(InferenceConfiguration::builder()
        .set_max_tokens(max_tokens)
        .set_temperature(temperature)
        .build())
}

fn parse_sdk_tools(value: &Value) -> Result<ToolConfiguration, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "Bedrock toolConfig must be an object".to_owned())?;
    let tools = object
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| "Bedrock toolConfig.tools must be an array".to_owned())?
        .iter()
        .map(|tool| {
            let spec = tool
                .get("toolSpec")
                .ok_or_else(|| "Bedrock toolSpec is required".to_owned())?;
            let schema_value = spec
                .get("inputSchema")
                .and_then(|schema| schema.get("json"))
                .ok_or_else(|| "Bedrock tool input schema is required".to_owned())?;
            let specification = ToolSpecification::builder()
                .name(field_string(spec, "name")?)
                .description(field_string(spec, "description")?)
                .input_schema(document_schema(schema_value))
                .build()
                .map_err(|error| error.to_string())?;
            Ok(AwsTool::ToolSpec(specification))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let choice = object
        .get("toolChoice")
        .map(parse_sdk_tool_choice)
        .transpose()?;
    ToolConfiguration::builder()
        .set_tools(Some(tools))
        .set_tool_choice(choice)
        .build()
        .map_err(|error| error.to_string())
}

fn parse_sdk_tool_choice(value: &Value) -> Result<AwsToolChoice, String> {
    if value.get("auto").is_some() {
        Ok(AwsToolChoice::Auto(AutoToolChoice::builder().build()))
    } else if value.get("any").is_some() {
        Ok(AwsToolChoice::Any(AnyToolChoice::builder().build()))
    } else if let Some(tool) = value.get("tool") {
        let choice = SpecificToolChoice::builder()
            .name(field_string(tool, "name")?)
            .build()
            .map_err(|error| error.to_string())?;
        Ok(AwsToolChoice::Tool(choice))
    } else {
        Err("Unsupported Bedrock toolChoice".to_owned())
    }
}

fn parse_sdk_tool_result(value: &Value) -> Result<ToolResultBlock, String> {
    let content = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| "Bedrock toolResult content must be an array".to_owned())?
        .iter()
        .map(|content| {
            if let Some(text) = content.get("text").and_then(Value::as_str) {
                Ok(ToolResultContentBlock::Text(text.to_owned()))
            } else if let Some(image) = content.get("image") {
                Ok(ToolResultContentBlock::Image(parse_sdk_image(image)?))
            } else {
                Err("Unsupported Bedrock toolResult content".to_owned())
            }
        })
        .collect::<Result<Vec<_>, String>>()?;
    let status = match value.get("status").and_then(Value::as_str) {
        Some("error") => ToolResultStatus::Error,
        _ => ToolResultStatus::Success,
    };
    ToolResultBlock::builder()
        .tool_use_id(field_string(value, "toolUseId")?)
        .set_content(Some(content))
        .status(status)
        .build()
        .map_err(|error| error.to_string())
}

fn parse_sdk_reasoning(value: &Value) -> Result<ReasoningContentBlock, String> {
    if let Some(redacted) = value.get("redactedContent").and_then(Value::as_str) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(redacted)
            .map_err(|error| format!("Invalid Bedrock redacted reasoning data: {error}"))?;
        return Ok(ReasoningContentBlock::RedactedContent(Blob::new(bytes)));
    }
    let text = value
        .get("reasoningText")
        .ok_or_else(|| "Bedrock reasoningText is required".to_owned())?;
    let block = ReasoningTextBlock::builder()
        .text(field_string(text, "text")?)
        .set_signature(
            text.get("signature")
                .and_then(Value::as_str)
                .map(str::to_owned),
        )
        .build()
        .map_err(|error| error.to_string())?;
    Ok(ReasoningContentBlock::ReasoningText(block))
}

fn parse_sdk_image(value: &Value) -> Result<ImageBlock, String> {
    let format = match value.get("format").and_then(Value::as_str) {
        Some("jpeg") => ImageFormat::Jpeg,
        Some("png") => ImageFormat::Png,
        Some("gif") => ImageFormat::Gif,
        Some("webp") => ImageFormat::Webp,
        _ => return Err("Unsupported Bedrock image format".to_owned()),
    };
    let encoded = value
        .get("source")
        .and_then(|source| source.get("bytes"))
        .and_then(Value::as_str)
        .ok_or_else(|| "Bedrock image bytes are required".to_owned())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("Invalid base64 Bedrock image: {error}"))?;
    ImageBlock::builder()
        .format(format)
        .source(ImageSource::Bytes(Blob::new(bytes)))
        .build()
        .map_err(|error| error.to_string())
}

fn parse_sdk_cache_point(value: &Value) -> Result<CachePointBlock, String> {
    if value.get("type").and_then(Value::as_str) != Some("default") {
        return Err("Bedrock cachePoint type must be default".to_owned());
    }
    let ttl = match value.get("ttl").and_then(Value::as_str) {
        None => None,
        Some("1h") => Some(CacheTtl::OneHour),
        Some("5m") => Some(CacheTtl::FiveMinutes),
        Some(_) => return Err("Unsupported Bedrock cachePoint ttl".to_owned()),
    };
    CachePointBlock::builder()
        .r#type(CachePointType::Default)
        .set_ttl(ttl)
        .build()
        .map_err(|error| error.to_string())
}

fn parse_sdk_metadata(value: &Value) -> Result<HashMap<String, String>, String> {
    value
        .as_object()
        .ok_or_else(|| "Bedrock requestMetadata must be an object".to_owned())?
        .iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), value.to_owned()))
                .ok_or_else(|| format!("Bedrock requestMetadata value for {key} must be a string"))
        })
        .collect()
}

fn field_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("Bedrock field {field} is required"))
}

/// Wrap a recursively converted Smithy `Document` in an SDK union that names the otherwise
/// transitive document type. This keeps `aws-smithy-types` out of this crate's public dependency
/// surface while preserving arbitrary JSON schemas and model-specific fields.
fn document_schema(value: &Value) -> ToolInputSchema {
    ToolInputSchema::Json(match value {
        Value::Null => Option::<bool>::None.into(),
        Value::Bool(value) => (*value).into(),
        Value::Number(value) => {
            if let Some(value) = value.as_u64() {
                value.into()
            } else if let Some(value) = value.as_i64() {
                value.into()
            } else {
                value.as_f64().unwrap_or_default().into()
            }
        }
        Value::String(value) => value.clone().into(),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                document_schema(value)
                    .as_json()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .into(),
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    document_schema(value)
                        .as_json()
                        .cloned()
                        .unwrap_or_default(),
                )
            })
            .collect::<HashMap<_, _>>()
            .into(),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockKind {
    Text,
    Thinking,
    Tool,
}

#[derive(Clone, Debug)]
struct BlockScratch {
    content_index: u64,
    kind: BlockKind,
    partial_json: String,
    thinking_signature: String,
    redacted: bool,
    closed: bool,
}

impl BlockScratch {
    fn new(content_index: u64, kind: BlockKind) -> Self {
        Self {
            content_index,
            kind,
            partial_json: String::new(),
            thinking_signature: String::new(),
            redacted: false,
            closed: false,
        }
    }
}

#[derive(Clone, Debug)]
enum TerminalStatus {
    Done(DoneReason),
    Error(String),
}

struct StreamAssembly {
    state: AssistantState,
    blocks: BTreeMap<i32, BlockScratch>,
    usage: Usage,
    saw_message_start: bool,
    terminal: Option<TerminalStatus>,
}

impl StreamAssembly {
    fn new(message: AssistantMessage) -> Self {
        Self {
            state: AssistantState::new(message),
            blocks: BTreeMap::new(),
            usage: Usage::default(),
            saw_message_start: false,
            terminal: None,
        }
    }

    fn snapshot(&self) -> Arc<AssistantMessage> {
        self.state.snapshot()
    }

    fn process(
        &mut self,
        event: ConverseStreamOutput,
    ) -> Result<Vec<AssistantMessageEvent>, AdapterFailure> {
        match event {
            ConverseStreamOutput::MessageStart(event) => {
                if self.saw_message_start {
                    return Err(AdapterFailure::Semantic(
                        "Bedrock stream emitted more than one message_start".to_owned(),
                    ));
                }
                if event.role() != &ConversationRole::Assistant {
                    return Err(AdapterFailure::Semantic(
                        "Unexpected assistant message start but got user message start instead"
                            .to_owned(),
                    ));
                }
                self.saw_message_start = true;
                Ok(Vec::new())
            }
            ConverseStreamOutput::ContentBlockStart(event) => {
                self.require_active_message()?;
                let index = event.content_block_index();
                if self.blocks.contains_key(&index) {
                    return Err(AdapterFailure::Semantic(format!(
                        "Bedrock content block {index} started more than once"
                    )));
                }
                let Some(ContentBlockStart::ToolUse(tool)) = event.start() else {
                    return Err(AdapterFailure::Semantic(format!(
                        "Unsupported Bedrock content block start at index {index}"
                    )));
                };
                let semantic = self
                    .state
                    .start_tool_call(normalize_tool_call_id(tool.tool_use_id()), tool.name())
                    .map_err(|error| AdapterFailure::Semantic(error.to_string()))?;
                let content_index = event_content_index(&semantic)?;
                self.blocks
                    .insert(index, BlockScratch::new(content_index, BlockKind::Tool));
                Ok(vec![semantic])
            }
            ConverseStreamOutput::ContentBlockDelta(event) => {
                self.require_active_message()?;
                let index = event.content_block_index();
                let delta = event.delta().ok_or_else(|| {
                    AdapterFailure::Semantic(format!(
                        "Bedrock content block delta {index} had no delta payload"
                    ))
                })?;
                self.process_delta(index, delta)
            }
            ConverseStreamOutput::ContentBlockStop(event) => {
                self.require_active_message()?;
                self.stop_block(event.content_block_index())
            }
            ConverseStreamOutput::MessageStop(event) => {
                if !self.saw_message_start {
                    return Err(AdapterFailure::Semantic(
                        "Bedrock stream emitted message_stop before message_start".to_owned(),
                    ));
                }
                if self.terminal.is_some() {
                    return Err(AdapterFailure::Semantic(
                        "Bedrock stream emitted more than one message_stop".to_owned(),
                    ));
                }
                self.terminal = Some(match map_stop_reason(event.stop_reason()) {
                    Ok(reason) => TerminalStatus::Done(reason),
                    Err(message) => TerminalStatus::Error(message),
                });
                Ok(Vec::new())
            }
            ConverseStreamOutput::Metadata(event) => {
                self.update_usage(&event);
                Ok(Vec::new())
            }
            _ => Err(AdapterFailure::Semantic(
                "Unsupported Bedrock ConverseStream event".to_owned(),
            )),
        }
    }

    fn require_active_message(&self) -> Result<(), AdapterFailure> {
        if !self.saw_message_start {
            return Err(AdapterFailure::Semantic(
                "Bedrock content event arrived before message_start".to_owned(),
            ));
        }
        if self.terminal.is_some() {
            return Err(AdapterFailure::Semantic(
                "Bedrock content event arrived after message_stop".to_owned(),
            ));
        }
        Ok(())
    }

    fn process_delta(
        &mut self,
        provider_index: i32,
        delta: &ContentBlockDelta,
    ) -> Result<Vec<AssistantMessageEvent>, AdapterFailure> {
        match delta {
            ContentBlockDelta::Text(text) => {
                let mut events = Vec::with_capacity(2);
                if !self.blocks.contains_key(&provider_index) {
                    let start = self
                        .state
                        .start_text()
                        .map_err(|error| AdapterFailure::Semantic(error.to_string()))?;
                    let content_index = event_content_index(&start)?;
                    self.blocks.insert(
                        provider_index,
                        BlockScratch::new(content_index, BlockKind::Text),
                    );
                    events.push(start);
                }
                let block = self.open_block(provider_index, BlockKind::Text)?;
                events.push(
                    self.state
                        .text_delta(block.content_index, text)
                        .map_err(|error| AdapterFailure::Semantic(error.to_string()))?,
                );
                Ok(events)
            }
            ContentBlockDelta::ToolUse(tool) => {
                let content_index = {
                    let block = self.open_block_mut(provider_index, BlockKind::Tool)?;
                    block.partial_json.push_str(tool.input());
                    block.content_index
                };
                let event = self
                    .state
                    .tool_call_delta(content_index, tool.input())
                    .map_err(|error| AdapterFailure::Semantic(error.to_string()))?;
                Ok(vec![event])
            }
            ContentBlockDelta::ReasoningContent(reasoning) => {
                let mut events = Vec::with_capacity(2);
                if !self.blocks.contains_key(&provider_index) {
                    let start = self
                        .state
                        .start_thinking()
                        .map_err(|error| AdapterFailure::Semantic(error.to_string()))?;
                    let content_index = event_content_index(&start)?;
                    self.blocks.insert(
                        provider_index,
                        BlockScratch::new(content_index, BlockKind::Thinking),
                    );
                    events.push(start);
                }
                let content_index = self
                    .open_block(provider_index, BlockKind::Thinking)?
                    .content_index;
                match reasoning {
                    ReasoningContentBlockDelta::Text(text) => events.push(
                        self.state
                            .thinking_delta(content_index, text)
                            .map_err(|error| AdapterFailure::Semantic(error.to_string()))?,
                    ),
                    ReasoningContentBlockDelta::Signature(signature) => {
                        self.open_block_mut(provider_index, BlockKind::Thinking)?
                            .thinking_signature
                            .push_str(signature);
                    }
                    ReasoningContentBlockDelta::RedactedContent(data) => {
                        let block = self.open_block_mut(provider_index, BlockKind::Thinking)?;
                        block.redacted = true;
                        block.thinking_signature =
                            base64::engine::general_purpose::STANDARD.encode(data.as_ref());
                    }
                    _ => {
                        return Err(AdapterFailure::Semantic(
                            "Unsupported Bedrock reasoning delta".to_owned(),
                        ));
                    }
                }
                Ok(events)
            }
            _ => Err(AdapterFailure::Semantic(
                "Unsupported Bedrock content block delta".to_owned(),
            )),
        }
    }

    fn stop_block(
        &mut self,
        provider_index: i32,
    ) -> Result<Vec<AssistantMessageEvent>, AdapterFailure> {
        let block = self.blocks.get_mut(&provider_index).ok_or_else(|| {
            AdapterFailure::Semantic(format!(
                "Bedrock content block {provider_index} stopped before it started"
            ))
        })?;
        if block.closed {
            return Err(AdapterFailure::Semantic(format!(
                "Bedrock content block {provider_index} stopped more than once"
            )));
        }
        block.closed = true;
        let content_index = block.content_index;
        let kind = block.kind;
        let partial_json = block.partial_json.clone();
        let event = match kind {
            BlockKind::Text => self.state.end_text(content_index),
            BlockKind::Thinking => self.state.end_thinking(content_index),
            BlockKind::Tool => self
                .state
                .end_tool_call(content_index, parse_streaming_json(&partial_json)),
        }
        .map_err(|error| AdapterFailure::Semantic(error.to_string()))?;
        Ok(vec![event])
    }

    fn ensure_open_block(
        &self,
        provider_index: i32,
        expected: BlockKind,
    ) -> Result<(), AdapterFailure> {
        let block = self.blocks.get(&provider_index).ok_or_else(|| {
            AdapterFailure::Semantic(format!(
                "Bedrock content block {provider_index} emitted a delta before it started"
            ))
        })?;
        if block.closed {
            return Err(AdapterFailure::Semantic(format!(
                "Bedrock content block {provider_index} emitted a delta after it stopped"
            )));
        }
        if block.kind != expected {
            return Err(AdapterFailure::Semantic(format!(
                "Bedrock content block {provider_index} changed type"
            )));
        }
        Ok(())
    }

    fn open_block(
        &self,
        provider_index: i32,
        expected: BlockKind,
    ) -> Result<&BlockScratch, AdapterFailure> {
        self.ensure_open_block(provider_index, expected)?;
        self.blocks.get(&provider_index).ok_or_else(|| {
            AdapterFailure::Semantic(format!(
                "Bedrock content block {provider_index} emitted a delta before it started"
            ))
        })
    }

    fn open_block_mut(
        &mut self,
        provider_index: i32,
        expected: BlockKind,
    ) -> Result<&mut BlockScratch, AdapterFailure> {
        self.ensure_open_block(provider_index, expected)?;
        self.blocks.get_mut(&provider_index).ok_or_else(|| {
            AdapterFailure::Semantic(format!(
                "Bedrock content block {provider_index} emitted a delta before it started"
            ))
        })
    }

    fn update_usage(&mut self, event: &ConverseStreamMetadataEvent) {
        let Some(usage) = event.usage() else {
            return;
        };
        self.usage.input = non_negative_tokens(usage.input_tokens());
        self.usage.output = non_negative_tokens(usage.output_tokens());
        self.usage.cache_read = usage
            .cache_read_input_tokens()
            .map_or(0, non_negative_tokens);
        self.usage.cache_write = usage
            .cache_write_input_tokens()
            .map_or(0, non_negative_tokens);
        self.usage.total_tokens = if usage.total_tokens() >= 0 {
            non_negative_tokens(usage.total_tokens())
        } else {
            self.usage.input.saturating_add(self.usage.output)
        };
    }

    fn required_terminal(&self) -> Result<DoneReason, AdapterFailure> {
        if !self.saw_message_start {
            return Err(AdapterFailure::Semantic(
                "Bedrock ConverseStream ended before message_start".to_owned(),
            ));
        }
        match &self.terminal {
            Some(TerminalStatus::Done(reason)) => Ok(*reason),
            Some(TerminalStatus::Error(message)) => Err(AdapterFailure::Semantic(message.clone())),
            None => Err(AdapterFailure::Semantic(
                "Bedrock ConverseStream ended before message_stop".to_owned(),
            )),
        }
    }

    fn finish(&mut self, reason: DoneReason, model: &Model, aborted: bool) -> AssistantMessage {
        let mut message = self.state.finish(reason);
        self.patch_final_message(&mut message, model, aborted);
        message
    }

    fn fail(
        &mut self,
        reason: ErrorReason,
        error: impl Into<String>,
        model: &Model,
    ) -> AssistantMessage {
        let aborted = reason == ErrorReason::Aborted;
        let mut message = self.state.fail(reason, error);
        self.patch_final_message(&mut message, model, aborted);
        message
    }

    fn patch_final_message(&self, message: &mut AssistantMessage, model: &Model, aborted: bool) {
        for block in self.blocks.values() {
            let Ok(index) = usize::try_from(block.content_index) else {
                continue;
            };
            match message.content.get_mut(index) {
                Some(AssistantContent::Thinking(thinking)) if block.kind == BlockKind::Thinking => {
                    if !block.thinking_signature.is_empty() {
                        thinking.thinking_signature = Some(block.thinking_signature.clone());
                    }
                    if block.redacted {
                        thinking.redacted = Some(true);
                    }
                }
                Some(AssistantContent::ToolCall(tool_call)) if block.kind == BlockKind::Tool => {
                    tool_call.arguments = parse_streaming_json(&block.partial_json);
                }
                _ => {}
            }
        }
        if aborted {
            message.usage = Usage::default();
        } else {
            message.usage = self.usage.clone();
            calculate_cost(model, &mut message.usage);
        }
    }
}

fn event_content_index(event: &AssistantMessageEvent) -> Result<u64, AdapterFailure> {
    match event {
        AssistantMessageEvent::TextStart { content_index, .. }
        | AssistantMessageEvent::ThinkingStart { content_index, .. }
        | AssistantMessageEvent::ToolCallStart { content_index, .. } => Ok(*content_index),
        _ => Err(AdapterFailure::Semantic(
            "Bedrock adapter created an invalid block start event".to_owned(),
        )),
    }
}

fn non_negative_tokens(tokens: i32) -> u64 {
    u64::try_from(tokens).unwrap_or_default()
}

fn map_stop_reason(
    reason: &aws_sdk_bedrockruntime::types::StopReason,
) -> Result<DoneReason, String> {
    match reason.as_str() {
        "end_turn" | "stop_sequence" => Ok(DoneReason::Stop),
        "max_tokens" | "model_context_window_exceeded" => Ok(DoneReason::Length),
        "tool_use" => Ok(DoneReason::ToolUse),
        other => Err(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_bedrockruntime::types::{
        ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent,
        ConverseStreamMetadataEvent, MessageStartEvent, MessageStopEvent, StopReason, TokenUsage,
        ToolUseBlockDelta, ToolUseBlockStart,
    };
    use futures::StreamExt as _;

    use crate::types::{
        AssistantContent, ModelCost, ModelInput, TextContent, ThinkingContent, Tool, ToolCall,
        ToolResultMessage, UserMessage,
    };

    fn test_model(id: &str, name: &str) -> Model {
        Model {
            id: id.to_owned(),
            name: name.to_owned(),
            api: API.to_owned(),
            provider: "amazon-bedrock".to_owned(),
            base_url: "https://bedrock-runtime.us-west-2.amazonaws.com".to_owned(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 1.25,
                tiers: None,
            },
            context_window: 200_000,
            max_tokens: 8_192,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    fn assistant_message(model: &Model) -> AssistantMessage {
        AssistantMessage::new(API, model.provider.clone(), model.id.clone(), 0)
    }

    fn must_build<T, E: std::fmt::Display>(result: Result<T, E>) -> Result<T, String> {
        result.map_err(|error| format!("SDK test builder failed: {error}"))
    }

    #[test]
    fn request_conversion_distinguishes_claude_thinking_and_cache() -> Result<(), String> {
        let model = test_model("us.anthropic.claude-sonnet-4-6-v1:0", "Claude Sonnet 4.6");
        let mut prior = assistant_message(&model);
        let mut thinking = ThinkingContent::new("reasoning");
        thinking.thinking_signature = Some("signature".to_owned());
        prior.content.push(AssistantContent::Thinking(thinking));
        prior
            .content
            .push(AssistantContent::Text(TextContent::new("answer")));
        let context = Context {
            system_prompt: Some("system".to_owned()),
            messages: vec![
                Message::User(UserMessage::new(
                    UserMessageContent::Text("hello".to_owned()),
                    0,
                )),
                Message::Assistant(prior),
            ],
            tools: None,
        };
        let mut options = StreamOptions {
            cache_retention: Some(CacheRetention::Long),
            ..StreamOptions::default()
        };
        options.insert_extra(StreamOptionKey::REASONING, Value::String("high".to_owned()));

        let payload = build_request_payload(&model, &context, &options)
            .map_err(|error| format!("request conversion failed: {error}"))?;
        assert_eq!(
            payload.pointer("/additionalModelRequestFields/thinking/type"),
            Some(&Value::String("adaptive".to_owned()))
        );
        assert_eq!(
            payload.pointer("/additionalModelRequestFields/output_config/effort"),
            Some(&Value::String("high".to_owned()))
        );
        assert_eq!(
            payload.pointer("/system/1/cachePoint/ttl"),
            Some(&Value::String("1h".to_owned()))
        );
        assert_eq!(
            payload.pointer("/messages/1/content/0/reasoningContent/reasoningText/signature"),
            Some(&Value::String("signature".to_owned()))
        );
        Ok(())
    }

    #[test]
    fn non_claude_omits_signature_thinking_fields_and_explicit_cache_points() -> Result<(), String>
    {
        let model = test_model("qwen.qwen3", "Qwen 3");
        let mut prior = assistant_message(&model);
        let mut thinking = ThinkingContent::new("reasoning");
        thinking.thinking_signature = Some("must-not-be-sent".to_owned());
        prior.content.push(AssistantContent::Thinking(thinking));
        let context = Context {
            system_prompt: Some("system".to_owned()),
            messages: vec![Message::Assistant(prior)],
            tools: None,
        };
        let mut options = StreamOptions::default();
        options.insert_extra(StreamOptionKey::REASONING, Value::String("high".to_owned()));

        let payload = build_request_payload(&model, &context, &options)
            .map_err(|error| format!("request conversion failed: {error}"))?;
        assert!(payload.get("additionalModelRequestFields").is_none());
        assert_eq!(payload.pointer("/system/1"), None);
        assert_eq!(
            payload.pointer("/messages/0/content/0/reasoningContent/reasoningText/text"),
            Some(&Value::String("reasoning".to_owned()))
        );
        assert_eq!(
            payload.pointer("/messages/0/content/0/reasoningContent/reasoningText/signature"),
            None
        );
        Ok(())
    }
    #[test]
    fn redacted_reasoning_replays_without_display_text() -> Result<(), String> {
        let model = test_model("us.anthropic.claude-sonnet-4-6-v1:0", "Claude Sonnet 4.6");
        let mut prior = assistant_message(&model);
        let mut thinking = ThinkingContent::new(String::new());
        thinking.redacted = Some(true);
        thinking.thinking_signature = Some("opaque-signature".to_owned());
        prior.content.push(AssistantContent::Thinking(thinking));
        let context = Context {
            system_prompt: None,
            messages: vec![Message::Assistant(prior)],
            tools: None,
        };
        let payload = build_request_payload(&model, &context, &StreamOptions::default())
            .map_err(|error| format!("request conversion failed: {error}"))?;
        assert_eq!(
            payload.pointer("/messages/0/content/0/reasoningContent/redactedContent"),
            Some(&Value::String("opaque-signature".to_owned()))
        );
        Ok(())
    }

    #[test]
    fn request_normalizes_tool_ids_and_coalesces_tool_results() -> Result<(), String> {
        let model = test_model("anthropic.claude-3-7-sonnet", "Claude 3.7 Sonnet");
        let original_id = format!("call|{}", "x".repeat(80));
        let normalized = normalize_tool_call_id(&original_id);
        let mut prior = assistant_message(&model);
        prior.content.push(AssistantContent::ToolCall(ToolCall::new(
            original_id.clone(),
            "read",
            Map::new(),
        )));
        let result_one = ToolResultMessage::new(
            original_id.clone(),
            "read",
            vec![ToolResultContent::Text(TextContent::new("one"))],
            false,
            0,
        );
        let result_two = ToolResultMessage::new(
            "second.bad|id",
            "read",
            vec![ToolResultContent::Text(TextContent::new("two"))],
            false,
            0,
        );
        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::Assistant(prior),
                Message::ToolResult(result_one),
                Message::ToolResult(result_two),
            ],
            tools: Some(vec![Tool {
                name: "read".to_owned(),
                description: "read a file".to_owned(),
                parameters: json!({ "type": "object" }),
            }]),
        };
        let options = StreamOptions {
            cache_retention: Some(CacheRetention::None),
            ..StreamOptions::default()
        };
        let payload = build_request_payload(&model, &context, &options)
            .map_err(|error| format!("request conversion failed: {error}"))?;

        assert_eq!(normalized.len(), 64);
        assert_eq!(
            payload.pointer("/messages/0/content/0/toolUse/toolUseId"),
            Some(&Value::String(normalized.clone()))
        );
        assert_eq!(
            payload.pointer("/messages/1/content/0/toolResult/toolUseId"),
            Some(&Value::String(normalized))
        );
        assert_eq!(
            payload
                .pointer("/messages/1/content")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
        Ok(())
    }

    #[test]
    fn object_events_finalize_tool_id_arguments_usage_and_cost() -> Result<(), String> {
        let model = test_model("anthropic.claude-3-7-sonnet", "Claude 3.7 Sonnet");
        let mut assembly = StreamAssembly::new(assistant_message(&model));

        let start = must_build(
            MessageStartEvent::builder()
                .role(ConversationRole::Assistant)
                .build(),
        )?;
        let events = assembly
            .process(ConverseStreamOutput::MessageStart(start))
            .map_err(|error| format!("message start failed: {error}"))?;
        assert!(events.is_empty());

        let tool_start = must_build(
            ToolUseBlockStart::builder()
                .tool_use_id("tool|bad")
                .name("read")
                .build(),
        )?;
        let block_start = must_build(
            ContentBlockStartEvent::builder()
                .content_block_index(3)
                .start(ContentBlockStart::ToolUse(tool_start))
                .build(),
        )?;
        let events = assembly
            .process(ConverseStreamOutput::ContentBlockStart(block_start))
            .map_err(|error| format!("content start failed: {error}"))?;
        assert!(matches!(
            events.as_slice(),
            [AssistantMessageEvent::ToolCallStart { .. }]
        ));

        let delta = must_build(
            ToolUseBlockDelta::builder()
                .input("{\"path\":\"x\"}")
                .build(),
        )?;
        let delta_event = must_build(
            ContentBlockDeltaEvent::builder()
                .content_block_index(3)
                .delta(ContentBlockDelta::ToolUse(delta))
                .build(),
        )?;
        assembly
            .process(ConverseStreamOutput::ContentBlockDelta(delta_event))
            .map_err(|error| format!("content delta failed: {error}"))?;
        let stop = must_build(
            ContentBlockStopEvent::builder()
                .content_block_index(3)
                .build(),
        )?;
        assembly
            .process(ConverseStreamOutput::ContentBlockStop(stop))
            .map_err(|error| format!("content stop failed: {error}"))?;

        let usage = must_build(
            TokenUsage::builder()
                .input_tokens(100)
                .output_tokens(20)
                .total_tokens(120)
                .cache_read_input_tokens(10)
                .cache_write_input_tokens(5)
                .build(),
        )?;
        let metadata = ConverseStreamMetadataEvent::builder().usage(usage).build();
        assembly
            .process(ConverseStreamOutput::Metadata(metadata))
            .map_err(|error| format!("metadata failed: {error}"))?;
        let message_stop = must_build(
            MessageStopEvent::builder()
                .stop_reason(StopReason::ToolUse)
                .build(),
        )?;
        assembly
            .process(ConverseStreamOutput::MessageStop(message_stop))
            .map_err(|error| format!("message stop failed: {error}"))?;

        let reason = assembly
            .required_terminal()
            .map_err(|error| format!("terminal missing: {error}"))?;
        let message = assembly.finish(reason, &model, false);
        assert_eq!(reason, DoneReason::ToolUse);
        let Some(AssistantContent::ToolCall(tool_call)) = message.content.first() else {
            return Err("expected tool call".to_owned());
        };
        assert_eq!(tool_call.id, "tool_bad");
        assert_eq!(
            tool_call.arguments.get("path"),
            Some(&Value::String("x".to_owned()))
        );
        assert_eq!(message.usage.input, 100);
        assert_eq!(message.usage.output, 20);
        assert_eq!(message.usage.cache_read, 10);
        assert_eq!(message.usage.cache_write, 5);
        assert_eq!(message.usage.total_tokens, 120);
        assert!((message.usage.cost.total - 0.000_147_25).abs() < 1.0e-12);
        Ok(())
    }

    #[test]
    fn eof_without_message_stop_is_an_error() -> Result<(), String> {
        let model = test_model("anthropic.claude-3-7-sonnet", "Claude 3.7 Sonnet");
        let mut assembly = StreamAssembly::new(assistant_message(&model));
        let start = must_build(
            MessageStartEvent::builder()
                .role(ConversationRole::Assistant)
                .build(),
        )?;
        assembly
            .process(ConverseStreamOutput::MessageStart(start))
            .map_err(|error| format!("message start failed: {error}"))?;
        let error = match assembly.required_terminal() {
            Ok(_) => return Err("missing message_stop was accepted".to_owned()),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("message_stop"));
        Ok(())
    }

    struct ErrorFactory;

    impl BedrockClientFactory for ErrorFactory {
        fn create_client(
            &self,
            _request: BedrockClientRequest,
        ) -> BoxFuture<'static, Result<Client, ProviderError>> {
            Box::pin(async { Err(ProviderError::new("factory rejected request")) })
        }
    }

    #[tokio::test]
    async fn ordinary_factory_failure_is_one_error_after_start() {
        let model = test_model("anthropic.claude-3-7-sonnet", "Claude 3.7 Sonnet");
        let provider = BedrockConverseStream::new(Arc::new(ErrorFactory));
        let events = provider
            .stream(&model, Context::default(), StreamOptions::default())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events.first(),
            Some(Ok(AssistantMessageEvent::Start { .. }))
        ));
        assert!(matches!(
            events.get(1),
            Some(Ok(AssistantMessageEvent::Error { .. }))
        ));
    }

    #[tokio::test]
    async fn pre_cancelled_request_is_one_aborted_error_after_start() {
        let model = test_model("anthropic.claude-3-7-sonnet", "Claude 3.7 Sonnet");
        let provider = BedrockConverseStream::new(Arc::new(ErrorFactory));
        let signal = tokio_util::sync::CancellationToken::new();
        signal.cancel();
        let options = StreamOptions {
            signal: Some(signal),
            ..StreamOptions::default()
        };
        let events = provider
            .stream(&model, Context::default(), options)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events.first(),
            Some(Ok(AssistantMessageEvent::Start { .. }))
        ));
        assert!(matches!(
            events.get(1),
            Some(Ok(AssistantMessageEvent::Error {
                reason: ErrorReason::Aborted,
                ..
            }))
        ));
    }

    fn sample_client_request() -> BedrockClientRequest {
        BedrockClientRequest {
            region: "us-west-2".to_owned(),
            endpoint_url: Some("https://bedrock-runtime.us-west-2.amazonaws.com".to_owned()),
            model_id: "anthropic.claude-3-7-sonnet".to_owned(),
            headers: BTreeMap::from([("x-custom".to_owned(), "1".to_owned())]),
            profile: Some("dev".to_owned()),
            static_credentials: Some(BedrockStaticCredentials {
                access_key_id: "AKIAEXAMPLEKEY".to_owned(),
                secret_access_key: "super-secret-value".to_owned(),
                session_token: Some("session-token-value".to_owned()),
            }),
        }
    }

    #[test]
    fn build_client_request_selects_region_endpoint_and_profile() {
        let mut model = test_model("anthropic.claude-3-7-sonnet", "Claude 3.7 Sonnet");
        model.base_url = "https://bedrock-runtime.eu-central-1.amazonaws.com".to_owned();
        let mut options = StreamOptions::default();
        options.insert_extra(
            StreamOptionKey::REGION,
            Value::String("ap-southeast-2".to_owned()),
        );
        options.insert_extra(StreamOptionKey::PROFILE, Value::String("prod".to_owned()));

        let request = build_client_request(&model, &options, model.id.clone());
        assert_eq!(request.region, "ap-southeast-2");
        // Explicit region is configured, so the standard catalog endpoint is not forced.
        assert_eq!(request.endpoint_url, None);
        assert_eq!(request.profile.as_deref(), Some("prod"));
        assert!(request.static_credentials.is_none());
    }

    #[test]
    fn build_client_request_uses_env_overlay_for_profile_and_static_credentials() {
        let model = test_model("anthropic.claude-3-7-sonnet", "Claude 3.7 Sonnet");
        let options = StreamOptions {
            env: Some(BTreeMap::from([
                ("AWS_PROFILE".to_owned(), "from-env".to_owned()),
                ("AWS_ACCESS_KEY_ID".to_owned(), "AKIATESTKEYID".to_owned()),
                (
                    "AWS_SECRET_ACCESS_KEY".to_owned(),
                    "secret-from-env".to_owned(),
                ),
                ("AWS_SESSION_TOKEN".to_owned(), "token-from-env".to_owned()),
                ("AWS_REGION".to_owned(), "ca-central-1".to_owned()),
            ])),
            ..StreamOptions::default()
        };

        let request = build_client_request(&model, &options, model.id.clone());
        assert_eq!(request.region, "ca-central-1");
        assert_eq!(request.profile.as_deref(), Some("from-env"));
        assert_eq!(
            request.static_credentials.as_ref().map(|credentials| {
                (
                    credentials.access_key_id.as_str(),
                    credentials.secret_access_key.as_str(),
                    credentials.session_token.as_deref(),
                )
            }),
            Some(("AKIATESTKEYID", "secret-from-env", Some("token-from-env"),)),
        );
    }

    #[test]
    fn static_credentials_require_both_access_and_secret() {
        let model = test_model("anthropic.claude-3-7-sonnet", "Claude 3.7 Sonnet");
        let options = StreamOptions {
            env: Some(BTreeMap::from([(
                "AWS_ACCESS_KEY_ID".to_owned(),
                "AKIATESTKEYID".to_owned(),
            )])),
            ..StreamOptions::default()
        };
        let request = build_client_request(&model, &options, model.id.clone());
        assert!(request.static_credentials.is_none());
    }

    #[test]
    fn static_credentials_debug_redacts_secrets() {
        let credentials = BedrockStaticCredentials {
            access_key_id: "AKIAEXAMPLEKEY".to_owned(),
            secret_access_key: "super-secret-value".to_owned(),
            session_token: Some("session-token-value".to_owned()),
        };
        let rendered = format!("{credentials:?}");
        assert!(rendered.contains("****EKEY"));
        assert!(!rendered.contains("super-secret-value"));
        assert!(!rendered.contains("session-token-value"));
        assert!(rendered.contains("** redacted **"));

        let request = sample_client_request();
        let rendered_request = format!("{request:?}");
        assert!(!rendered_request.contains("super-secret-value"));
        assert!(!rendered_request.contains("session-token-value"));
    }

    #[test]
    fn default_factory_constructs_for_registry_use() {
        let factory = DefaultBedrockClientFactory::new();
        let _ = DefaultBedrockClientFactory;
        let provider = BedrockConverseStream::with_default_client_factory();
        let trait_object: Arc<dyn BedrockClientFactory> = Arc::new(factory);
        assert!(Arc::strong_count(&trait_object) >= 1);
        let _ = provider;
    }

    #[tokio::test]
    async fn default_factory_prefers_explicit_overlay_credentials_without_network() {
        // Static credentials + explicit endpoint keep aws-config off ambient chain lookups and
        // remote credential providers; the returned value is still the official SDK client type.
        let factory = DefaultBedrockClientFactory::new();
        let request = BedrockClientRequest {
            region: "us-east-1".to_owned(),
            endpoint_url: Some("http://127.0.0.1:9".to_owned()),
            model_id: "anthropic.claude-3-7-sonnet".to_owned(),
            headers: BTreeMap::new(),
            profile: None,
            static_credentials: Some(BedrockStaticCredentials {
                access_key_id: "AKIATEST".to_owned(),
                secret_access_key: "secret".to_owned(),
                session_token: None,
            }),
        };
        match factory.create_client(request).await {
            Ok(client) => {
                let _ = client;
            }
            Err(error) => {
                assert_eq!(
                    error.message(),
                    "",
                    "static overlay credentials should build an official client: {}",
                    error.message()
                );
            }
        }
    }
}
