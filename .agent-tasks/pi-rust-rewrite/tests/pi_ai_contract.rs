use pi_ai::{
    AssistantMessageEvent, Context, Message, Model, Provider, ProviderError, StreamOptions,
};
use serde_json::{Value, json};

#[test]
fn preserves_message_and_event_wire_shapes() -> Result<(), serde_json::Error> {
    let user = json!({
        "role": "user",
        "content": "hello",
        "timestamp": 1
    });
    let message: Message = serde_json::from_value(user.clone())?;
    assert_eq!(serde_json::to_value(message)?, user);

    let assistant = json!({
        "role": "assistant",
        "content": [],
        "api": "custom-api",
        "provider": "custom-provider",
        "model": "model-id",
        "usage": {
            "input": 1,
            "output": 2,
            "cacheRead": 0,
            "cacheWrite": 0,
            "totalTokens": 3,
            "cost": {
                "input": 0.1,
                "output": 0.2,
                "cacheRead": 0.0,
                "cacheWrite": 0.0,
                "total": 0.3
            }
        },
        "stopReason": "toolUse",
        "timestamp": 2
    });
    let message: Message = serde_json::from_value(assistant.clone())?;
    assert_eq!(serde_json::to_value(message)?, assistant);

    let tool_result = json!({
        "role": "toolResult",
        "toolCallId": "call-1",
        "toolName": "read",
        "content": [],
        "isError": false,
        "timestamp": 3
    });
    let message: Message = serde_json::from_value(tool_result.clone())?;
    assert_eq!(serde_json::to_value(message)?, tool_result);

    let event = json!({
        "type": "toolcall_delta",
        "contentIndex": 0,
        "delta": "{\"path\"",
        "partial": assistant.clone()
    });
    let parsed: AssistantMessageEvent = serde_json::from_value(event.clone())?;
    assert_eq!(serde_json::to_value(parsed)?, event);
    Ok(())
}

#[test]
fn preserves_model_nulls_and_unknown_fields() -> Result<(), serde_json::Error> {
    let model = json!({
        "id": "future-model",
        "name": "Future Model",
        "api": "custom-api",
        "provider": "custom-provider",
        "baseUrl": "https://example.test",
        "reasoning": true,
        "thinkingLevelMap": { "off": null, "high": "high" },
        "input": ["text", "image"],
        "cost": {
            "input": 0.0,
            "output": 0.0,
            "cacheRead": 0.0,
            "cacheWrite": 0.0
        },
        "contextWindow": 1000,
        "maxTokens": 100,
        "compat": { "allow_fallbacks": false },
        "futureField": { "nested": [1, null, true] }
    });
    let parsed: Model = serde_json::from_value(model.clone())?;
    assert_eq!(serde_json::to_value(parsed)?, model);
    Ok(())
}

#[allow(dead_code)]
fn accepts_provider_contract(
    provider: &dyn Provider,
    model: &Model,
    context: Context,
    error: &ProviderError,
) {
    let _stream = provider.stream(model, context, StreamOptions::default());
    let _message = error.to_string();
    let _opaque: Value = Value::Null;
}
