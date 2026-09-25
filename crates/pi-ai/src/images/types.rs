//! Image-generation wire contracts.
//!
//! Ports the image-side subset of the frozen
//! `packages/ai/src/types.ts` surface (`ImagesModel`, `ImagesContext`,
//! `AssistantImages`, `ImagesOptions` at lines 31-32, 300-316, 352-356,
//! 556-574, and 1019-1024). These types live beside the image subsystem, not
//! in the central [`crate::types`] module, because the frozen chat `Model`
//! surface omits image-specific fields (`output` modalities) while dropping
//! reasoning/context fields that image generation never uses.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::auth::types::{ProviderEnv, ProviderHeaders};
use crate::provider::{ProviderError, ProviderResponse};
use crate::types::{
    ImageContent, ModelCost, ModelInput, ModelInputLimits, ModelPromptCache, TextContent, Usage,
};

/// API-shape identifier selecting an image-generation transport.
pub type ImagesApi = String;

/// Built-in `OpenRouter` image-generation API shape.
pub const OPENROUTER_IMAGES_API: &str = "openrouter-images";

/// Output modality produced by an image model.
///
/// Distinct from [`ModelInput`] because the frozen surface types image-model
/// `output` as its own `"text" | "image"` union.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImagesOutput {
    /// The model returns generated images.
    Image,
    /// The model also returns accompanying text.
    Text,
}

/// Provider model metadata for image generation.
///
/// Mirrors the frozen `ImagesModel` shape: the chat [`crate::types::Model`]
/// surface minus `reasoning`, `contextWindow`, `maxTokens`, and `compat`,
/// plus the required `output` modality list. Unknown catalog fields survive
/// round trips through `extra`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagesModel {
    /// Provider model identifier.
    pub id: String,
    /// Display name.
    pub name: String,
    /// API shape used by the model.
    pub api: ImagesApi,
    /// Provider identifier.
    pub provider: String,
    /// Provider endpoint base URL.
    pub base_url: String,
    /// Accepted input modalities.
    pub input: Vec<ModelInput>,
    /// Produced output modalities.
    pub output: Vec<ImagesOutput>,
    /// Model pricing.
    pub cost: ModelCost,
    /// Provider-specific mapping of supported thinking levels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<BTreeMap<String, Option<String>>>,
    /// Provider input limits and image preprocessing metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_limits: Option<ModelInputLimits>,
    /// Prompt-cache lifetimes in seconds by retention tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<ModelPromptCache>,
    /// Default arbitrary sampling parameters for OpenAI-compatible adapters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<Map<String, Value>>,
    /// Additional static request headers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// Unknown catalog fields preserved across round trips.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl ImagesModel {
    /// Create a model with the required catalog fields and no optional data.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        api: impl Into<String>,
        provider: impl Into<String>,
        base_url: impl Into<String>,
        input: Vec<ModelInput>,
        output: Vec<ImagesOutput>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api: api.into(),
            provider: provider.into(),
            base_url: base_url.into(),
            input,
            output,
            cost: ModelCost::default(),
            thinking_level_map: None,
            input_limits: None,
            prompt_cache: None,
            sampling_params: None,
            headers: None,
            extra: BTreeMap::new(),
        }
    }

    /// Whether the model produces text alongside images.
    #[must_use]
    pub fn outputs_text(&self) -> bool {
        self.output.contains(&ImagesOutput::Text)
    }
}

/// One input content block accepted by image generation.
///
/// Untagged like the frozen `ImagesInputContent = TextContent | ImageContent`
/// union; JSON shape decides the variant.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ImagesInputContent {
    /// Text prompt block.
    Text(TextContent),
    /// Base64-encoded image block.
    Image(ImageContent),
}

impl From<String> for ImagesInputContent {
    fn from(text: String) -> Self {
        Self::Text(TextContent::new(text))
    }
}

impl From<&str> for ImagesInputContent {
    fn from(text: &str) -> Self {
        Self::Text(TextContent::new(text))
    }
}

impl From<ImageContent> for ImagesInputContent {
    fn from(image: ImageContent) -> Self {
        Self::Image(image)
    }
}

/// One output content block returned by image generation.
///
/// Untagged like the frozen `ImagesOutputContent` union; JSON shape decides
/// the variant.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ImagesOutputContent {
    /// Generated or accompanying text.
    Text(TextContent),
    /// Generated image.
    Image(ImageContent),
}

impl From<TextContent> for ImagesOutputContent {
    fn from(text: TextContent) -> Self {
        Self::Text(text)
    }
}

impl From<ImageContent> for ImagesOutputContent {
    fn from(image: ImageContent) -> Self {
        Self::Image(image)
    }
}

/// Complete image-generation request input.
///
/// The frozen surface is a flat content-block array; there is no system
/// prompt, message history, or tool state on this path.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ImagesContext {
    /// Ordered prompt blocks sent to the model.
    pub input: Vec<ImagesInputContent>,
}

impl ImagesContext {
    /// Create request input from ordered prompt blocks.
    #[must_use]
    pub fn new(input: Vec<ImagesInputContent>) -> Self {
        Self { input }
    }
}

impl From<Vec<ImagesInputContent>> for ImagesContext {
    fn from(input: Vec<ImagesInputContent>) -> Self {
        Self { input }
    }
}

/// Terminal reason recorded on an image-generation result.
///
/// The frozen surface keeps a dedicated three-variant union instead of the
/// chat [`crate::types::StopReason`] set; image generation never streams and
/// therefore never reports `length`, `toolUse`, `pending`, or `deferred`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ImagesStopReason {
    /// The provider completed normally.
    #[serde(rename = "stop")]
    Stop,
    /// The provider or request failed.
    #[serde(rename = "error")]
    Error,
    /// The request was cancelled.
    #[serde(rename = "aborted")]
    Aborted,
}

/// Result of one image-generation request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantImages {
    /// API shape used for the request.
    pub api: ImagesApi,
    /// Provider used for the request.
    pub provider: String,
    /// Requested model identifier.
    pub model: String,
    /// Ordered text and image output blocks.
    pub output: Vec<ImagesOutputContent>,
    /// Provider-specific response identifier, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Token usage and cost, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Terminal response reason.
    pub stop_reason: ImagesStopReason,
    /// Error description for failed or aborted requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

impl AssistantImages {
    /// Create a result frame for `model` with empty output and `Stop` reason.
    #[must_use]
    pub fn new(model: &ImagesModel, timestamp: i64) -> Self {
        Self {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            output: Vec::new(),
            response_id: None,
            usage: None,
            stop_reason: ImagesStopReason::Stop,
            error_message: None,
            timestamp,
        }
    }

    /// Convert this result into an error result with empty output.
    pub fn fail(&mut self, reason: ImagesStopReason, message: impl Into<String>) {
        self.output.clear();
        self.response_id = None;
        self.usage = None;
        self.stop_reason = reason;
        self.error_message = Some(message.into());
    }
}

/// Callback invoked before an image request payload is sent.
///
/// The callback mutates `payload` in place; a returned error becomes the
/// terminal error message of the resulting [`AssistantImages`].
pub type OnImagesPayloadFn = Arc<
    dyn for<'a> Fn(&'a mut Value, &'a ImagesModel) -> BoxFuture<'a, Result<(), ProviderError>>
        + Send
        + Sync,
>;

/// Callback invoked after the provider response body has been received.
///
/// A returned error becomes the terminal error message of the resulting
/// [`AssistantImages`].
pub type OnImagesResponseFn = Arc<
    dyn for<'a> Fn(
            &'a ProviderResponse,
            &'a ImagesModel,
        ) -> BoxFuture<'a, Result<(), ProviderError>>
        + Send
        + Sync,
>;

/// Options that control one image-generation request.
///
/// Mirrors the frozen `ImagesOptions` extension of the shared request-option
/// surface. This type is not `Debug` or serde because it contains async
/// callbacks.
#[derive(Clone, Default)]
pub struct ImagesOptions {
    /// Cancellation token for the request.
    pub signal: Option<CancellationToken>,

    /// Explicit API key for this request.
    pub api_key: Option<String>,

    /// Provider-scoped environment overrides.
    pub env: Option<ProviderEnv>,

    /// Optional payload mutation callback.
    pub on_payload: Option<OnImagesPayloadFn>,

    /// Optional response inspection callback.
    pub on_response: Option<OnImagesResponseFn>,

    /// Custom request headers merged over model defaults per key.
    pub headers: Option<ProviderHeaders>,

    /// HTTP request timeout in milliseconds.
    pub timeout_ms: Option<u64>,

    /// Maximum retry attempts for retryable failures.
    ///
    /// Default when unset: `0`, matching the frozen adapter which invokes the
    /// SDK with `maxRetries: 0` and wraps requests with the shared retry
    /// helper whose own default is also `0`.
    pub max_retries: Option<u32>,

    /// Maximum delay in milliseconds to honor when a server requests a long
    /// retry wait.
    ///
    /// Default when unset: `60000`; `0` disables the cap.
    pub max_retry_delay_ms: Option<u64>,

    /// Optional metadata included in API requests.
    ///
    /// Adapters extract the fields they understand and ignore the rest.
    pub metadata: Option<Map<String, Value>>,
}

impl ImagesOptions {
    /// Whether the request was cancelled before completion.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.signal
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ModelInput;
    use serde_json::json;

    fn sample_model() -> ImagesModel {
        let mut model = ImagesModel::new(
            "test-model",
            "Test Model",
            OPENROUTER_IMAGES_API,
            "openrouter",
            "https://image.example.test/api/v1",
            vec![ModelInput::Text, ModelInput::Image],
            vec![ImagesOutput::Image],
        );
        model.headers = Some(BTreeMap::from([("X-Title".to_owned(), "pi".to_owned())]));
        model
    }

    #[test]
    fn images_model_round_trips_camel_case_and_extra_fields() -> Result<(), serde_json::Error> {
        let mut model = sample_model();
        model.extra.insert("customTier".to_owned(), json!("gold"));

        let encoded = serde_json::to_value(&model)?;
        assert_eq!(encoded["id"], json!("test-model"));
        assert_eq!(
            encoded["baseUrl"],
            json!("https://image.example.test/api/v1")
        );
        assert_eq!(encoded["output"], json!(["image"]));
        assert_eq!(encoded["input"], json!(["text", "image"]));
        assert_eq!(encoded["customTier"], json!("gold"));

        let decoded: ImagesModel = serde_json::from_value(encoded)?;
        assert_eq!(decoded, model);
        Ok(())
    }

    #[test]
    fn assistant_images_encodes_camel_case_wire_fields() -> Result<(), serde_json::Error> {
        let mut result = AssistantImages::new(&sample_model(), 1_758_000_000_000);
        result
            .output
            .push(ImagesOutputContent::Text(TextContent::new(
                "here is your image",
            )));
        result.stop_reason = ImagesStopReason::Aborted;
        result.error_message = Some("Request aborted".to_owned());
        result.response_id = Some("resp_1".to_owned());

        let encoded = serde_json::to_value(&result)?;
        assert_eq!(encoded["api"], json!(OPENROUTER_IMAGES_API));
        assert_eq!(encoded["stopReason"], json!("aborted"));
        assert_eq!(encoded["responseId"], json!("resp_1"));
        assert_eq!(encoded["errorMessage"], json!("Request aborted"));
        assert_eq!(encoded["output"][0]["type"], json!("text"));
        Ok(())
    }

    #[test]
    fn images_context_input_blocks_are_untagged() -> Result<(), serde_json::Error> {
        let context = ImagesContext::new(vec![
            ImagesInputContent::from("draw a cat"),
            ImagesInputContent::from(ImageContent::new("AA==", "image/png")),
        ]);
        let encoded = serde_json::to_value(&context)?;
        assert_eq!(
            encoded["input"][0],
            json!({"type": "text", "text": "draw a cat"})
        );
        assert_eq!(
            encoded["input"][1],
            json!({"type": "image", "data": "AA==", "mimeType": "image/png"})
        );

        let decoded: ImagesContext = serde_json::from_value(encoded)?;
        assert_eq!(decoded, context);
        Ok(())
    }
}
