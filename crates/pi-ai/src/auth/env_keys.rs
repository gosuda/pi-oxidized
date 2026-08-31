//! Environment and ambient API-key detection.
//!
//! Ports `.references/pi/packages/ai/src/env-api-keys.ts` and the
//! `envApiKeyAuth` / `lazyOAuth` helpers from `auth/helpers.ts`.
//!
//! Ambient Vertex/Bedrock detection returns the [`AMBIENT_AUTH_MARKER`]
//! sentinel for status only. That value is never bearer material and must not
//! be forwarded as [`crate::auth::ModelAuth::api_key`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use futures::future::BoxFuture;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use super::builtin::{api_key_env_vars, auth_env_vars};
use super::config_value::resolve_config_value;
use super::error::AuthError;
use super::types::{
    ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthInteraction, AuthPrompt, AuthResult,
    ModelAuth, OAuthAuth, OAuthCredential, ProviderEnv,
};

/// Status-only marker returned when Vertex ADC or Bedrock ambient credentials
/// are configured without an explicit API key/bearer secret.
pub const AMBIENT_AUTH_MARKER: &str = "<authenticated>";

static VERTEX_ADC_CACHE: LazyLock<Mutex<Option<bool>>> = LazyLock::new(|| Mutex::new(None));

/// Whether `value` is the ambient configured-status sentinel.
#[must_use]
pub fn is_ambient_auth_marker(value: &str) -> bool {
    value == AMBIENT_AUTH_MARKER
}

fn get_provider_env_value(name: &str, env: Option<&ProviderEnv>) -> Option<String> {
    if let Some(map) = env
        && let Some(value) = map.get(name)
        && !value.is_empty()
    {
        // Match JS `env?.[name] || process.env[name]`: empty is falsy.
        return Some(value.clone());
    }
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

fn default_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn default_vertex_adc_path() -> Option<PathBuf> {
    default_home_dir().map(|home| {
        home.join(".config")
            .join("gcloud")
            .join("application_default_credentials.json")
    })
}

/// Clear the process-lifetime Vertex ADC existence cache. Test helper.
pub fn clear_vertex_adc_cache() {
    if let Ok(mut cache) = VERTEX_ADC_CACHE.lock() {
        *cache = None;
    }
}

/// Whether Google Application Default Credentials appear present.
///
/// When `env` supplies an explicit `GOOGLE_APPLICATION_CREDENTIALS` path, that
/// path is checked without caching. Otherwise the result of the process env /
/// default ADC path probe is cached for the process lifetime.
#[must_use]
pub fn has_vertex_adc_credentials(env: Option<&ProviderEnv>) -> bool {
    if let Some(path) = env
        .and_then(|map| map.get("GOOGLE_APPLICATION_CREDENTIALS"))
        .filter(|path| !path.is_empty())
    {
        return Path::new(path).exists();
    }

    let mut cache = VERTEX_ADC_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cached) = *cache {
        return cached;
    }

    let exists = if let Some(path) = get_provider_env_value("GOOGLE_APPLICATION_CREDENTIALS", env) {
        Path::new(&path).exists()
    } else if let Some(path) = default_vertex_adc_path() {
        path.exists()
    } else {
        false
    };
    *cache = Some(exists);
    exists
}

/// Configured environment variable names that can authenticate a provider.
///
/// Excludes ambient credential sources (AWS profiles, ADC files, IAM roles).
#[must_use]
pub fn find_env_keys(provider: &str, env: Option<&ProviderEnv>) -> Option<Vec<String>> {
    let env_vars = auth_env_vars(provider)?;
    let found: Vec<String> = env_vars
        .iter()
        .filter(|env_var| get_provider_env_value(env_var, env).is_some())
        .map(|env_var| (*env_var).to_owned())
        .collect();
    if found.is_empty() { None } else { Some(found) }
}

/// Resolve an API key from known environment variables or ambient status.
///
/// For `google-vertex` and `amazon-bedrock`, when no explicit key env var is
/// set but ambient credentials are configured, returns [`AMBIENT_AUTH_MARKER`].
/// That sentinel is status only and must never be used as request bearer material.
#[must_use]
pub fn get_env_api_key(provider: &str, env: Option<&ProviderEnv>) -> Option<String> {
    if let Some(value) = api_key_env_vars(provider).and_then(|env_vars| {
        env_vars
            .iter()
            .find_map(|name| get_provider_env_value(name, env))
    }) {
        return Some(value);
    }

    // Vertex AI: explicit API key (above) or Application Default Credentials.
    if provider == "google-vertex" {
        let has_credentials = has_vertex_adc_credentials(env);
        let has_project = get_provider_env_value("GOOGLE_CLOUD_PROJECT", env).is_some()
            || get_provider_env_value("GCLOUD_PROJECT", env).is_some();
        let has_location = get_provider_env_value("GOOGLE_CLOUD_LOCATION", env).is_some();
        if has_credentials && has_project && has_location {
            return Some(AMBIENT_AUTH_MARKER.to_owned());
        }
    }

    if provider == "amazon-bedrock" {
        // Ambient AWS credential sources — status only, not a bearer secret.
        if get_provider_env_value("AWS_PROFILE", env).is_some()
            || (get_provider_env_value("AWS_ACCESS_KEY_ID", env).is_some()
                && get_provider_env_value("AWS_SECRET_ACCESS_KEY", env).is_some())
            || get_provider_env_value("AWS_BEARER_TOKEN_BEDROCK", env).is_some()
            || get_provider_env_value("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", env).is_some()
            || get_provider_env_value("AWS_CONTAINER_CREDENTIALS_FULL_URI", env).is_some()
            || get_provider_env_value("AWS_WEB_IDENTITY_TOKEN_FILE", env).is_some()
        {
            return Some(AMBIENT_AUTH_MARKER.to_owned());
        }
    }

    None
}

/// Standard API-key auth: stored credential key wins, otherwise the first set
/// env var resolves. Includes a `login` that prompts for the key.
#[must_use]
pub fn env_api_key_auth(
    name: impl Into<String>,
    env_vars: &'static [&'static str],
) -> Arc<dyn ApiKeyAuth> {
    Arc::new(EnvApiKeyAuth {
        name: name.into(),
        env_vars,
    })
}

struct EnvApiKeyAuth {
    name: String,
    env_vars: &'static [&'static str],
}

impl EnvApiKeyAuth {
    async fn resolve_from_env(&self, ctx: &dyn AuthContext) -> Option<AuthResult> {
        for env_var in self.env_vars {
            if let Some(value) = ctx.env(env_var).await {
                if is_ambient_auth_marker(&value) {
                    return Some(AuthResult {
                        auth: ModelAuth::default(),
                        env: None,
                        source: Some((*env_var).to_owned()),
                    });
                }
                return Some(AuthResult {
                    auth: ModelAuth {
                        api_key: Some(value),
                        headers: None,
                        base_url: None,
                    },
                    env: None,
                    source: Some((*env_var).to_owned()),
                });
            }
        }
        None
    }
}

impl ApiKeyAuth for EnvApiKeyAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
    ) -> Option<BoxFuture<'a, Result<ApiKeyCredential, AuthError>>> {
        Some(Box::pin(async move {
            let key = interaction
                .prompt(AuthPrompt::Secret {
                    message: format!("Enter {}", self.name),
                    placeholder: None,
                    signal: None,
                })
                .await?;
            Ok(ApiKeyCredential {
                key: Some(key),
                env: None,
            })
        }))
    }

    fn check<'a>(
        &'a self,
        _ctx: &'a dyn AuthContext,
        _credential: Option<&'a ApiKeyCredential>,
    ) -> Option<BoxFuture<'a, Option<AuthCheck>>> {
        None
    }

    fn resolve<'a>(
        &'a self,
        ctx: &'a dyn AuthContext,
        credential: Option<&'a ApiKeyCredential>,
    ) -> BoxFuture<'a, Option<AuthResult>> {
        Box::pin(async move {
            if let Some(key) = credential.and_then(|cred| cred.key.as_ref()) {
                // Callers may pass already-expanded copies (from resolve.rs) or
                // raw templates. Expand once more; unresolved templates fall
                // through to env rather than treating the raw form as a secret.
                let resolved =
                    resolve_config_value(key, credential.and_then(|cred| cred.env.as_ref()));
                if let Some(resolved) = resolved {
                    if is_ambient_auth_marker(&resolved) {
                        // Never promote the ambient status sentinel to bearer material.
                        return Some(AuthResult {
                            auth: ModelAuth::default(),
                            env: credential.and_then(|cred| cred.env.clone()),
                            source: Some("stored credential".to_owned()),
                        });
                    }
                    return Some(AuthResult {
                        auth: ModelAuth {
                            api_key: Some(resolved),
                            headers: None,
                            base_url: None,
                        },
                        env: credential.and_then(|cred| cred.env.clone()),
                        source: Some("stored credential".to_owned()),
                    });
                }
                // Unresolved stored template: try env fallback only when no
                // usable stored value remains. Prefer env over a missing key.
                return self.resolve_from_env(ctx).await;
            }

            self.resolve_from_env(ctx).await
        })
    }
}

/// Lazily load an [`OAuthAuth`] implementation on first use.
#[must_use]
pub fn lazy_oauth(
    name: impl Into<String>,
    login_label: Option<String>,
    load: impl Fn() -> BoxFuture<'static, Arc<dyn OAuthAuth>> + Send + Sync + 'static,
) -> Arc<dyn OAuthAuth> {
    Arc::new(LazyOAuthAuth {
        name: name.into(),
        login_label,
        load: Box::new(load),
        cached: OnceCell::new(),
    })
}

struct LazyOAuthAuth {
    name: String,
    login_label: Option<String>,
    load: Box<dyn Fn() -> BoxFuture<'static, Arc<dyn OAuthAuth>> + Send + Sync>,
    cached: OnceCell<Arc<dyn OAuthAuth>>,
}

impl LazyOAuthAuth {
    async fn loaded(&self) -> Arc<dyn OAuthAuth> {
        self.cached.get_or_init(|| (self.load)()).await.clone()
    }
}

impl OAuthAuth for LazyOAuthAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn login_label(&self) -> Option<&str> {
        self.login_label.as_deref()
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move { self.loaded().await.login(interaction).await })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        signal: Option<CancellationToken>,
    ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
        Box::pin(async move { self.loaded().await.refresh(credential, signal).await })
    }

    fn to_auth<'a>(
        &'a self,
        credential: &'a OAuthCredential,
    ) -> BoxFuture<'a, Result<ModelAuth, AuthError>> {
        Box::pin(async move { self.loaded().await.to_auth(credential).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::context::MapAuthContext;
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::fs;
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    type TestResult = Result<(), Box<dyn Error>>;

    fn required<T>(value: Option<T>, message: &'static str) -> Result<T, io::Error> {
        value.ok_or_else(|| io::Error::other(message))
    }

    fn env_map(pairs: &[(&str, &str)]) -> ProviderEnv {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn exact_provider_precedence_table() {
        let cases = [
            ("ant-ling", "ANT_LING_API_KEY"),
            ("openai", "OPENAI_API_KEY"),
            ("azure-openai-responses", "AZURE_OPENAI_API_KEY"),
            ("nvidia", "NVIDIA_API_KEY"),
            ("deepseek", "DEEPSEEK_API_KEY"),
            ("google", "GEMINI_API_KEY"),
            ("google-vertex", "GOOGLE_CLOUD_API_KEY"),
            ("groq", "GROQ_API_KEY"),
            ("cerebras", "CEREBRAS_API_KEY"),
            ("xai", "XAI_API_KEY"),
            ("radius", "RADIUS_API_KEY"),
            ("openrouter", "OPENROUTER_API_KEY"),
            ("vercel-ai-gateway", "AI_GATEWAY_API_KEY"),
            ("zai", "ZAI_API_KEY"),
            ("zai-coding-cn", "ZAI_CODING_CN_API_KEY"),
            ("mistral", "MISTRAL_API_KEY"),
            ("minimax", "MINIMAX_API_KEY"),
            ("minimax-cn", "MINIMAX_CN_API_KEY"),
            ("moonshotai", "MOONSHOT_API_KEY"),
            ("moonshotai-cn", "MOONSHOT_API_KEY"),
            ("huggingface", "HF_TOKEN"),
            ("fireworks", "FIREWORKS_API_KEY"),
            ("together", "TOGETHER_API_KEY"),
            ("opencode", "OPENCODE_API_KEY"),
            ("opencode-go", "OPENCODE_API_KEY"),
            ("kimi-coding", "KIMI_API_KEY"),
            ("cloudflare-workers-ai", "CLOUDFLARE_API_KEY"),
            ("cloudflare-ai-gateway", "CLOUDFLARE_API_KEY"),
            ("xiaomi", "XIAOMI_API_KEY"),
            ("xiaomi-token-plan-cn", "XIAOMI_TOKEN_PLAN_CN_API_KEY"),
            ("xiaomi-token-plan-ams", "XIAOMI_TOKEN_PLAN_AMS_API_KEY"),
            ("xiaomi-token-plan-sgp", "XIAOMI_TOKEN_PLAN_SGP_API_KEY"),
            ("github-copilot", "COPILOT_GITHUB_TOKEN"),
        ];
        for (provider, variable) in cases {
            let env = env_map(&[(variable, "value")]);
            assert_eq!(api_key_env_vars(provider), Some(&[variable][..]));
            assert_eq!(
                get_env_api_key(provider, Some(&env)).as_deref(),
                Some("value")
            );
        }

        let anthropic = env_map(&[
            ("ANTHROPIC_API_KEY", "api-key"),
            ("ANTHROPIC_OAUTH_TOKEN", "oauth-token"),
        ]);
        assert_eq!(
            api_key_env_vars("anthropic"),
            Some(&["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"][..])
        );
        assert_eq!(
            get_env_api_key("anthropic", Some(&anthropic)).as_deref(),
            Some("oauth-token")
        );
        assert_eq!(
            find_env_keys("anthropic", Some(&anthropic)),
            Some(vec![
                "ANTHROPIC_OAUTH_TOKEN".to_owned(),
                "ANTHROPIC_API_KEY".to_owned()
            ])
        );
        assert!(api_key_env_vars("amazon-bedrock").is_none());
        assert!(api_key_env_vars("unknown").is_none());
    }

    #[test]
    fn canonical_provider_env_keys_cover_baseten_and_qwen() {
        let cases = [
            ("baseten", "BASETEN_API_KEY"),
            ("qwen-token-plan", "QWEN_TOKEN_PLAN_API_KEY"),
            ("qwen-token-plan-cn", "QWEN_TOKEN_PLAN_CN_API_KEY"),
            ("qwen-token-plan-individual", "QWEN_TOKEN_PLAN_API_KEY"),
        ];

        for (provider, variable) in cases {
            assert_eq!(api_key_env_vars(provider), Some(&[variable][..]));
        }
    }

    #[test]
    fn anthropic_bearer_token_is_discovered_but_not_used_as_api_key() {
        let env = env_map(&[
            ("ANTHROPIC_AUTH_TOKEN", "bearer-token"),
            ("ANTHROPIC_OAUTH_TOKEN", "oauth-token"),
            ("ANTHROPIC_API_KEY", "api-key"),
        ]);

        assert_eq!(
            find_env_keys("anthropic", Some(&env)),
            Some(vec![
                "ANTHROPIC_AUTH_TOKEN".to_owned(),
                "ANTHROPIC_OAUTH_TOKEN".to_owned(),
                "ANTHROPIC_API_KEY".to_owned(),
            ])
        );
        assert_eq!(
            get_env_api_key("anthropic", Some(&env)).as_deref(),
            Some("oauth-token")
        );
    }

    #[test]
    fn bedrock_ambient_sources_are_status_only() {
        let sources = [
            vec![("AWS_PROFILE", "dev")],
            vec![
                ("AWS_ACCESS_KEY_ID", "AKIA"),
                ("AWS_SECRET_ACCESS_KEY", "secret"),
            ],
            vec![("AWS_BEARER_TOKEN_BEDROCK", "token")],
            vec![("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI", "/credentials")],
            vec![("AWS_CONTAINER_CREDENTIALS_FULL_URI", "http://metadata")],
            vec![("AWS_WEB_IDENTITY_TOKEN_FILE", "/var/run/token")],
        ];
        for source in sources {
            let env = env_map(&source);
            assert_eq!(
                get_env_api_key("amazon-bedrock", Some(&env)).as_deref(),
                Some(AMBIENT_AUTH_MARKER)
            );
        }
        let incomplete = env_map(&[("AWS_ACCESS_KEY_ID", "AKIA")]);
        assert!(get_env_api_key("amazon-bedrock", Some(&incomplete)).is_none());
    }

    #[test]
    fn vertex_adc_is_status_only_and_explicit_key_wins() -> TestResult {
        clear_vertex_adc_cache();
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let dir = std::env::temp_dir().join(format!("pi-ai-vertex-adc-{suffix}"));
        fs::create_dir_all(&dir)?;
        let adc = dir.join("adc.json");
        fs::write(&adc, "{}")?;
        let adc_path = adc.to_string_lossy().into_owned();

        let ambient = env_map(&[
            ("GOOGLE_APPLICATION_CREDENTIALS", &adc_path),
            ("GCLOUD_PROJECT", "project"),
            ("GOOGLE_CLOUD_LOCATION", "us-central1"),
        ]);
        assert_eq!(
            get_env_api_key("google-vertex", Some(&ambient)).as_deref(),
            Some(AMBIENT_AUTH_MARKER)
        );

        let explicit = env_map(&[
            ("GOOGLE_APPLICATION_CREDENTIALS", &adc_path),
            ("GOOGLE_CLOUD_PROJECT", "project"),
            ("GOOGLE_CLOUD_LOCATION", "us-central1"),
            ("GOOGLE_CLOUD_API_KEY", "real-key"),
        ]);
        assert_eq!(
            get_env_api_key("google-vertex", Some(&explicit)).as_deref(),
            Some("real-key")
        );

        fs::remove_dir_all(dir)?;
        clear_vertex_adc_cache();
        Ok(())
    }

    #[tokio::test]
    async fn standard_auth_precedence_and_sentinel_filtering() -> TestResult {
        let auth = env_api_key_auth("Test key", &["PRIMARY_KEY", "SECONDARY_KEY"]);
        let context = MapAuthContext::new()
            .with_env("PRIMARY_KEY", "primary")
            .with_env("SECONDARY_KEY", "secondary");
        let stored = ApiKeyCredential {
            key: Some("stored".to_owned()),
            env: None,
        };
        let result = required(auth.resolve(&context, Some(&stored)).await, "stored auth")?;
        assert_eq!(result.auth.api_key.as_deref(), Some("stored"));

        let result = required(auth.resolve(&context, None).await, "ambient auth")?;
        assert_eq!(result.auth.api_key.as_deref(), Some("primary"));

        let sentinel_context = MapAuthContext::new().with_env("PRIMARY_KEY", AMBIENT_AUTH_MARKER);
        let result = required(
            auth.resolve(&sentinel_context, None).await,
            "sentinel status",
        )?;
        assert!(result.auth.api_key.is_none());
        Ok(())
    }

    struct StaticOauth;

    impl OAuthAuth for StaticOauth {
        fn name(&self) -> &'static str {
            "static"
        }

        fn login_label(&self) -> Option<&str> {
            Some("Static")
        }

        fn login<'a>(
            &'a self,
            _interaction: &'a dyn AuthInteraction,
        ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
            Box::pin(async {
                Ok(OAuthCredential {
                    refresh: "refresh".to_owned(),
                    access: "access".to_owned(),
                    expires: i64::MAX,
                    extra: BTreeMap::new(),
                })
            })
        }

        fn refresh<'a>(
            &'a self,
            credential: &'a OAuthCredential,
            _signal: Option<CancellationToken>,
        ) -> BoxFuture<'a, Result<OAuthCredential, AuthError>> {
            Box::pin(async move { Ok(credential.clone()) })
        }

        fn to_auth<'a>(
            &'a self,
            credential: &'a OAuthCredential,
        ) -> BoxFuture<'a, Result<ModelAuth, AuthError>> {
            Box::pin(async move {
                Ok(ModelAuth {
                    api_key: Some(credential.access.clone()),
                    headers: None,
                    base_url: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn lazy_oauth_loads_once_concurrently() -> TestResult {
        let loads = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&loads);
        let oauth = lazy_oauth("Lazy", Some("Login".to_owned()), move || {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Arc::new(StaticOauth) as Arc<dyn OAuthAuth>
            })
        });
        let credential = OAuthCredential {
            refresh: "refresh".to_owned(),
            access: "access".to_owned(),
            expires: i64::MAX,
            extra: BTreeMap::new(),
        };
        let (first, second) = tokio::join!(oauth.to_auth(&credential), oauth.to_auth(&credential));
        assert_eq!(first?.api_key.as_deref(), Some("access"));
        assert_eq!(second?.api_key.as_deref(), Some("access"));
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        Ok(())
    }
}
