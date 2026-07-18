//! Native provider adapters and builtin routing.
mod anthropic_messages;
mod azure_openai_responses;
mod bedrock_converse_stream;
mod google_generative_ai;
mod google_vertex;
mod mistral_conversations;
mod openai_codex_responses;
mod openai_completions;
mod openai_responses;
mod pi_messages;
mod registry;

pub use anthropic_messages::AnthropicMessages;
pub use azure_openai_responses::AzureOpenAiResponses;
pub use bedrock_converse_stream::{
    BedrockClientFactory, BedrockClientRequest, BedrockConverseStream, BedrockStaticCredentials,
    DefaultBedrockClientFactory,
};
pub use google_generative_ai::GoogleGenerativeAi;
pub use google_vertex::{
    DefaultVertexTokenProvider, GoogleVertex, VertexTokenProvider, VertexTokenRequest,
};
pub use mistral_conversations::MistralConversations;
pub use openai_codex_responses::OpenAiCodexResponses;
pub use openai_completions::OpenAiCompletions;
pub use openai_responses::OpenAiResponses;
pub use pi_messages::PiMessages;
pub use registry::{
    BUILTIN_PROVIDERS, BuiltinProviderSpec, KnownApi, KnownProvider, ProviderRegistry,
};

pub(crate) mod shared;
pub(crate) mod stream_state;
pub(crate) mod transport;
