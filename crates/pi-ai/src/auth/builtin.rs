//! Builtin provider authentication metadata.
//!
//! This module owns API-key environment precedence, OAuth capability, and
//! default [`ProviderAuth`] composition. Provider-to-API dispatch remains in
//! `crate::providers::registry` because it changes for a different reason.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;

use super::config_value::resolve_config_value;
use super::env_keys::{env_api_key_auth, is_ambient_auth_marker};
use super::error::AuthError;
use super::http::AuthHttpClient;
use super::oauth::radius::RadiusOAuthOptions;
use super::oauth::{
    anthropic::AnthropicOAuth, github_copilot::GitHubCopilotOAuth, kimi_coding::KimiCodingOAuth,
    openai_codex::OpenAiCodexOAuth, openrouter::OpenRouterOAuth, radius::RadiusOAuth,
    xai::XaiOAuth,
};
use super::types::{
    ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthInteraction, AuthResult, ModelAuth,
    OAuthAuth, ProviderAuth,
};

/// OAuth capability advertised by a builtin provider.
#[derive(Clone, Copy)]
pub struct BuiltinOAuth {
    name: &'static str,
    build: fn() -> Arc<dyn OAuthAuth>,
}

impl BuiltinOAuth {
    /// Provider name shown by OAuth login listings.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Construct this provider's OAuth handler.
    #[must_use]
    pub fn create(&self) -> Arc<dyn OAuthAuth> {
        (self.build)()
    }
}

#[derive(Clone, Copy)]
enum ApiKeyPolicy {
    Generic,
    Anthropic,
}

#[derive(Clone, Copy)]
struct BuiltinProviderAuth {
    id: &'static str,
    api_key_env_vars: &'static [&'static str],
    discovery_env_vars: Option<&'static [&'static str]>,
    api_key_policy: ApiKeyPolicy,
    oauth: Option<BuiltinOAuth>,
}

const fn oauth(name: &'static str, build: fn() -> Arc<dyn OAuthAuth>) -> BuiltinOAuth {
    BuiltinOAuth { name, build }
}

const fn provider(
    id: &'static str,
    api_key_env_vars: &'static [&'static str],
) -> BuiltinProviderAuth {
    BuiltinProviderAuth {
        id,
        api_key_env_vars,
        discovery_env_vars: None,
        api_key_policy: ApiKeyPolicy::Generic,
        oauth: None,
    }
}

const fn oauth_provider(
    id: &'static str,
    name: &'static str,
    api_key_env_vars: &'static [&'static str],
    build: fn() -> Arc<dyn OAuthAuth>,
) -> BuiltinProviderAuth {
    BuiltinProviderAuth {
        id,
        api_key_env_vars,
        discovery_env_vars: None,
        api_key_policy: ApiKeyPolicy::Generic,
        oauth: Some(oauth(name, build)),
    }
}

static BUILTIN_PROVIDER_AUTH: &[BuiltinProviderAuth] = &[
    provider("ant-ling", &["ANT_LING_API_KEY"]),
    BuiltinProviderAuth {
        id: "anthropic",
        api_key_env_vars: &["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"],
        discovery_env_vars: Some(&[
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ]),
        api_key_policy: ApiKeyPolicy::Anthropic,
        oauth: Some(oauth("Anthropic", build_anthropic_oauth)),
    },
    provider("azure-openai-responses", &["AZURE_OPENAI_API_KEY"]),
    provider("baseten", &["BASETEN_API_KEY"]),
    provider("cerebras", &["CEREBRAS_API_KEY"]),
    provider("cloudflare-ai-gateway", &["CLOUDFLARE_API_KEY"]),
    provider("cloudflare-workers-ai", &["CLOUDFLARE_API_KEY"]),
    provider("deepseek", &["DEEPSEEK_API_KEY"]),
    provider("fireworks", &["FIREWORKS_API_KEY"]),
    oauth_provider(
        "github-copilot",
        "GitHub Copilot",
        &["COPILOT_GITHUB_TOKEN"],
        build_github_copilot_oauth,
    ),
    provider("google", &["GEMINI_API_KEY"]),
    provider("google-vertex", &["GOOGLE_CLOUD_API_KEY"]),
    provider("groq", &["GROQ_API_KEY"]),
    provider("huggingface", &["HF_TOKEN"]),
    oauth_provider(
        "kimi-coding",
        "Kimi For Coding",
        &["KIMI_API_KEY"],
        build_kimi_coding_oauth,
    ),
    provider("minimax", &["MINIMAX_API_KEY"]),
    provider("minimax-cn", &["MINIMAX_CN_API_KEY"]),
    provider("mistral", &["MISTRAL_API_KEY"]),
    provider("moonshotai", &["MOONSHOT_API_KEY"]),
    provider("moonshotai-cn", &["MOONSHOT_API_KEY"]),
    provider("nvidia", &["NVIDIA_API_KEY"]),
    provider("openai", &["OPENAI_API_KEY"]),
    oauth_provider(
        "openai-codex",
        "OpenAI Codex",
        &[],
        build_openai_codex_oauth,
    ),
    provider("opencode", &["OPENCODE_API_KEY"]),
    provider("opencode-go", &["OPENCODE_API_KEY"]),
    oauth_provider(
        "openrouter",
        "OpenRouter",
        &["OPENROUTER_API_KEY"],
        build_openrouter_oauth,
    ),
    provider("qwen-token-plan", &["QWEN_TOKEN_PLAN_API_KEY"]),
    provider("qwen-token-plan-cn", &["QWEN_TOKEN_PLAN_CN_API_KEY"]),
    provider("qwen-token-plan-individual", &["QWEN_TOKEN_PLAN_API_KEY"]),
    oauth_provider("radius", "Radius", &["RADIUS_API_KEY"], build_radius_oauth),
    provider("together", &["TOGETHER_API_KEY"]),
    provider("vercel-ai-gateway", &["AI_GATEWAY_API_KEY"]),
    oauth_provider("xai", "xAI", &["XAI_API_KEY"], build_xai_oauth),
    provider("xiaomi", &["XIAOMI_API_KEY"]),
    provider("xiaomi-token-plan-ams", &["XIAOMI_TOKEN_PLAN_AMS_API_KEY"]),
    provider("xiaomi-token-plan-cn", &["XIAOMI_TOKEN_PLAN_CN_API_KEY"]),
    provider("xiaomi-token-plan-sgp", &["XIAOMI_TOKEN_PLAN_SGP_API_KEY"]),
    provider("zai", &["ZAI_API_KEY"]),
    provider("zai-coding-cn", &["ZAI_CODING_CN_API_KEY"]),
];

fn metadata(provider_id: &str) -> Option<&'static BuiltinProviderAuth> {
    BUILTIN_PROVIDER_AUTH
        .iter()
        .find(|entry| entry.id == provider_id)
}

/// Every OAuth-capable builtin provider in canonical catalog order.
pub fn builtin_oauth_providers() -> impl Iterator<Item = (&'static str, &'static BuiltinOAuth)> {
    BUILTIN_PROVIDER_AUTH
        .iter()
        .filter_map(|entry| entry.oauth.as_ref().map(|oauth| (entry.id, oauth)))
}

/// Find one OAuth-capable builtin provider.
#[must_use]
pub fn builtin_oauth_provider(provider_id: &str) -> Option<&'static BuiltinOAuth> {
    metadata(provider_id).and_then(|entry| entry.oauth.as_ref())
}

/// Known API-key environment variables for a provider, in precedence order.
///
/// Ambient-only sources and bearer-header-only values are excluded.
#[must_use]
pub fn api_key_env_vars(provider_id: &str) -> Option<&'static [&'static str]> {
    metadata(provider_id)
        .map(|entry| entry.api_key_env_vars)
        .filter(|env_vars| !env_vars.is_empty())
}

pub(super) fn auth_env_vars(provider_id: &str) -> Option<&'static [&'static str]> {
    metadata(provider_id)
        .map(|entry| entry.discovery_env_vars.unwrap_or(entry.api_key_env_vars))
        .filter(|env_vars| !env_vars.is_empty())
}

/// Compose the default request-time authentication handlers for a provider.
///
/// Unknown extension providers retain generic explicit and stored API-key
/// support, but receive no builtin environment names. An injected OAuth
/// handler replaces the builtin handler.
#[must_use]
pub fn default_provider_auth(
    provider_id: &str,
    oauth_override: Option<Arc<dyn OAuthAuth>>,
) -> ProviderAuth {
    let entry = metadata(provider_id);
    let env_vars = entry.map_or(&[][..], |entry| entry.api_key_env_vars);
    let api_key = match entry.map_or(ApiKeyPolicy::Generic, |entry| entry.api_key_policy) {
        ApiKeyPolicy::Generic => env_api_key_auth(format!("{provider_id} API key"), env_vars),
        ApiKeyPolicy::Anthropic => anthropic_api_key_auth(),
    };
    let oauth = oauth_override
        .or_else(|| entry.and_then(|entry| entry.oauth.as_ref().map(BuiltinOAuth::create)));

    ProviderAuth {
        api_key: Some(api_key),
        oauth,
    }
}

struct AnthropicApiKeyAuth {
    fallback: Arc<dyn ApiKeyAuth>,
}

impl ApiKeyAuth for AnthropicApiKeyAuth {
    fn name(&self) -> &str {
        self.fallback.name()
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
    ) -> Option<BoxFuture<'a, Result<ApiKeyCredential, AuthError>>> {
        self.fallback.login(interaction)
    }

    fn check<'a>(
        &'a self,
        ctx: &'a dyn AuthContext,
        credential: Option<&'a ApiKeyCredential>,
    ) -> Option<BoxFuture<'a, Option<AuthCheck>>> {
        self.fallback.check(ctx, credential)
    }

    fn resolve<'a>(
        &'a self,
        ctx: &'a dyn AuthContext,
        credential: Option<&'a ApiKeyCredential>,
    ) -> BoxFuture<'a, Option<AuthResult>> {
        Box::pin(async move {
            if let Some(key) = credential.and_then(|credential| credential.key.as_ref())
                && let Some(api_key) = resolve_config_value(
                    key,
                    credential.and_then(|credential| credential.env.as_ref()),
                )
            {
                if is_ambient_auth_marker(&api_key) {
                    return Some(AuthResult {
                        auth: ModelAuth::default(),
                        env: credential.and_then(|credential| credential.env.clone()),
                        source: Some("stored credential".to_owned()),
                    });
                }
                return Some(AuthResult {
                    auth: ModelAuth {
                        api_key: Some(api_key),
                        headers: None,
                        base_url: None,
                    },
                    env: credential.and_then(|credential| credential.env.clone()),
                    source: Some("stored credential".to_owned()),
                });
            }

            if let Some(token) = ctx.env("ANTHROPIC_AUTH_TOKEN").await {
                return Some(AuthResult {
                    auth: ModelAuth {
                        api_key: None,
                        headers: Some(BTreeMap::from([(
                            "Authorization".to_owned(),
                            Some(format!("Bearer {token}")),
                        )])),
                        base_url: None,
                    },
                    env: None,
                    source: Some("ANTHROPIC_AUTH_TOKEN".to_owned()),
                });
            }

            self.fallback.resolve(ctx, None).await
        })
    }
}

fn anthropic_api_key_auth() -> Arc<dyn ApiKeyAuth> {
    Arc::new(AnthropicApiKeyAuth {
        fallback: env_api_key_auth(
            "Anthropic API key",
            &["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"],
        ),
    })
}

fn build_anthropic_oauth() -> Arc<dyn OAuthAuth> {
    AnthropicOAuth::new().map_or_else(
        |_| Arc::new(AnthropicOAuth::default()) as Arc<dyn OAuthAuth>,
        |auth| Arc::new(auth) as Arc<dyn OAuthAuth>,
    )
}

fn build_github_copilot_oauth() -> Arc<dyn OAuthAuth> {
    GitHubCopilotOAuth::shared().unwrap_or_else(|_| {
        Arc::new(GitHubCopilotOAuth::with_client(
            AuthHttpClient::from_client(reqwest::Client::new()),
        )) as Arc<dyn OAuthAuth>
    })
}

fn build_kimi_coding_oauth() -> Arc<dyn OAuthAuth> {
    KimiCodingOAuth::shared()
        .unwrap_or_else(|_| Arc::new(KimiCodingOAuth::default()) as Arc<dyn OAuthAuth>)
}

fn build_openai_codex_oauth() -> Arc<dyn OAuthAuth> {
    OpenAiCodexOAuth::shared().unwrap_or_else(|_| {
        Arc::new(OpenAiCodexOAuth::with_http(AuthHttpClient::from_client(
            reqwest::Client::new(),
        ))) as Arc<dyn OAuthAuth>
    })
}

fn build_openrouter_oauth() -> Arc<dyn OAuthAuth> {
    OpenRouterOAuth::shared()
        .unwrap_or_else(|_| Arc::new(OpenRouterOAuth::default()) as Arc<dyn OAuthAuth>)
}

fn build_radius_oauth() -> Arc<dyn OAuthAuth> {
    let options = RadiusOAuthOptions {
        name: "Radius".to_owned(),
        gateway: "https://radius.pi.dev".to_owned(),
    };
    RadiusOAuth::new(options.clone()).map_or_else(
        |_| {
            Arc::new(RadiusOAuth::with_client(
                options,
                AuthHttpClient::from_client(reqwest::Client::new()),
                1456,
            )) as Arc<dyn OAuthAuth>
        },
        |auth| Arc::new(auth) as Arc<dyn OAuthAuth>,
    )
}

fn build_xai_oauth() -> Arc<dyn OAuthAuth> {
    XaiOAuth::shared().unwrap_or_else(|_| Arc::new(XaiOAuth::default()) as Arc<dyn OAuthAuth>)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_provider_order_names_and_constructors_match_reference() {
        let providers: Vec<_> = builtin_oauth_providers()
            .map(|(id, oauth)| (id, oauth.name(), oauth.create().name().to_owned()))
            .collect();

        assert_eq!(
            providers,
            vec![
                (
                    "anthropic",
                    "Anthropic",
                    "Anthropic (Claude Pro/Max)".to_owned(),
                ),
                (
                    "github-copilot",
                    "GitHub Copilot",
                    "GitHub Copilot".to_owned(),
                ),
                (
                    "kimi-coding",
                    "Kimi For Coding",
                    "Kimi Code (subscription)".to_owned(),
                ),
                (
                    "openai-codex",
                    "OpenAI Codex",
                    "OpenAI (ChatGPT Plus/Pro)".to_owned(),
                ),
                ("openrouter", "OpenRouter", "OpenRouter OAuth".to_owned(),),
                ("radius", "Radius", "Radius".to_owned()),
                ("xai", "xAI", "xAI (Grok/X subscription)".to_owned()),
            ]
        );
    }

    #[test]
    fn unknown_provider_gets_api_key_auth_without_oauth() {
        let auth = default_provider_auth("extension-provider", None);

        assert!(auth.api_key.is_some());
        assert!(auth.oauth.is_none());
        assert!(api_key_env_vars("extension-provider").is_none());
    }
}
