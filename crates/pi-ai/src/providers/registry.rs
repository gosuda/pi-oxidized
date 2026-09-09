//! Built-in provider metadata and native API-shape dispatch.

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::BoxStream;

use crate::provider::deferred::{unsupported_cancel_future, unsupported_fetch_stream};
use crate::provider::{
    CancelDeferredFn, DeferredCallbacks, FetchDeferredFn, Provider, ProviderError, StreamOptions,
    error_event_stream,
};
use crate::types::{AssistantMessageEvent, Context, DeferredHandle, Model};

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
    /// One adapter per [`KnownApi`], in [`KnownApi::ALL`] order.
    adapters: [Arc<dyn Provider>; 10],
    /// Aggregate deferred-callback record synthesized from the adapters at
    /// construction. `None` when no adapter registered either callback, so
    /// capability reflects live adapter callbacks rather than a separately
    /// maintained flag.
    deferred: Option<DeferredCallbacks>,
}

impl ProviderRegistry {
    /// Construct a registry from one prebuilt adapter for each known API shape.
    ///
    /// Adapters must be supplied in [`KnownApi::ALL`] order.
    #[must_use]
    pub fn new(adapters: [Arc<dyn Provider>; 10]) -> Self {
        let deferred = aggregate_deferred(&adapters);
        Self { adapters, deferred }
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
        match route_adapter(&self.adapters, model) {
            Ok(adapter) => adapter.stream(model, context, options),
            Err(message) => error_event_stream(model, message),
        }
    }

    fn deferred(&self) -> Option<&DeferredCallbacks> {
        self.deferred.as_ref()
    }
}

const fn provider_index(provider: KnownProvider) -> usize {
    provider as usize
}

const fn api_index(api: KnownApi) -> usize {
    api as usize
}

/// Resolve the adapter that would serve `model`, or the routing failure
/// message shared by every dispatch operation.
fn route_adapter<'a>(
    adapters: &'a [Arc<dyn Provider>; 10],
    model: &Model,
) -> Result<&'a Arc<dyn Provider>, String> {
    let Some(api) = KnownApi::from_id(&model.api) else {
        return Err(format!("No API implementation for \"{}\"", model.api));
    };

    if let Some(provider) = KnownProvider::from_id(&model.provider) {
        let spec = ProviderRegistry::builtin_spec(provider);
        if !spec.apis.contains(&api) {
            return Err(format!(
                "Provider {} has no API implementation for \"{}\"",
                model.provider, model.api
            ));
        }
    }

    Ok(&adapters[api_index(api)])
}

/// Build the registry's aggregate deferred record.
///
/// Each callback is present iff at least one adapter registered it, matching
/// `createProvider`'s `streams.some(...)` gate. The registered closures
/// re-dispatch to the adapter serving `model.api`, so a routed API whose
/// adapter lacks the callback — or an API that cannot be routed at all —
/// yields the source's per-API unsupported error.
fn aggregate_deferred(adapters: &[Arc<dyn Provider>; 10]) -> Option<DeferredCallbacks> {
    let fetch = adapters
        .iter()
        .any(|adapter| adapter.deferred().is_some_and(|d| d.fetch.is_some()))
        .then(|| deferred_fetch_dispatch(adapters.clone()));
    let cancel = adapters
        .iter()
        .any(|adapter| adapter.deferred().is_some_and(|d| d.cancel.is_some()))
        .then(|| deferred_cancel_dispatch(adapters.clone()));
    if fetch.is_none() && cancel.is_none() {
        return None;
    }
    Some(DeferredCallbacks { fetch, cancel })
}

fn deferred_fetch_dispatch(adapters: [Arc<dyn Provider>; 10]) -> FetchDeferredFn {
    Arc::new(
        move |model: &Model,
              handle: DeferredHandle,
              options: StreamOptions|
              -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
            let fetch = route_adapter(&adapters, model)
                .ok()
                .and_then(|adapter| adapter.deferred().and_then(|d| d.fetch.clone()));
            match fetch {
                Some(fetch) => fetch(model, handle, options),
                None => unsupported_fetch_stream(model, Some(model.api.as_str())),
            }
        },
    )
}

fn deferred_cancel_dispatch(adapters: [Arc<dyn Provider>; 10]) -> CancelDeferredFn {
    Arc::new(
        move |model: &Model,
              handle: DeferredHandle,
              options: StreamOptions|
              -> BoxFuture<'static, Result<(), ProviderError>> {
            let cancel = route_adapter(&adapters, model)
                .ok()
                .and_then(|adapter| adapter.deferred().and_then(|d| d.cancel.clone()));
            match cancel {
                Some(cancel) => cancel(model, handle, options),
                None => unsupported_cancel_future(model, Some(model.api.as_str())),
            }
        },
    )
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
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Mutex;

    use futures::StreamExt;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::provider::ProviderResponse;
    use crate::types::{
        AssistantContent, AssistantMessage, DoneReason, ErrorReason, ModelCost, ModelInput,
        StopReason, TextContent,
    };

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

    /// A provider carrying a registered deferred-callback record.
    struct DeferredFixture {
        deferred: DeferredCallbacks,
    }

    impl Provider for DeferredFixture {
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

    fn deferred_handle(id: &str) -> DeferredHandle {
        DeferredHandle {
            provider: "custom-provider".into(),
            model_id: "test-model".into(),
            api: "openai-responses".into(),
            id: id.into(),
            expires_at: None,
            poll_after_ms: None,
            data: None,
        }
    }

    /// Registry with `provider` installed for `KnownApi::ALL[index]` and
    /// stream-only recorders elsewhere.
    fn deferred_registry(index: usize, provider: Arc<dyn Provider>) -> ProviderRegistry {
        let mut adapters = recorders().map(|recorder| -> Arc<dyn Provider> { recorder });
        adapters[index] = provider;
        ProviderRegistry::new(adapters)
    }

    /// A fetch callback that records `"{api}:{handle.id}"` at dispatch time,
    /// invokes `on_response` like the source's faux provider, and answers
    /// with a terminal `Done` event stamped with the model's metadata.
    fn recording_fetch(calls: Arc<Mutex<Vec<String>>>) -> FetchDeferredFn {
        Arc::new(
            move |model: &Model,
                  handle: DeferredHandle,
                  options: StreamOptions|
                  -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
                if let Ok(mut calls) = calls.lock() {
                    calls.push(format!("{}:{}", model.api, handle.id));
                }
                let model = model.clone();
                futures::stream::once(async move {
                    if let Some(on_response) = options.on_response {
                        let metadata = ProviderResponse {
                            status: 200,
                            headers: BTreeMap::new(),
                        };
                        on_response(&metadata, &model).await?;
                    }
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

    /// What a registered cancel callback observed about one dispatch.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RecordedCancel {
        api: String,
        handle: String,
        has_signal: bool,
        timeout_ms: Option<u64>,
        has_headers: bool,
    }

    /// A cancel callback that records API, handle, and forwarded option
    /// presence at dispatch time.
    fn recording_cancel(calls: Arc<Mutex<Vec<RecordedCancel>>>) -> CancelDeferredFn {
        Arc::new(
            move |model: &Model,
                  handle: DeferredHandle,
                  options: StreamOptions|
                  -> BoxFuture<'static, Result<(), ProviderError>> {
                let api = model.api.clone();
                let calls = Arc::clone(&calls);
                let has_signal = options.signal.is_some();
                let timeout_ms = options.timeout_ms;
                let has_headers = options.headers.is_some();
                Box::pin(async move {
                    if let Ok(mut calls) = calls.lock() {
                        calls.push(RecordedCancel {
                            api,
                            handle: handle.id,
                            has_signal,
                            timeout_ms,
                            has_headers,
                        });
                    }
                    Ok(())
                })
            },
        )
    }

    #[test]
    fn api_index_matches_all_order() {
        for (index, api) in KnownApi::ALL.into_iter().enumerate() {
            assert_eq!(api as usize, index);
        }
    }

    #[test]
    fn deferred_capability_reflects_live_adapter_callbacks() {
        // No adapter registers deferred callbacks → no record at all.
        let registry = registry_with(&recorders());
        assert!(registry.deferred().is_none());

        // Fetch-only adapter → fetch capability only.
        let registry = deferred_registry(
            1,
            Arc::new(DeferredFixture {
                deferred: DeferredCallbacks {
                    fetch: Some(recording_fetch(Arc::new(Mutex::new(Vec::new())))),
                    cancel: None,
                },
            }),
        );
        let deferred = registry.deferred().expect("aggregate record");
        assert!(deferred.supports_fetch());
        assert!(!deferred.supports_cancel());

        // Cancel-only adapter → cancel capability only.
        let registry = deferred_registry(
            1,
            Arc::new(DeferredFixture {
                deferred: DeferredCallbacks {
                    fetch: None,
                    cancel: Some(recording_cancel(Arc::new(Mutex::new(Vec::new())))),
                },
            }),
        );
        let deferred = registry.deferred().expect("aggregate record");
        assert!(!deferred.supports_fetch());
        assert!(deferred.supports_cancel());
    }

    #[tokio::test]
    async fn fetch_deferred_dispatches_registered_callback() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let registry = deferred_registry(
            1,
            Arc::new(DeferredFixture {
                deferred: DeferredCallbacks {
                    fetch: Some(recording_fetch(Arc::clone(&calls))),
                    cancel: None,
                },
            }),
        );
        let test_model = model(
            "custom-provider",
            "openai-responses",
            "https://example.test",
        );
        let events = registry
            .fetch_deferred(
                &test_model,
                deferred_handle("handle-1"),
                StreamOptions::default(),
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            calls.lock().map(|calls| calls.clone()).unwrap_or_default(),
            vec!["openai-responses:handle-1".to_owned()]
        );
        assert!(matches!(
            events.as_slice(),
            [Ok(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                ..
            })]
        ));
    }

    #[tokio::test]
    async fn deferred_dispatch_without_api_callback_is_unsupported() {
        let registry = deferred_registry(
            1,
            Arc::new(DeferredFixture {
                deferred: DeferredCallbacks {
                    fetch: Some(recording_fetch(Arc::new(Mutex::new(Vec::new())))),
                    cancel: Some(recording_cancel(Arc::new(Mutex::new(Vec::new())))),
                },
            }),
        );

        // A routed API whose adapter registered no fetch callback, an
        // unroutable API, and a builtin provider/API mismatch all produce the
        // source's per-API unsupported error event.
        for (test_model, expected) in [
            (
                model(
                    "custom-provider",
                    "openai-completions",
                    "https://example.test",
                ),
                "Provider custom-provider does not support deferred responses for \"openai-completions\"",
            ),
            (
                model("custom-provider", "unknown-api", "https://example.test"),
                "Provider custom-provider does not support deferred responses for \"unknown-api\"",
            ),
            (
                model("openai", "anthropic-messages", "https://example.test"),
                "Provider openai does not support deferred responses for \"anthropic-messages\"",
            ),
        ] {
            let events = registry
                .fetch_deferred(&test_model, deferred_handle("h"), StreamOptions::default())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(events.len(), 1);
            match events.first() {
                Some(Ok(AssistantMessageEvent::Error { reason, error })) => {
                    assert_eq!(*reason, ErrorReason::Error);
                    assert_eq!(error.stop_reason, StopReason::Error);
                    assert_eq!(error.error_message.as_deref(), Some(expected));
                }
                other => panic!("expected unsupported error event, got {other:?}"),
            }
        }

        // Cancel on an API whose adapter registered no cancel callback.
        let error = registry
            .cancel_deferred(
                &model(
                    "custom-provider",
                    "openai-completions",
                    "https://example.test",
                ),
                deferred_handle("h"),
                StreamOptions::default(),
            )
            .await
            .expect_err("absent api cancel must fail");
        assert_eq!(
            error.message(),
            "Provider custom-provider cannot cancel deferred responses for \"openai-completions\""
        );
    }

    #[tokio::test]
    async fn cancel_deferred_dispatches_registered_callback() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let registry = deferred_registry(
            1,
            Arc::new(DeferredFixture {
                deferred: DeferredCallbacks {
                    fetch: None,
                    cancel: Some(recording_cancel(Arc::clone(&calls))),
                },
            }),
        );
        let signal = CancellationToken::new();
        let mut headers = BTreeMap::new();
        headers.insert("x-cancel".to_owned(), Some("value".to_owned()));
        let options = StreamOptions {
            signal: Some(signal),
            timeout_ms: Some(43),
            headers: Some(headers),
            ..StreamOptions::default()
        };
        registry
            .cancel_deferred(
                &model(
                    "custom-provider",
                    "openai-responses",
                    "https://example.test",
                ),
                deferred_handle("handle-2"),
                options,
            )
            .await
            .expect("registered cancel must succeed");
        assert_eq!(
            calls.lock().map(|calls| calls.clone()).unwrap_or_default(),
            vec![RecordedCancel {
                api: "openai-responses".to_owned(),
                handle: "handle-2".to_owned(),
                has_signal: true,
                timeout_ms: Some(43),
                has_headers: true,
            }]
        );
    }

    #[tokio::test]
    async fn replacement_without_callbacks_drops_stale_dispatch() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let registry = deferred_registry(
            1,
            Arc::new(DeferredFixture {
                deferred: DeferredCallbacks {
                    fetch: Some(recording_fetch(Arc::clone(&calls))),
                    cancel: None,
                },
            }),
        );
        let test_model = model(
            "custom-provider",
            "openai-responses",
            "https://example.test",
        );
        drop(registry.fetch_deferred(
            &test_model,
            deferred_handle("h-1"),
            StreamOptions::default(),
        ));
        assert_eq!(calls.lock().map(|calls| calls.len()).unwrap_or_default(), 1);

        // Re-registration with a stream-only adapter: the previous callback
        // is gone and dispatch reports the provider-level unsupported error.
        let registry = deferred_registry(1, Arc::new(RecordingProvider::default()));
        assert!(registry.deferred().is_none());
        let events = registry
            .fetch_deferred(
                &test_model,
                deferred_handle("h-1"),
                StreamOptions::default(),
            )
            .collect::<Vec<_>>()
            .await;
        match events.first() {
            Some(Ok(AssistantMessageEvent::Error { error, .. })) => {
                assert_eq!(
                    error.error_message.as_deref(),
                    Some("Provider custom-provider does not support deferred responses")
                );
            }
            other => panic!("expected unsupported error event, got {other:?}"),
        }
        assert_eq!(calls.lock().map(|calls| calls.len()).unwrap_or_default(), 1);
    }

    /// Serve two loopback HTTP requests for the deferred-callback smoke: a GET
    /// answers with the deferred body, a POST acknowledges the cancel.
    async fn serve_deferred_loopback()
    -> Result<(u16, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let server = tokio::spawn(async move {
            let mut served = 0;
            'accept: while served < 2 {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0_u8; 8192];
                let n = loop {
                    match socket.try_read(&mut buf) {
                        Ok(n) => break n,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if socket.readable().await.is_err() {
                                break 'accept;
                            }
                        }
                        Err(_) => break 'accept,
                    }
                };
                let request = String::from_utf8_lossy(&buf[..n]);
                let body = if request.starts_with("POST ") {
                    r#"{"cancelled":true}"#
                } else {
                    r#"{"text":"deferred result"}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let bytes = response.as_bytes();
                let mut written = 0;
                while written < bytes.len() {
                    match socket.try_write(&bytes[written..]) {
                        Ok(0) => break,
                        Ok(count) => written += count,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if socket.writable().await.is_err() {
                                break 'accept;
                            }
                        }
                        Err(_) => break 'accept,
                    }
                }
                if written != bytes.len() {
                    break;
                }
                served += 1;
            }
        });
        Ok((port, server))
    }

    /// A fetch callback that performs a real GET against the model's base URL,
    /// forwards live response metadata through `on_response`, and ends with a
    /// terminal `Done` event carrying the polled text.
    fn real_http_fetch_callback() -> FetchDeferredFn {
        Arc::new(
            |model: &Model,
             handle: DeferredHandle,
             options: StreamOptions|
             -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
                let url = format!("{}/deferred/{}", model.base_url, handle.id);
                let model = model.clone();
                futures::stream::once(async move {
                    let response = reqwest::get(&url).await.map_err(|error| {
                        ProviderError::new(format!("deferred fetch failed: {error}"))
                    })?;
                    if let Some(on_response) = options.on_response {
                        let metadata = ProviderResponse {
                            status: response.status().as_u16(),
                            headers: response
                                .headers()
                                .iter()
                                .map(|(name, value)| {
                                    (
                                        name.as_str().to_owned(),
                                        value.to_str().unwrap_or_default().to_owned(),
                                    )
                                })
                                .collect(),
                        };
                        on_response(&metadata, &model).await?;
                    }
                    let body: serde_json::Value = response.json().await.map_err(|error| {
                        ProviderError::new(format!("deferred fetch body failed: {error}"))
                    })?;
                    let text = body["text"].as_str().unwrap_or_default();
                    let mut message = AssistantMessage::new(
                        model.api.clone(),
                        model.provider.clone(),
                        model.id.clone(),
                        0,
                    );
                    message
                        .content
                        .push(AssistantContent::Text(TextContent::new(text)));
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

    /// A cancel callback that performs a real POST to the cancel endpoint and
    /// maps a non-2xx status to a `ProviderError`.
    fn real_http_cancel_callback() -> CancelDeferredFn {
        Arc::new(
            |model: &Model,
             handle: DeferredHandle,
             _options: StreamOptions|
             -> BoxFuture<'static, Result<(), ProviderError>> {
                let url = format!("{}/deferred/{}/cancel", model.base_url, handle.id);
                Box::pin(async move {
                    reqwest::Client::new()
                        .post(&url)
                        .send()
                        .await
                        .map_err(|error| {
                            ProviderError::new(format!("deferred cancel failed: {error}"))
                        })?
                        .error_for_status()
                        .map_err(|error| {
                            ProviderError::new(format!("deferred cancel failed: {error}"))
                        })?;
                    Ok(())
                })
            },
        )
    }

    /// Real-I/O smoke: the registered fetch/cancel callbacks perform actual
    /// loopback HTTP against a local server, dispatched through the registry.
    #[tokio::test]
    #[ignore = "explicit local HTTP callback smoke"]
    async fn registered_callbacks_perform_real_http_io() -> Result<(), Box<dyn std::error::Error>> {
        let (port, server) = serve_deferred_loopback().await?;
        let registry = deferred_registry(
            1,
            Arc::new(DeferredFixture {
                deferred: DeferredCallbacks {
                    fetch: Some(real_http_fetch_callback()),
                    cancel: Some(real_http_cancel_callback()),
                },
            }),
        );
        let test_model = model(
            "custom-provider",
            "openai-responses",
            &format!("http://127.0.0.1:{port}"),
        );

        let events = registry
            .fetch_deferred(
                &test_model,
                deferred_handle("response-1"),
                StreamOptions::default(),
            )
            .collect::<Vec<_>>()
            .await;
        match events.as_slice() {
            [Ok(AssistantMessageEvent::Done { reason, message })] => {
                assert_eq!(*reason, DoneReason::Stop);
                assert_eq!(message.provider, "custom-provider");
                assert_eq!(message.api, "openai-responses");
                assert!(matches!(
                    message.content.as_slice(),
                    [AssistantContent::Text(text)] if text.text == "deferred result"
                ));
            }
            other => panic!("expected one done event, got {other:?}"),
        }

        registry
            .cancel_deferred(
                &test_model,
                deferred_handle("response-1"),
                StreamOptions::default(),
            )
            .await?;

        server.await?;
        Ok(())
    }
}
