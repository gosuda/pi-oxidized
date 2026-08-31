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
use pi_ai::auth::types::{AuthEvent, AuthInteraction, AuthPrompt, Credential, CredentialStore};
use pi_ai::auth::{builtin_oauth_provider, builtin_oauth_providers};
use tokio_util::sync::CancellationToken;

const AUTH_FILE: &str = "auth.json";

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
    let provider = builtin_oauth_provider(provider_id)
        .ok_or_else(|| format!("Unknown provider: {provider_id}"))?;

    let oauth = provider.create();
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
    let provider_list = builtin_oauth_providers()
        .map(|(id, provider)| format!("  {id:<20} {}", provider.name()))
        .collect::<Vec<_>>()
        .join("\n");
    println!(
        "Usage: pi-ai <command> [provider]\n\nCommands:\n  login [provider]  Login to an OAuth provider\n  list              List available providers\n\nProviders:\n{provider_list}"
    );
}

fn print_list() {
    for (id, provider) in builtin_oauth_providers() {
        println!("{id:<20} {}", provider.name());
    }
}

async fn main_impl() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map_or("", String::as_str);

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

        let provider_id = if let Some(id) = provider_id {
            id.to_owned()
        } else {
            for (index, (_, provider)) in builtin_oauth_providers().enumerate() {
                println!("  {}. {}", index + 1, provider.name());
            }
            let count = builtin_oauth_providers().count();
            let answer = prompt_line(&format!("Enter number (1-{count}): "));
            let index = answer
                .parse::<usize>()
                .ok()
                .filter(|n| *n >= 1 && *n <= count)
                .map_or(usize::MAX, |n| n - 1);
            builtin_oauth_providers()
                .nth(index)
                .map(|(id, _)| id.to_owned())
                .unwrap_or_default()
        };

        if provider_id.is_empty() || builtin_oauth_provider(&provider_id).is_none() {
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
