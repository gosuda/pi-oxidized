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

use futures::FutureExt;
use serde_json::{Value, json};
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;

use pi_ext::server::{
    ExtensionFault, NativeExtension, NativeFuture, RegistrySnapshot, ToolCall, ToolSnapshotEntry,
    ToolUpdateSink, serve,
};

const TOOL_NAME: &str = "native_echo";
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
            label: "native_echo".to_owned(),
            description: "Echoes the supplied `text` argument back to the model.".to_owned(),
            parameters: parameters_schema(),
            execution_mode: None,
        }],
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

struct NativeEcho;

impl NativeExtension for NativeEcho {
    fn snapshot(&self) -> RegistrySnapshot {
        snapshot()
    }

    fn prepare_tool(
        &self,
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

    fn execute_command(
        &self,
        command: String,
        _args: String,
    ) -> NativeFuture<Result<(), ExtensionFault>> {
        async move {
            // Commands are out of scope for this minimal example; unknown
            // slash names report `not_found`, matching `execute_tool`.
            Err(ExtensionFault::not_found(format!(
                "Command not found: {command}"
            )))
        }
        .boxed()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(NativeEcho))?;
    Ok(())
}
