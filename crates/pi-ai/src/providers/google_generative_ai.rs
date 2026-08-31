//! Native Google Gemini API `GenerateContent` adapter.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::stream::BoxStream;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

use crate::provider::{Provider, ProviderError, StreamOptionKey, StreamOptions};
use crate::types::{AssistantMessage, AssistantMessageEvent, Context, Model, ModelThinkingLevel};

use super::shared::google::{
    EVENT_CAPACITY, GoogleFailure, GoogleThinkingLevel, build_request_body, consume_response,
    emit_failure,
};
use super::shared::truncate_error_body;
use super::stream_state::ProviderEventSender;
use super::transport::{HttpTransport, TransportError};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const API_KEY_HEADER: &str = "x-goog-api-key";

/// Streams Google's public Gemini `GenerateContent` SSE API.
#[derive(Clone, Debug)]
pub struct GoogleGenerativeAi {
    transport: HttpTransport,
    tool_call_counter: Arc<AtomicU64>,
}

impl GoogleGenerativeAi {
    /// Construct an adapter around a configured HTTP client.
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            transport: HttpTransport::new(client),
            tool_call_counter: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Provider for GoogleGenerativeAi {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
        let model = model.clone();
        let transport = self.transport.clone();
        let tool_call_counter = Arc::clone(&self.tool_call_counter);
        let (sender, stream) = ProviderEventSender::channel(
            NonZeroUsize::new(EVENT_CAPACITY).unwrap_or(NonZeroUsize::MIN),
        );
        tokio::spawn(async move {
            let mut output = AssistantMessage::new(
                model.api.clone(),
                model.provider.clone(),
                model.id.clone(),
                unix_millis(),
            );
            if sender.start(Arc::new(output.clone())).await.is_err() {
                return;
            }
            if let Err(failure) = run_request(
                &transport,
                &model,
                context,
                &options,
                &sender,
                &mut output,
                &tool_call_counter,
            )
            .await
            {
                emit_failure(&sender, &mut output, failure).await;
            }
        });
        stream
    }
}

async fn run_request(
    transport: &HttpTransport,
    model: &Model,
    context: Context,
    options: &StreamOptions,
    sender: &ProviderEventSender,
    output: &mut AssistantMessage,
    tool_call_counter: &AtomicU64,
) -> Result<(), GoogleFailure> {
    let api_key = options
        .api_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            GoogleFailure::error(format!("No API key for provider: {}", model.provider))
        })?;
    if options
        .signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        return Err(GoogleFailure::aborted());
    }

    let thinking = thinking_config(model, options)?;
    let mut payload = build_request_body(model, &context, options, thinking);
    if let Some(callback) = &options.on_payload {
        callback(&mut payload, model)
            .await
            .map_err(|error| GoogleFailure::error(error.to_string()))?;
    }

    let endpoint = endpoint(model)?;
    let headers = build_headers(model, options, api_key)?;
    let mut request = transport.post(endpoint).headers(headers).json(&payload);
    if let Some(timeout_ms) = options.timeout_ms {
        request = request.timeout(Duration::from_millis(timeout_ms));
    }
    let request = request.build().map_err(|error| {
        GoogleFailure::error(format!(
            "failed to build Google Generative AI request: {error}"
        ))
    })?;
    let response = transport
        .execute(
            request,
            model,
            options.signal.as_ref(),
            options.on_response.as_ref(),
        )
        .await
        .map_err(map_transport_error)?;
    let status = response.status();
    if !status.is_success() {
        let body = HttpTransport::read_error_body(response, options.signal.as_ref())
            .await
            .map_err(map_transport_error)?;
        return Err(GoogleFailure::error(format!(
            "Google Generative AI API error ({}): {}",
            status.as_u16(),
            truncate_error_body(&body)
        )));
    }

    consume_response(response, model, options, sender, output, tool_call_counter).await
}

fn map_transport_error(error: TransportError) -> GoogleFailure {
    match error {
        TransportError::Cancelled => GoogleFailure::aborted(),
        TransportError::Request(error) | TransportError::Body(error) => {
            GoogleFailure::error(format!("Google Generative AI request failed: {error}"))
        }
        TransportError::Callback(error) => GoogleFailure::error(error.to_string()),
    }
}

fn endpoint(model: &Model) -> Result<String, GoogleFailure> {
    let base = model.base_url.trim();
    let base = if base.is_empty() {
        DEFAULT_BASE_URL
    } else {
        base
    };
    reqwest::Url::parse(base)
        .map_err(|error| GoogleFailure::error(format!("invalid Google base URL: {error}")))?;
    let model_path = google_model_path(&model.id)?;
    Ok(format!(
        "{}/{model_path}:streamGenerateContent?alt=sse",
        base.trim_end_matches('/')
    ))
}

fn google_model_path(model_id: &str) -> Result<String, GoogleFailure> {
    if model_id.is_empty()
        || model_id.contains("..")
        || model_id.contains('?')
        || model_id.contains('&')
    {
        return Err(GoogleFailure::error("invalid Google model identifier"));
    }
    if model_id.starts_with("models/") || model_id.starts_with("tunedModels/") {
        Ok(model_id.to_owned())
    } else {
        Ok(format!("models/{model_id}"))
    }
}

fn build_headers(
    model: &Model,
    options: &StreamOptions,
    api_key: &str,
) -> Result<HeaderMap, GoogleFailure> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    insert_header(&mut headers, API_KEY_HEADER, api_key)?;
    apply_custom_headers(&mut headers, model, options)?;
    Ok(headers)
}

fn apply_custom_headers(
    headers: &mut HeaderMap,
    model: &Model,
    options: &StreamOptions,
) -> Result<(), GoogleFailure> {
    if let Some(model_headers) = &model.headers {
        for (name, value) in model_headers {
            insert_header(headers, name, value)?;
        }
    }
    if let Some(option_headers) = &options.headers {
        for (name, value) in option_headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| GoogleFailure::error(format!("invalid header name: {error}")))?;
            if let Some(value) = value {
                headers.insert(
                    name,
                    HeaderValue::from_str(value).map_err(|error| {
                        GoogleFailure::error(format!("invalid header value: {error}"))
                    })?,
                );
            } else {
                headers.remove(name);
            }
        }
    }
    Ok(())
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), GoogleFailure> {
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|error| GoogleFailure::error(format!("invalid header name: {error}")))?;
    let value = HeaderValue::from_str(value)
        .map_err(|error| GoogleFailure::error(format!("invalid header value: {error}")))?;
    headers.insert(name, value);
    Ok(())
}

fn thinking_config(model: &Model, options: &StreamOptions) -> Result<Option<Value>, GoogleFailure> {
    if !model.reasoning {
        return Ok(None);
    }
    if let Some(thinking) = options
        .extra_value(StreamOptionKey::THINKING)
        .and_then(Value::as_object)
    {
        let enabled = thinking
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !enabled {
            return Ok(Some(disabled_thinking_config(model)));
        }
        let mut config = Map::new();
        config.insert("includeThoughts".to_owned(), Value::Bool(true));
        if let Some(level) = thinking.get("level").and_then(Value::as_str) {
            let level = GoogleThinkingLevel::parse(level).ok_or_else(|| {
                GoogleFailure::error(format!("unsupported Google thinking level: {level}"))
            })?;
            config.insert(
                "thinkingLevel".to_owned(),
                Value::String(level.as_str().to_owned()),
            );
        } else if let Some(budget) = thinking.get("budgetTokens").and_then(Value::as_i64) {
            config.insert("thinkingBudget".to_owned(), Value::from(budget));
        }
        return Ok(Some(Value::Object(config)));
    }

    let Some(requested) = options
        .extra_value(StreamOptionKey::REASONING)
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let effort = clamp_effort(model, requested)?;
    if effort == Effort::Off {
        return Ok(Some(disabled_thinking_config(model)));
    }
    let effective = if effort == Effort::Off {
        Effort::High
    } else {
        effort
    };
    let mut config = Map::new();
    config.insert("includeThoughts".to_owned(), Value::Bool(true));
    if is_gemini3_pro(model) || is_gemini3_flash(model) || is_gemma4(model) {
        config.insert(
            "thinkingLevel".to_owned(),
            Value::String(thinking_level(effective, model).as_str().to_owned()),
        );
    } else {
        config.insert(
            "thinkingBudget".to_owned(),
            Value::from(thinking_budget(model, effective, options)),
        );
    }
    Ok(Some(Value::Object(config)))
}

fn disabled_thinking_config(model: &Model) -> Value {
    if is_gemini3_pro(model) {
        json!({"thinkingLevel": "LOW"})
    } else if is_gemini3_flash(model) || is_gemma4(model) {
        json!({"thinkingLevel": "MINIMAL"})
    } else {
        json!({"thinkingBudget": 0})
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Effort {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    const ALL: [Self; 7] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    const fn model_level(self) -> ModelThinkingLevel {
        match self {
            Self::Off => ModelThinkingLevel::Off,
            Self::Minimal => ModelThinkingLevel::Minimal,
            Self::Low => ModelThinkingLevel::Low,
            Self::Medium => ModelThinkingLevel::Medium,
            Self::High => ModelThinkingLevel::High,
            Self::Xhigh => ModelThinkingLevel::Xhigh,
            Self::Max => ModelThinkingLevel::Max,
        }
    }
}

fn clamp_effort(model: &Model, requested: &str) -> Result<Effort, GoogleFailure> {
    let requested = Effort::parse(requested).ok_or_else(|| {
        GoogleFailure::error(format!("unsupported reasoning effort: {requested}"))
    })?;
    let available = Effort::ALL
        .into_iter()
        .filter(|effort| effort_supported(model, *effort))
        .collect::<Vec<_>>();
    if available.contains(&requested) {
        return Ok(requested);
    }
    let requested_index = Effort::ALL
        .iter()
        .position(|effort| *effort == requested)
        .unwrap_or(0);
    Effort::ALL[requested_index..]
        .iter()
        .chain(Effort::ALL[..requested_index].iter().rev())
        .find(|effort| available.contains(effort))
        .copied()
        .ok_or_else(|| GoogleFailure::error("model does not support reasoning"))
}

fn effort_supported(model: &Model, effort: Effort) -> bool {
    if !model.reasoning {
        return effort == Effort::Off;
    }
    let mapped = model
        .thinking_level_map
        .as_ref()
        .and_then(|levels| levels.get(&effort.model_level()));
    if mapped == Some(&None) {
        return false;
    }
    !matches!(effort, Effort::Xhigh | Effort::Max) || mapped.is_some()
}

fn thinking_level(effort: Effort, model: &Model) -> GoogleThinkingLevel {
    if is_gemini3_pro(model) {
        return match effort {
            Effort::Minimal | Effort::Low => GoogleThinkingLevel::Low,
            _ => GoogleThinkingLevel::High,
        };
    }
    if is_gemma4(model) {
        return match effort {
            Effort::Minimal | Effort::Low => GoogleThinkingLevel::Minimal,
            _ => GoogleThinkingLevel::High,
        };
    }
    match effort {
        Effort::Minimal => GoogleThinkingLevel::Minimal,
        Effort::Low => GoogleThinkingLevel::Low,
        Effort::Medium => GoogleThinkingLevel::Medium,
        _ => GoogleThinkingLevel::High,
    }
}

fn thinking_budget(model: &Model, effort: Effort, options: &StreamOptions) -> i64 {
    let key = match effort {
        Effort::Minimal => "minimal",
        Effort::Low => "low",
        Effort::Medium => "medium",
        _ => "high",
    };
    if let Some(custom) = options
        .extra_value(StreamOptionKey::THINKING_BUDGETS)
        .and_then(Value::as_object)
        .and_then(|budgets| budgets.get(key))
        .and_then(Value::as_i64)
    {
        return custom;
    }
    if model.id.contains("2.5-pro") {
        match effort {
            Effort::Minimal => 128,
            Effort::Low => 2_048,
            Effort::Medium => 8_192,
            _ => 32_768,
        }
    } else if model.id.contains("2.5-flash-lite") {
        match effort {
            Effort::Minimal => 512,
            Effort::Low => 2_048,
            Effort::Medium => 8_192,
            _ => 24_576,
        }
    } else if model.id.contains("2.5-flash") {
        match effort {
            Effort::Minimal => 128,
            Effort::Low => 2_048,
            Effort::Medium => 8_192,
            _ => 24_576,
        }
    } else {
        -1
    }
}

fn is_gemini3_pro(model: &Model) -> bool {
    model_family_matches(&model.id, "pro")
}

fn is_gemini3_flash(model: &Model) -> bool {
    let id = model.id.to_ascii_lowercase();
    model_family_matches(&id, "flash")
        || matches!(
            id.as_str(),
            "gemini-flash-latest" | "gemini-flash-lite-latest"
        )
}

fn model_family_matches(model_id: &str, family: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    let Some(rest) = id.strip_prefix("gemini-3") else {
        return false;
    };
    let rest = if let Some(rest) = rest.strip_prefix('.') {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        &rest[digits..]
    } else {
        rest
    };
    rest.starts_with(&format!("-{family}"))
}

fn is_gemma4(model: &Model) -> bool {
    let id = model.id.to_ascii_lowercase();
    id.contains("gemma-4") || id.contains("gemma4")
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use futures::StreamExt;

    use super::*;
    use crate::types::{ModelCost, ModelInput};

    fn model(id: &str) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            api: "google-generative-ai".into(),
            provider: "google".into(),
            base_url: DEFAULT_BASE_URL.into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 1_000,
            max_tokens: 100,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn endpoint_keeps_api_version_and_sse_query() -> Result<(), GoogleFailure> {
        assert_eq!(
            endpoint(&model("gemini-2.5-flash"))?,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        let mut tuned = model("tunedModels/team-model");
        tuned.base_url = "http://127.0.0.1:1234/custom/v1".into();
        assert_eq!(
            endpoint(&tuned)?,
            "http://127.0.0.1:1234/custom/v1/tunedModels/team-model:streamGenerateContent?alt=sse"
        );
        Ok(())
    }

    #[test]
    fn headers_use_x_goog_key_and_honor_request_overrides() -> Result<(), GoogleFailure> {
        let model = model("gemini-2.5-flash");
        let mut options = StreamOptions::default();
        options
            .headers
            .get_or_insert_default()
            .insert(API_KEY_HEADER.into(), Some("override".into()));
        let headers = build_headers(&model, &options, "secret")?;
        assert_eq!(
            headers.get(API_KEY_HEADER).and_then(|v| v.to_str().ok()),
            Some("override")
        );
        assert!(headers.get("authorization").is_none());
        Ok(())
    }

    #[test]
    fn model_families_choose_distinct_thinking_controls() -> Result<(), GoogleFailure> {
        let mut disabled = StreamOptions::default();
        disabled.insert_extra(StreamOptionKey::THINKING, json!({"enabled": false}));
        assert_eq!(
            thinking_config(&model("gemini-3.1-pro-preview"), &disabled)?,
            Some(json!({"thinkingLevel": "LOW"}))
        );
        assert_eq!(
            thinking_config(&model("gemini-3-flash-preview"), &disabled)?,
            Some(json!({"thinkingLevel": "MINIMAL"}))
        );
        assert_eq!(
            thinking_config(&model("gemma-4-26b"), &disabled)?,
            Some(json!({"thinkingLevel": "MINIMAL"}))
        );
        assert_eq!(
            thinking_config(&model("gemini-2.5-flash"), &disabled)?,
            Some(json!({"thinkingBudget": 0}))
        );

        let mut enabled = StreamOptions::default();
        enabled.insert_extra(StreamOptionKey::REASONING, json!("medium"));
        assert_eq!(
            thinking_config(&model("gemini-3.1-pro-preview"), &enabled)?,
            Some(json!({"includeThoughts": true, "thinkingLevel": "HIGH"}))
        );
        assert_eq!(
            thinking_config(&model("gemma-4-26b"), &enabled)?,
            Some(json!({"includeThoughts": true, "thinkingLevel": "HIGH"}))
        );
        assert_eq!(
            thinking_config(&model("gemini-2.5-flash-lite"), &enabled)?,
            Some(json!({"includeThoughts": true, "thinkingBudget": 8192}))
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_key_still_emits_start_then_one_error() {
        let provider = GoogleGenerativeAi::new(reqwest::Client::new());
        let events = provider
            .stream(
                &model("gemini-2.5-flash"),
                Context::default(),
                StreamOptions::default(),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.first(),
            Some(Ok(AssistantMessageEvent::Start { .. }))
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(AssistantMessageEvent::Error { .. })))
                .count(),
            1
        );
        assert_eq!(events.len(), 2);
    }
}
