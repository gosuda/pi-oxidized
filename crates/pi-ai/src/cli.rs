//! Crate-local OAuth CLI binary.
//!
//! Ports `.references/pi/packages/ai/src/cli.ts` at 8fa7eeb: commands
//! `help|--help|-h`, `list`, `login [provider]`; readline 1-indexed
//! selection; URL/device-code/progress console output in upstream ordering;
//! credentials to `auth.json` in cwd via the A7 auth store; `exit(1)` text
//! classes `Unknown command: <cmd>`, `Unknown provider: <id>`,
//! `Invalid selection`.

use std::io::{BufRead, Write};
use std::path::Path;

use futures::future::BoxFuture;
use pi_ai::auth::error::{AuthError, StoreError};
use pi_ai::auth::file_store::FileCredentialStore;
use pi_ai::auth::oauth::radius::RadiusOAuthOptions;
use pi_ai::auth::oauth::{
    anthropic::AnthropicOAuth, github_copilot::GitHubCopilotOAuth, kimi_coding::KimiCodingOAuth,
    openai_codex::OpenAiCodexOAuth, openrouter::OpenRouterOAuth, radius::RadiusOAuth,
    xai::XaiOAuth,
};
use pi_ai::auth::types::{
    AuthEvent, AuthInteraction, AuthPrompt, Credential, CredentialStore, OAuthAuth,
};
use tokio_util::sync::CancellationToken;

const AUTH_FILE: &str = "auth.json";

/// One OAuth-capable provider entry.
struct OAuthProvider {
    id: &'static str,
    name: &'static str,
    build: fn() -> std::sync::Arc<dyn OAuthAuth>,
}

/// All OAuth-capable providers in catalog order (matching upstream
/// `builtinProviders().filter(p => p.auth.oauth != null)`).
const PROVIDERS: &[OAuthProvider] = &[
    OAuthProvider {
        id: "anthropic",
        name: "Anthropic",
        build: || {
            AnthropicOAuth::new()
                .map(|auth| std::sync::Arc::new(auth) as std::sync::Arc<dyn OAuthAuth>)
                .unwrap_or_else(|_| {
                    std::sync::Arc::new(AnthropicOAuth::default()) as std::sync::Arc<dyn OAuthAuth>
                })
        },
    },
    OAuthProvider {
        id: "github-copilot",
        name: "GitHub Copilot",
        build: || {
            GitHubCopilotOAuth::shared().unwrap_or_else(|_| {
                std::sync::Arc::new(GitHubCopilotOAuth::with_client(
                    pi_ai::auth::http::AuthHttpClient::from_client(reqwest::Client::new()),
                )) as std::sync::Arc<dyn OAuthAuth>
            })
        },
    },
    OAuthProvider {
        id: "kimi-coding",
        name: "Kimi For Coding",
        build: || {
            KimiCodingOAuth::shared().unwrap_or_else(|_| {
                std::sync::Arc::new(KimiCodingOAuth::default()) as std::sync::Arc<dyn OAuthAuth>
            })
        },
    },
    OAuthProvider {
        id: "openai-codex",
        name: "OpenAI Codex",
        build: || {
            OpenAiCodexOAuth::shared().unwrap_or_else(|_| {
                std::sync::Arc::new(OpenAiCodexOAuth::with_http(
                    pi_ai::auth::http::AuthHttpClient::from_client(reqwest::Client::new()),
                )) as std::sync::Arc<dyn OAuthAuth>
            })
        },
    },
    OAuthProvider {
        id: "openrouter",
        name: "OpenRouter",
        build: || {
            OpenRouterOAuth::shared().unwrap_or_else(|_| {
                std::sync::Arc::new(OpenRouterOAuth::default()) as std::sync::Arc<dyn OAuthAuth>
            })
        },
    },
    OAuthProvider {
        id: "radius",
        name: "Radius",
        build: || {
            RadiusOAuth::new(RadiusOAuthOptions {
                name: "Radius".to_owned(),
                gateway: "https://radius.pi.dev".to_owned(),
            })
            .map(|auth| std::sync::Arc::new(auth) as std::sync::Arc<dyn OAuthAuth>)
            .unwrap_or_else(|_| {
                std::sync::Arc::new(RadiusOAuth::with_client(
                    RadiusOAuthOptions {
                        name: "Radius".to_owned(),
                        gateway: "https://radius.pi.dev".to_owned(),
                    },
                    pi_ai::auth::http::AuthHttpClient::from_client(reqwest::Client::new()),
                    1456,
                )) as std::sync::Arc<dyn OAuthAuth>
            })
        },
    },
    OAuthProvider {
        id: "xai",
        name: "xAI",
        build: || {
            XaiOAuth::shared().unwrap_or_else(|_| {
                std::sync::Arc::new(XaiOAuth::default()) as std::sync::Arc<dyn OAuthAuth>
            })
        },
    },
];

fn find_provider(id: &str) -> Option<&'static OAuthProvider> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}

fn prompt_line(question: &str) -> String {
    let stdout = std::io::stdout();
    let mut stdout_lock = stdout.lock();
    let _ = write!(stdout_lock, "{question}");
    let _ = stdout_lock.flush();

    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return String::new();
    }
    line.trim_end_matches(['\r', '\n']).to_owned()
}

async fn persist_credential(
    path: &Path,
    provider_id: &str,
    credential: Credential,
) -> Result<(), StoreError> {
    let store = FileCredentialStore::new(path)?;
    store
        .modify(
            provider_id,
            Box::new(move |_| Box::pin(async move { Ok(Some(credential)) })),
        )
        .await?;
    Ok(())
}

struct StdinInteraction {
    signal: CancellationToken,
}

impl AuthInteraction for StdinInteraction {
    fn prompt(&self, auth_prompt: AuthPrompt) -> BoxFuture<'_, Result<String, AuthError>> {
        Box::pin(async move {
            match auth_prompt {
                AuthPrompt::Select {
                    message, options, ..
                } => {
                    println!("\n{message}");
                    for (index, option) in options.iter().enumerate() {
                        println!("  {}. {}", index + 1, option.label);
                    }
                    let count = options.len();
                    let answer = prompt_line(&format!("Enter number (1-{count}): "));
                    let choice: usize = answer
                        .parse::<usize>()
                        .ok()
                        .filter(|n| *n >= 1 && *n <= count)
                        .map(|n| n - 1)
                        .unwrap_or(usize::MAX);
                    let selected = options.get(choice);
                    match selected {
                        Some(option) => Ok(option.id.clone()),
                        None => Err(AuthError::message("Invalid selection")),
                    }
                }
                AuthPrompt::Text {
                    message,
                    placeholder,
                    ..
                } => {
                    let suffix = placeholder
                        .as_deref()
                        .filter(|p| !p.is_empty())
                        .map(|p| format!(" ({p})"))
                        .unwrap_or_default();
                    Ok(prompt_line(&format!("{message}{suffix}: ")))
                }
                AuthPrompt::Secret {
                    message,
                    placeholder,
                    ..
                } => {
                    let suffix = placeholder
                        .as_deref()
                        .filter(|p| !p.is_empty())
                        .map(|p| format!(" ({p})"))
                        .unwrap_or_default();
                    Ok(prompt_line(&format!("{message}{suffix}: ")))
                }
                AuthPrompt::ManualCode {
                    message,
                    placeholder,
                    ..
                } => {
                    let suffix = placeholder
                        .as_deref()
                        .filter(|p| !p.is_empty())
                        .map(|p| format!(" ({p})"))
                        .unwrap_or_default();
                    Ok(prompt_line(&format!("{message}{suffix}: ")))
                }
            }
        })
    }

    fn notify(&self, event: AuthEvent) {
        match event {
            AuthEvent::AuthUrl { url, instructions } => {
                println!("\nOpen this URL in your browser:\n{url}");
                if let Some(instructions) = instructions {
                    println!("{instructions}");
                }
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                ..
            } => {
                println!("\nOpen this URL in your browser:\n{verification_uri}");
                println!("Enter code: {user_code}");
            }
            AuthEvent::Info { message, .. } => {
                println!("{message}");
            }
            AuthEvent::Progress { message } => {
                println!("{message}");
            }
        }
    }

    fn signal(&self) -> Option<CancellationToken> {
        Some(self.signal.clone())
    }
}

async fn login(provider_id: &str) -> Result<(), String> {
    let provider =
        find_provider(provider_id).ok_or_else(|| format!("Unknown provider: {provider_id}"))?;

    let oauth = (provider.build)();
    let interaction = StdinInteraction {
        signal: CancellationToken::new(),
    };

    let credential = oauth.login(&interaction).await.map_err(|e| e.to_string())?;

    let auth_path = std::path::PathBuf::from(AUTH_FILE);
    persist_credential(&auth_path, provider_id, Credential::Oauth(credential))
        .await
        .map_err(|error| error.to_string())?;
    println!("\nCredentials saved to {AUTH_FILE}");
    Ok(())
}

fn print_help() {
    let provider_list = PROVIDERS
        .iter()
        .map(|p| format!("  {:<20} {}", p.id, p.name))
        .collect::<Vec<_>>()
        .join("\n");
    println!(
        "Usage: pi-ai <command> [provider]\n\nCommands:\n  login [provider]  Login to an OAuth provider\n  list              List available providers\n\nProviders:\n{provider_list}"
    );
}

fn print_list() {
    for provider in PROVIDERS {
        println!("{:<20} {}", provider.id, provider.name);
    }
}

async fn main_impl() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("");

    if command.is_empty() || command == "help" || command == "--help" || command == "-h" {
        print_help();
        return Ok(());
    }

    if command == "list" {
        print_list();
        return Ok(());
    }

    if command == "login" {
        let provider_id = args.get(1).map(String::as_str);

        let provider_id = match provider_id {
            Some(id) => id.to_owned(),
            None => {
                for (index, provider) in PROVIDERS.iter().enumerate() {
                    println!("  {}. {}", index + 1, provider.name);
                }
                let count = PROVIDERS.len();
                let answer = prompt_line(&format!("Enter number (1-{count}): "));
                let index: usize = answer
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n >= 1 && *n <= count)
                    .map(|n| n - 1)
                    .unwrap_or(usize::MAX);
                PROVIDERS
                    .get(index)
                    .map(|p| p.id.to_owned())
                    .unwrap_or_default()
            }
        };

        if provider_id.is_empty() || !PROVIDERS.iter().any(|p| p.id == provider_id) {
            return Err(format!("Unknown provider: {provider_id}"));
        }

        login(&provider_id).await?;
        return Ok(());
    }

    Err(format!("Unknown command: {command}"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match main_impl().await {
        Ok(()) => {}
        Err(message) => {
            eprintln!("Error: {message}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use pi_ai::auth::types::OAuthCredential;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    fn oauth(access: &str) -> Credential {
        Credential::Oauth(OAuthCredential {
            refresh: "refresh".to_owned(),
            access: access.to_owned(),
            expires: 1,
            extra: BTreeMap::new(),
        })
    }

    #[tokio::test]
    async fn corrupt_auth_bytes_are_not_replaced() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("auth.json");
        let corrupt = b"{\"openai-codex\":";
        fs::write(&path, corrupt)?;

        let result = persist_credential(&path, "openai-codex", oauth("new")).await;

        assert!(result.is_err());
        assert_eq!(fs::read(path)?, corrupt);
        Ok(())
    }

    #[tokio::test]
    async fn missing_auth_file_is_created() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("auth.json");
        let expected = oauth("first");

        persist_credential(&path, "openai-codex", expected.clone()).await?;

        let store = FileCredentialStore::new(path)?;
        assert_eq!(store.read("openai-codex").await?, Some(expected));
        Ok(())
    }

    #[tokio::test]
    async fn valid_auth_entries_survive_an_update() -> TestResult {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("auth.json");
        let first = oauth("first");
        let second = oauth("second");
        persist_credential(&path, "anthropic", first.clone()).await?;

        persist_credential(&path, "openai-codex", second.clone()).await?;

        let store = FileCredentialStore::new(path)?;
        assert_eq!(store.read("anthropic").await?, Some(first));
        assert_eq!(store.read("openai-codex").await?, Some(second));
        Ok(())
    }
}
