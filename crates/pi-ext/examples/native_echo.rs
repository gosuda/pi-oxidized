//! Mode 3 native extension executable: registers a single `native_echo`
//! tool that returns its `text` argument. Demonstrates the unstable
//! `pi_ext::server` contract end-to-end: handshake, snapshot, prepare,
//! validate, and execute (with cancellation and streaming updates).
//!
//! Build with `cargo build -p pi-ext --release --example native_echo`, then point the
//! host at a `pi-extension.json` like:
//!
//! ```json
//! {
//!   "$schema": "pi.extension.v1",
//!   "name": "@demo/native-echo",
//!   "version": "0.1.0",
//!   "runtime": "native",
//!   "protocolVersion": 1,
//!   "entry": {
//!     "<host-target-triple>": "./target/release/examples/native_echo"
//!   }
//! }
//! ```
//!
//! `runtime: native` opts into the JSONL endpoint the host spawns and binds
//! to stdin/stdout; `protocolVersion` must match the compiled
//! `PROTOCOL_VERSION`. Replace the placeholder target triple with the one
//! `cargo build --release --example native_echo` produces for your
//! platform.

use std::sync::Arc;

use futures::FutureExt;
use serde_json::{Value, json};
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

use pi_ai::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, DoneReason, TextContent,
};
use pi_ext::protocol::{ProviderSnapshotEntry, RegistrySnapshot, ToolSnapshotEntry};
use pi_ext::server::{
    ExtensionFault, NativeEventSink, NativeExtension, NativeExtensionContext, NativeFuture,
    ProviderEventSink, ProviderStreamCall, ToolCall, ToolUpdateSink, serve,
};

const TOOL_NAME: &str = "native_echo";
const PROVIDER_NAME: &str = "native_echo";
const MODEL_ID: &str = "native-echo";
const PROVIDER_BASE_URL: &str = "native://example";
const TICK: Duration = Duration::from_millis(10);

/// Strict JSON Schema the host advertises for `native_echo`.
fn parameters_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["text"],
        "properties": {
            "text": {
                "type": "string",
                "description": "Exact text the tool echoes back to the model.",
            },
        },
    })
}

fn snapshot() -> RegistrySnapshot {
    RegistrySnapshot {
        tools: vec![ToolSnapshotEntry {
            name: TOOL_NAME.to_owned(),
            label: TOOL_NAME.to_owned(),
            description: "Echoes the supplied `text` argument back to the model.".to_owned(),
            parameters: parameters_schema(),
            execution_mode: None,
        }],
        providers: vec![ProviderSnapshotEntry {
            name: PROVIDER_NAME.to_owned(),
            stream_simple: true,
            base_url: Some(PROVIDER_BASE_URL.to_owned()),
            api: Some(PROVIDER_NAME.to_owned()),
            display_name: Some("Native echo".to_owned()),
            models: Some(json!([{
                "id": MODEL_ID,
                "name": "Native Echo Model",
                "api": PROVIDER_NAME,
            }])),
            extension_path: Some("native://example".to_owned()),
            ..ProviderSnapshotEntry::default()
        }],
        handlers: vec!["session_start".to_owned()],
        ..RegistrySnapshot::default()
    }
}

/// Single source of truth for the tool-name and argument invariants.
fn validate_payload(name: &str, args: &Value) -> Result<String, ExtensionFault> {
    if name != TOOL_NAME {
        return Err(ExtensionFault::not_found(format!("Tool not found: {name}")));
    }
    let object = args.as_object().ok_or_else(|| {
        ExtensionFault::new("invalid_request", "tool arguments must be a JSON object")
    })?;
    if let Some(key) = object.keys().find(|key| key.as_str() != "text") {
        return Err(ExtensionFault::new(
            "invalid_request",
            format!("unknown field `{key}` is not permitted by the schema"),
        ));
    }
    object
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ExtensionFault::new("invalid_request", "missing string field `text`"))
}

/// Wait one tick or until cancellation arrives.
async fn tick_or_cancel(cancel: &CancellationToken) -> Result<(), ExtensionFault> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ExtensionFault::new("cancelled", "extension tool cancelled")),
        () = sleep(TICK) => Ok(()),
    }
}

fn provider_stream_events(
    call: &ProviderStreamCall,
) -> Result<Vec<AssistantMessageEvent>, ExtensionFault> {
    if call.provider_id != PROVIDER_NAME {
        return Err(ExtensionFault::not_found(format!(
            "Provider not found: {}",
            call.provider_id
        )));
    }
    let model = call
        .model
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(MODEL_ID);
    let mut message = AssistantMessage::new(PROVIDER_NAME, PROVIDER_NAME, model, 0);
    let mut stream = vec![
        AssistantMessageEvent::Start {
            partial: message.clone(),
        },
        AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: message.clone(),
        },
    ];
    let text = "native provider ready";
    message
        .content
        .push(AssistantContent::Text(TextContent::new(text)));
    stream.extend([
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: text.to_owned(),
            partial: message.clone(),
        },
        AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: text.to_owned(),
            partial: message.clone(),
        },
        AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message,
        },
    ]);
    Ok(stream)
}

struct NativeEcho;

impl NativeExtension for NativeEcho {
    fn snapshot(&self) -> RegistrySnapshot {
        snapshot()
    }

    fn prepare_tool(
        &self,
        _context: Arc<NativeExtensionContext>,
        name: String,
        args: Value,
    ) -> NativeFuture<Result<Value, ExtensionFault>> {
        async move {
            validate_payload(&name, &args)?;
            Ok(args)
        }
        .boxed()
    }

    fn validate_tool(
        &self,
        _context: Arc<NativeExtensionContext>,
        name: String,
        args: Value,
        _tool_call_id: Option<String>,
    ) -> NativeFuture<Result<Value, ExtensionFault>> {
        async move {
            validate_payload(&name, &args)?;
            Ok(args)
        }
        .boxed()
    }

    fn execute_tool(
        &self,
        _context: Arc<NativeExtensionContext>,
        call: ToolCall,
        updates: ToolUpdateSink,
        cancel: CancellationToken,
    ) -> NativeFuture<Result<Value, ExtensionFault>> {
        async move {
            let text = validate_payload(&call.name, &call.args)?;

            // Race a tiny sleep against cancellation on every iteration so
            // mid-flight cancel frames are observed by the server.
            for stage in ["started", "almost done"] {
                tick_or_cancel(&cancel).await?;
                // Product adapter forwards streamed `ToolUpdate` payloads
                // through `serde_json::from_value::<AgentToolResult>`; the
                // decoder rejects bare objects, so the update must already
                // be AgentToolResult-shaped (`content` array + `details`
                // object). Stage info lives in `details`; `content` carries
                // a short human-readable line so partial updates surface in
                // TUI rendering without waiting for the terminal result.
                let _ = updates.send(json!({
                    "content": [{ "type": "text", "text": format!("echo: {stage}") }],
                    "details": { "stage": stage },
                }));
            }

            // AgentToolResult-shaped JSON: `content` is text-only and
            // `details` carries structured echo metadata.
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "details": { "echoed": true, "length": text.chars().count() },
            }))
        }
        .boxed()
    }

    fn stream_provider(
        &self,
        _context: Arc<NativeExtensionContext>,
        call: ProviderStreamCall,
        events: ProviderEventSink,
        cancel: CancellationToken,
    ) -> NativeFuture<Result<Value, ExtensionFault>> {
        async move {
            for event in provider_stream_events(&call)? {
                if !events.send(event).await {
                    let message = if cancel.is_cancelled() {
                        "native provider cancelled"
                    } else {
                        "provider event channel closed"
                    };
                    return Err(ExtensionFault::extension_error(message));
                }
            }
            Ok(json!({}))
        }
        .boxed()
    }

    fn on_lifecycle(
        &self,
        _context: Arc<NativeExtensionContext>,
        event_type: String,
        _payload: Value,
        events: NativeEventSink,
    ) -> NativeFuture<Result<Value, ExtensionFault>> {
        async move {
            if event_type != "session_start" {
                return Err(ExtensionFault::not_found(format!(
                    "Lifecycle handler not found: {event_type}"
                )));
            }
            if !events
                .send(
                    "uiSlot",
                    json!({
                        "key": "native_echo.lifecycle",
                        "generation": 1,
                        "placement": "aboveEditor",
                        "height": 1,
                        "runs": [[{
                            "text": "native extension ready",
                            "style": {},
                        }]],
                    }),
                )
                .await
            {
                return Err(ExtensionFault::extension_error(
                    "native lifecycle event channel closed",
                ));
            }
            Ok(json!({}))
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_echo_provider_is_selectable_and_streams() -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = snapshot();
        let tool = snapshot
            .tools
            .iter()
            .find(|tool| tool.name == TOOL_NAME)
            .ok_or("native echo tool missing from snapshot")?;
        assert_eq!(tool.label, TOOL_NAME);
        let provider = snapshot
            .providers
            .iter()
            .find(|provider| provider.name == PROVIDER_NAME)
            .ok_or("native echo provider missing from snapshot")?;
        assert!(provider.stream_simple);
        assert_eq!(provider.api.as_deref(), Some(PROVIDER_NAME));
        assert_eq!(provider.base_url.as_deref(), Some(PROVIDER_BASE_URL));
        let models = provider
            .models
            .as_ref()
            .and_then(Value::as_array)
            .ok_or("native echo provider model catalog missing")?;
        assert_eq!(
            models.as_slice(),
            [json!({
                "id": MODEL_ID,
                "name": "Native Echo Model",
                "api": PROVIDER_NAME,
            })]
            .as_slice()
        );

        let call = ProviderStreamCall {
            provider_id: PROVIDER_NAME.to_owned(),
            model: json!({ "id": MODEL_ID }),
            context: json!({}),
            options: json!({}),
        };
        let events = provider_stream_events(&call)?;
        assert_eq!(events.len(), 5);
        let start = match &events[0] {
            AssistantMessageEvent::Start { partial } => partial,
            event => return Err(format!("expected provider start, got {event:?}").into()),
        };
        assert_eq!(start.api, PROVIDER_NAME);
        assert_eq!(start.provider, PROVIDER_NAME);
        assert_eq!(start.model, MODEL_ID);
        assert!(matches!(
            &events[2],
            AssistantMessageEvent::TextDelta { delta, .. } if delta == "native provider ready"
        ));
        let terminal = match &events[4] {
            AssistantMessageEvent::Done { reason, message }
                if matches!(reason, DoneReason::Stop) =>
            {
                message
            }
            event => return Err(format!("expected provider terminal event, got {event:?}").into()),
        };
        assert_eq!(terminal.api, PROVIDER_NAME);
        assert_eq!(terminal.provider, PROVIDER_NAME);
        assert_eq!(terminal.model, MODEL_ID);

        let unknown = ProviderStreamCall {
            provider_id: "other".to_owned(),
            ..call
        };
        let error =
            provider_stream_events(&unknown).expect_err("unknown provider must be rejected");
        assert_eq!(error.code, "not_found");
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(NativeEcho))?;
    Ok(())
}
