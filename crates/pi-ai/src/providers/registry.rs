//! Built-in provider metadata and native API-shape dispatch.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::stream::BoxStream;

use crate::provider::{Provider, ProviderError, StreamOptions};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Context, ErrorReason, Model, StopReason,
};

/// A native provider API shape implemented by this crate.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum KnownApi {
    /// OpenAI-compatible Chat Completions.
    OpenAiCompletions,
    /// `OpenAI` Responses.
    OpenAiResponses,
    /// Azure `OpenAI` Responses.
    AzureOpenAiResponses,
    /// `OpenAI` Codex Responses.
    OpenAiCodexResponses,
    /// Anthropic Messages.
    AnthropicMessages,
    /// AWS Bedrock `ConverseStream`.
    BedrockConverseStream,
    /// Google Generative AI.
    GoogleGenerativeAi,
    /// Google Vertex AI.
    GoogleVertex,
    /// Mistral Conversations.
    MistralConversations,
    /// Native pi messages.
    PiMessages,
}

impl KnownApi {
    /// Every native API shape in stable registry order.
    pub const ALL: [Self; 10] = [
        Self::OpenAiCompletions,
        Self::OpenAiResponses,
        Self::AzureOpenAiResponses,
        Self::OpenAiCodexResponses,
        Self::AnthropicMessages,
        Self::BedrockConverseStream,
        Self::GoogleGenerativeAi,
        Self::GoogleVertex,
        Self::MistralConversations,
        Self::PiMessages,
    ];

    /// The wire identifier used by model metadata.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::AzureOpenAiResponses => "azure-openai-responses",
            Self::OpenAiCodexResponses => "openai-codex-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::BedrockConverseStream => "bedrock-converse-stream",
            Self::GoogleGenerativeAi => "google-generative-ai",
            Self::GoogleVertex => "google-vertex",
            Self::MistralConversations => "mistral-conversations",
            Self::PiMessages => "pi-messages",
        }
    }

    /// Parse a model API identifier recognized by the native registry.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|api| api.as_str() == id)
    }
}

/// A built-in chat provider with native routing metadata.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum KnownProvider {
    /// Amazon Bedrock.
    AmazonBedrock,
    /// Ant Ling.
    AntLing,
    /// Anthropic.
    Anthropic,
    /// Azure `OpenAI` Responses.
    AzureOpenAiResponses,
    /// Baseten.
    Baseten,
    /// Cerebras.
    Cerebras,
    /// Cloudflare AI Gateway.
    CloudflareAiGateway,
    /// Cloudflare Workers AI.
    CloudflareWorkersAi,
    /// `DeepSeek`.
    Deepseek,
    /// Fireworks.
    Fireworks,
    /// GitHub Copilot.
    GithubCopilot,
    /// Google Generative AI.
    Google,
    /// Google Vertex AI.
    GoogleVertex,
    /// Groq.
    Groq,
    /// Hugging Face.
    Huggingface,
    /// Kimi Coding.
    KimiCoding,
    /// `MiniMax`.
    Minimax,
    /// `MiniMax` China.
    MinimaxCn,
    /// Mistral.
    Mistral,
    /// Moonshot AI.
    MoonshotAi,
    /// Moonshot AI China.
    MoonshotAiCn,
    /// NVIDIA.
    Nvidia,
    /// `OpenAI`.
    OpenAi,
    /// `OpenAI` Codex.
    OpenAiCodex,
    /// `OpenCode`.
    Opencode,
    /// `OpenCode` Go.
    OpencodeGo,
    /// `OpenRouter`.
    Openrouter,
    /// Qwen Token Plan.
    QwenTokenPlan,
    /// Qwen Token Plan CN.
    QwenTokenPlanCn,
    /// Qwen Token Plan Individual.
    QwenTokenPlanIndividual,
    /// Radius.
    Radius,
    /// Together AI.
    Together,
    /// Vercel AI Gateway.
    VercelAiGateway,
    /// xAI.
    Xai,
    /// Xiaomi.
    Xiaomi,
    /// Xiaomi token plan (Amsterdam).
    XiaomiTokenPlanAms,
    /// Xiaomi token plan (China).
    XiaomiTokenPlanCn,
    /// Xiaomi token plan (Singapore).
    XiaomiTokenPlanSgp,
    /// Z.AI.
    Zai,
    /// Z.AI Coding China.
    ZaiCodingCn,
}

impl KnownProvider {
    /// Every built-in provider in stable catalog order.
    pub const ALL: [Self; 40] = [
        Self::AmazonBedrock,
        Self::AntLing,
        Self::Anthropic,
        Self::AzureOpenAiResponses,
        Self::Baseten,
        Self::Cerebras,
        Self::CloudflareAiGateway,
        Self::CloudflareWorkersAi,
        Self::Deepseek,
        Self::Fireworks,
        Self::GithubCopilot,
        Self::Google,
        Self::GoogleVertex,
        Self::Groq,
        Self::Huggingface,
        Self::KimiCoding,
        Self::Minimax,
        Self::MinimaxCn,
        Self::Mistral,
        Self::MoonshotAi,
        Self::MoonshotAiCn,
        Self::Nvidia,
        Self::OpenAi,
        Self::OpenAiCodex,
        Self::Opencode,
        Self::OpencodeGo,
        Self::Openrouter,
        Self::QwenTokenPlan,
        Self::QwenTokenPlanCn,
        Self::QwenTokenPlanIndividual,
        Self::Radius,
        Self::Together,
        Self::VercelAiGateway,
        Self::Xai,
        Self::Xiaomi,
        Self::XiaomiTokenPlanAms,
        Self::XiaomiTokenPlanCn,
        Self::XiaomiTokenPlanSgp,
        Self::Zai,
        Self::ZaiCodingCn,
    ];

    /// The provider identifier used by model metadata.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AmazonBedrock => "amazon-bedrock",
            Self::AntLing => "ant-ling",
            Self::Anthropic => "anthropic",
            Self::AzureOpenAiResponses => "azure-openai-responses",
            Self::Baseten => "baseten",
            Self::Cerebras => "cerebras",
            Self::CloudflareAiGateway => "cloudflare-ai-gateway",
            Self::CloudflareWorkersAi => "cloudflare-workers-ai",
            Self::Deepseek => "deepseek",
            Self::Fireworks => "fireworks",
            Self::GithubCopilot => "github-copilot",
            Self::Google => "google",
            Self::GoogleVertex => "google-vertex",
            Self::Groq => "groq",
            Self::Huggingface => "huggingface",
            Self::KimiCoding => "kimi-coding",
            Self::Minimax => "minimax",
            Self::MinimaxCn => "minimax-cn",
            Self::Mistral => "mistral",
            Self::MoonshotAi => "moonshotai",
            Self::MoonshotAiCn => "moonshotai-cn",
            Self::Nvidia => "nvidia",
            Self::OpenAi => "openai",
            Self::OpenAiCodex => "openai-codex",
            Self::Opencode => "opencode",
            Self::OpencodeGo => "opencode-go",
            Self::Openrouter => "openrouter",
            Self::QwenTokenPlan => "qwen-token-plan",
            Self::QwenTokenPlanCn => "qwen-token-plan-cn",
            Self::QwenTokenPlanIndividual => "qwen-token-plan-individual",
            Self::Radius => "radius",
            Self::Together => "together",
            Self::VercelAiGateway => "vercel-ai-gateway",
            Self::Xai => "xai",
            Self::Xiaomi => "xiaomi",
            Self::XiaomiTokenPlanAms => "xiaomi-token-plan-ams",
            Self::XiaomiTokenPlanCn => "xiaomi-token-plan-cn",
            Self::XiaomiTokenPlanSgp => "xiaomi-token-plan-sgp",
            Self::Zai => "zai",
            Self::ZaiCodingCn => "zai-coding-cn",
        }
    }

    /// Parse a built-in provider identifier.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|provider| provider.as_str() == id)
    }
}

/// Static routing metadata for a built-in provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinProviderSpec {
    /// Built-in provider identifier.
    pub id: KnownProvider,
    /// API shapes supported by this provider.
    pub apis: &'static [KnownApi],
}

const COMPLETIONS: &[KnownApi] = &[KnownApi::OpenAiCompletions];
const RESPONSES: &[KnownApi] = &[KnownApi::OpenAiResponses];
const AZURE_RESPONSES: &[KnownApi] = &[KnownApi::AzureOpenAiResponses];
const CODEX_RESPONSES: &[KnownApi] = &[KnownApi::OpenAiCodexResponses];
const ANTHROPIC: &[KnownApi] = &[KnownApi::AnthropicMessages];
const BEDROCK: &[KnownApi] = &[KnownApi::BedrockConverseStream];
const GENERATIVE_AI: &[KnownApi] = &[KnownApi::GoogleGenerativeAi];
const VERTEX: &[KnownApi] = &[KnownApi::GoogleVertex];
const MISTRAL: &[KnownApi] = &[KnownApi::MistralConversations];
const PI_MESSAGES: &[KnownApi] = &[KnownApi::PiMessages];
const ANTHROPIC_COMPLETIONS: &[KnownApi] =
    &[KnownApi::AnthropicMessages, KnownApi::OpenAiCompletions];
const ANTHROPIC_COMPLETIONS_RESPONSES: &[KnownApi] = &[
    KnownApi::AnthropicMessages,
    KnownApi::OpenAiCompletions,
    KnownApi::OpenAiResponses,
];
const ANTHROPIC_GENERATIVE_COMPLETIONS_RESPONSES: &[KnownApi] = &[
    KnownApi::AnthropicMessages,
    KnownApi::GoogleGenerativeAi,
    KnownApi::OpenAiCompletions,
    KnownApi::OpenAiResponses,
];
const COMPLETIONS_RESPONSES: &[KnownApi] =
    &[KnownApi::OpenAiCompletions, KnownApi::OpenAiResponses];

/// Every built-in chat provider in catalog order with its allowed native APIs.
pub const BUILTIN_PROVIDERS: [BuiltinProviderSpec; 40] = [
    BuiltinProviderSpec {
        id: KnownProvider::AmazonBedrock,
        apis: BEDROCK,
    },
    BuiltinProviderSpec {
        id: KnownProvider::AntLing,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Anthropic,
        apis: ANTHROPIC,
    },
    BuiltinProviderSpec {
        id: KnownProvider::AzureOpenAiResponses,
        apis: AZURE_RESPONSES,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Baseten,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Cerebras,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::CloudflareAiGateway,
        apis: ANTHROPIC_COMPLETIONS_RESPONSES,
    },
    BuiltinProviderSpec {
        id: KnownProvider::CloudflareWorkersAi,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Deepseek,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Fireworks,
        apis: ANTHROPIC_COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::GithubCopilot,
        apis: ANTHROPIC_COMPLETIONS_RESPONSES,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Google,
        apis: GENERATIVE_AI,
    },
    BuiltinProviderSpec {
        id: KnownProvider::GoogleVertex,
        apis: VERTEX,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Groq,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Huggingface,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::KimiCoding,
        apis: ANTHROPIC,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Minimax,
        apis: ANTHROPIC,
    },
    BuiltinProviderSpec {
        id: KnownProvider::MinimaxCn,
        apis: ANTHROPIC,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Mistral,
        apis: MISTRAL,
    },
    BuiltinProviderSpec {
        id: KnownProvider::MoonshotAi,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::MoonshotAiCn,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Nvidia,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::OpenAi,
        apis: RESPONSES,
    },
    BuiltinProviderSpec {
        id: KnownProvider::OpenAiCodex,
        apis: CODEX_RESPONSES,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Opencode,
        apis: ANTHROPIC_GENERATIVE_COMPLETIONS_RESPONSES,
    },
    BuiltinProviderSpec {
        id: KnownProvider::OpencodeGo,
        apis: ANTHROPIC_COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Openrouter,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::QwenTokenPlan,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::QwenTokenPlanCn,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::QwenTokenPlanIndividual,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Radius,
        apis: PI_MESSAGES,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Together,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::VercelAiGateway,
        apis: ANTHROPIC,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Xai,
        apis: COMPLETIONS_RESPONSES,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Xiaomi,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::XiaomiTokenPlanAms,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::XiaomiTokenPlanCn,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::XiaomiTokenPlanSgp,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::Zai,
        apis: COMPLETIONS,
    },
    BuiltinProviderSpec {
        id: KnownProvider::ZaiCodingCn,
        apis: COMPLETIONS,
    },
];

/// Native adapters used to dispatch every known API shape.
pub struct ProviderRegistry {
    openai_completions: Arc<dyn Provider>,
    openai_responses: Arc<dyn Provider>,
    azure_openai_responses: Arc<dyn Provider>,
    openai_codex_responses: Arc<dyn Provider>,
    anthropic_messages: Arc<dyn Provider>,
    bedrock_converse_stream: Arc<dyn Provider>,
    google_generative_ai: Arc<dyn Provider>,
    google_vertex: Arc<dyn Provider>,
    mistral_conversations: Arc<dyn Provider>,
    pi_messages: Arc<dyn Provider>,
}

impl ProviderRegistry {
    /// Construct a registry from one prebuilt adapter for each known API shape.
    ///
    /// Adapters must be supplied in [`KnownApi::ALL`] order.
    #[must_use]
    pub fn new(adapters: [Arc<dyn Provider>; 10]) -> Self {
        let [
            openai_completions,
            openai_responses,
            azure_openai_responses,
            openai_codex_responses,
            anthropic_messages,
            bedrock_converse_stream,
            google_generative_ai,
            google_vertex,
            mistral_conversations,
            pi_messages,
        ] = adapters;
        Self {
            openai_completions,
            openai_responses,
            azure_openai_responses,
            openai_codex_responses,
            anthropic_messages,
            bedrock_converse_stream,
            google_generative_ai,
            google_vertex,
            mistral_conversations,
            pi_messages,
        }
    }

    fn adapter(&self, api: KnownApi) -> &Arc<dyn Provider> {
        match api {
            KnownApi::OpenAiCompletions => &self.openai_completions,
            KnownApi::OpenAiResponses => &self.openai_responses,
            KnownApi::AzureOpenAiResponses => &self.azure_openai_responses,
            KnownApi::OpenAiCodexResponses => &self.openai_codex_responses,
            KnownApi::AnthropicMessages => &self.anthropic_messages,
            KnownApi::BedrockConverseStream => &self.bedrock_converse_stream,
            KnownApi::GoogleGenerativeAi => &self.google_generative_ai,
            KnownApi::GoogleVertex => &self.google_vertex,
            KnownApi::MistralConversations => &self.mistral_conversations,
            KnownApi::PiMessages => &self.pi_messages,
        }
    }

    fn builtin_spec(provider: KnownProvider) -> &'static BuiltinProviderSpec {
        &BUILTIN_PROVIDERS[provider_index(provider)]
    }
}

impl Provider for ProviderRegistry {
    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
        let Some(api) = KnownApi::from_id(&model.api) else {
            return semantic_error_stream(
                model,
                format!("No API implementation for \"{}\"", model.api),
            );
        };

        if let Some(provider) = KnownProvider::from_id(&model.provider) {
            let spec = Self::builtin_spec(provider);
            if !spec.apis.contains(&api) {
                return semantic_error_stream(
                    model,
                    format!(
                        "Provider {} has no API implementation for \"{}\"",
                        model.provider, model.api
                    ),
                );
            }
        }

        self.adapter(api).stream(model, context, options)
    }
}

const fn provider_index(provider: KnownProvider) -> usize {
    provider as usize
}

fn semantic_error_stream(
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

fn now_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Mutex;

    use futures::StreamExt;

    use super::*;
    use crate::types::{ModelCost, ModelInput};

    #[derive(Default)]
    struct RecordingProvider {
        calls: Mutex<Vec<(String, String)>>,
    }

    impl Provider for RecordingProvider {
        fn stream(
            &self,
            model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push((model.api.clone(), model.base_url.clone()));
            }
            futures::stream::empty().boxed()
        }
    }

    fn model(provider: &str, api: &str, base_url: &str) -> Model {
        Model {
            id: "test-model".into(),
            name: "Test model".into(),
            api: api.into(),
            provider: provider.into(),
            base_url: base_url.into(),
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

    fn registry_with(recorders: &[Arc<RecordingProvider>; 10]) -> ProviderRegistry {
        ProviderRegistry::new([
            recorders[0].clone(),
            recorders[1].clone(),
            recorders[2].clone(),
            recorders[3].clone(),
            recorders[4].clone(),
            recorders[5].clone(),
            recorders[6].clone(),
            recorders[7].clone(),
            recorders[8].clone(),
            recorders[9].clone(),
        ])
    }

    fn recorders() -> [Arc<RecordingProvider>; 10] {
        std::array::from_fn(|_| Arc::new(RecordingProvider::default()))
    }

    #[test]
    fn api_table_is_exact_and_unique() {
        let ids = KnownApi::ALL.map(KnownApi::as_str);
        assert_eq!(
            ids,
            [
                "openai-completions",
                "openai-responses",
                "azure-openai-responses",
                "openai-codex-responses",
                "anthropic-messages",
                "bedrock-converse-stream",
                "google-generative-ai",
                "google-vertex",
                "mistral-conversations",
                "pi-messages",
            ]
        );
        assert_eq!(ids.into_iter().collect::<BTreeSet<_>>().len(), 10);
    }

    #[test]
    fn provider_table_is_exact_ordered_and_unique() {
        let ids = KnownProvider::ALL.map(KnownProvider::as_str);
        assert_eq!(
            ids,
            [
                "amazon-bedrock",
                "ant-ling",
                "anthropic",
                "azure-openai-responses",
                "baseten",
                "cerebras",
                "cloudflare-ai-gateway",
                "cloudflare-workers-ai",
                "deepseek",
                "fireworks",
                "github-copilot",
                "google",
                "google-vertex",
                "groq",
                "huggingface",
                "kimi-coding",
                "minimax",
                "minimax-cn",
                "mistral",
                "moonshotai",
                "moonshotai-cn",
                "nvidia",
                "openai",
                "openai-codex",
                "opencode",
                "opencode-go",
                "openrouter",
                "qwen-token-plan",
                "qwen-token-plan-cn",
                "qwen-token-plan-individual",
                "radius",
                "together",
                "vercel-ai-gateway",
                "xai",
                "xiaomi",
                "xiaomi-token-plan-ams",
                "xiaomi-token-plan-cn",
                "xiaomi-token-plan-sgp",
                "zai",
                "zai-coding-cn",
            ]
        );
        assert_eq!(ids.into_iter().collect::<BTreeSet<_>>().len(), 40);
        assert_eq!(BUILTIN_PROVIDERS.map(|spec| spec.id), KnownProvider::ALL);
        assert_eq!(
            BUILTIN_PROVIDERS.map(|spec| spec.apis),
            [
                BEDROCK,
                COMPLETIONS,
                ANTHROPIC,
                AZURE_RESPONSES,
                COMPLETIONS,
                COMPLETIONS,
                ANTHROPIC_COMPLETIONS_RESPONSES,
                COMPLETIONS,
                COMPLETIONS,
                ANTHROPIC_COMPLETIONS,
                ANTHROPIC_COMPLETIONS_RESPONSES,
                GENERATIVE_AI,
                VERTEX,
                COMPLETIONS,
                COMPLETIONS,
                ANTHROPIC,
                ANTHROPIC,
                ANTHROPIC,
                MISTRAL,
                COMPLETIONS,
                COMPLETIONS,
                COMPLETIONS,
                RESPONSES,
                CODEX_RESPONSES,
                ANTHROPIC_GENERATIVE_COMPLETIONS_RESPONSES,
                ANTHROPIC_COMPLETIONS,
                COMPLETIONS,
                COMPLETIONS,
                COMPLETIONS,
                COMPLETIONS,
                PI_MESSAGES,
                COMPLETIONS,
                ANTHROPIC,
                COMPLETIONS_RESPONSES,
                COMPLETIONS,
                COMPLETIONS,
                COMPLETIONS,
                COMPLETIONS,
                COMPLETIONS,
                COMPLETIONS,
            ]
        );
    }

    #[test]
    fn multi_api_sets_are_exact() {
        let multi = BUILTIN_PROVIDERS
            .iter()
            .filter(|spec| spec.apis.len() > 1)
            .map(|spec| (spec.id.as_str(), spec.apis))
            .collect::<Vec<_>>();
        assert_eq!(
            multi,
            vec![
                ("cloudflare-ai-gateway", ANTHROPIC_COMPLETIONS_RESPONSES),
                ("fireworks", ANTHROPIC_COMPLETIONS),
                ("github-copilot", ANTHROPIC_COMPLETIONS_RESPONSES),
                ("opencode", ANTHROPIC_GENERATIVE_COMPLETIONS_RESPONSES),
                ("opencode-go", ANTHROPIC_COMPLETIONS),
                ("xai", COMPLETIONS_RESPONSES),
            ]
        );
    }

    #[test]
    fn known_ids_round_trip_to_their_specs() {
        for (index, provider) in KnownProvider::ALL.into_iter().enumerate() {
            assert_eq!(KnownProvider::from_id(provider.as_str()), Some(provider));
            assert_eq!(
                ProviderRegistry::builtin_spec(provider),
                &BUILTIN_PROVIDERS[index]
            );
        }
        for api in KnownApi::ALL {
            assert_eq!(KnownApi::from_id(api.as_str()), Some(api));
        }
        assert_eq!(KnownProvider::from_id("custom"), None);
        assert_eq!(KnownApi::from_id("custom-api"), None);
    }

    #[test]
    fn known_and_custom_models_route_by_api() {
        let recorders = recorders();
        let registry = registry_with(&recorders);

        drop(registry.stream(
            &model("openai", "openai-responses", "https://example.test"),
            Context::default(),
            StreamOptions::default(),
        ));
        for api in KnownApi::ALL {
            drop(registry.stream(
                &model("custom-provider", api.as_str(), "https://example.test"),
                Context::default(),
                StreamOptions::default(),
            ));
        }

        for (index, recorder) in recorders.iter().enumerate() {
            let expected_calls = if index == 1 { 2 } else { 1 };
            assert!(
                recorder
                    .calls
                    .lock()
                    .is_ok_and(|calls| calls.len() == expected_calls)
            );
        }
    }

    #[tokio::test]
    async fn mismatch_and_unknown_api_are_single_semantic_errors() {
        let recorders = recorders();
        let registry = registry_with(&recorders);

        for (test_model, expected_message) in [
            (
                model("openai", "anthropic-messages", "https://example.test"),
                "Provider openai has no API implementation for \"anthropic-messages\"",
            ),
            (
                model("custom-provider", "unknown-api", "https://example.test"),
                "No API implementation for \"unknown-api\"",
            ),
        ] {
            let events = registry
                .stream(&test_model, Context::default(), StreamOptions::default())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(events.len(), 1);
            assert!(matches!(
                events.first(),
                Some(Ok(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    ..
                }))
            ));
            if let Some(Ok(AssistantMessageEvent::Error { error, .. })) = events.first() {
                assert_eq!(error.stop_reason, StopReason::Error);
                assert_eq!(error.error_message.as_deref(), Some(expected_message));
            }
        }
        assert!(
            recorders
                .iter()
                .all(|recorder| { recorder.calls.lock().is_ok_and(|calls| calls.is_empty()) })
        );
    }

    #[test]
    fn cloudflare_models_reach_selected_adapters_unchanged() {
        let recorders = recorders();
        let registry = registry_with(&recorders);
        let template = "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai";
        let mut env = BTreeMap::new();
        env.insert("CLOUDFLARE_ACCOUNT_ID".into(), "account".into());
        env.insert("CLOUDFLARE_GATEWAY_ID".into(), "gateway".into());
        let options = StreamOptions {
            env: Some(env),
            ..StreamOptions::default()
        };

        drop(registry.stream(
            &model("cloudflare-ai-gateway", "openai-completions", template),
            Context::default(),
            options.clone(),
        ));
        drop(registry.stream(
            &model("cloudflare-workers-ai", "openai-completions", template),
            Context::default(),
            options.clone(),
        ));
        drop(registry.stream(
            &model("custom-provider", "openai-completions", template),
            Context::default(),
            options,
        ));

        let urls = recorders[0]
            .calls
            .lock()
            .map(|calls| calls.iter().map(|call| call.1.clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        assert_eq!(urls, vec![template, template, template]);
    }
}
