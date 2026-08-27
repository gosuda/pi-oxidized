//! Shared OAuth primitives used by provider-specific login flows.

pub mod anthropic;
pub mod callback_server;
pub mod device_code;
pub mod github_copilot;
pub mod kimi_coding;
pub mod openai_codex;
pub mod openrouter;
pub mod page;
pub mod pkce;
pub mod radius;
pub mod xai;

pub use callback_server::{
    CallbackServerError, DEFAULT_CALLBACK_HOST, OAUTH_CALLBACK_HOST_ENV, OAuthCallbackCode,
    OAuthCallbackConfig, OAuthCallbackServer, callback_host_from_env,
    default_callback_host_is_loopback, parse_callback_host, race_callback_and_manual,
};
pub use device_code::{
    DEFAULT_POLL_INTERVAL_SECONDS, DeviceCodeClock, DeviceCodeSleeper, MINIMUM_INTERVAL_MS,
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, SLOW_DOWN_INTERVAL_INCREMENT_MS,
    SystemDeviceCodeClock, TokioDeviceCodeSleeper, abortable_sleep, poll_oauth_device_code_flow,
};
pub use page::{escape_html, oauth_error_html, oauth_success_html};
pub use pkce::{
    PkceCodes, PkceError, base64url_encode, generate_pkce, generate_state, s256_challenge,
};
