//! `pi auth` command suite: `auth check`, `auth print-api-key`, and
//! `auth print-bearer-token`.
//!
//! Ports coding-agent `cli/auth-command.ts`, `cli/auth-check.ts`,
//! `cli/credential-print.ts`, and the `runAuthCommand` dispatch in `main.ts`:
//! source help, command/option parsing, provider/model selection, JSON and
//! credentials output, no-refresh and min-expiry semantics, and exit codes
//! 0/1/2. Credential material is emitted only on the explicitly requested
//! print paths and never logged; every other surface reports status alone.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use pi_ai::auth::types::CredentialModifyFn;
use pi_ai::auth::{
    AuthResult, AuthType, Credential, CredentialInfo, CredentialKind, CredentialStore,
    FileCredentialStore, StoreError,
};
use pi_ai::models_store::InMemoryModelsStore;
use pi_ai::types::Model;
use regex::Regex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::cli::args::{Args, parse_args};
use crate::cli::bootstrap::BootstrapIo;
use crate::core::config::{APP_NAME, get_agent_dir};
use crate::core::model_resolver::{ResolveCliModelOptions, resolve_cli_model};
use crate::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};

/// Upper bound for one credential-print invocation
/// (source `AbortSignal.timeout(15_000)` around the print path).
const PRINT_TIMEOUT: Duration = Duration::from_secs(15);
/// Default remaining validity required of an exported bearer token
/// (source `DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS`).
const DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS: i64 = 30 * 60_000;
/// Error carried by read-only credential writes
/// (source `ReadOnlyAuthStorage.modify`/`delete`).
const READ_ONLY_CREDENTIAL_ERROR: &str = "Read-only credential storage cannot modify auth.json";

static BEARER_TOKEN_REGEX: std::sync::LazyLock<Option<Regex>> =
    std::sync::LazyLock::new(|| Regex::new(r"(?i)^Bearer\s+(.+)$").ok());

/// Which `auth` subcommand parsed out of argv.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthCommandKind {
    /// `auth check`: status probe with optional JSON/credentials output.
    Check,
    /// `auth print-api-key`: print one configured API key.
    ApiKey,
    /// `auth print-bearer-token`: print one OAuth access token.
    BearerToken,
}

impl AuthCommandKind {
    /// Source command name used in diagnostics.
    #[must_use]
    pub fn command_name(self) -> &'static str {
        match self {
            Self::Check => "auth check",
            Self::ApiKey => "auth print-api-key",
            Self::BearerToken => "auth print-bearer-token",
        }
    }

    /// Source usage line for unknown-option diagnostics.
    #[must_use]
    pub fn usage(self) -> String {
        match self {
            Self::Check => {
                format!(
                    "{APP_NAME} auth check --provider <provider> [--json] [--credentials] [--no-refresh]"
                )
            }
            Self::ApiKey => {
                format!("{APP_NAME} auth print-api-key --provider <provider> [--model <model>]")
            }
            Self::BearerToken => format!(
                "{APP_NAME} auth print-bearer-token --provider <provider> [--model <model>] [--min-expiry <duration>]"
            ),
        }
    }
}

/// Parsed `auth` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthCommand {
    /// Subcommand.
    pub kind: AuthCommandKind,
    /// Remaining arguments forwarded to the shared flag parser.
    pub args: Vec<String>,
    /// `--json` (check only).
    pub json: bool,
    /// `--credentials` (check only).
    pub credentials: bool,
    /// `--no-refresh` (check only).
    pub no_refresh: bool,
    /// `--min-expiry` in milliseconds (bearer token only).
    pub min_expiry_ms: Option<i64>,
}

/// Source `AuthCommandError`: message-only failure surfaced as `Error: …`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthCommandError {
    /// Human-readable message printed after the `Error: ` prefix.
    pub message: String,
}

impl AuthCommandError {
    /// Create an error carrying one message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// `--provider`/`--model` targets after trimming and emptiness filtering.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuthTarget {
    /// Trimmed `--provider` value, `None` when absent or blank.
    pub provider: Option<String>,
    /// Trimmed `--model` value, `None` when absent or blank.
    pub model: Option<String>,
}

/// Whether argv starts an `auth` command at all (source `args[0] === "auth"`).
#[must_use]
pub fn is_auth_invocation(args: &[String]) -> bool {
    args.first().is_some_and(|first| first == "auth")
}

/// Source `isAuthCommandHelp`: bare `auth`, `auth help`, or a help flag.
#[must_use]
pub fn is_auth_command_help(args: &[String]) -> bool {
    is_auth_invocation(args)
        && (args.len() == 1
            || args.get(1).is_some_and(|second| second == "help")
            || args.iter().any(|arg| arg == "--help" || arg == "-h"))
}

/// Source `printAuthCommandHelp` text (single trailing newline added by the writer).
#[must_use]
pub fn auth_command_help_text() -> String {
    format!(
        "Usage:\n  \
         {APP_NAME} auth print-api-key [--provider <provider>] [--model <model>]\n  \
         {APP_NAME} auth print-bearer-token [--provider <provider>] [--model <model>] [--min-expiry <duration>]\n  \
         {APP_NAME} auth check [--provider <provider>] [--model <model>] [--json] [--credentials] [--no-refresh]\n\n\
         Auth commands require at least one of --provider or --model. Checks refresh expired \
         OAuth credentials by default; --no-refresh prevents this. --credentials emits the \
         credential, or includes it in JSON output."
    )
}

/// Parses one duration such as `30m`, `500ms`, or `1h`
/// (source regex `^(\d+)(ms|s|m|h)$` case-insensitive).
fn parse_duration_ms(value: &str) -> Option<i64> {
    let lower = value.to_ascii_lowercase();
    let (digits, multiplier) = split_duration_suffix(&lower)?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i64>().ok()?.checked_mul(multiplier)
}

/// Split a lowercased duration into its digit prefix and unit multiplier.
fn split_duration_suffix(lower: &str) -> Option<(&str, i64)> {
    if let Some(rest) = lower.strip_suffix("ms") {
        Some((rest, 1_i64))
    } else if let Some(rest) = lower.strip_suffix('s') {
        Some((rest, 1_000))
    } else if let Some(rest) = lower.strip_suffix('m') {
        Some((rest, 60_000))
    } else if let Some(rest) = lower.strip_suffix('h') {
        Some((rest, 3_600_000))
    } else {
        None
    }
}

/// Source `parseAuthCommand`. The caller checks [`is_auth_invocation`] first;
/// `Err` covers unknown subcommands and auth-flag misuse.
///
/// # Errors
///
/// Returns [`AuthCommandError`] for an unknown subcommand, flags restricted to
/// another subcommand, or a malformed `--min-expiry` value.
pub fn parse_auth_command(args: &[String]) -> Result<AuthCommand, AuthCommandError> {
    let kind = match args.get(1).map(String::as_str) {
        Some("check") => AuthCommandKind::Check,
        Some("print-api-key") => AuthCommandKind::ApiKey,
        Some("print-bearer-token") => AuthCommandKind::BearerToken,
        other => {
            return Err(AuthCommandError::new(format!(
                "Unknown auth command \"{}\". Use \"{APP_NAME} auth print-api-key\", \
                 \"{APP_NAME} auth print-bearer-token\", or \"{APP_NAME} auth check\".",
                other.unwrap_or(""),
            )));
        }
    };

    let mut command_args: Vec<String> = Vec::new();
    let mut json = false;
    let mut credentials = false;
    let mut no_refresh = false;
    let mut min_expiry_ms: Option<i64> = None;
    let mut index = 2;
    while let Some(arg) = args.get(index).map(String::as_str) {
        index += 1;
        if arg == "--min-expiry" {
            if kind != AuthCommandKind::BearerToken {
                return Err(AuthCommandError::new(
                    "--min-expiry is only supported by print-bearer-token",
                ));
            }
            let duration = args
                .get(index)
                .map(String::as_str)
                .and_then(parse_duration_ms);
            index += 1;
            let Some(duration) = duration else {
                return Err(AuthCommandError::new(
                    "--min-expiry must use a duration such as 30m or 1h",
                ));
            };
            min_expiry_ms = Some(duration);
            continue;
        }
        if arg == "--json" || arg == "--credentials" || arg == "--no-refresh" {
            if kind != AuthCommandKind::Check {
                return Err(AuthCommandError::new(format!(
                    "{arg} is only supported by auth check"
                )));
            }
            match arg {
                "--json" => json = true,
                "--credentials" => credentials = true,
                _ => no_refresh = true,
            }
            continue;
        }
        command_args.push(arg.to_owned());
    }

    Ok(AuthCommand {
        kind,
        args: command_args,
        json,
        credentials,
        no_refresh,
        min_expiry_ms,
    })
}

/// Source `validateAuthCommandArgs` minus the unknown-flag check: the runner
/// pre-checks unknown flags with the usage hint before validation runs.
///
/// # Errors
///
/// Returns [`AuthCommandError`] when non-auth payload flags are present or
/// neither `--provider` nor `--model` resolves.
pub fn validate_auth_command_args(
    args: &Args,
    kind: AuthCommandKind,
) -> Result<AuthTarget, AuthCommandError> {
    let target = AuthTarget {
        provider: non_blank(args.provider.as_deref()),
        model: non_blank(args.model.as_deref()),
    };
    if args.api_key.is_some() || !args.messages.is_empty() || !args.file_args.is_empty() {
        return Err(AuthCommandError::new(
            "Auth commands only accept --provider and --model",
        ));
    }
    if target.provider.is_none() && target.model.is_none() {
        return Err(AuthCommandError::new(match kind {
            AuthCommandKind::Check => {
                "Auth checks require --provider <provider> or --model <model>"
            }
            AuthCommandKind::ApiKey | AuthCommandKind::BearerToken => {
                "Credential printing requires --provider <provider> or --model <model>"
            }
        }));
    }
    Ok(target)
}

fn non_blank(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|trimmed| !trimmed.is_empty())
        .map(str::to_owned)
}

/// Source `getAuthCredential`: the auth API key, else the `Bearer` token of an
/// `Authorization` header. Blank keys fall through to headers like the source.
#[must_use]
pub fn get_auth_credential(auth: Option<&AuthResult>) -> Option<String> {
    let auth = auth?;
    if let Some(key) = auth.auth.api_key.as_deref().filter(|key| !key.is_empty()) {
        return Some(key.to_owned());
    }
    let headers = auth.auth.headers.as_ref()?;
    let (_, value) = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))?;
    let value = value.as_deref()?;
    let bearer = (*BEARER_TOKEN_REGEX).as_ref()?;
    bearer
        .captures(value)?
        .get(1)
        .map(|matched| matched.as_str().to_owned())
}

/// Read-only credential-store view backing `--no-refresh`
/// (source `ReadOnlyAuthStorage`): reads pass through, writes fail so a
/// refresh can never persist while the command promised not to mutate.
pub struct ReadOnlyCredentialStore {
    inner: Arc<dyn CredentialStore>,
}

impl ReadOnlyCredentialStore {
    /// Wrap a backing store.
    #[must_use]
    pub fn new(inner: Arc<dyn CredentialStore>) -> Self {
        Self { inner }
    }
}

impl CredentialStore for ReadOnlyCredentialStore {
    fn read<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
        self.inner.read(provider_id)
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<CredentialInfo>, StoreError>> {
        self.inner.list()
    }

    fn modify<'a>(
        &'a self,
        _provider_id: &'a str,
        _callback: Box<CredentialModifyFn<'a>>,
    ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
        Box::pin(async { Err(StoreError::Message(READ_ONLY_CREDENTIAL_ERROR.to_owned())) })
    }

    fn delete<'a>(&'a self, _provider_id: &'a str) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async { Err(StoreError::Message(READ_ONLY_CREDENTIAL_ERROR.to_owned())) })
    }
}

/// Source `AuthCheckStatus`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthCheckStatus {
    /// Provider is configured and resolvable.
    Ready,
    /// Provider is known but credentials are missing or unavailable.
    NotReady,
    /// Runtime or credential state could not be resolved.
    Invalid,
}

impl AuthCheckStatus {
    /// Source JSON/text spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NotReady => "not_ready",
            Self::Invalid => "invalid",
        }
    }
}

/// Source `AuthCheckReason`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthCheckReason {
    /// Provider id is not in the composed catalog.
    ProviderNotFound,
    /// No stored or ambient credential is configured.
    CredentialsNotConfigured,
    /// `--credentials` was requested but no credential printed.
    CredentialNotAvailable,
    /// Runtime or resolution state is broken.
    InvalidState,
}

impl AuthCheckResult {
    fn ready(provider: String, auth_type: AuthType) -> Self {
        Self {
            status: AuthCheckStatus::Ready,
            provider,
            reason: None,
            auth_type: Some(auth_type),
        }
    }

    fn not_ready(provider: String, reason: AuthCheckReason) -> Self {
        Self {
            status: AuthCheckStatus::NotReady,
            provider,
            reason: Some(reason),
            auth_type: None,
        }
    }

    fn invalid(provider: String) -> Self {
        Self {
            status: AuthCheckStatus::Invalid,
            provider,
            reason: Some(AuthCheckReason::InvalidState),
            auth_type: None,
        }
    }
}

/// Source `AuthCheckResult`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthCheckResult {
    /// Overall status.
    pub status: AuthCheckStatus,
    /// Resolved provider id.
    pub provider: String,
    /// Failure reason when not ready.
    pub reason: Option<AuthCheckReason>,
    /// Configured auth mechanism when ready.
    pub auth_type: Option<AuthType>,
}

fn auth_type_str(auth_type: AuthType) -> &'static str {
    match auth_type {
        AuthType::ApiKey => "api_key",
        AuthType::Oauth => "oauth",
    }
}

fn reason_str(reason: AuthCheckReason) -> &'static str {
    match reason {
        AuthCheckReason::ProviderNotFound => "provider_not_found",
        AuthCheckReason::CredentialsNotConfigured => "credentials_not_configured",
        AuthCheckReason::CredentialNotAvailable => "credential_not_available",
        AuthCheckReason::InvalidState => "invalid_state",
    }
}

/// Source JSON shape: `{status, provider, reason?, authType?, credentials?}`.
fn auth_check_json(result: &AuthCheckResult, credential: Option<&str>) -> Value {
    let mut value = json!({
        "status": result.status.as_str(),
        "provider": result.provider,
    });
    if let Some(reason) = result.reason {
        value["reason"] = json!(reason_str(reason));
    }
    if let Some(auth_type) = result.auth_type {
        value["authType"] = json!(auth_type_str(auth_type));
    }
    if let Some(credential) = credential {
        value["credentials"] = json!(credential);
    }
    value
}

fn has_provider(model_runtime: &ModelRuntime, provider_id: &str) -> bool {
    model_runtime
        .get_provider_ids()
        .iter()
        .any(|known| known == provider_id)
}

/// Source `checkProviderAuth`: validation and model resolution errors escape;
/// runtime, probe, and refresh failures map onto `invalid`/`not_ready` states.
///
/// # Errors
///
/// Returns [`AuthCommandError`] for validation or model-resolution failures
/// (printed with exit code 2 by the runner).
pub async fn check_provider_auth(
    args: &Args,
    model_runtime: &ModelRuntime,
    refresh: bool,
) -> Result<AuthCheckResult, AuthCommandError> {
    let target = validate_auth_command_args(args, AuthCommandKind::Check)?;
    let provider = resolve_check_provider(model_runtime, &target)?;
    if model_runtime.get_error().is_some() {
        return Ok(AuthCheckResult::invalid(provider));
    }
    if !has_provider(model_runtime, &provider) {
        return Ok(AuthCheckResult::not_ready(
            provider,
            AuthCheckReason::ProviderNotFound,
        ));
    }
    let Some(auth) = model_runtime.check_auth(&provider).await else {
        return Ok(AuthCheckResult::not_ready(
            provider,
            AuthCheckReason::CredentialsNotConfigured,
        ));
    };
    if refresh {
        match model_runtime
            .get_auth_with_options(&provider, None, None, None)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Ok(AuthCheckResult::not_ready(
                    provider,
                    AuthCheckReason::CredentialsNotConfigured,
                ));
            }
            Err(_) => return Ok(AuthCheckResult::invalid(provider)),
        }
    }
    Ok(AuthCheckResult::ready(provider, auth.kind))
}

/// Resolve the checked provider id from `--provider`/`--model`
/// (source `resolveCliModel` delegation in `checkProviderAuth`).
fn resolve_check_provider(
    model_runtime: &ModelRuntime,
    target: &AuthTarget,
) -> Result<String, AuthCommandError> {
    if let Some(cli_model) = target.model.as_deref() {
        let resolved = resolve_cli_model(ResolveCliModelOptions {
            cli_provider: target.provider.as_deref(),
            cli_model: Some(cli_model),
            cli_thinking: None,
            model_runtime,
        });
        if let Some(error) = resolved.error {
            return Err(AuthCommandError::new(error));
        }
        let Some(model) = resolved.model else {
            return Err(AuthCommandError::new(format!(
                "Unable to resolve model \"{cli_model}\""
            )));
        };
        return Ok(model.provider);
    }
    target
        .provider
        .clone()
        .ok_or_else(|| AuthCommandError::new("Unable to resolve an auth provider"))
}

/// Source `getProviderCredential`: direct stored access under `--no-refresh`,
/// otherwise the (floor-enforced) runtime resolution.
async fn get_provider_credential(
    provider_id: &str,
    model_runtime: &ModelRuntime,
    credentials: &dyn CredentialStore,
    refresh: bool,
) -> Result<Option<String>, ()> {
    let stored = credentials.read(provider_id).await.map_err(|_| ())?;
    if !refresh && let Some(Credential::Oauth(oauth)) = stored {
        return Ok(Some(oauth.access));
    }
    let auth = model_runtime
        .get_auth_with_options(provider_id, None, None, None)
        .await
        .map_err(|_| ())?;
    Ok(get_auth_credential(auth.as_ref()))
}

/// Errors from the print path: source `AuthCommandError` messages print
/// verbatim, everything else collapses to `Failed to resolve credential`.
#[derive(Debug)]
enum PrintCredentialError {
    /// Source validation/selection failure carrying its message.
    Command(AuthCommandError),
    /// Runtime/store/timeout failure printed generically.
    Runtime,
}

impl From<AuthCommandError> for PrintCredentialError {
    fn from(error: AuthCommandError) -> Self {
        Self::Command(error)
    }
}

/// Resolve the provider/model candidates a credential print scans: the
/// explicit `--provider` (plus `--model` when given), else every credentialed
/// provider resolving `--model`.
fn resolve_print_provider_candidates(
    target: &AuthTarget,
    model_runtime: &ModelRuntime,
    credential_types: &BTreeMap<String, CredentialKind>,
) -> Result<Vec<(String, Option<Model>)>, PrintCredentialError> {
    let mut providers: Vec<(String, Option<Model>)> = Vec::new();
    if let Some(cli_provider) = target.provider.as_deref() {
        if !has_provider(model_runtime, cli_provider) {
            return Err(AuthCommandError::new(format!(
                "Unknown provider \"{cli_provider}\". Use --list-models to see available providers."
            ))
            .into());
        }
        if let Some(cli_model) = target.model.as_deref() {
            let resolved = resolve_cli_model(ResolveCliModelOptions {
                cli_provider: Some(cli_provider),
                cli_model: Some(cli_model),
                cli_thinking: None,
                model_runtime,
            });
            if let Some(error) = resolved.error {
                return Err(AuthCommandError::new(error).into());
            }
            let Some(model) = resolved.model else {
                return Err(AuthCommandError::new(
                    "Unable to resolve the requested provider/model",
                )
                .into());
            };
            providers.push((cli_provider.to_owned(), Some(model)));
        } else {
            providers.push((cli_provider.to_owned(), None));
        }
    } else if let Some(cli_model) = target.model.as_deref() {
        for provider_id in model_runtime.get_provider_ids() {
            if !credential_types.contains_key(&provider_id) {
                continue;
            }
            let resolved = resolve_cli_model(ResolveCliModelOptions {
                cli_provider: Some(&provider_id),
                cli_model: Some(cli_model),
                cli_thinking: None,
                model_runtime,
            });
            let custom_fallback = resolved
                .warning
                .as_deref()
                .is_some_and(|warning| warning.contains("Using custom model id"));
            if resolved.error.is_none()
                && let Some(model) = resolved.model
                && !custom_fallback
            {
                providers.push((provider_id, Some(model)));
            }
        }
        if providers.is_empty() {
            return Err(AuthCommandError::new(format!(
                "Model \"{cli_model}\" not found. Use --list-models to see available models."
            ))
            .into());
        }
    }
    Ok(providers)
}

/// Describe why a credential print found no usable credential: a requested
/// provider holding the wrong credential kind, else the generic absence.
fn print_credential_not_found(
    target: &AuthTarget,
    kind: AuthCommandKind,
    providers: &[(String, Option<Model>)],
    credential_types: &BTreeMap<String, CredentialKind>,
) -> PrintCredentialError {
    let (provider_id, credential_kind) = providers.first().map_or((None, None), |(id, _)| {
        (Some(id.as_str()), credential_types.get(id).copied())
    });
    if target.provider.is_some()
        && kind == AuthCommandKind::ApiKey
        && credential_kind == Some(CredentialKind::Oauth)
    {
        return AuthCommandError::new(format!(
            "Provider \"{}\" is configured with OAuth, not an API key",
            provider_id.unwrap_or_default(),
        ))
        .into();
    }
    if target.provider.is_some()
        && kind == AuthCommandKind::BearerToken
        && credential_kind != Some(CredentialKind::Oauth)
    {
        return AuthCommandError::new(format!(
            "Provider \"{}\" is not configured with an OAuth bearer token",
            provider_id.unwrap_or_default(),
        ))
        .into();
    }
    AuthCommandError::new(format!(
        "No usable {} is configured",
        if kind == AuthCommandKind::ApiKey {
            "API key"
        } else {
            "OAuth bearer token"
        }
    ))
    .into()
}

/// Source `resolveCredentialForPrint`: resolve exactly one provider credential
/// of the requested kind. Refreshes OAuth tokens through the normal request
/// auth path; bearer tokens require the requested minimum remaining validity.
async fn resolve_credential_for_print(
    args: &Args,
    model_runtime: &ModelRuntime,
    kind: AuthCommandKind,
    min_expiry_ms: Option<i64>,
    signal: Option<CancellationToken>,
) -> Result<String, PrintCredentialError> {
    let target = validate_auth_command_args(args, kind)?;
    let credential_types: BTreeMap<String, CredentialKind> = model_runtime
        .list_credentials()
        .await
        .map_err(|_| PrintCredentialError::Runtime)?
        .into_iter()
        .map(|info| (info.provider_id, info.kind))
        .collect();

    let providers = resolve_print_provider_candidates(&target, model_runtime, &credential_types)?;

    let min_oauth_validity_ms = match kind {
        AuthCommandKind::BearerToken => {
            Some(min_expiry_ms.unwrap_or(DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS))
        }
        AuthCommandKind::Check | AuthCommandKind::ApiKey => None,
    };

    let mut matches: Vec<(String, String)> = Vec::new();
    for (provider_id, model) in &providers {
        let Some(credential_kind) = credential_types.get(provider_id) else {
            continue;
        };
        if kind == AuthCommandKind::ApiKey && *credential_kind == CredentialKind::Oauth {
            continue;
        }
        if kind == AuthCommandKind::BearerToken && *credential_kind != CredentialKind::Oauth {
            continue;
        }
        let auth = model_runtime
            .get_auth_with_options(
                provider_id,
                model.as_ref(),
                min_oauth_validity_ms,
                signal.clone(),
            )
            .await
            .map_err(|_| PrintCredentialError::Runtime)?;
        if let Some(value) = get_auth_credential(auth.as_ref()) {
            matches.push((provider_id.clone(), value));
        }
    }

    match matches.as_slice() {
        [(_provider_id, value)] => Ok(value.clone()),
        [] => Err(print_credential_not_found(
            &target,
            kind,
            &providers,
            &credential_types,
        )),
        matches => Err(AuthCommandError::new(format!(
            "Multiple configured providers matched ({}). Specify --provider.",
            matches
                .iter()
                .map(|(provider_id, _)| provider_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .into()),
    }
}

fn file_credentials_store(auth_path: &Path) -> Result<Arc<dyn CredentialStore>, ()> {
    let store = Arc::new(FileCredentialStore::new(auth_path).map_err(|_| ())?);
    Ok(store)
}

fn check_credentials_store(
    auth_path: &Path,
    no_refresh: bool,
) -> Result<Arc<dyn CredentialStore>, ()> {
    let store = file_credentials_store(auth_path)?;
    if no_refresh {
        return Ok(Arc::new(ReadOnlyCredentialStore::new(store)));
    }
    Ok(store)
}

/// Source `createAuthCheckModelRuntime`: injected credentials, in-memory
/// models store, no model network, no create-time refresh.
async fn create_check_model_runtime(
    credentials: Arc<dyn CredentialStore>,
) -> Result<ModelRuntime, ()> {
    ModelRuntime::create(CreateModelRuntimeOptions {
        credentials: Some(credentials),
        models_path: Some(get_agent_dir().join("models.json")),
        models_store: Some(Arc::new(InMemoryModelsStore::new())),
        allow_model_network: Some(false),
        ..CreateModelRuntimeOptions::default()
    })
    .await
    .map_err(|_| ())
}

/// Print-path runtime: default file stores, no model network
/// (source `ModelRuntime.create({ allowModelNetwork: false, signal })`).
async fn create_print_model_runtime() -> Result<ModelRuntime, ()> {
    let agent_dir = get_agent_dir();
    ModelRuntime::create(CreateModelRuntimeOptions {
        auth_path: Some(agent_dir.join("auth.json")),
        models_path: Some(agent_dir.join("models.json")),
        models_store_path: Some(agent_dir.join("models-store.json")),
        allow_model_network: Some(false),
        ..CreateModelRuntimeOptions::default()
    })
    .await
    .map_err(|_| ())
}

/// Failure classes of the shared `try` block in source `runAuthCommand`:
/// messages print verbatim, everything else prints the generic fallback.
enum AuthFlowError {
    /// Source `AuthCommandError` carrying its message.
    Command(AuthCommandError),
    /// Any other failure; the runner prints `Failed to resolve credential`.
    Runtime,
}

/// Source `runAuthCommand` body after parse: diagnostics gate, print path, and
/// the check path with its inner catch-all `invalid_state` mapping.
async fn execute_auth_command(
    command: &AuthCommand,
    parsed: &Args,
    io: &dyn BootstrapIo,
) -> Result<u8, AuthFlowError> {
    if !parsed.diagnostics.is_empty() {
        let joined = parsed
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.clone())
            .collect::<Vec<_>>()
            .join("\n");
        return Err(AuthFlowError::Command(AuthCommandError::new(joined)));
    }
    if command.kind != AuthCommandKind::Check {
        return run_print_command(command, parsed, io).await;
    }
    run_check_command(command, parsed, io).await
}

async fn run_print_command(
    command: &AuthCommand,
    parsed: &Args,
    io: &dyn BootstrapIo,
) -> Result<u8, AuthFlowError> {
    let outcome = tokio::time::timeout(PRINT_TIMEOUT, async {
        let model_runtime = create_print_model_runtime()
            .await
            .map_err(|()| PrintCredentialError::Runtime)?;
        resolve_credential_for_print(
            parsed,
            &model_runtime,
            command.kind,
            command.min_expiry_ms,
            Some(CancellationToken::new()),
        )
        .await
    })
    .await;
    let credential = match outcome {
        Ok(Ok(credential)) => credential,
        Ok(Err(PrintCredentialError::Command(error))) => {
            return Err(AuthFlowError::Command(error));
        }
        Ok(Err(PrintCredentialError::Runtime)) | Err(_) => {
            return Err(AuthFlowError::Runtime);
        }
    };
    io.write_stdout(&credential);
    Ok(0)
}

async fn run_check_command(
    command: &AuthCommand,
    parsed: &Args,
    io: &dyn BootstrapIo,
) -> Result<u8, AuthFlowError> {
    let target = validate_auth_command_args(parsed, AuthCommandKind::Check)
        .map_err(AuthFlowError::Command)?;
    let refresh = !command.no_refresh;
    let auth_path = get_agent_dir().join("auth.json");

    // Source inner try: any failure maps onto `invalid_state` with the
    // requested provider (or model) as the fallback provider label.
    let inner = async {
        let credentials = check_credentials_store(&auth_path, command.no_refresh)?;
        let model_runtime = create_check_model_runtime(Arc::clone(&credentials)).await?;
        let mut result = check_provider_auth(parsed, &model_runtime, refresh)
            .await
            .map_err(|_| ())?;
        let mut credential = None;
        if command.credentials && result.status == AuthCheckStatus::Ready {
            credential = get_provider_credential(
                &result.provider,
                &model_runtime,
                credentials.as_ref(),
                refresh,
            )
            .await?;
            if credential.is_none() {
                result = AuthCheckResult::not_ready(
                    result.provider.clone(),
                    AuthCheckReason::CredentialNotAvailable,
                );
            }
        }
        Ok::<_, ()>((result, credential))
    }
    .await;

    let (result, credential) = inner.unwrap_or_else(|()| {
        let fallback = target
            .provider
            .clone()
            .or_else(|| target.model.clone())
            .unwrap_or_default();
        (AuthCheckResult::invalid(fallback), None)
    });

    let output = if command.json {
        auth_check_json(&result, credential.as_deref()).to_string()
    } else {
        credential
            .clone()
            .unwrap_or_else(|| result.status.as_str().to_owned())
    };
    io.write_stdout(&output);
    Ok(match result.status {
        AuthCheckStatus::Ready => 0,
        AuthCheckStatus::NotReady => 1,
        AuthCheckStatus::Invalid => 2,
    })
}

/// Source `runAuthCommand`: handles every `auth` invocation. Returns the
/// process exit code (source `process.exitCode`).
pub async fn run_auth_command(args: &[String], io: &dyn BootstrapIo) -> u8 {
    if is_auth_command_help(args) {
        io.write_stdout(&auth_command_help_text());
        return 0;
    }
    let command = match parse_auth_command(args) {
        Ok(command) => command,
        Err(error) => {
            io.write_stderr(&format!("Error: {}", error.message));
            return 1;
        }
    };
    let parsed = parse_args(&command.args);
    if let Some((option, _)) = parsed.unknown_flags.iter().next() {
        io.write_stderr(&format!(
            "Error: Unknown option --{option} for \"{}\".",
            command.kind.command_name()
        ));
        io.write_stderr(&format!(
            "Use \"{APP_NAME} --help\" or \"{}\".",
            command.kind.usage()
        ));
        return 1;
    }
    match execute_auth_command(&command, &parsed, io).await {
        Ok(code) => code,
        Err(AuthFlowError::Command(error)) => {
            io.write_stderr(&format!("Error: {}", error.message));
            if command.kind == AuthCommandKind::Check {
                2
            } else {
                1
            }
        }
        Err(AuthFlowError::Runtime) => {
            io.write_stderr("Error: Failed to resolve credential");
            if command.kind == AuthCommandKind::Check {
                2
            } else {
                1
            }
        }
    }
}

/// Entry-point outcome for an early `pi auth …` dispatch.
pub enum AuthDispatch {
    /// The auth command ran; the process exits with this code.
    Handled(u8),
    /// Not an auth invocation; continue with these args.
    Continue(Vec<String>),
}

/// Run `pi auth …` before the bootstrap pipeline and report how `run`
/// continues. Non-auth invocations pass through untouched.
#[must_use]
pub fn dispatch_auth_args(args: &[String], io: &dyn BootstrapIo) -> AuthDispatch {
    if !is_auth_invocation(args) {
        return AuthDispatch::Continue(args.to_vec());
    }
    let code = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_or(1, |runtime| runtime.block_on(run_auth_command(args, io)));
    AuthDispatch::Handled(code)
}
