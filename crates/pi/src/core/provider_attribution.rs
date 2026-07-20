//! Provider request attribution and session headers.

use std::collections::BTreeMap;

use pi_ai::types::Model;
use url::Url;

use super::settings::SettingsManager;

const OPENROUTER_HOST: &str = "openrouter.ai";
const NVIDIA_NIM_HOST: &str = "integrate.api.nvidia.com";
const CLOUDFLARE_API_HOST: &str = "api.cloudflare.com";
const CLOUDFLARE_AI_GATEWAY_HOST: &str = "gateway.ai.cloudflare.com";
const OPENCODE_HOST: &str = "opencode.ai";

/// Whether anonymous install telemetry and provider attribution are enabled.
///
/// `PI_TELEMETRY` overrides the persisted setting exactly as the reference
/// implementation does: only `1`, `true`, and `yes` (case-insensitive) enable it.
#[must_use]
pub fn is_install_telemetry_enabled(settings: &SettingsManager) -> bool {
    is_install_telemetry_enabled_for_env(settings, std::env::var("PI_TELEMETRY").ok().as_deref())
}

#[must_use]
fn is_install_telemetry_enabled_for_env(
    settings: &SettingsManager,
    telemetry_env: Option<&str>,
) -> bool {
    telemetry_env.map_or_else(
        || settings.get_enable_install_telemetry(),
        |value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
    )
}

/// Merge session and telemetry-gated provider attribution headers with explicit
/// sources. Later sources win case-insensitively.
#[must_use]
pub fn merge_provider_attribution_headers(
    model: &Model,
    settings: &SettingsManager,
    session_id: Option<&str>,
    header_sources: impl IntoIterator<Item = Option<BTreeMap<String, Option<String>>>>,
) -> Option<BTreeMap<String, Option<String>>> {
    merge_provider_attribution_headers_with_telemetry(
        model,
        is_install_telemetry_enabled(settings),
        session_id,
        header_sources,
    )
}

/// Merge session and telemetry-gated provider attribution headers with explicit
/// sources. Later sources win case-insensitively.
#[must_use]
pub fn merge_provider_attribution_headers_with_telemetry(
    model: &Model,
    telemetry_enabled: bool,
    session_id: Option<&str>,
    header_sources: impl IntoIterator<Item = Option<BTreeMap<String, Option<String>>>>,
) -> Option<BTreeMap<String, Option<String>>> {
    let mut headers = BTreeMap::new();
    if let Some(session_headers) = session_headers(model, session_id) {
        merge_headers(&mut headers, session_headers);
    }
    if telemetry_enabled && let Some(attribution_headers) = attribution_headers(model) {
        merge_headers(&mut headers, attribution_headers);
    }
    for source in header_sources.into_iter().flatten() {
        merge_headers(&mut headers, source);
    }
    (!headers.is_empty()).then_some(headers)
}

fn merge_headers(
    destination: &mut BTreeMap<String, Option<String>>,
    source: BTreeMap<String, Option<String>>,
) {
    for (name, value) in source {
        destination.retain(|existing, _| !existing.eq_ignore_ascii_case(&name));
        destination.insert(name, value);
    }
}

fn attribution_headers(model: &Model) -> Option<BTreeMap<String, Option<String>>> {
    let headers = if is_openrouter_model(model) {
        BTreeMap::from([
            ("HTTP-Referer".to_owned(), Some("https://pi.dev".to_owned())),
            ("X-OpenRouter-Title".to_owned(), Some("pi".to_owned())),
            (
                "X-OpenRouter-Categories".to_owned(),
                Some("cli-agent".to_owned()),
            ),
        ])
    } else if is_nvidia_nim_model(model) {
        BTreeMap::from([("X-BILLING-INVOKE-ORIGIN".to_owned(), Some("Pi".to_owned()))])
    } else if is_cloudflare_model(model) {
        BTreeMap::from([("User-Agent".to_owned(), Some("pi-coding-agent".to_owned()))])
    } else {
        return None;
    };
    Some(headers)
}

fn session_headers(
    model: &Model,
    session_id: Option<&str>,
) -> Option<BTreeMap<String, Option<String>>> {
    let session_id = session_id?;
    if model.provider != "opencode"
        && model.provider != "opencode-go"
        && !matches_host(&model.base_url, OPENCODE_HOST)
    {
        return None;
    }
    Some(BTreeMap::from([
        ("x-opencode-session".to_owned(), Some(session_id.to_owned())),
        ("x-opencode-client".to_owned(), Some("pi".to_owned())),
    ]))
}

fn is_openrouter_model(model: &Model) -> bool {
    model.provider == "openrouter" || model.base_url.contains(OPENROUTER_HOST)
}

fn is_nvidia_nim_model(model: &Model) -> bool {
    model.provider == "nvidia" || matches_host(&model.base_url, NVIDIA_NIM_HOST)
}

fn is_cloudflare_model(model: &Model) -> bool {
    matches!(
        model.provider.as_str(),
        "cloudflare-workers-ai" | "cloudflare-ai-gateway"
    ) || matches_host(&model.base_url, CLOUDFLARE_API_HOST)
        || matches_host(&model.base_url, CLOUDFLARE_AI_GATEWAY_HOST)
}

fn matches_host(base_url: &str, expected_host: &str) -> bool {
    Url::parse(base_url).is_ok_and(|url| url.host_str() == Some(expected_host))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings::{Settings, SettingsManagerCreateOptions};
    use pi_ai::types::{ModelCost, ModelInput};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn settings(telemetry_enabled: bool) -> SettingsManager {
        SettingsManager::in_memory(
            &Settings {
                enable_install_telemetry: Some(telemetry_enabled),
                ..Settings::default()
            },
            SettingsManagerCreateOptions::default(),
        )
    }

    fn model(provider: &str, base_url: &str) -> Model {
        Model {
            id: "test".to_owned(),
            name: "test".to_owned(),
            api: "openai-completions".to_owned(),
            provider: provider.to_owned(),
            base_url: base_url.to_owned(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: ModelCost::default(),
            context_window: 1,
            max_tokens: 1,
            headers: None,
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    fn header<'a>(headers: &'a BTreeMap<String, Option<String>>, name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .and_then(|(_, value)| value.as_deref())
    }

    #[test]
    fn attribution_matches_supported_provider_hosts() -> TestResult {
        let settings = settings(true);
        for (model, expected) in [
            (
                model("openrouter", "https://openrouter.ai/api/v1"),
                [
                    ("HTTP-Referer", "https://pi.dev"),
                    ("X-OpenRouter-Title", "pi"),
                    ("X-OpenRouter-Categories", "cli-agent"),
                ]
                .as_slice(),
            ),
            (
                model("nvidia", "https://integrate.api.nvidia.com/v1"),
                [("X-BILLING-INVOKE-ORIGIN", "Pi")].as_slice(),
            ),
            (
                model(
                    "cloudflare-workers-ai",
                    "https://api.cloudflare.com/client/v4",
                ),
                [("User-Agent", "pi-coding-agent")].as_slice(),
            ),
        ] {
            let headers = merge_provider_attribution_headers(&model, &settings, None, [])
                .ok_or("expected attribution headers")?;
            for (name, value) in expected {
                assert_eq!(header(&headers, name), Some(*value));
            }
        }
        Ok(())
    }

    #[test]
    fn telemetry_setting_and_env_override_gate_attribution() {
        let disabled = settings(false);
        let model = model("openrouter", "https://openrouter.ai/api/v1");
        assert!(merge_provider_attribution_headers(&model, &disabled, None, []).is_none());
        assert!(!is_install_telemetry_enabled_for_env(
            &settings(true),
            Some("0")
        ));
        assert!(is_install_telemetry_enabled_for_env(&disabled, Some("yes")));
    }

    #[test]
    fn explicit_headers_override_attribution_and_session_headers() -> TestResult {
        let settings = settings(true);
        let openrouter = model("openrouter", "https://openrouter.ai/api/v1");
        let explicit = BTreeMap::from([(
            "http-referer".to_owned(),
            Some("https://example.test".to_owned()),
        )]);
        let headers =
            merge_provider_attribution_headers(&openrouter, &settings, None, [Some(explicit)])
                .ok_or("expected headers")?;
        assert_eq!(
            header(&headers, "http-referer"),
            Some("https://example.test")
        );

        let opencode = model("opencode", "https://opencode.ai/api");
        let explicit =
            BTreeMap::from([("X-OPENCODE-CLIENT".to_owned(), Some("custom".to_owned()))]);
        let headers = merge_provider_attribution_headers(
            &opencode,
            &settings,
            Some("session-1"),
            [Some(explicit)],
        )
        .ok_or("expected headers")?;
        assert_eq!(header(&headers, "x-opencode-session"), Some("session-1"));
        assert_eq!(header(&headers, "x-opencode-client"), Some("custom"));
        Ok(())
    }
}
