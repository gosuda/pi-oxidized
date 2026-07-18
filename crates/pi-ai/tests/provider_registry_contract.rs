//! Provider registry contract tests.

#[path = "support/mod.rs"]
mod support;

use std::collections::BTreeSet;

use pi_ai::providers::{BUILTIN_PROVIDERS, KnownApi, KnownProvider};

#[test]
fn exposes_exact_builtin_api_and_provider_tables() {
    let apis = KnownApi::ALL.map(KnownApi::as_str);
    assert_eq!(
        apis,
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

    let providers = BUILTIN_PROVIDERS
        .iter()
        .map(|spec| spec.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        providers,
        [
            "amazon-bedrock",
            "ant-ling",
            "anthropic",
            "azure-openai-responses",
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
    assert_eq!(
        providers.iter().copied().collect::<BTreeSet<_>>().len(),
        providers.len()
    );
    assert_eq!(KnownApi::from_id("pi-messages"), Some(KnownApi::PiMessages));
    assert_eq!(
        KnownProvider::from_id("openai"),
        Some(KnownProvider::OpenAi)
    );
    assert_eq!(KnownApi::from_id("custom-api"), None);
    assert_eq!(KnownProvider::from_id("custom-provider"), None);
}
