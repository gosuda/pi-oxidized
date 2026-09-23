//! Loopback integration for the native image-generation subsystem.
//!
//! Every test drives the real `openrouter-images` adapter through real
//! reqwest HTTP against the shared axum loopback harness; no mock stands in
//! for the adapter under test. Live paid generation stays out of scope, so
//! success, error, retry, cancel, timeout, and auth-merge transitions are
//! proven against queued loopback responses.

#![expect(
    clippy::expect_used,
    reason = "integration tests use contextual failure messages"
)]
#[path = "support/mod.rs"]
mod support;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use axum::http::{HeaderValue, StatusCode};
use futures::future::BoxFuture;
use pi_ai::auth::{ProviderEnv, default_provider_auth};
use pi_ai::images::{
    CreateImagesProviderOptions, ImagesApiDispatch, ImagesContext, ImagesInputContent, ImagesModel,
    ImagesOptions, ImagesOutput, ImagesOutputContent, ImagesStopReason, OPENROUTER_IMAGES_API,
    create_images_models, openrouter_images_api, openrouter_images_provider,
};
use pi_ai::types::{ImageContent, ModelCost, ModelInput};
use serde_json::{Value, json};
use support::http::{LocalHttpServer, ResponseChunk, ResponseSpec};
use tokio_util::sync::CancellationToken;

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

fn image_model(base_url: impl Into<String>, output: Vec<ImagesOutput>) -> ImagesModel {
    let mut model = ImagesModel::new(
        "test-image",
        "Test Image",
        OPENROUTER_IMAGES_API,
        "openrouter",
        base_url,
        vec![ModelInput::Text, ModelInput::Image],
        output,
    );
    model.cost = ModelCost {
        input: 2.0,
        output: 8.0,
        cache_read: 0.2,
        cache_write: 0.4,
        tiers: None,
    };
    model
}

fn prompt_context() -> ImagesContext {
    ImagesContext::new(vec![
        ImagesInputContent::from("draw a cat"),
        ImagesInputContent::from(ImageContent::new("QQ==", "image/png")),
    ])
}

fn completion_body() -> Value {
    json!({
        "id": "gen-abc",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "here is your image",
                "images": [
                    {"image_url": "data:image/png;base64,AAE="},
                    {"image_url": {"url": "data:image/jpeg;base64,ABM="}},
                    {"image_url": "https://example.test/remote.png"}
                ]
            }
        }],
        "usage": {
            "prompt_tokens": 12,
            "completion_tokens": 34,
            "prompt_tokens_details": {"cached_tokens": 4}
        }
    })
}

fn ok_response(body: &Value) -> ResponseSpec {
    let mut spec = ResponseSpec::bytes(StatusCode::OK, body.to_string());
    spec.headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    spec
}

#[tokio::test]
async fn generation_posts_chat_completions_and_parses_text_images_usage() {
    let server = LocalHttpServer::start([ok_response(&completion_body())])
        .await
        .expect("loopback server starts");
    let model = image_model(server.base_url(), vec![ImagesOutput::Image]);

    let observed_response = Arc::new(std::sync::Mutex::new(None));
    let observed_for_callback = Arc::clone(&observed_response);
    let options = ImagesOptions {
        api_key: Some("sk-test".to_owned()),
        on_response: Some(Arc::new(move |response, model| {
            let observed = Arc::clone(&observed_for_callback);
            Box::pin(async move {
                assert_eq!(model.id, "test-image");
                *observed.lock().expect("callback lock") = Some(response.clone());
                Ok(())
            }) as BoxFuture<'_, Result<(), _>>
        })),
        ..ImagesOptions::default()
    };

    let result = openrouter_images_api()
        .generate_images(&model, prompt_context(), options)
        .await;

    assert_eq!(result.stop_reason, ImagesStopReason::Stop);
    assert_eq!(result.error_message, None);
    assert_eq!(result.response_id.as_deref(), Some("gen-abc"));
    assert_eq!(result.output.len(), 3);
    let ImagesOutputContent::Text(text) = &result.output[0] else {
        unreachable!("first block is text");
    };
    assert_eq!(text.text, "here is your image");
    let ImagesOutputContent::Image(first) = &result.output[1] else {
        unreachable!("second block is an image");
    };
    assert_eq!(
        (first.mime_type.as_str(), first.data.as_str()),
        ("image/png", "AAE=")
    );
    let ImagesOutputContent::Image(second) = &result.output[2] else {
        unreachable!("third block is an image");
    };
    assert_eq!(
        (second.mime_type.as_str(), second.data.as_str()),
        ("image/jpeg", "ABM=")
    );

    let usage = result.usage.expect("usage present");
    assert_eq!(
        (
            usage.input,
            usage.output,
            usage.cache_read,
            usage.cache_write,
            usage.total_tokens
        ),
        (8, 34, 4, 0, 46)
    );
    assert!((usage.cost.input - 8.0 * 2.0 / 1_000_000.0).abs() < 1e-12);
    assert!((usage.cost.output - 34.0 * 8.0 / 1_000_000.0).abs() < 1e-12);
    // 16e-6 input + 272e-6 output + 0.8e-6 cache-read.
    assert!((usage.cost.total - 0.000_288_8).abs() < 1e-9);

    let response = observed_response
        .lock()
        .expect("callback lock")
        .clone()
        .expect("on_response ran");
    assert_eq!(response.status, 200);
    assert_eq!(
        response.headers.get("content-type").map(String::as_str),
        Some("application/json")
    );

    server
        .wait_for_requests(1, CAPTURE_TIMEOUT)
        .await
        .expect("request captured");
    let requests = server.shutdown().await.expect("shutdown");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, axum::http::Method::POST);
    assert_eq!(request.path, "/chat/completions");
    let authorization = request
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .expect("bearer header");
    assert_eq!(authorization, "Bearer sk-test");

    let body: Value = serde_json::from_slice(&request.body).expect("json body");
    assert_eq!(body["model"], json!("test-image"));
    assert_eq!(body["stream"], json!(false));
    assert_eq!(body["modalities"], json!(["image"]));
    assert_eq!(body["messages"][0]["role"], json!("user"));
    assert_eq!(
        body["messages"][0]["content"][0],
        json!({"type": "text", "text": "draw a cat"})
    );
    assert_eq!(
        body["messages"][0]["content"][1]["image_url"]["url"],
        json!("data:image/png;base64,QQ==")
    );
}

#[tokio::test]
async fn text_capable_models_request_both_modalities() {
    let server = LocalHttpServer::start([ok_response(&completion_body())])
        .await
        .expect("loopback server starts");
    let model = image_model(
        server.base_url(),
        vec![ImagesOutput::Image, ImagesOutput::Text],
    );
    let options = ImagesOptions {
        api_key: Some("sk-test".to_owned()),
        ..ImagesOptions::default()
    };

    let result = openrouter_images_api()
        .generate_images(&model, prompt_context(), options)
        .await;
    assert_eq!(result.stop_reason, ImagesStopReason::Stop);

    let requests = server.shutdown().await.expect("shutdown");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let body: Value = serde_json::from_slice(&request.body).expect("json body");
    assert_eq!(body["modalities"], json!(["image", "text"]));
}

#[tokio::test]
async fn http_error_composes_status_and_body_message() {
    let error_body = json!({"error": {"message": "Invalid key", "code": 401}});
    let server = LocalHttpServer::start([ResponseSpec::bytes(
        StatusCode::UNAUTHORIZED,
        error_body.to_string(),
    )])
    .await
    .expect("loopback server starts");
    let model = image_model(server.base_url(), vec![ImagesOutput::Image]);
    let options = ImagesOptions {
        api_key: Some("sk-bad".to_owned()),
        ..ImagesOptions::default()
    };

    let result = openrouter_images_api()
        .generate_images(&model, prompt_context(), options)
        .await;

    assert_eq!(result.stop_reason, ImagesStopReason::Error);
    let message = result.error_message.as_deref().expect("error message");
    let (status, body) = message.split_once(": ").expect("status prefix");
    assert_eq!(status, "401");
    let body: Value = serde_json::from_str(body).expect("error body");
    assert_eq!(body, json!({"message": "Invalid key", "code": 401}));
    assert!(result.output.is_empty());
    assert_eq!(result.response_id, None);
    server.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn retryable_status_retries_then_succeeds() {
    let server = LocalHttpServer::start([
        {
            let mut spec = ResponseSpec::bytes(StatusCode::INTERNAL_SERVER_ERROR, "boom");
            spec.headers
                .insert("retry-after-ms", HeaderValue::from_static("0"));
            spec
        },
        ok_response(&completion_body()),
    ])
    .await
    .expect("loopback server starts");
    let model = image_model(server.base_url(), vec![ImagesOutput::Image]);
    let options = ImagesOptions {
        api_key: Some("sk-test".to_owned()),
        max_retries: Some(1),
        ..ImagesOptions::default()
    };

    let result = openrouter_images_api()
        .generate_images(&model, prompt_context(), options)
        .await;

    assert_eq!(result.stop_reason, ImagesStopReason::Stop);
    assert_eq!(result.response_id.as_deref(), Some("gen-abc"));
    let requests = server.shutdown().await.expect("shutdown");
    assert_eq!(requests.len(), 2, "one retry after the 500");
}

#[tokio::test]
async fn retry_delay_above_cap_fails_without_second_request() {
    let mut too_slow = ResponseSpec::bytes(StatusCode::TOO_MANY_REQUESTS, "slow down");
    too_slow
        .headers
        .insert("retry-after-ms", HeaderValue::from_static("120000"));

    let server = LocalHttpServer::start([too_slow])
        .await
        .expect("loopback server starts");
    let model = image_model(server.base_url(), vec![ImagesOutput::Image]);
    let options = ImagesOptions {
        api_key: Some("sk-test".to_owned()),
        max_retries: Some(3),
        max_retry_delay_ms: Some(60_000),
        ..ImagesOptions::default()
    };

    let result = openrouter_images_api()
        .generate_images(&model, prompt_context(), options)
        .await;

    assert_eq!(result.stop_reason, ImagesStopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Server requested 120s retry delay (max: 60s). 429 slow down")
    );
    let requests = server.shutdown().await.expect("shutdown");
    assert_eq!(requests.len(), 1, "cap fails before the retry fires");
}

#[tokio::test]
async fn non_retryable_status_fails_without_retry() {
    let server = LocalHttpServer::start([ResponseSpec::bytes(StatusCode::UNAUTHORIZED, "nope")])
        .await
        .expect("loopback server starts");
    let model = image_model(server.base_url(), vec![ImagesOutput::Image]);
    let options = ImagesOptions {
        api_key: Some("sk-bad".to_owned()),
        max_retries: Some(2),
        ..ImagesOptions::default()
    };

    let result = openrouter_images_api()
        .generate_images(&model, prompt_context(), options)
        .await;

    assert_eq!(result.stop_reason, ImagesStopReason::Error);
    // Non-JSON body: the raw text rides in the SDK message position.
    assert_eq!(result.error_message.as_deref(), Some("401 nope"));
    let requests = server.shutdown().await.expect("shutdown");
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn timeout_fails_with_request_timed_out() {
    let mut slow = ResponseSpec::new(StatusCode::OK);
    slow.chunks = vec![ResponseChunk::delayed(
        completion_body().to_string().into_bytes(),
        Duration::from_millis(500),
    )];
    let server = LocalHttpServer::start([slow])
        .await
        .expect("loopback server starts");
    let model = image_model(server.base_url(), vec![ImagesOutput::Image]);
    let options = ImagesOptions {
        api_key: Some("sk-test".to_owned()),
        timeout_ms: Some(20),
        ..ImagesOptions::default()
    };

    let result = openrouter_images_api()
        .generate_images(&model, prompt_context(), options)
        .await;

    assert_eq!(result.stop_reason, ImagesStopReason::Error);
    assert_eq!(result.error_message.as_deref(), Some("Request timed out."));
    server.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn cancellation_before_send_reports_aborted() {
    let server = LocalHttpServer::start([ok_response(&completion_body())])
        .await
        .expect("loopback server starts");
    let model = image_model(server.base_url(), vec![ImagesOutput::Image]);
    let signal = CancellationToken::new();
    signal.cancel();
    let options = ImagesOptions {
        api_key: Some("sk-test".to_owned()),
        signal: Some(signal),
        ..ImagesOptions::default()
    };

    let result = openrouter_images_api()
        .generate_images(&model, prompt_context(), options)
        .await;

    assert_eq!(result.stop_reason, ImagesStopReason::Aborted);
    assert_eq!(result.error_message.as_deref(), Some("Request aborted"));
    let requests = server.shutdown().await.expect("shutdown");
    assert!(requests.is_empty(), "no request escapes a cancelled signal");
}

#[tokio::test]
async fn cancellation_mid_request_reports_aborted() {
    let mut slow = ResponseSpec::new(StatusCode::OK);
    slow.chunks = vec![ResponseChunk::delayed(
        completion_body().to_string().into_bytes(),
        Duration::from_millis(2_000),
    )];
    let server = LocalHttpServer::start([slow])
        .await
        .expect("loopback server starts");
    let model = image_model(server.base_url(), vec![ImagesOutput::Image]);
    let signal = CancellationToken::new();
    let canceller = {
        let signal = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            signal.cancel();
        })
    };
    let options = ImagesOptions {
        api_key: Some("sk-test".to_owned()),
        signal: Some(signal),
        ..ImagesOptions::default()
    };

    let result = openrouter_images_api()
        .generate_images(&model, prompt_context(), options)
        .await;
    canceller.await.expect("canceller task");

    assert_eq!(result.stop_reason, ImagesStopReason::Aborted);
    assert_eq!(result.error_message.as_deref(), Some("Request aborted"));
    assert!(result.output.is_empty());
    server.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn on_payload_mutates_the_wire_request() {
    let server = LocalHttpServer::start([ok_response(&completion_body())])
        .await
        .expect("loopback server starts");
    let model = image_model(server.base_url(), vec![ImagesOutput::Image]);
    let options = ImagesOptions {
        api_key: Some("sk-test".to_owned()),
        on_payload: Some(Arc::new(|payload, _model| {
            Box::pin(async move {
                payload["size"] = json!("1024x1024");
                Ok(())
            }) as BoxFuture<'_, Result<(), _>>
        })),
        ..ImagesOptions::default()
    };

    let result = openrouter_images_api()
        .generate_images(&model, prompt_context(), options)
        .await;
    assert_eq!(result.stop_reason, ImagesStopReason::Stop);

    let requests = server.shutdown().await.expect("shutdown");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let body: Value = serde_json::from_slice(&request.body).expect("json body");
    assert_eq!(
        body["size"],
        json!("1024x1024"),
        "mutation reaches the wire"
    );
}

#[tokio::test]
async fn collection_resolves_env_auth_and_merges_over_model_defaults() {
    let server = LocalHttpServer::start([ok_response(&completion_body())])
        .await
        .expect("loopback server starts");
    let model = image_model(server.base_url(), vec![ImagesOutput::Image]);
    let mut with_headers = model.clone();
    with_headers.headers = Some(BTreeMap::from([(
        "X-Title".to_owned(),
        "model-default".to_owned(),
    )]));

    let models = create_images_models(default_models_options());
    models.set_provider(openrouter_images_provider_with(
        vec![with_headers],
        openrouter_images_api(),
    ));

    let env: ProviderEnv =
        BTreeMap::from([("OPENROUTER_API_KEY".to_owned(), "sk-from-env".to_owned())]);
    let options = ImagesOptions {
        env: Some(env),
        headers: Some(BTreeMap::from([(
            "X-Request".to_owned(),
            Some("option".to_owned()),
        )])),
        ..ImagesOptions::default()
    };

    let result = models
        .generate_images(&model, prompt_context(), options)
        .await;
    assert_eq!(result.stop_reason, ImagesStopReason::Stop);

    let requests = server.shutdown().await.expect("shutdown");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    let authorization = request
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .expect("bearer header");
    assert_eq!(authorization, "Bearer sk-from-env");
    assert_eq!(
        request
            .headers
            .get("x-title")
            .and_then(|value| value.to_str().ok()),
        Some("model-default")
    );
    assert_eq!(
        request
            .headers
            .get("x-request")
            .and_then(|value| value.to_str().ok()),
        Some("option")
    );
}

#[tokio::test]
async fn unconfigured_provider_reports_missing_key_through_result() {
    let server = LocalHttpServer::start([])
        .await
        .expect("loopback server starts");
    let model = image_model(server.base_url(), vec![ImagesOutput::Image]);

    let models = create_images_models(default_models_options());
    models.set_provider(openrouter_images_provider_with(
        vec![model.clone()],
        openrouter_images_api(),
    ));

    let result = models
        .generate_images(&model, prompt_context(), ImagesOptions::default())
        .await;
    assert_eq!(result.stop_reason, ImagesStopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("No API key for provider: openrouter")
    );
    server.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn unknown_provider_returns_error_frame() {
    let server = LocalHttpServer::start([])
        .await
        .expect("loopback server starts");
    let mut model = image_model(server.base_url(), vec![ImagesOutput::Image]);
    model.provider = "nobody".to_owned();

    let models = create_images_models(default_models_options());
    let result = models
        .generate_images(&model, prompt_context(), ImagesOptions::default())
        .await;
    assert_eq!(result.stop_reason, ImagesStopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("Unknown provider: nobody")
    );
    assert_eq!(result.provider, "nobody");
    server.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn refresh_failure_keeps_last_known_models_in_collection() {
    let models = create_images_models(default_models_options());

    // The builtin provider is static; the collection refresh is a no-op that
    // resolves, and its model list stays at the static catalog state.
    models.set_provider(openrouter_images_provider());
    models
        .refresh(Some("openrouter"))
        .await
        .expect("static refresh");
    let expected = models.get_models(Some("openrouter")).len();
    assert!(expected > 0, "builtin provider lists the generated catalog");

    models.refresh(None).await.expect("all-provider refresh");
    assert_eq!(models.get_models(None).len(), expected);

    assert!(models.get_model("openrouter", "not-a-model").is_none());
}

fn default_models_options() -> pi_ai::images::ImagesModelsOptions {
    pi_ai::images::ImagesModelsOptions::default()
}

fn openrouter_images_provider_with(
    models: Vec<ImagesModel>,
    api: Arc<dyn ImagesApiDispatch>,
) -> Arc<dyn pi_ai::images::ImagesProvider> {
    pi_ai::images::create_images_provider(CreateImagesProviderOptions {
        id: "openrouter".to_owned(),
        name: Some("OpenRouter".to_owned()),
        auth: default_provider_auth("openrouter", None),
        models,
        refresh_models: None,
        api,
    })
}
