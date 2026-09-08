//! Native Radius relay authentication resolution.
//!
//! The relay deliberately reuses the product [`ModelRuntime`] credential
//! resolver.  This keeps OAuth refresh and the on-disk `auth.json` store in
//! one place and avoids a second token format or a secret-bearing log path.

use thiserror::Error;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::cli::experimental::command_options::AuthInput;
use crate::core::config::{get_auth_path, resolve_path};
use crate::core::model_runtime::{
    CreateModelRuntimeOptions, ModelRuntime, ModelRuntimeAuthOverrides,
};
use pi_ai::auth::AuthResult;
use pi_ai::auth::oauth::radius::{DEFAULT_RADIUS_GATEWAY, normalize_radius_gateway_url};

/// Environment variable selecting the Radius gateway used by relay sessions.
pub const ENV_RADIUS_GATEWAY: &str = "PI_RADIUS_GATEWAY";

/// Credentials used by one authenticated Radius relay WebSocket connection.
#[derive(Clone, Eq, PartialEq)]
pub struct RadiusRelayAuth {
    /// HTTP(S) gateway origin.  The relay converts it to WS(S) for transport.
    pub gateway: String,
    /// Access token.  It is never included in [`Debug`] output.
    pub token: String,
}

impl std::fmt::Debug for RadiusRelayAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RadiusRelayAuth")
            .field("gateway", &self.gateway)
            .field("token", &"[redacted]")
            .finish()
    }
}

/// Options for one credential resolution attempt.
#[derive(Clone, Debug, Default)]
pub struct RadiusRelayAuthResolveOptions {
    /// Whether missing credentials are terminal for this attempt.
    pub required: bool,
    /// Cancellation shared with the owning relay lifecycle.
    pub signal: Option<CancellationToken>,
}

/// Failures while resolving explicit or stored Radius credentials.
#[derive(Debug, Error)]
pub enum RadiusRelayAuthError {
    /// Relay use is disabled by offline mode.
    #[error("Radius relay connections are unavailable in offline mode")]
    Offline,
    /// An explicit token file could not be read.
    #[error("Could not read Radius authentication token file {path}: {message}")]
    TokenFile {
        /// Resolved token-file path (never token contents).
        path: String,
        /// Bounded operating-system error text.
        message: String,
    },
    /// Explicit input or stored auth resolved to an empty token.
    #[error("Radius authentication token must not be empty")]
    EmptyToken,
    /// The caller required a stored credential but none was available.
    #[error("Radius authentication is required; start Pi and run /login radius, then retry")]
    Required,
    /// The owning relay was cancelled while auth was resolving.
    #[error("Radius authentication resolution cancelled")]
    Cancelled,
    /// Native model/auth storage could not be initialized or queried.
    #[error("Radius authentication resolution failed: {message}")]
    Runtime {
        /// Diagnostic from the native auth runtime; it never contains token data.
        message: String,
    },
}

/// Resolve explicit or stored Radius credentials anew for each relay attempt.
///
/// The model runtime is initialized lazily and retained after the first
/// successful initialization.  Its file store is pinned to the same
/// `auth.json` path used by the rest of the product, rather than the runtime's
/// in-memory convenience constructor.
pub struct RadiusRelayAuthResolver {
    input: Option<AuthInput>,
    gateway: String,
    model_runtime: OnceCell<ModelRuntime>,
}

impl std::fmt::Debug for RadiusRelayAuthResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RadiusRelayAuthResolver")
            .field("input", &self.input.as_ref().map(|_| "[redacted]"))
            .field("gateway", &self.gateway)
            .field("model_runtime_initialized", &self.model_runtime.get().is_some())
            .finish()
    }
}

impl RadiusRelayAuthResolver {
    /// Creates a resolver using `PI_RADIUS_GATEWAY` or the native Radius
    /// default.  The environment is sampled once, matching the source host.
    #[must_use]
    pub fn new(input: Option<AuthInput>) -> Self {
        let gateway = std::env::var(ENV_RADIUS_GATEWAY)
            .ok()
            .unwrap_or_else(|| DEFAULT_RADIUS_GATEWAY.to_owned());
        Self::with_gateway(input, gateway)
    }

    /// Creates a resolver with an explicit gateway (primarily useful to local
    /// loopback protocol tests and development hosts).
    #[must_use]
    pub fn with_gateway(input: Option<AuthInput>, gateway: impl AsRef<str>) -> Self {
        Self {
            input,
            gateway: normalize_radius_gateway_url(gateway.as_ref()),
            model_runtime: OnceCell::const_new(),
        }
    }

    /// Returns the normalized gateway origin.
    #[must_use]
    pub fn gateway(&self) -> &str {
        &self.gateway
    }

    /// Resolve credentials for one connection attempt.
    ///
    /// Explicit input wins over the native credential store.  Stored Radius
    /// OAuth credentials are resolved through `ModelRuntime`, including any
    /// required refresh, and only the resulting bearer material is returned.
    /// No credential value is logged or included in an error.
    pub async fn resolve(
        &self,
        options: RadiusRelayAuthResolveOptions,
    ) -> Result<Option<RadiusRelayAuth>, RadiusRelayAuthError> {
        check_cancelled(options.signal.as_ref())?;
        if std::env::var_os("PI_OFFLINE").is_some() {
            return if options.required {
                Err(RadiusRelayAuthError::Offline)
            } else {
                Ok(None)
            };
        }

        if let Some(token) = self.explicit_token(options.signal.as_ref()).await? {
            return Ok(Some(RadiusRelayAuth {
                gateway: self.gateway.clone(),
                token,
            }));
        }
        let runtime_future = self.model_runtime.get_or_try_init(|| async {
            ModelRuntime::create(CreateModelRuntimeOptions {
                auth_path: Some(get_auth_path()),
                allow_model_network: Some(false),
                ..CreateModelRuntimeOptions::default()
            })
            .await
        });

        let runtime = match wait_with_cancellation(runtime_future, options.signal.as_ref()).await? {
            Ok(runtime) => runtime,
            Err(error) => {
                return Err(RadiusRelayAuthError::Runtime {
                    message: error.to_string(),
                });
            }
        };
        let auth_future =
            runtime.get_auth_for_provider("radius", ModelRuntimeAuthOverrides::default());
        let resolved = match wait_with_cancellation(auth_future, options.signal.as_ref()).await? {
            Ok(resolved) => resolved,
            Err(error) => {
                return Err(RadiusRelayAuthError::Runtime {
                    message: error.to_string(),
                });
            }
        };

        let token = resolved.as_ref().and_then(extract_auth_token);
        if let Some(token) = token.filter(|value| !value.is_empty()) {
            return Ok(Some(RadiusRelayAuth {
                gateway: self.gateway.clone(),
                token,
            }));
        }

        if options.required {
            Err(RadiusRelayAuthError::Required)
        } else {
            Ok(None)
        }
    }

    async fn explicit_token(
        &self,
        signal: Option<&CancellationToken>,
    ) -> Result<Option<String>, RadiusRelayAuthError> {
        let Some(input) = self.input.as_ref() else {
            return Ok(None);
        };
        let value = match input {
            AuthInput::Token { token } => token.clone(),
            AuthInput::File { path } => {
                let resolved = resolve_path(path);
                let display_path = resolved.to_string_lossy().into_owned();
                let read = tokio::fs::read_to_string(&resolved);
                let contents = wait_with_cancellation(read, signal).await?;
                contents.map_err(|error| RadiusRelayAuthError::TokenFile {
                    path: display_path,
                    message: error.to_string(),
                })?
            }
        };
        check_cancelled(signal)?;
        let token = value.trim().to_owned();
        if token.is_empty() {
            return Err(RadiusRelayAuthError::EmptyToken);
        }
        Ok(Some(token))
    }
}

fn extract_auth_token(auth: &AuthResult) -> Option<String> {
    if let Some(api_key) = auth.auth.api_key.as_deref()
        && !api_key.is_empty()
    {
        return Some(api_key.to_owned());
    }
    let headers = auth.auth.headers.as_ref()?;
    let authorization = headers.iter().find_map(|(name, value)| {
        name.eq_ignore_ascii_case("authorization")
            .then_some(value.as_deref())
            .flatten()
    })?;
    let Some(separator) = authorization.find(char::is_whitespace) else {
        return None;
    };
    let (scheme, remainder) = authorization.split_at(separator);
    let token = remainder.trim_start();

    if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
        Some(token.to_owned())
    } else {
        None
    }
}

fn check_cancelled(signal: Option<&CancellationToken>) -> Result<(), RadiusRelayAuthError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        Err(RadiusRelayAuthError::Cancelled)
    } else {
        Ok(())
    }
}

async fn wait_with_cancellation<F, T>(
    future: F,
    signal: Option<&CancellationToken>,
) -> Result<T, RadiusRelayAuthError>
where
    F: Future<Output = T>,
{
    let Some(signal) = signal else {
        return Ok(future.await);
    };
    tokio::select! {
        biased;
        _ = signal.cancelled() => Err(RadiusRelayAuthError::Cancelled),
        value = future => Ok(value),
    }
}

 
