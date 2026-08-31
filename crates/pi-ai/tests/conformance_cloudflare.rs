//! Provider conformance for Cloudflare URL-template resolution.

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use axum::http::StatusCode;
use futures::{StreamExt, stream::BoxStream};
use pi_ai::{
    Provider, ProviderError, StreamOptions,
    providers::{AnthropicMessages, OpenAiCompletions, OpenAiResponses},
    types::{AssistantMessageEvent, Context, Model, ModelCost, ModelInput},
};
use reqwest::Client;
use support::http::{LocalHttpServer, ResponseSpec};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
const ACCOUNT: &str = "CLOUDFLARE_ACCOUNT_ID";
const GATEWAY: &str = "CLOUDFLARE_GATEWAY_ID";
const FULL_ENV: &[(&str, &str)] = &[(ACCOUNT, "account"), (GATEWAY, "gateway")];
const ACCOUNT_ENV: &[(&str, &str)] = &[(ACCOUNT, "account")];

#[tokio::test]
async fn resolves_templates_only_for_cloudflare_adapter_calls() -> Result<(), String> {
    let server = LocalHttpServer::start(
        std::iter::repeat_with(|| ResponseSpec::bytes(StatusCode::BAD_REQUEST, "{}")).take(7),
    )
    .await
    .map_err(|error| error.to_string())?;
    let base = server.base_url();
    let client = Client::new();

    drain(AnthropicMessages::new(client.clone()).stream(
        &model(
            "cloudflare-ai-gateway",
            "anthropic-messages",
            &format!("{base}/v1/{{{ACCOUNT}}}/{{{GATEWAY}}}/anthropic"),
        ),
        Context::default(),
        options(Some(FULL_ENV)),
    ))
    .await;
    drain(OpenAiCompletions::new(client.clone()).stream(
        &model(
            "cloudflare-ai-gateway",
            "openai-completions",
            &format!("{base}/v1/{{{ACCOUNT}}}/{{{GATEWAY}}}/compat"),
        ),
        Context::default(),
        options(Some(FULL_ENV)),
    ))
    .await;
    drain(OpenAiResponses::new(client.clone()).stream(
        &model(
            "cloudflare-ai-gateway",
            "openai-responses",
            &format!("{base}/v1/{{{ACCOUNT}}}/{{{GATEWAY}}}/openai"),
        ),
        Context::default(),
        options(Some(FULL_ENV)),
    ))
    .await;
    drain(OpenAiCompletions::new(client.clone()).stream(
        &model(
            "cloudflare-workers-ai",
            "openai-completions",
            &format!("{base}/client/v4/accounts/{{{ACCOUNT}}}/ai/v1"),
        ),
        Context::default(),
        options(Some(FULL_ENV)),
    ))
    .await;
    drain(OpenAiCompletions::new(client.clone()).stream(
        &model(
            "custom-provider",
            "openai-completions",
            &format!("{base}/v1/{{{ACCOUNT}}}/{{{GATEWAY}}}/custom"),
        ),
        Context::default(),
        options(Some(FULL_ENV)),
    ))
    .await;
    drain(OpenAiCompletions::new(client.clone()).stream(
        &model(
            "cloudflare-ai-gateway",
            "openai-completions",
            &format!("{base}/v1/{{{ACCOUNT}}}/{{{GATEWAY}}}/missing"),
        ),
        Context::default(),
        options(None),
    ))
    .await;
    drain(OpenAiCompletions::new(client).stream(
        &model(
            "cloudflare-ai-gateway",
            "openai-completions",
            &format!("{base}/v1/{{{ACCOUNT}}}/{{{GATEWAY}}}/partial"),
        ),
        Context::default(),
        options(Some(ACCOUNT_ENV)),
    ))
    .await;

    let paths = capture_paths(server, 7).await?;
    assert_eq!(
        paths,
        [
            "/v1/account/gateway/anthropic/v1/messages",
            "/v1/account/gateway/compat/chat/completions",
            "/v1/account/gateway/openai/responses",
            "/client/v4/accounts/account/ai/v1/chat/completions",
            "/v1/%7BCLOUDFLARE_ACCOUNT_ID%7D/%7BCLOUDFLARE_GATEWAY_ID%7D/custom/chat/completions",
            "/v1/%7BCLOUDFLARE_ACCOUNT_ID%7D/%7BCLOUDFLARE_GATEWAY_ID%7D/missing/chat/completions",
            "/v1/account/%7BCLOUDFLARE_GATEWAY_ID%7D/partial/chat/completions",
        ]
    );
    Ok(())
}

async fn capture_paths(server: LocalHttpServer, expected: usize) -> Result<Vec<String>, String> {
    server
        .wait_for_requests(expected, CAPTURE_TIMEOUT)
        .await
        .map_err(|error| error.to_string())?;
    Ok(server
        .shutdown()
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|request| request.path)
        .collect())
}

async fn drain(stream: BoxStream<'static, Result<AssistantMessageEvent, ProviderError>>) {
    let _events = stream.collect::<Vec<_>>().await;
}

fn options(env: Option<&[(&str, &str)]>) -> StreamOptions {
    StreamOptions {
        api_key: Some("test-key".to_owned()),
        env: env.map(|entries| {
            entries
                .iter()
                .map(|&(key, value)| (key.to_owned(), value.to_owned()))
                .collect::<BTreeMap<_, _>>()
        }),
        ..StreamOptions::default()
    }
}

fn model(provider: &str, api: &str, base_url: &str) -> Model {
    Model {
        id: "test-model".to_owned(),
        name: "Test model".to_owned(),
        api: api.to_owned(),
        provider: provider.to_owned(),
        base_url: base_url.to_owned(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 32_000,
        max_tokens: 4_096,
        headers: None,
        compat: None,
        extra: BTreeMap::new(),
    }
}
