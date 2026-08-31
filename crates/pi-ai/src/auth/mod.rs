//! Credential storage, resolution, and interactive authentication contracts.

mod builtin;
pub mod config_value;
pub mod context;
pub mod credential_store;
pub mod env_keys;
pub mod error;
pub mod file_store;
pub mod http;
pub mod oauth;
pub mod resolve;
pub mod runtime_credentials;
pub mod types;

pub use builtin::{
    BuiltinOAuth, api_key_env_vars, builtin_oauth_provider, builtin_oauth_providers,
    default_provider_auth,
};
pub use context::{DefaultAuthContext, OverlayEnvAuthContext, overlay_env_auth_context};
pub use credential_store::InMemoryCredentialStore;
pub use env_keys::{
    AMBIENT_AUTH_MARKER, env_api_key_auth, find_env_keys, get_env_api_key, is_ambient_auth_marker,
};
pub use error::{AuthError, ModelsError, ModelsErrorCode, StoreError};
pub use file_store::{FileCredentialStore, FileLockBackend, read_stored_credential};
pub use resolve::{AuthResolutionOverrides, resolve_provider_auth};
pub use runtime_credentials::RuntimeCredentials;
pub use types::{
    ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthEvent, AuthInteraction, AuthPrompt,
    AuthResult, AuthType, Credential, CredentialInfo, CredentialKind, CredentialStore, ModelAuth,
    OAuthAuth, OAuthCredential, ProviderAuth, ProviderEnv, ProviderHeaders,
};
