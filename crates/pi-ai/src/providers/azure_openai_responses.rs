//! Native Azure `OpenAI` Responses API adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{StreamExt, stream::BoxStream};
use reqwest::{Client, Request, Response, Url};
use serde_json::{Value, json};

use super::shared::responses::{
    ConvertMessagesOptions, ConvertToolsOptions, ProcessOptions, ResponsesStreamProcessor,
    convert_messages, convert_tools,
};
use super::shared::truncate_error_body;
use super::transport::{DataSseDecoder, DataSseEvent, HttpTransport, TransportError};
use crate::provider::{Provider, ProviderError, StreamOptionKey, StreamOptions};
use crate::types::{AssistantMessage, Context, ErrorReason, Model, ModelThinkingLevel};

const DEFAULT_API_VERSION: &str = "v1";
const EVENT_CHANNEL_CAPACITY: usize = 64;
const MIN_OUTPUT_TOKENS: u64 = 16;

/// Azure `OpenAI`'s Responses streaming adapter.
#[derive(Clone, Debug)]
pub struct AzureOpenAiResponses {
    transport: HttpTransport,
}

impl AzureOpenAiResponses {
    /// Create an adapter backed by an already-configured reqwest client.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            transport: HttpTransport::new(client),
        }
    }
}

impl Provider for AzureOpenAiResponses {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> BoxStream<'static, Result<crate::types::AssistantMessageEvent, ProviderError>> {
        let (sender, stream) = super::stream_state::ProviderEventSender::channel(
            NonZeroUsize::new(EVENT_CHANNEL_CAPACITY).unwrap_or(NonZeroUsize::MIN),
        );
        let adapter = self.clone();
        let model = model.clone();
        tokio::spawn(async move {
            let message = AssistantMessage::new(
                "azure-openai-responses",
                model.provider.clone(),
                model.id.clone(),
                unix_millis(),
            );
            let mut processor = ResponsesStreamProcessor::new(
                model.clone(),
                message,
                sender,
                ProcessOptions::default(),
            );
            if processor.start().await.is_err() {
                return;
            }
            if let Err(failure) = adapter
                .run(&model, &context, &options, &mut processor)
                .await
            {
                let reason = if options
                    .signal
                    .as_ref()
                    .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
                    || failure.aborted
                {
                    ErrorReason::Aborted
                } else {
                    ErrorReason::Error
                };
                let _terminal = processor
                    .fail(
                        reason,
                        format!("Azure OpenAI API error: {}", failure.message),
                    )
                    .await;
            }
        });
        stream
    }
}

impl AzureOpenAiResponses {
    async fn run(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
        processor: &mut ResponsesStreamProcessor,
    ) -> Result<(), AdapterFailure> {
        let api_key = options
            .api_key
            .as_deref()
            .filter(|key| !key.is_empty())
            .ok_or_else(|| {
                AdapterFailure::new(format!("No API key for provider: {}", model.provider))
            })?;
        let config = resolve_azure_config(model, options)?;
        let deployment = resolve_deployment_name(model, options);
        let mut payload = build_payload(model, context, options, &deployment);
        if let Some(callback) = options.on_payload.as_ref() {
            callback(&mut payload, model)
                .await
                .map_err(|error| AdapterFailure::new(error.to_string()))?;
        }
        let request = build_request(&self.transport, &config, api_key, model, options, &payload)?;
        let response = self
            .transport
            .execute(
                request,
                model,
                options.signal.as_ref(),
                options.on_response.as_ref(),
            )
            .await
            .map_err(AdapterFailure::from_transport)?;
        consume_response(response, options, processor).await
    }
}

async fn consume_response(
    response: Response,
    options: &StreamOptions,
    processor: &mut ResponsesStreamProcessor,
) -> Result<(), AdapterFailure> {
    let status = response.status();
    if !status.is_success() {
        let body = HttpTransport::read_error_body(response, options.signal.as_ref())
            .await
            .map_err(AdapterFailure::from_transport)?;
        return Err(AdapterFailure::new(format!(
            "{}: {}",
            status.as_u16(),
            truncate_error_body(&body)
        )));
    }
    let mut decoder = DataSseDecoder::default();
    let mut body = response.bytes_stream();
    let mut provider_done = false;
    while !provider_done {
        let next = if let Some(signal) = options.signal.as_ref() {
            tokio::select! {
                () = signal.cancelled() => return Err(AdapterFailure::aborted("Request was aborted")),
                next = body.next() => next,
            }
        } else {
            body.next().await
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|error| AdapterFailure::new(error.to_string()))?;
        for event in decoder
            .push(&chunk)
            .map_err(|error| AdapterFailure::new(error.to_string()))?
        {
            match event {
                DataSseEvent::Done => {
                    provider_done = true;
                    break;
                }
                DataSseEvent::Data(data) => {
                    if process_sse_data(data, processor).await? {
                        return Ok(());
                    }
                }
            }
        }
    }
    if !provider_done {
        for event in decoder
            .finish()
            .map_err(|error| AdapterFailure::new(error.to_string()))?
        {
            match event {
                DataSseEvent::Done => break,
                DataSseEvent::Data(data) => {
                    if process_sse_data(data, processor).await? {
                        return Ok(());
                    }
                }
            }
        }
    }
    processor
        .finish()
        .map_err(|error| AdapterFailure::new(error.to_string()))
}

async fn process_sse_data(
    data: String,
    processor: &mut ResponsesStreamProcessor,
) -> Result<bool, AdapterFailure> {
    let value = serde_json::from_str::<Value>(&data)
        .map_err(|error| AdapterFailure::new(format!("invalid Responses event: {error}")))?;
    processor
        .handle(value)
        .await
        .map_err(|error| AdapterFailure::new(error.to_string()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AzureConfig {
    base_url: String,
    api_version: String,
}

fn resolve_azure_config(
    model: &Model,
    options: &StreamOptions,
) -> Result<AzureConfig, AdapterFailure> {
    let api_version = extra_string(options, StreamOptionKey::AZURE_API_VERSION)
        .or_else(|| env_value(options, "AZURE_OPENAI_API_VERSION").map(str::to_owned))
        .unwrap_or_else(|| DEFAULT_API_VERSION.to_owned());
    let explicit_base = extra_string(options, StreamOptionKey::AZURE_BASE_URL).or_else(|| {
        env_value(options, "AZURE_OPENAI_BASE_URL")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    });
    let resource = extra_string(options, StreamOptionKey::AZURE_RESOURCE_NAME)
        .or_else(|| env_value(options, "AZURE_OPENAI_RESOURCE_NAME").map(str::to_owned));
    let base_url = explicit_base
        .or_else(|| resource.map(|resource| format!("https://{resource}.openai.azure.com/openai/v1")))
        .or_else(|| (!model.base_url.trim().is_empty()).then(|| model.base_url.clone()))
        .ok_or_else(|| AdapterFailure::new(
            "Azure OpenAI base URL is required. Set AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME, or pass azureBaseUrl, azureResourceName, or model.baseUrl.",
        ))?;
    Ok(AzureConfig {
        base_url: normalize_azure_base_url(&base_url)?,
        api_version,
    })
}

fn normalize_azure_base_url(base_url: &str) -> Result<String, AdapterFailure> {
    let mut url = Url::parse(base_url.trim().trim_end_matches('/'))
        .map_err(|_| AdapterFailure::new(format!("Invalid Azure OpenAI base URL: {base_url}")))?;
    let host = url.host_str().unwrap_or_default();
    let azure_host = host.ends_with(".openai.azure.com")
        || host.ends_with(".cognitiveservices.azure.com")
        || host.ends_with(".ai.azure.com");
    let path = url.path().trim_end_matches('/');
    if azure_host && matches!(path, "" | "/" | "/openai" | "/openai/v1/responses") {
        url.set_path("/openai/v1");
        url.set_query(None);
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn resolve_deployment_name(model: &Model, options: &StreamOptions) -> String {
    if let Some(deployment) = extra_string(options, StreamOptionKey::AZURE_DEPLOYMENT_NAME) {
        return deployment;
    }
    let mapping = env_value(options, "AZURE_OPENAI_DEPLOYMENT_NAME_MAP").unwrap_or("");
    for entry in mapping
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        if let Some((model_id, deployment)) = entry.split_once('=')
            && model_id.trim() == model.id
            && !deployment.trim().is_empty()
        {
            return deployment.trim().to_owned();
        }
    }
    model.id.clone()
}

fn build_request(
    transport: &HttpTransport,
    config: &AzureConfig,
    api_key: &str,
    model: &Model,
    options: &StreamOptions,
    payload: &Value,
) -> Result<Request, AdapterFailure> {
    let mut url = Url::parse(&format!(
        "{}/responses",
        config.base_url.trim_end_matches('/')
    ))
    .map_err(|error| AdapterFailure::new(error.to_string()))?;
    url.query_pairs_mut()
        .append_pair("api-version", &config.api_version);
    let mut headers = model.headers.clone().unwrap_or_default();
    headers.insert("api-key".into(), api_key.to_owned());
    merge_option_headers(&mut headers, options.headers.as_ref());
    let mut builder = transport.post(url).json(&payload);
    for (name, value) in headers {
        builder = builder.header(&name, &value);
    }
    if let Some(timeout_ms) = options.timeout_ms {
        builder = builder.timeout(Duration::from_millis(timeout_ms));
    }
    builder
        .build()
        .map_err(|error| AdapterFailure::new(error.to_string()))
}

fn build_payload(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    deployment: &str,
) -> Value {
    let allowed: BTreeSet<String> = [
        "openai",
        "openai-codex",
        "opencode",
        "azure-openai-responses",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let input = convert_messages(
        model,
        context,
        &allowed,
        &ConvertMessagesOptions {
            include_system_prompt: true,
            deferred_tools: BTreeMap::new(),
        },
    );
    let mut payload = json!({
        "model": deployment,
        "input": input,
        "stream": true,
        "store": false,
    });
    if let Some(session_id) = options.session_id.as_deref() {
        payload["prompt_cache_key"] = Value::String(session_id.chars().take(64).collect());
    }
    if let Some(max_tokens) = options.max_tokens {
        payload["max_output_tokens"] = Value::from(max_tokens.max(MIN_OUTPUT_TOKENS));
    }
    if let Some(temperature) = options.temperature {
        payload["temperature"] = Value::from(temperature);
    }
    if let Some(tools) = context.tools.as_deref().filter(|tools| !tools.is_empty()) {
        payload["tools"] = Value::Array(convert_tools(tools, ConvertToolsOptions::default()));
    }
    apply_reasoning(model, options, &mut payload);
    payload
}

fn apply_reasoning(model: &Model, options: &StreamOptions, payload: &mut Value) {
    if !model.reasoning {
        return;
    }
    let effort = extra_string(options, StreamOptionKey::REASONING_EFFORT);
    let summary = options
        .extra_value(StreamOptionKey::REASONING_SUMMARY)
        .and_then(Value::as_str);
    if effort.is_some() || summary.is_some() {
        let effort = effort
            .as_deref()
            .map_or_else(|| "medium".into(), |value| map_thinking_level(model, value));
        payload["reasoning"] = json!({
            "effort": effort,
            "summary": summary.unwrap_or("auto"),
        });
        payload["include"] = json!(["reasoning.encrypted_content"]);
    } else {
        let off = model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(&ModelThinkingLevel::Off));
        if !matches!(off, Some(None)) {
            payload["reasoning"] = json!({
                "effort": off.and_then(Clone::clone).unwrap_or_else(|| "none".into()),
            });
        }
    }
}

fn map_thinking_level(model: &Model, value: &str) -> String {
    let level = match value {
        "minimal" => Some(ModelThinkingLevel::Minimal),
        "low" => Some(ModelThinkingLevel::Low),
        "medium" => Some(ModelThinkingLevel::Medium),
        "high" => Some(ModelThinkingLevel::High),
        "xhigh" => Some(ModelThinkingLevel::Xhigh),
        "max" => Some(ModelThinkingLevel::Max),
        _ => None,
    };
    level
        .and_then(|level| model.thinking_level_map.as_ref()?.get(&level)?.clone())
        .unwrap_or_else(|| value.to_owned())
}

fn extra_string(options: &StreamOptions, key: StreamOptionKey) -> Option<String> {
    options
        .extra_value(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn env_value<'a>(options: &'a StreamOptions, name: &str) -> Option<&'a str> {
    options
        .env
        .as_ref()
        .and_then(|environment| environment.get(name))
        .map(String::as_str)
}

fn merge_option_headers(
    headers: &mut BTreeMap<String, String>,
    overrides: Option<&BTreeMap<String, Option<String>>>,
) {
    for (name, value) in overrides.into_iter().flatten() {
        if let Some(existing) = headers
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(name))
            .cloned()
        {
            headers.remove(&existing);
        }
        if let Some(value) = value {
            headers.insert(name.clone(), value.clone());
        }
    }
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
struct AdapterFailure {
    message: String,
    aborted: bool,
}

impl AdapterFailure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            aborted: false,
        }
    }

    fn aborted(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            aborted: true,
        }
    }

    fn from_transport(error: TransportError) -> Self {
        match error {
            TransportError::Cancelled => Self::aborted("request cancelled"),
            TransportError::Request(error) => Self::new(format!("request failed: {error}")),
            TransportError::Callback(error) => {
                Self::new(format!("response callback failed: {error}"))
            }
            TransportError::Body(error) => Self::new(format!("response body failed: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCost, ModelInput};

    fn model(base_url: &str) -> Model {
        Model {
            id: "gpt-5".into(),
            name: "GPT-5".into(),
            api: "azure-openai-responses".into(),
            provider: "azure-openai-responses".into(),
            base_url: base_url.into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 8_192,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn azure_urls_resource_and_api_version_are_resolved() -> Result<(), String> {
        let mut options = StreamOptions::default();
        options.insert_extra(
            StreamOptionKey::AZURE_RESOURCE_NAME,
            Value::String("resource".into()),
        );
        options.insert_extra(
            StreamOptionKey::AZURE_API_VERSION,
            Value::String("2026-01-01".into()),
        );
        let config =
            resolve_azure_config(&model(""), &options).map_err(|error| error.message.clone())?;
        assert_eq!(
            config.base_url,
            "https://resource.openai.azure.com/openai/v1"
        );
        assert_eq!(config.api_version, "2026-01-01");
        let normalized = normalize_azure_base_url(
            "https://x.openai.azure.com/openai/v1/responses?api-version=old",
        )
        .map_err(|error| error.message.clone())?;
        assert_eq!(normalized, "https://x.openai.azure.com/openai/v1");
        Ok(())
    }

    #[test]
    fn deployment_map_and_payload_are_azure_specific() {
        let mut options = StreamOptions {
            session_id: Some("session".into()),
            ..StreamOptions::default()
        };
        options.env = Some(BTreeMap::from([(
            "AZURE_OPENAI_DEPLOYMENT_NAME_MAP".into(),
            "gpt-5=production,other=x".into(),
        )]));
        let model = model("https://x.openai.azure.com");
        let deployment = resolve_deployment_name(&model, &options);
        assert_eq!(deployment, "production");
        let payload = build_payload(&model, &Context::default(), &options, &deployment);
        assert_eq!(payload["model"], "production");
        assert_eq!(payload["prompt_cache_key"], "session");
        assert!(payload.get("prompt_cache_retention").is_none());
        assert_eq!(payload["store"], false);
    }

    #[test]
    fn done_marker_is_terminal_for_azure_decoder() -> Result<(), String> {
        let mut decoder = DataSseDecoder::default();
        let events = decoder
            .push(
                b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"azure\",\"status\":\"completed\",\"output\":[]}}\n\ndata: [DONE]\n\n",
            )
            .map_err(|error| error.to_string())?;
        assert!(matches!(
            events.as_slice(),
            [DataSseEvent::Data(_), DataSseEvent::Done]
        ));
        Ok(())
    }
    #[test]
    fn deployment_precedence_skips_empty_mappings() {
        fn mapped_options(mapping: &str) -> StreamOptions {
            StreamOptions {
                env: Some(BTreeMap::from([(
                    "AZURE_OPENAI_DEPLOYMENT_NAME_MAP".into(),
                    mapping.into(),
                )])),
                ..StreamOptions::default()
            }
        }
        let model = model("https://x.openai.azure.com");
        // Explicit option wins over every mapping.
        let mut options = mapped_options("gpt-5=mapped");
        options.insert_extra(
            StreamOptionKey::AZURE_DEPLOYMENT_NAME,
            Value::String("explicit".into()),
        );
        assert_eq!(resolve_deployment_name(&model, &options), "explicit");
        // Empty mapped names fall through to the model id, like the reference.
        let options = mapped_options("gpt-5=,other=x");
        assert_eq!(resolve_deployment_name(&model, &options), "gpt-5");
        // A later entry still matches after an empty one is skipped.
        let options = mapped_options("gpt-5=,gpt-5=production");
        assert_eq!(resolve_deployment_name(&model, &options), "production");
    }

    #[test]
    fn max_tokens_clamp_and_reasoning_defaults_are_exact() {
        let mut reasoning = model("https://x.openai.azure.com");
        reasoning.reasoning = true;
        let deployment = "gpt-5".to_owned();
        // Sub-minimum requests clamp to 16; effort maps with auto summary.
        let mut options = StreamOptions {
            max_tokens: Some(4),
            ..StreamOptions::default()
        };
        options.insert_extra(
            StreamOptionKey::REASONING_EFFORT,
            Value::String("high".into()),
        );
        let payload = build_payload(&reasoning, &Context::default(), &options, &deployment);
        assert_eq!(payload["max_output_tokens"], 16);
        assert_eq!(payload["reasoning"]["effort"], "high");
        assert_eq!(payload["reasoning"]["summary"], "auto");
        assert_eq!(payload["include"], json!(["reasoning.encrypted_content"]));
        // No effort and no summary pin the off-map value when off is unset.
        let payload = build_payload(
            &reasoning,
            &Context::default(),
            &StreamOptions::default(),
            &deployment,
        );
        assert_eq!(payload["reasoning"]["effort"], "none");
    }
}
