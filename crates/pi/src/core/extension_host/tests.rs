//! Fake-host acceptance tests for [`HostExtensionRunner`].
//!
//! Drives an in-memory JSONL fake host (no real Bun) through every
//! [`ExtensionRunner`] hook family, the full 35-event handler-presence set,
//! host-owned transforms/merge, non-retryable error/crash/timeout isolation
//! with pending-close / no-replay, registry first-wins dedup, reload
//! generation + stale-slot invalidation, the HTML renderer, and exactly-once
//! shutdown.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_agent::{
    AfterToolCallContext, AgentContext, AgentMessage, AgentToolResult, BeforeToolCallContext,
    CustomAgentMessage,
};
use pi_ai::{AssistantContent, AssistantMessage, AssistantMessageEvent, TextContent, ToolCall};
use pi_ext::client::HostClient;
#[cfg(unix)]
use pi_ext::host::{HostSource, HostSpec};
use pi_ext::protocol::{
    ExtensionErrorEvent, FlagValueWire, Frame, FrameKind, HelloAck, KeyEventKindWire,
    KeyModifiersWire, UiEventRequest, UiEventWire, decode_frame_str, encode_frame,
};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::super::agent_session::bridge_types::{BridgeRequestId, SessionCommand};
use super::super::agent_session::events::{
    AgentSessionEvent, SessionShutdownReason as ShutdownReason,
};
use super::super::agent_session::extension_runner::{ExtensionRunner, SessionHooks};
use super::super::extension_runtime_set::{EndpointId, EndpointKind, ExtensionRuntimeSet};
use super::super::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use super::{
    ALL_EVENT_TYPES, HostExtensionRunner, HostStartError, SessionBridgeEvent, ToolRenderPhase,
    compact_message_update_event, finish_direct_session_control_delivery, sanitize_html,
};

type BoxErr = Box<dyn Error>;
type R = Result<(), BoxErr>;

const FAST_TIMEOUT: Duration = Duration::from_millis(200);

/// Command injected into the fake host task.
enum FakeCmd {
    /// Emit an unsolicited event frame to the client.
    Emit(Frame),
    /// Close the host→client pipe (simulate crash / EOF).
    Close,
}

#[derive(Clone)]
struct FakeHost {
    cmd_tx: mpsc::Sender<FakeCmd>,
    responses: Arc<Mutex<HashMap<String, Value>>>,
    drop_methods: Arc<Mutex<HashSet<String>>>,
    requests: Arc<Mutex<Vec<Frame>>>,
}

impl FakeHost {
    fn set_response(&self, method: &str, payload: Value) {
        if let Ok(mut map) = self.responses.lock() {
            map.insert(method.to_owned(), payload);
        }
    }

    fn drop_method(&self, method: &str) {
        if let Ok(mut set) = self.drop_methods.lock() {
            set.insert(method.to_owned());
        }
    }

    async fn wait_for_request(&self, method: &str) -> R {
        tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                if self
                    .requests
                    .lock()
                    .is_ok_and(|requests| requests.iter().any(|request| request.method == method))
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| format!("fake host did not receive {method}"))?;
        Ok(())
    }

    async fn emit(&self, frame: Frame) {
        let _ = self.cmd_tx.send(FakeCmd::Emit(frame)).await;
    }

    async fn close(&self) {
        let _ = self.cmd_tx.send(FakeCmd::Close).await;
    }
}

/// Build a client + fake host pair and a ready runner loaded with `snapshot`.
async fn make_runner(snapshot: Value) -> Result<(Arc<HostExtensionRunner>, FakeHost), BoxErr> {
    make_runner_with_trust(snapshot, false).await
}

async fn make_runner_with_trust(
    snapshot: Value,
    project_trusted: bool,
) -> Result<(Arc<HostExtensionRunner>, FakeHost), BoxErr> {
    let (client_to_host, host_read) = tokio::io::duplex(64 * 1024);
    let (host_write, client_read) = tokio::io::duplex(64 * 1024);
    let (err_write, _err_read) = tokio::io::duplex(4096);
    let client = HostClient::connect_boxed(
        Box::new(client_to_host),
        Box::new(client_read),
        Box::new(err_write),
        None,
    );
    let client = Arc::new(client);

    let responses = Arc::new(Mutex::new(HashMap::new()));
    let drop_methods = Arc::new(Mutex::new(HashSet::new()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let resp_clone = Arc::clone(&responses);
    let drop_clone = Arc::clone(&drop_methods);
    let requests_clone = Arc::clone(&requests);
    tokio::spawn(fake_host_task(
        host_read,
        host_write,
        snapshot,
        resp_clone,
        drop_clone,
        requests_clone,
        cmd_rx,
    ));
    let runner = HostExtensionRunner::connect_with_cwd_and_trust(
        Arc::clone(&client),
        vec![],
        "/workspace",
        project_trusted,
        FAST_TIMEOUT,
    )
    .await?;
    Ok((
        runner,
        FakeHost {
            cmd_tx,
            responses,
            drop_methods,
            requests,
        },
    ))
}

async fn fake_host_task(
    read: tokio::io::DuplexStream,
    mut write: tokio::io::DuplexStream,
    snapshot: Value,
    responses: Arc<Mutex<HashMap<String, Value>>>,
    drop_methods: Arc<Mutex<HashSet<String>>>,
    requests: Arc<Mutex<Vec<Frame>>>,
    mut cmd_rx: mpsc::Receiver<FakeCmd>,
) {
    let mut reader = BufReader::new(read);
    let mut buf = String::new();
    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(FakeCmd::Emit(frame)) => {
                        let bytes = encode_frame(&frame).unwrap_or_default();
                        if !bytes.is_empty() {
                            let _ = write.write_all(&bytes).await;
                            let _ = write.flush().await;
                        }
                    }
                    Some(FakeCmd::Close) => {
                        let _ = write.shutdown().await;
                        return;
                    }
                    None => return,
                }
            }
            n = reader.read_line(&mut buf) => {
                match n {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if let Ok(req) = decode_frame_str(&buf) {
                            if let Ok(mut recorded) = requests.lock() {
                                recorded.push(req.clone());
                            }
                            if let Some(resp) = dispatch(&req, &snapshot, &responses, &drop_methods) {
                                let bytes = encode_frame(&resp).unwrap_or_default();
                                let _ = write.write_all(&bytes).await;
                                let _ = write.flush().await;
                            }
                        }
                        buf.clear();
                    }
                }
            }
        }
    }
}

fn dispatch(
    req: &Frame,
    snapshot: &Value,
    responses: &Mutex<HashMap<String, Value>>,
    drop_methods: &Mutex<HashSet<String>>,
) -> Option<Frame> {
    if drop_methods
        .lock()
        .is_ok_and(|set| set.contains(&req.method))
    {
        return None;
    }
    let payload = if req.method == "hello" {
        serde_json::to_value(HelloAck::local()).unwrap_or(Value::Null)
    } else if req.method == "extensions.load" {
        snapshot.clone()
    } else if let Some(payload) = responses
        .lock()
        .ok()
        .and_then(|map| map.get(&req.method).cloned())
    {
        payload
    } else if req.method == pi_ext::protocol::FLAGS_SET_METHOD {
        json!({"ok": true})
    } else {
        Value::Object(Map::new())
    };
    Some(Frame {
        id: req.id,
        kind: FrameKind::Res,
        method: req.method.clone(),
        payload,
    })
}

/// Snapshot carrying every registered surface + all 35 handler types.
fn full_snapshot() -> Value {
    let handlers: Vec<&str> = ALL_EVENT_TYPES.to_vec();
    json!({
        "tools": [
            {"name": "extTool", "label": "Ext", "description": "d", "parameters": {"type": "object"}},
            {"name": "extTool2", "label": "Ext2", "description": "d2", "parameters": {}}
        ],
        "commands": [{"name": "extCmd"}, {"name": "extCmd"}],
        "shortcuts": [{"key": "ctrl+e"}],
        "flags": [{"name": "extFlag", "type": "string", "default": "x"}],
        "renderers": [{"type": "message", "name": "r1"}],
        "providers": [{"name": "extProv"}],
        "handlers": handlers,
    })
}

fn assistant_text(text: &str) -> AgentMessage {
    AgentMessage::Custom(CustomAgentMessage::new(
        "customRole",
        Map::from_iter([("text".to_owned(), Value::String(text.to_owned()))]),
    ))
}

/// Receive one broadcast error within `dur`.
/// `broadcast::recv -> Result<T, RecvError>`; `timeout -> Result<inner, Elapsed>`.
async fn next_error(
    rx: &mut tokio::sync::broadcast::Receiver<ExtensionErrorEvent>,
    dur: Duration,
) -> Result<ExtensionErrorEvent, BoxErr> {
    Ok(tokio::time::timeout(dur, rx.recv()).await??)
}

/// Receive one broadcast item within `dur`.
async fn next_item<T: Clone + Send + 'static>(
    rx: &mut tokio::sync::broadcast::Receiver<T>,
    dur: Duration,
) -> Result<T, BoxErr> {
    Ok(tokio::time::timeout(dur, rx.recv()).await??)
}

// ===========================================================================
// Load + 35-event handler presence + trait hook families
// ===========================================================================

#[tokio::test]
async fn load_reports_all_35_handlers_and_registry_surfaces() -> R {
    let (runner, _host) = make_runner(full_snapshot()).await?;

    let expected_window: &[&str] = &[
        "agent_settled",
        "ui_prompt_start",
        "ui_prompt_end",
        "turn_start",
    ];
    let actual_window = ALL_EVENT_TYPES
        .windows(expected_window.len())
        .find(|w| w[0] == "agent_settled")
        .expect("agent_settled must be present in ALL_EVENT_TYPES");
    assert_eq!(
        actual_window, expected_window,
        "ui_prompt_start and ui_prompt_end must follow agent_settled and precede turn_start"
    );
    assert_eq!(
        ALL_EVENT_TYPES.len(),
        35,
        "expected 35 lifecycle event types, got {}",
        ALL_EVENT_TYPES.len()
    );

    for event in ALL_EVENT_TYPES {
        assert!(runner.has_handlers(event), "expected handler for {event}");
    }
    assert!(!runner.has_handlers("not_a_real_event"));

    let registry = runner.registry();
    assert_eq!(registry.tools().len(), 2);
    assert_eq!(registry.commands().len(), 1, "duplicate command deduped");
    assert_eq!(registry.shortcuts().len(), 1);
    assert_eq!(registry.flags().len(), 1);
    assert_eq!(registry.renderers().len(), 1);
    assert_eq!(registry.providers().len(), 1);

    assert!(runner.get_all_registered_tools().contains_key("extTool"));
    assert!(runner.providers().contains_key("extProv"));
    assert_eq!(
        runner.get_flag_values().get("extFlag"),
        Some(&Value::String("x".to_owned()))
    );
    Ok(())
}

/// ARC11: the generated cross-language witness manifest is bound to the Rust
/// lifecycle authority by ordered, exact equality. The generator parses
/// `ALL_EVENT_TYPES` from this crate's source, so this test proves the
/// committed artifact cannot drift from the authority — a reordered, added,
/// or dropped discriminant fails here by name and index.
#[test]
fn witness_manifest_matches_all_event_types() {
    const WITNESS_MANIFEST: &str = include_str!(
        "../../../../../packages/pi-tui-protocol/tests/fixtures/witness-manifest.json"
    );
    let manifest: serde_json::Value =
        serde_json::from_str(WITNESS_MANIFEST).expect("witness-manifest.json must parse");
    let declared = manifest["lifecycleDiscriminants"]
        .as_array()
        .expect("lifecycleDiscriminants array");
    let declared: Vec<&str> = declared
        .iter()
        .map(|name| name.as_str().expect("discriminant must be a string"))
        .collect();
    assert_eq!(
        declared, ALL_EVENT_TYPES,
        "witness manifest lifecycle discriminants drifted from ALL_EVENT_TYPES"
    );
}

#[tokio::test]
async fn native_boolean_flag_default_decodes_and_preserves_typed_fallback() -> R {
    let snapshot = json!({
        "flags": [
            {
                "name": "verbose",
                "type": "boolean",
                "default": false,
                "extensionPath": "native://demo"
            },
            {
                "name": "mode",
                "type": "string",
                "default": "quiet",
                "extensionPath": "native://demo"
            }
        ],
        "handlers": [],
    });
    let (runner, _host) = make_runner(snapshot).await?;

    assert_eq!(
        runner.get_flag_values().get("verbose"),
        Some(&Value::Bool(false))
    );
    assert_eq!(
        runner.get_flag_values().get("mode"),
        Some(&Value::String("quiet".to_owned()))
    );

    let registry = runner.registry();
    let flags = registry.flags();
    let verbose = flags
        .iter()
        .find(|flag| flag.name == "verbose")
        .ok_or("missing verbose flag registration")?;
    assert_eq!(verbose.kind, pi_ext::adapters::FlagKind::Boolean);
    assert_eq!(verbose.default.as_deref(), Some("false"));

    let mode = flags
        .iter()
        .find(|flag| flag.name == "mode")
        .ok_or("missing mode flag registration")?;
    assert_eq!(mode.kind, pi_ext::adapters::FlagKind::String);
    assert_eq!(mode.default.as_deref(), Some("quiet"));
    Ok(())
}

#[tokio::test]
async fn unset_boolean_flag_falls_back_to_typed_false() -> R {
    let snapshot = json!({
        "flags": [
            {
                "name": "verbose",
                "type": "boolean",
                "extensionPath": "native://demo"
            },
            {
                "name": "mode",
                "type": "string",
                "extensionPath": "native://demo"
            }
        ],
        "handlers": [],
    });
    let (runner, _host) = make_runner(snapshot).await?;

    // Boolean flag with no value and no default must be typed false,
    // not an empty string.
    assert_eq!(
        runner.get_flag_values().get("verbose"),
        Some(&Value::Bool(false))
    );
    // String flag with no value and no default stays an empty string.
    assert_eq!(
        runner.get_flag_values().get("mode"),
        Some(&Value::String(String::new()))
    );

    // The typed fallback survives the restart-and-rewire flag preservation
    // path: Value::Bool(false) converts to FlagValueWire::Boolean(false) and
    // back, remaining typed through apply_flag_values.
    let preserved = runner.get_flag_values();
    let overlay: BTreeMap<String, FlagValueWire> = preserved
        .iter()
        .map(|(name, value)| match value {
            Value::Bool(b) => Ok((name.clone(), FlagValueWire::Boolean(*b))),
            Value::String(s) => Ok((name.clone(), FlagValueWire::String(s.clone()))),
            other => Err(format!("unsupported flag value for {name}: {other}")),
        })
        .collect::<Result<_, _>>()?;
    runner.apply_flag_values(&overlay).await?;
    assert_eq!(
        runner.get_flag_values().get("verbose"),
        Some(&Value::Bool(false)),
        "boolean fallback must stay typed through apply_flag_values"
    );
    assert_eq!(
        runner.get_flag_values().get("mode"),
        Some(&Value::String(String::new())),
        "string fallback must stay typed through apply_flag_values"
    );
    Ok(())
}

#[tokio::test]
async fn native_flag_default_yields_to_host_resolved_value() -> R {
    let snapshot = json!({
        "flags": [{
            "name": "verbose",
            "type": "boolean",
            "default": false,
            "value": true,
            "extensionPath": "native://demo"
        }],
        "handlers": [],
    });
    let (runner, _host) = make_runner(snapshot).await?;
    assert_eq!(
        runner.get_flag_values().get("verbose"),
        Some(&Value::Bool(true))
    );
    Ok(())
}

#[tokio::test]
async fn hook_tool_call_maps_typed_block_result() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    host.set_response(
        "tool_call",
        json!({"block": true, "reason": "denied by policy"}),
    );
    let result = runner.emit_tool_call("read", "tc1", Map::new()).await?;
    let mapped = result.map(|r| (r.block, r.reason));
    assert_eq!(mapped, Some((true, Some("denied by policy".to_owned()))));
    Ok(())
}

#[tokio::test]
async fn hook_tool_result_trusts_host_owned_merge() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    host.set_response("tool_result", json!({"terminate": true, "isError": false}));
    let result = runner
        .emit_tool_result("read", "tc1", Map::new(), Vec::new(), Value::Null, false)
        .await?;
    let mapped = result.map(|r| (r.terminate, r.is_error));
    assert_eq!(mapped, Some((Some(true), Some(false))));
    Ok(())
}

#[tokio::test]
async fn hook_message_end_replaces_same_role_message() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    let replacement = assistant_text("rewritten");
    host.set_response("message_end", json!({ "message": replacement }));
    let result = runner.emit_message_end(assistant_text("original")).await?;
    let role = result.map(|m| m.role().to_owned());
    assert_eq!(role.as_deref(), Some("customRole"));
    Ok(())
}

#[tokio::test]
async fn hook_message_end_drops_role_mismatch_and_reports() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    let mut errors = runner.subscribe_errors();
    // Use a custom role (not an LLM role) so deserialization always succeeds as
    // Custom; the mismatch is pure role inequality against the original message.
    let mismatched = AgentMessage::Custom(CustomAgentMessage::new(
        "otherRole",
        Map::from_iter([("text".to_owned(), Value::String("x".to_owned()))]),
    ));
    host.set_response("message_end", json!({ "message": mismatched }));
    let result = runner.emit_message_end(assistant_text("original")).await?;
    assert!(result.is_none(), "mismatched role must be dropped");
    let err = next_error(&mut errors, Duration::from_millis(500)).await?;
    assert_eq!(err.code, "extension_message_end");
    Ok(())
}

#[tokio::test]
async fn hook_input_transform_handled() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    host.set_response("input", json!({"action": "handled"}));
    let result = runner.emit_input("hi", None, "user", None).await?;
    assert!(result.handled);
    Ok(())
}

#[tokio::test]
async fn hook_before_agent_start_injection() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    host.set_response(
        "before_agent_start",
        json!({"systemPrompt": "override", "messages": []}),
    );
    let result = runner.emit_before_agent_start("prompt", None).await?;
    let mapped = result.map(|r| (r.system_prompt, r.messages.len()));
    assert_eq!(mapped, Some((Some("override".to_owned()), 0)));
    Ok(())
}

#[tokio::test]
async fn hook_resources_discover_returns_paths() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    host.set_response(
        "resources_discover",
        json!({
            "skillPaths": [{"path": "skills", "extensionPath": "/ext/a.ts"}],
            "themePaths": [{"path": "theme.json", "extensionPath": "/ext/b.ts"}]
        }),
    );
    let paths = runner.emit_resources_discover("/cwd", "startup").await?;
    assert_eq!(
        paths,
        crate::core::resources::ResourceExtensionPaths {
            skill_paths: vec![crate::core::resources::ExtensionResourcePath::discovered(
                "skills".to_owned(),
                "/ext/a.ts",
            )],
            prompt_paths: Vec::new(),
            theme_paths: vec![crate::core::resources::ExtensionResourcePath::discovered(
                "theme.json".to_owned(),
                "/ext/b.ts",
            )],
        }
    );
    Ok(())
}

#[tokio::test]
async fn emit_generic_event_acks_and_parses_cancel() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    let result = runner.emit(AgentSessionEvent::AgentStart).await?;
    assert!(result.is_none_or(|r| !r.cancel));

    host.set_response("turn_end", json!({"cancel": true, "reason": "abort"}));
    let result = runner
        .emit(AgentSessionEvent::TurnEnd {
            message: assistant_text("m"),
            tool_results: Vec::new(),
        })
        .await?;
    let mapped = result.map(|c| (c.cancel, c.reason));
    assert_eq!(mapped, Some((true, Some("abort".to_owned()))));
    Ok(())
}

#[tokio::test]
async fn execute_command_dispatches_registered_and_rejects_unknown() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    host.set_response("command.execute", json!({"ok": true}));
    assert!(runner.execute_command("extCmd", "arg").await?);
    assert!(!runner.execute_command("nope", "").await?);
    Ok(())
}

#[tokio::test]
async fn get_registered_commands_lists_names() -> R {
    let (runner, _host) = make_runner(full_snapshot()).await?;
    assert_eq!(runner.get_registered_commands(), vec!["extCmd".to_owned()]);
    assert!(runner.has_command("extCmd"));
    assert!(!runner.has_command("missing"));
    Ok(())
}

// ===========================================================================
// Transforms / merge are host-owned (Rust trusts one validated response)
// ===========================================================================

#[tokio::test]
async fn host_owned_transform_chain_reflected_in_single_response() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    // The host already merged multiple input handlers; Rust sees only the
    // final transformed text and trusts it verbatim.
    host.set_response(
        "input",
        json!({"action": "transform", "text": "merged-text", "images": null}),
    );
    let result = runner
        .emit_input("raw", None, "user", Some("steer"))
        .await?;
    assert!(!result.handled);
    assert_eq!(result.text.as_deref(), Some("merged-text"));
    Ok(())
}

// ===========================================================================
// Error / crash / timeout → non-retryable mapping, pending close, no replay
// ===========================================================================

#[tokio::test]
async fn host_crash_isolates_as_non_retryable_and_closes_pending() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    let mut errors = runner.subscribe_errors();
    host.drop_method("tool_call");
    let runner_clone = Arc::clone(&runner);
    let call =
        tokio::spawn(async move { runner_clone.emit_tool_call("read", "tc1", Map::new()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    host.close().await;
    // In-flight hook resolves once with Ok(None) (isolation, no abort).
    let resolved = call.await??;
    assert!(resolved.is_none());

    let mut saw_non_retryable = false;
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(800), errors.recv()).await
    {
        assert!(!event.retryable, "extension errors must be non-retryable");
        assert!(
            event.code.starts_with("extension_"),
            "stable error code prefix: {}",
            event.code
        );
        saw_non_retryable = true;
    }
    assert!(
        saw_non_retryable,
        "expected at least one non-retryable error"
    );
    assert!(!runner.is_running(), "runner disabled after crash");
    Ok(())
}

#[tokio::test]
async fn host_timeout_isolates_as_non_retryable() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    let mut errors = runner.subscribe_errors();
    host.drop_method("tool_result");
    let result = runner
        .emit_tool_result("read", "tc1", Map::new(), Vec::new(), Value::Null, false)
        .await?;
    assert!(result.is_none(), "timeout must not abort the turn");
    let event = next_error(&mut errors, Duration::from_secs(2)).await?;
    assert!(!event.retryable);
    assert_eq!(event.code, "extension_timeout");
    Ok(())
}

#[tokio::test]
async fn after_failure_subsequent_hooks_short_circuit_without_replay() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    let mut errors = runner.subscribe_errors();
    host.close().await;
    // Drain crash error(s): loop while an event arrives inside the window.
    while tokio::time::timeout(Duration::from_millis(300), errors.recv())
        .await
        .is_ok_and(|inner| inner.is_ok())
    {}

    let start = std::time::Instant::now();
    let result = runner.emit_tool_call("read", "tc2", Map::new()).await?;
    assert!(
        start.elapsed() < FAST_TIMEOUT,
        "disabled hooks must short-circuit, not wait for a timeout"
    );
    assert!(result.is_none());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), errors.recv())
            .await
            .is_err(),
        "no new error / replay after disable"
    );
    Ok(())
}

// ===========================================================================
// Registry duplicates (first registration wins)
// ===========================================================================

#[tokio::test]
async fn registry_first_registration_wins_for_duplicates() -> R {
    let snapshot = json!({
        "tools": [
            {"name": "dup", "label": "first", "description": "a", "parameters": {}},
            {"name": "dup", "label": "second", "description": "b", "parameters": {}}
        ],
        "commands": [{"name": "cmd"}, {"name": "cmd"}],
        "providers": [{"name": "p"}, {"name": "p"}],
        "handlers": ["tool_call"],
    });
    let (runner, _host) = make_runner(snapshot).await?;
    let registry = runner.registry();
    assert_eq!(registry.tools().len(), 1);
    assert_eq!(registry.tools()[0].label, "first");
    assert_eq!(registry.commands().len(), 1);
    assert_eq!(registry.providers().len(), 1);
    assert_eq!(runner.get_all_registered_tools().len(), 1);
    Ok(())
}

/// M5 witness: command suffix disambiguation is observable in the Rust
/// registry. The TypeScript host's `resolveRegisteredCommands` assigns
/// unique invocation names (`cmd:1`, `cmd:2`) to duplicate command names
/// before serializing the snapshot. The Rust side stores these
/// already-disambiguated names via first-wins on the *invocation name*.
///
/// Mutation: drop the suffix (send `cmd` twice instead of `cmd:1`/`cmd:2`)
/// → first-wins deduplicates to one → `cmd:2` is absent → test fails.
#[tokio::test]
async fn command_suffix_disambiguation_observed() -> R {
    let snapshot = json!({
        "commands": [
            {"name": "cmd:1", "description": "first cmd", "source": "/ext/a.ts"},
            {"name": "cmd:2", "description": "second cmd", "source": "/ext/b.ts"}
        ],
        "handlers": ["tool_call"],
    });
    let (runner, _host) = make_runner(snapshot).await?;
    let registry = runner.registry();
    let commands = registry.commands();
    assert_eq!(
        commands.len(),
        2,
        "suffix-disambiguated commands must both survive first-wins dedup"
    );
    assert_eq!(commands[0].name, "cmd:1");
    assert_eq!(commands[0].description.as_deref(), Some("first cmd"));
    assert_eq!(commands[1].name, "cmd:2");
    assert_eq!(commands[1].description.as_deref(), Some("second cmd"));
    assert_eq!(
        runner.get_registered_commands(),
        vec!["cmd:1", "cmd:2"],
        "invocation names must be observable via get_registered_commands"
    );
    Ok(())
}

#[tokio::test]
async fn shortcut_order_and_extension_metadata_are_preserved() -> R {
    let snapshot = json!({
        "shortcuts": [
            {"key": "ctrl+x", "description": "first", "extensionPath": "/ext/a.ts"},
            {"key": "ctrl+x", "description": "second", "extensionPath": "/ext/b.ts"}
        ],
        "flags": [{
            "name": "mode",
            "type": "string",
            "extensionPath": "/ext/flags.ts"
        }]
    });
    let (runner, _host) = make_runner(snapshot).await?;
    let registry = runner.registry();
    assert_eq!(
        registry.shortcuts().len(),
        1,
        "legacy registry stays first-wins"
    );
    assert_eq!(
        registry.shortcuts()[0].extension_path.as_deref(),
        Some("/ext/a.ts")
    );
    assert_eq!(
        registry.flags()[0].extension_path.as_deref(),
        Some("/ext/flags.ts")
    );
    let raw = runner.raw_shortcuts();
    assert_eq!(raw.len(), 2, "product projection needs every registration");
    assert_eq!(raw[0].description.as_deref(), Some("first"));
    assert_eq!(raw[1].description.as_deref(), Some("second"));
    assert_eq!(raw[1].extension_path.as_deref(), Some("/ext/b.ts"));
    Ok(())
}

#[tokio::test]
async fn flags_set_acks_before_updating_local_values_and_rejection_preserves_state() -> R {
    let snapshot = json!({
        "flags": [{
            "name": "verbose",
            "type": "boolean",
            "value": false,
            "extensionPath": "/ext/a.ts"
        }]
    });
    let (runner, host) = make_runner(snapshot).await?;
    let values = BTreeMap::from([("verbose".to_owned(), FlagValueWire::Boolean(true))]);
    runner.apply_flag_values(&values).await?;
    let request = host
        .requests
        .lock()
        .map_err(|_| "request lock poisoned")?
        .iter()
        .find(|request| request.method == pi_ext::protocol::FLAGS_SET_METHOD)
        .cloned()
        .ok_or("missing flags.set request")?;
    assert_eq!(request.payload, json!({"values": {"verbose": true}}));
    assert_eq!(
        runner.get_flag_values().get("verbose"),
        Some(&Value::Bool(true))
    );

    host.set_response(pi_ext::protocol::FLAGS_SET_METHOD, json!({"ok": false}));
    let rejected = BTreeMap::from([("verbose".to_owned(), FlagValueWire::Boolean(false))]);
    assert!(runner.apply_flag_values(&rejected).await.is_err());
    assert_eq!(
        runner.get_flag_values().get("verbose"),
        Some(&Value::Bool(true)),
        "rejected values must not leak into local state"
    );
    Ok(())
}

#[tokio::test]
async fn flags_set_must_ack_before_startup_can_complete() -> R {
    let snapshot = json!({
        "flags": [{"name": "verbose", "type": "boolean", "value": false}]
    });
    let (runner, host) = make_runner(snapshot).await?;
    host.drop_method(pi_ext::protocol::FLAGS_SET_METHOD);
    let task_runner = Arc::clone(&runner);
    let apply = tokio::spawn(async move {
        task_runner
            .apply_flag_values(&BTreeMap::from([(
                "verbose".to_owned(),
                FlagValueWire::Boolean(true),
            )]))
            .await
    });
    host.wait_for_request(pi_ext::protocol::FLAGS_SET_METHOD)
        .await?;
    assert!(!apply.is_finished(), "startup must wait for the host ACK");
    assert_eq!(
        runner.get_flag_values().get("verbose"),
        Some(&Value::Bool(false)),
        "local state must remain unchanged while flags.set is pending"
    );
    let request = host
        .requests
        .lock()
        .map_err(|_| "request lock poisoned")?
        .iter()
        .find(|request| request.method == pi_ext::protocol::FLAGS_SET_METHOD)
        .cloned()
        .ok_or("missing pending flags.set")?;
    host.emit(Frame {
        id: request.id,
        kind: FrameKind::Res,
        method: request.method,
        payload: json!({"ok": true}),
    })
    .await;
    apply.await??;
    assert_eq!(
        runner.get_flag_values().get("verbose"),
        Some(&Value::Bool(true))
    );
    Ok(())
}

#[tokio::test]
async fn shortcut_and_ui_event_requests_use_typed_wire_payloads() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    host.set_response(
        pi_ext::protocol::SHORTCUT_EXECUTE_METHOD,
        json!({"handled": true}),
    );
    let shortcut = runner.execute_shortcut("ctrl+alt+p").await?;
    assert!(shortcut.handled);

    host.set_response("uiEvent", json!({"delivered": true}));
    let delivered = runner
        .send_ui_event(UiEventRequest {
            key: "overlay.1".to_owned(),
            generation: 7,
            event: UiEventWire::Key {
                code: "p".to_owned(),
                modifiers: KeyModifiersWire {
                    ctrl: Some(true),
                    ..KeyModifiersWire::default()
                },
                kind: KeyEventKindWire::Press,
            },
            data: Some("\u{10}".to_owned()),
        })
        .await?;
    assert!(delivered.delivered);

    let requests = host.requests.lock().map_err(|_| "request lock poisoned")?;
    let shortcut_request = requests
        .iter()
        .find(|request| request.method == pi_ext::protocol::SHORTCUT_EXECUTE_METHOD)
        .ok_or("missing shortcut.execute request")?;
    assert_eq!(shortcut_request.payload, json!({"key": "ctrl+alt+p"}));
    let ui_request = requests
        .iter()
        .find(|request| request.method == "uiEvent")
        .ok_or("missing uiEvent request")?;
    assert_eq!(
        ui_request.payload,
        json!({
            "key": "overlay.1",
            "generation": 7,
            "event": {
                "type": "key",
                "code": "p",
                "modifiers": {"ctrl": true},
                "kind": "press"
            },
            "data": "\u{10}"
        })
    );
    Ok(())
}

// ===========================================================================
// Reload generation + stale-slot invalidation
// ===========================================================================

fn ui_slot_frame(key: &str, generation: u64, text: &str) -> Frame {
    Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "uiSlot".to_owned(),
        payload: json!({
            "key": key,
            "generation": generation,
            "placement": "aboveEditor",
            "height": 1,
            "runs": [[{"text": text}]],
            "focusable": false,
        }),
    }
}

#[tokio::test]
async fn reload_bumps_generation_and_disposes_slots() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    assert_eq!(runner.reload_generation(), 1);

    let mut slot = runner.subscribe_slot("widget");
    host.emit(ui_slot_frame("widget", 1, "v1")).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(500), slot.changed())
            .await
            .is_ok(),
        "slot pushed"
    );
    assert!(slot.borrow().is_some(), "slot received content");
    let retained = runner.current_slots();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].key, "widget");

    let generation = runner.reload().await;
    assert_eq!(generation, 2);
    assert_eq!(runner.reload_generation(), 2);
    let _ = tokio::time::timeout(Duration::from_millis(500), slot.changed()).await;
    assert!(slot.borrow().is_none(), "slot disposed by reload");
    assert!(!runner.is_running(), "runner stopped after reload");
    Ok(())
}

#[tokio::test]
async fn stale_slot_generation_is_dropped() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    let mut slot = runner.subscribe_slot("w");
    host.emit(ui_slot_frame("w", 2, "newer")).await;
    let _ = tokio::time::timeout(Duration::from_millis(500), slot.changed()).await;
    // gen=1 (stale) must be discarded by the HostClient generation filter.
    host.emit(ui_slot_frame("w", 1, "older")).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let current = slot
        .borrow()
        .clone()
        .ok_or("slot should still be present")?;
    assert_eq!(current.generation, 2, "stale generation dropped");
    assert_eq!(current.lines.len(), 1);
    Ok(())
}

#[tokio::test]
async fn invalidate_drops_delayed_slot_and_notify_with_single_dispose() -> R {
    use crate::core::extension_host::ExtensionUiEvent;

    let (runner, host) = make_runner(full_snapshot()).await?;
    let mut ui = runner.subscribe_ui();

    // Barrier 1: the slot is fully inserted (public Slot event observed)
    // before teardown starts.
    host.emit(ui_slot_frame("w", 1, "live")).await;
    let first = tokio::time::timeout(Duration::from_millis(500), ui.recv()).await??;
    assert!(
        matches!(&first, ExtensionUiEvent::Slot(slot) if slot.key == "w"),
        "expected live slot first: {first:?}"
    );

    // Keep a burst of newer slot pushes AND notifies in flight while
    // invalidating, so the pump's Notify path races teardown for the slots
    // lock exactly like Slot does.
    for generation in 2..12u64 {
        host.emit(ui_slot_frame("w", generation, "delayed")).await;
        host.emit(Frame {
            id: 0,
            kind: FrameKind::Event,
            method: "notify".to_owned(),
            payload: json!({"message": format!("racing {generation}"), "type": "info"}),
        })
        .await;
    }
    runner.invalidate();
    host.emit(ui_slot_frame("w", 99, "post-teardown")).await;
    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "notify".to_owned(),
        payload: json!({"message": "late", "type": "info"}),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Barrier 2: drain the ORDERED public stream and verify the lock-enforced
    // contract: exactly one Dispose, and never a Slot/Notify after it.
    let mut disposals = 0;
    let mut late_events = Vec::new();
    while let Ok(event) = ui.try_recv() {
        match event {
            ExtensionUiEvent::Dispose { key } => {
                assert_eq!(key, "w");
                disposals += 1;
            }
            ExtensionUiEvent::Slot(slot) if disposals == 0 => {
                // Racing pre-teardown pushes may legally land before the
                // Dispose; they must still be for the live key.
                assert_eq!(slot.key, "w");
            }
            ExtensionUiEvent::Notify(notification) if disposals == 0 => {
                // Racing pre-teardown notifies are legal before the Dispose.
                assert!(notification.message.starts_with("racing"));
            }
            other => late_events.push(format!("{other:?}")),
        }
    }
    assert_eq!(disposals, 1, "disposal must be exactly once");
    assert!(
        late_events.is_empty(),
        "no slot/notify may follow the teardown dispose: {late_events:?}"
    );
    assert!(runner.slot_keys().is_empty(), "slot map must stay empty");
    runner.shutdown_once().await;
    Ok(())
}

// ===========================================================================
// HTML renderer (strips script/style, escapes markup)
// ===========================================================================

#[tokio::test]
async fn render_extension_tool_html_strips_active_content() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    host.set_response(
        "tool.renderHtml",
        json!({"html": "<b>ok</b><script>alert(1)</script><style>body{}</style>"}),
    );
    let html = runner
        .render_extension_tool_html(ToolRenderPhase::Result, "extTool", &json!({}))
        .await
        .ok_or("expected rendered html")?;
    let lower = html.to_ascii_lowercase();
    assert!(!lower.contains("<script"), "script block stripped");
    assert!(!lower.contains("<style"), "style block stripped");
    assert!(!lower.contains("alert"), "script body removed");
    assert!(html.contains("ok"), "safe content preserved");
    Ok(())
}

#[test]
fn sanitize_html_handles_interleaved_blocks() {
    let out = sanitize_html("<style>x</style><script>y</script><p>z</p>");
    let lower = out.to_ascii_lowercase();
    assert!(!lower.contains("<script") && !lower.contains("<style"));
    assert!(out.contains('z'));
    // Unterminated block drops the remainder.
    let out = sanitize_html("<p>a</p><script>evil");
    assert!(!out.contains("evil"));
    assert!(out.contains('a'));
}

// ===========================================================================
// Tool-update / provider-event pumps
// ===========================================================================

#[tokio::test]
async fn tool_update_and_provider_event_pumps_forward() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    let mut tool_updates = runner.subscribe_tool_updates();
    let mut provider_events = runner.subscribe_provider_events();

    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "toolUpdate".to_owned(),
        payload: json!({"toolCallId": "c1", "toolName": "extTool", "partialResult": {"content": []}}),
    })
    .await;
    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "providerEvent".to_owned(),
        payload: json!({"providerId": "extProv", "callId": "call1", "event": "chunk", "data": {}}),
    })
    .await;

    let update = next_item(&mut tool_updates, Duration::from_millis(500)).await?;
    assert_eq!(update.tool_call_id, "c1");
    let event = next_item(&mut provider_events, Duration::from_millis(500)).await?;
    assert_eq!(event.provider_id, "extProv");
    Ok(())
}

// ===========================================================================
// Immutable process fixture + reap helper
// ===========================================================================

/// One immutable `/bin/sh` fixture written per test process into a
/// process-unique tempdir. All behavioral variability is carried by argv[1]
/// (behavior selector) and a co-located `snapshot.json` file (for
/// `native-snapshot`). The fixture file itself is never rewritten or
/// truncated, eliminating the ETXTBSY write-then-execute race that mutable
/// per-test scripts produced under parallel scheduling.
///
/// Behaviors (argv[1]):
/// - `reject-handshake` — hello with `protocolVersion` 999 (handshake failure)
/// - `reject-load` — hello ok, `extensions.load` → null (load failure)
/// - `ready` — hello ok, load with `session_shutdown` handler, then drain
/// - `exit-after-load` — hello ok, load ok, then exit 0 (stdout EOF)
/// - `native-snapshot` — hello ok + load payload from `snapshot.json` in the
///   same directory as the invoked path, then drain (used by `runtime_set`
///   native-endpoint tests via hard link)
/// - `hang` — hello ok, load ok, then drain until stdin EOF
///
/// argv[2] = pid file path (startup tests), argv[3] = shutdown file path.
/// When omitted (native-snapshot via `build_generation`), the script
/// defaults to `native-snapshot` and skips pid/shutdown bookkeeping.
#[cfg(unix)]
const STARTUP_HOST_SCRIPT: &str = r#"#!/bin/sh
behavior="${1:-native-snapshot}"
pid_file="$2"
shutdown_file="$3"
if [ -n "$pid_file" ]; then
  printf '%s\n' "$$" > "$pid_file"
fi
IFS= read -r request || exit 10
case "$behavior" in
  reject-handshake)
    printf '%s\n' '{"id":1,"kind":"res","method":"hello","payload":{"protocolVersion":999,"compatibilityVersion":"rejected"}}'
    ;;
  *)
    printf '%s\n' '{"id":1,"kind":"res","method":"hello","payload":{"protocolVersion":1,"compatibilityVersion":"0.80.10"}}'
    ;;
esac
case "$behavior" in
  reject-handshake)
    :
    ;;
  reject-load)
    IFS= read -r request || exit 11
    printf '%s\n' '{"id":2,"kind":"res","method":"extensions.load","payload":null}'
    ;;
  ready|exit-after-load|hang)
    IFS= read -r request || exit 11
    printf '%s\n' '{"id":2,"kind":"res","method":"extensions.load","payload":{"handlers":["session_shutdown"]}}'
    ;;
  native-snapshot)
    IFS= read -r request || exit 11
    snapshot=$(cat "$(dirname "$0")/snapshot.json")
    printf '{"id":2,"kind":"res","method":"extensions.load","payload":%s}\n' "$snapshot"
    ;;
esac
case "$behavior" in
  exit-after-load)
    exit 0
    ;;
  native-snapshot|hang)
    while IFS= read -r request; do :; done
    exit 0
    ;;
esac
while IFS= read -r request; do
  case "$request" in
    *'"method":"session_shutdown"'*)
      if [ -n "$shutdown_file" ]; then
        printf '%s\n' "$request" >> "$shutdown_file"
      fi
      printf '%s\n' '{"id":3,"kind":"res","method":"session_shutdown","payload":{}}'
      ;;
  esac
done
if [ -n "$shutdown_file" ]; then
  printf '%s\n' shutdown >> "$shutdown_file"
fi
"#;

#[cfg(unix)]
#[expect(
    clippy::expect_used,
    reason = "fixture creation is irrecoverable; a broken test environment must abort"
)]
static STARTUP_HOST_FIXTURE: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(|| {
    let directory = tempfile::tempdir().expect("create fixture tempdir");
    let script_path = directory.path().join("startup-host");
    fs::write(&script_path, STARTUP_HOST_SCRIPT).expect("write fixture script");
    let mut permissions = fs::metadata(&script_path)
        .expect("stat fixture")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).expect("chmod fixture");
    // Prevent TempDir cleanup so the fixture persists for the process lifetime.
    let _ = directory.keep();
    script_path
});

/// Return the path to the shared immutable startup-host fixture, writing it
/// once per process on first call.
#[cfg(unix)]
pub(crate) fn startup_host() -> PathBuf {
    STARTUP_HOST_FIXTURE.clone()
}

/// Build a [`HostSpec`] for the shared fixture with the given behavior and
/// per-test pid/shutdown paths.
#[cfg(unix)]
fn startup_host_spec(behavior: &str, pid_path: &Path, shutdown_path: &Path) -> HostSpec {
    let fixture = startup_host();
    HostSpec {
        source: HostSource::Env(fixture.clone()),
        program: fixture,
        args: vec![
            behavior.to_owned(),
            pid_path.to_string_lossy().into_owned(),
            shutdown_path.to_string_lossy().into_owned(),
        ],
    }
}

/// Poll `kill -0` until the process is reaped (no longer alive), with a
/// 30-second timeout. Replaces both `assert_host_reaped` and the inline
/// polling loop in `pump_eof_shuts_down_and_reaps_exactly_once`.
#[cfg(unix)]
async fn wait_until_reaped(pid: u32) -> R {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let alive = std::process::Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| -> BoxErr { format!("host process {pid} was not reaped within 30s").into() })?;
    Ok(())
}

/// Read the pid file and wait for the process to be reaped.
#[cfg(unix)]
async fn wait_pidfile_reaped(pid_path: &Path) -> R {
    let pid: u32 = fs::read_to_string(pid_path)?
        .trim()
        .parse()
        .map_err(|e| -> BoxErr { format!("invalid pid: {e}").into() })?;
    wait_until_reaped(pid).await
}

#[cfg(unix)]
#[tokio::test]
async fn failed_handshake_startup_shuts_down_and_reaps() -> R {
    let directory = tempfile::tempdir()?;
    let pid_path = directory.path().join("host.pid");
    let shutdown_path = directory.path().join("shutdown");
    let spec = startup_host_spec("reject-handshake", &pid_path, &shutdown_path);
    let error = match HostExtensionRunner::spawn_from(&spec, Vec::new()).await {
        Ok(runner) => {
            runner.shutdown_once().await;
            return Err("rejected handshake unexpectedly started a runner".into());
        }
        Err(error) => error,
    };
    assert!(
        matches!(error, HostStartError::Handshake(_)),
        "unexpected startup error: {error}"
    );
    assert_eq!(fs::read_to_string(&shutdown_path)?, "shutdown\n");
    wait_pidfile_reaped(&pid_path).await
}

#[cfg(unix)]
#[tokio::test]
async fn failed_load_startup_shuts_down_and_reaps() -> R {
    let directory = tempfile::tempdir()?;
    let pid_path = directory.path().join("host.pid");
    let shutdown_path = directory.path().join("shutdown");
    let spec = startup_host_spec("reject-load", &pid_path, &shutdown_path);
    let error = match HostExtensionRunner::spawn_from(&spec, Vec::new()).await {
        Ok(runner) => {
            runner.shutdown_once().await;
            return Err("invalid load response unexpectedly started a runner".into());
        }
        Err(error) => error,
    };
    assert!(
        matches!(error, HostStartError::Load(_)),
        "unexpected startup error: {error}"
    );
    assert_eq!(fs::read_to_string(&shutdown_path)?, "shutdown\n");
    wait_pidfile_reaped(&pid_path).await
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_process_shutdown_reaps_and_remains_idempotent() -> R {
    let directory = tempfile::tempdir()?;
    let pid_path = directory.path().join("host.pid");
    let shutdown_path = directory.path().join("shutdown");
    let spec = startup_host_spec("ready", &pid_path, &shutdown_path);
    let runner = HostExtensionRunner::spawn_from(&spec, Vec::new()).await?;
    runner.shutdown_once().await;
    runner.shutdown_once().await;
    // Post-reap lifecycle emit is a no-op on the dead transport.
    let _ = ExtensionRunner::emit(
        runner.as_ref(),
        AgentSessionEvent::SessionShutdown {
            reason: ShutdownReason::Quit,
            target_session_file: None,
        },
    )
    .await?;
    assert!(!runner.is_running());
    assert_eq!(fs::read_to_string(&shutdown_path)?, "shutdown\n");
    wait_pidfile_reaped(&pid_path).await
}

#[cfg(unix)]
#[tokio::test]
async fn pump_eof_shuts_down_and_reaps_exactly_once() -> R {
    let directory = tempfile::tempdir()?;
    let pid_path = directory.path().join("host.pid");
    let shutdown_path = directory.path().join("shutdown");
    let spec = startup_host_spec("exit-after-load", &pid_path, &shutdown_path);
    let runner = HostExtensionRunner::spawn_from(&spec, Vec::new()).await?;
    let mut errors = runner.subscribe_errors();

    // The child exits right after load: the pump's EOF branch must publish
    // extension_closed and shut down / reap the transport on its own. The
    // broadcast can beat the subscription when the child exits fast, so the
    // error assertion only applies when the subscription won that race; the
    // load-bearing contract below is shutdown + exactly-once reap.
    tokio::time::timeout(Duration::from_secs(30), async {
        // Real child process: poll (no fake clock) both the error channel and
        // the running flag so a missed broadcast cannot wedge the test.
        loop {
            match errors.try_recv() {
                Ok(error) => {
                    assert_eq!(error.code, "extension_closed");
                    break;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                    if !runner.is_running() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(_) => break,
            }
        }
        while runner.is_running() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    wait_pidfile_reaped(&pid_path).await?;

    // Second shutdown is a no-op: the exactly-once latch already fired and
    // the dead transport recorded no session_shutdown hook delivery.
    runner.shutdown_once().await;
    let recorded = std::fs::read_to_string(&shutdown_path).unwrap_or_default();
    assert!(
        !recorded.contains("session_shutdown"),
        "dead transport must not record a shutdown hook: {recorded:?}"
    );
    Ok(())
}

#[tokio::test]
async fn load_request_carries_both_project_trust_values() -> R {
    for project_trusted in [false, true] {
        let (runner, host) = make_runner_with_trust(full_snapshot(), project_trusted).await?;
        let request = host
            .requests
            .lock()
            .ok()
            .and_then(|requests| {
                requests
                    .iter()
                    .find(|request| request.method == "extensions.load")
                    .cloned()
            })
            .ok_or("missing extensions.load request")?;
        assert_eq!(request.payload["cwd"], "/workspace");
        assert_eq!(request.payload["projectTrusted"], project_trusted);
        runner.shutdown_once().await;
    }
    Ok(())
}

#[test]
fn compact_message_updates_omit_growing_snapshot_content() {
    let mut partial = AssistantMessage::new("test-api", "test-provider", "m", 1);
    partial
        .content
        .push(AssistantContent::Text(TextContent::new("hello")));
    let partial = Arc::new(partial);

    let delta = compact_message_update_event(&AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "lo".to_owned(),
        partial: Arc::clone(&partial),
    });
    assert_eq!(delta["type"], "text_delta");
    assert_eq!(delta["delta"], "lo");
    assert!(delta.get("partial").is_none());
    assert!(delta.get("block").is_none());
    assert!(delta["meta"].get("content").is_none());

    let end = compact_message_update_event(&AssistantMessageEvent::TextEnd {
        content_index: 0,
        content: "hello".to_owned(),
        partial: Arc::clone(&partial),
    });
    assert_eq!(end["block"]["type"], "text");
    assert_eq!(end["block"]["text"], "hello");
    assert!(end["meta"].get("content").is_none());
}

#[tokio::test]
async fn trusted_restart_sends_true_in_replacement_load_request() -> R {
    let (runner, _host) = make_runner_with_trust(full_snapshot(), true).await?;
    let runtime = ModelRuntime::create(CreateModelRuntimeOptions::default()).await?;
    let observed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed_by_start = Arc::clone(&observed);
    let replacement_host = Arc::new(Mutex::new(None::<FakeHost>));
    let replacement_host_by_start = Arc::clone(&replacement_host);

    let replacement = runner
        .restart_and_rewire_with(
            &runtime,
            HashMap::new(),
            move |_paths, _cwd, project_trusted| async move {
                let (replacement, host) = make_runner_with_trust(full_snapshot(), project_trusted)
                    .await
                    .map_err(|error| HostStartError::Load(error.to_string()))?;
                let load = host
                    .requests
                    .lock()
                    .ok()
                    .and_then(|requests| {
                        requests
                            .iter()
                            .find(|request| request.method == "extensions.load")
                            .cloned()
                    })
                    .ok_or_else(|| HostStartError::Load("missing replacement load".to_owned()))?;
                observed_by_start
                    .lock()
                    .map_err(|_| HostStartError::Load("observed lock poisoned".to_owned()))?
                    .push(load.payload);
                *replacement_host_by_start
                    .lock()
                    .map_err(|_| HostStartError::Load("host lock poisoned".to_owned()))? =
                    Some(host);
                Ok(replacement)
            },
        )
        .await?;

    {
        let payloads = observed.lock().map_err(|_| "observed lock poisoned")?;
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0]["projectTrusted"], true);
    }
    let replacement_fake = replacement_host
        .lock()
        .map_err(|_| "host lock poisoned")?
        .as_ref()
        .cloned()
        .ok_or("replacement host missing")?;
    replacement_fake.set_response("resources_discover", json!({"paths": []}));
    let _ = replacement
        .emit_resources_discover("/workspace", "reload")
        .await?;
    let methods = replacement_fake
        .requests
        .lock()
        .map_err(|_| "request lock poisoned")?
        .iter()
        .map(|request| request.method.clone())
        .collect::<Vec<_>>();
    let flags_index = methods
        .iter()
        .position(|method| method == pi_ext::protocol::FLAGS_SET_METHOD)
        .ok_or("missing replacement flags.set")?;
    let hook_index = methods
        .iter()
        .position(|method| method == "resources_discover")
        .ok_or("missing replacement resources_discover")?;
    assert!(
        flags_index < hook_index,
        "replacement flags.set must precede reload resource hooks"
    );
    replacement.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn protocol_fatal_stops_event_pump_and_host_transport() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    let mut errors = runner.subscribe_errors();
    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "rogue.method".to_owned(),
        payload: json!({}),
    })
    .await;

    let error = tokio::time::timeout(Duration::from_secs(1), errors.recv()).await??;
    assert_eq!(error.code, "extension_protocol");
    tokio::time::timeout(Duration::from_secs(1), async {
        while runner.is_running() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert!(!runner.is_running());
    Ok(())
}

#[tokio::test]
async fn cancelled_before_tool_hook_returns_without_waiting_for_host_timeout() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    host.drop_method("tool_call");
    let hooks = Arc::new(SessionHooks::new(
        Arc::clone(&runner) as Arc<dyn ExtensionRunner>
    ));
    let hook = hooks.before_tool_call_hook();
    let cancel = tokio_util::sync::CancellationToken::new();
    let call_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        hook(
            BeforeToolCallContext {
                assistant_message: AssistantMessage::new("test-api", "test-provider", "m", 1),
                tool_call: ToolCall::new("tc1", "read", Map::new()),
                args: Map::new(),
                context: AgentContext {
                    system_prompt: String::new(),
                    messages: Vec::<AgentMessage>::new(),
                    tools: Vec::new(),
                },
            },
            call_cancel,
        )
        .await
    });
    host.wait_for_request("tool_call").await?;
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_millis(50), task).await??;
    assert!(result?.is_none());
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn cancelled_after_tool_hook_returns_without_waiting_for_host_timeout() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    host.drop_method("tool_result");
    let hooks = Arc::new(SessionHooks::new(
        Arc::clone(&runner) as Arc<dyn ExtensionRunner>
    ));
    let hook = hooks.after_tool_call_hook();
    let cancel = tokio_util::sync::CancellationToken::new();
    let call_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        hook(
            AfterToolCallContext {
                assistant_message: AssistantMessage::new("test-api", "test-provider", "m", 1),
                tool_call: ToolCall::new("tc2", "read", Map::new()),
                args: Map::new(),
                result: AgentToolResult::default(),
                is_error: false,
                context: AgentContext {
                    system_prompt: String::new(),
                    messages: Vec::<AgentMessage>::new(),
                    tools: Vec::new(),
                },
            },
            call_cancel,
        )
        .await
    });
    host.wait_for_request("tool_result").await?;
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_millis(50), task).await??;
    assert!(result?.is_none());
    runner.shutdown_once().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_hook_runs_once_before_transport_reap() -> R {
    let directory = tempfile::tempdir()?;
    let pid_path = directory.path().join("host.pid");
    let shutdown_path = directory.path().join("shutdown");
    let spec = startup_host_spec("ready", &pid_path, &shutdown_path);
    let runner = HostExtensionRunner::spawn_from(&spec, Vec::new()).await?;
    ExtensionRunner::emit(
        runner.as_ref(),
        AgentSessionEvent::SessionShutdown {
            reason: ShutdownReason::Quit,
            target_session_file: Some("/tmp/next.jsonl".to_owned()),
        },
    )
    .await?;
    runner.shutdown_once().await;

    let sequence = fs::read_to_string(&shutdown_path)?;
    let lines = sequence.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        2,
        "expected one hook request followed by one reap"
    );
    let request: Frame = serde_json::from_str(lines[0])?;
    assert_eq!(request.method, "session_shutdown");
    assert_eq!(request.payload["type"], "session_shutdown");
    assert_eq!(request.payload["reason"], "quit");
    assert_eq!(request.payload["targetSessionFile"], "/tmp/next.jsonl");
    assert_eq!(lines[1], "shutdown");
    wait_pidfile_reaped(&pid_path).await
}

#[tokio::test]
async fn shutdown_reaps_transport_when_hook_times_out() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    host.drop_method("session_shutdown");
    // Hook failure is isolated inside emit; reap still succeeds.
    let _ = ExtensionRunner::emit(
        runner.as_ref(),
        AgentSessionEvent::SessionShutdown {
            reason: ShutdownReason::Quit,
            target_session_file: None,
        },
    )
    .await?;
    runner.shutdown_once().await;
    assert!(!runner.is_running());
    Ok(())
}

#[tokio::test]
async fn shutdown_is_idempotent() -> R {
    let (runner, _host) = make_runner(full_snapshot()).await?;
    runner.shutdown_once().await;
    assert!(!runner.is_running());
    runner.shutdown_once().await;
    assert!(!runner.is_running());
    let _ = ExtensionRunner::emit(
        runner.as_ref(),
        AgentSessionEvent::SessionShutdown {
            reason: ShutdownReason::Quit,
            target_session_file: None,
        },
    )
    .await?;
    assert!(!runner.is_running());
    Ok(())
}
// ===========================================================================
// Real Host Integration (Cross-Language)
// ===========================================================================

/// Workspace-relative path to the compiled JS host artifact.
fn real_host_path() -> std::result::Result<std::path::PathBuf, BoxErr> {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest
        .parent()
        .and_then(|p| p.parent())
        .ok_or("workspace root not found")?;
    Ok(workspace_root
        .join("packages")
        .join("extension-host")
        .join("dist")
        .join("pi-extension-host"))
}
/// Spawn the compiled JS host through the production `HostClient` spawn path
/// and verify the real cross-language lifecycle: hello handshake,
/// extensions.load, registry surfaces via `getAllRegisteredTools` (the load
/// payload itself is an empty snapshot), plus the specialized hook shapes
/// that the host maps from `extensions.load`'s `extensionPaths` (the fixtures
/// register handlers).
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn real_host_lifecycle_and_dispatch() -> R {
    use pi_ext::client::HostNotification;
    use pi_ext::protocol::Method;

    let host_path = real_host_path()?;
    assert!(
        host_path.is_file(),
        "compiled host artifact missing: {}",
        host_path.display()
    );

    let spec = HostSpec {
        source: HostSource::Env(host_path.clone()),
        program: host_path,
        args: vec!["--cwd".to_owned(), ".".to_owned()],
    };
    let client =
        HostClient::spawn(&spec).map_err(|e| -> BoxErr { format!("spawn: {e}").into() })?;

    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .ok_or("workspace root")?;
    let fixtures_root = workspace_root
        .join("packages")
        .join("extension-host")
        .join("fixtures")
        .join("extensions");

    // Phase 1: hello handshake — protocol + compatibility version round-trip
    // through the production client path.
    client.handshake().await?;

    // Phase 2: extensions.load — hostile + tool fixtures register and report
    // errors.
    let load_resp = client
        .request_raw(
            "extensions.load",
            json!({
                "extensionPaths": [
                    fixtures_root.join("hostile.ts").to_string_lossy(),
                    fixtures_root.join("all-events.ts").to_string_lossy(),
                    fixtures_root.join("hooks.ts").to_string_lossy(),
                    fixtures_root.join("tool.ts").to_string_lossy(),
                ],
                "cwd": workspace_root.to_string_lossy(),
            }),
            Duration::from_secs(10),
        )
        .await?;
    assert_eq!(load_resp.kind, FrameKind::Res);
    assert_eq!(load_resp.method, "extensions.load");
    let load_payload = load_resp
        .payload
        .as_object()
        .ok_or("load payload not object")?;
    let extensions_loaded = load_payload
        .get("extensions")
        .and_then(Value::as_u64)
        .ok_or("extensions.load payload missing `extensions: number`")?;
    let load_errors = load_payload
        .get("errors")
        .and_then(Value::as_array)
        .ok_or("extensions.load payload missing `errors: array`")?;
    assert!(
        extensions_loaded >= 1,
        "expected at least one extension to load, got {extensions_loaded}"
    );
    assert!(
        load_errors.iter().all(Value::is_object),
        "errors array must contain objects, got: {load_errors:?}"
    );

    // Phase 3: session_start + uiSlot push (unsolicited notification).
    let mut notifications = client.subscribe_notifications();
    let session_resp = client
        .request_raw(
            "session_start",
            json!({"type": "session_start", "reason": "startup"}),
            Duration::from_secs(5),
        )
        .await?;
    assert_eq!(session_resp.kind, FrameKind::Res);
    assert_eq!(session_resp.method, "session_start");

    // Wait for the hostile widget's uiSlot push.
    let slot = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match notifications.recv().await {
                Ok(HostNotification::UiSlot(slot)) if slot.key == "widget.hostile" => {
                    return Ok::<_, BoxErr>(slot);
                }
                Ok(_) => {}
                Err(_) => return Err("notification stream closed".into()),
            }
        }
    })
    .await??;
    assert_eq!(slot.placement, pi_ext::protocol::SlotPlacement::AboveEditor);
    assert!(slot.generation > 0);
    assert!(
        !slot.runs.is_empty(),
        "hostile widget must push at least one sanitized run"
    );

    // Phase 4: render — sanitized runs must not contain raw ESC.
    let render_resp = client
        .request(
            Method::Render,
            json!({"key": "widget.hostile", "width": 80}),
            Duration::from_secs(5),
        )
        .await?;
    assert_eq!(render_resp.kind, FrameKind::Res);
    let render_runs = render_resp
        .payload
        .as_object()
        .and_then(|o| o.get("runs"))
        .and_then(Value::as_array)
        .ok_or("render payload missing runs")?;
    for line in render_runs {
        for run in line.as_array().into_iter().flatten() {
            let text = run.get("text").and_then(Value::as_str).unwrap_or_default();
            assert!(
                !text.contains('\x1b'),
                "rendered text contains raw ESC: {text:?}"
            );
        }
    }

    // Phase 5: measure — integer height > 0.
    let measure_resp = client
        .request(
            Method::Measure,
            json!({"key": "widget.hostile", "width": 80}),
            Duration::from_secs(5),
        )
        .await?;
    assert_eq!(measure_resp.kind, FrameKind::Res);
    let height = measure_resp
        .payload
        .as_object()
        .and_then(|o| o.get("height"))
        .and_then(Value::as_u64)
        .ok_or("measure missing height: u64")?;
    assert!(height > 0, "measure height must be > 0");

    // Phase 6: input hook — action must be "continue".
    let input_resp = client
        .request_raw(
            "input",
            json!({"text": "hi", "source": "interactive"}),
            Duration::from_secs(5),
        )
        .await?;
    assert_eq!(input_resp.kind, FrameKind::Res);
    assert_eq!(
        input_resp.payload.get("action").and_then(Value::as_str),
        Some("continue")
    );

    // Phase 7: resources_discover — must contain skill/prompt/theme paths.
    let res_resp = client
        .request_raw(
            "resources_discover",
            json!({"cwd": "/tmp", "reason": "startup"}),
            Duration::from_secs(5),
        )
        .await?;
    assert_eq!(res_resp.kind, FrameKind::Res);
    let res_payload = res_resp
        .payload
        .as_object()
        .ok_or("resources payload not object")?;
    assert!(res_payload.contains_key("skillPaths"));
    assert!(res_payload.contains_key("promptPaths"));
    assert!(res_payload.contains_key("themePaths"));

    // Phase 8: command.execute — unknown command must return a not_found
    // error. The HostClient converts error frames into HostClientError::Remote.
    match client
        .request_raw(
            "command.execute",
            json!({"command": "does-not-exist", "args": ""}),
            Duration::from_secs(5),
        )
        .await
    {
        Ok(_) => return Err("command.execute unexpectedly succeeded".into()),
        Err(pi_ext::client::HostClientError::Remote { code, message }) => {
            assert_eq!(
                code, "not_found",
                "unexpected error code: {code}: {message}"
            );
        }
        Err(error) => return Err(format!("unexpected error type: {error}").into()),
    }
    // Phase 9: message_end — must contain a message field.
    let msg_resp = client
        .request_raw(
            "message_end",
            json!({"type": "message_end", "message": {"role": "assistant", "content": []}}),
            Duration::from_secs(5),
        )
        .await?;
    assert_eq!(msg_resp.kind, FrameKind::Res);
    assert!(
        msg_resp
            .payload
            .as_object()
            .is_some_and(|o| o.contains_key("message"))
    );

    // Phase 10: agent_start — ok must be true.
    let agent_resp = client
        .request_raw(
            "agent_start",
            json!({"type": "agent_start"}),
            Duration::from_secs(5),
        )
        .await?;
    assert_eq!(agent_resp.kind, FrameKind::Res);
    assert_eq!(
        agent_resp.payload.get("ok").and_then(Value::as_bool),
        Some(true)
    );

    // Graceful shutdown through the production client path.
    client.shutdown().await?;
    Ok(())
}

/// Minimal in-repo reproduction of the E2E `rust-extension-flag-session-start`
/// marker bug: a real extension must observe the CLI-applied flag value inside
///
/// Fails on pre-lifecycle HEAD with an empty log (no emission at bind time).
#[tokio::test]
async fn real_host_bind_emits_session_start_with_cli_flag_before_discovery() -> R {
    use futures::stream::StreamExt;

    #[derive(Clone)]
    struct StubProvider;

    impl pi_ai::Provider for StubProvider {
        fn stream(
            &self,
            _model: &pi_ai::Model,
            _context: pi_ai::Context,
            _options: pi_ai::StreamOptions,
        ) -> futures::stream::BoxStream<
            'static,
            std::result::Result<AssistantMessageEvent, pi_ai::ProviderError>,
        > {
            futures::stream::empty().boxed()
        }
    }

    let host_path = real_host_path()?;
    assert!(
        host_path.is_file(),
        "compiled host artifact missing: {}",
        host_path.display()
    );

    let directory = tempfile::tempdir()?;
    let log_path = directory.path().join("lifecycle.log");
    let extension_path = directory.path().join("flag-observer.ts");
    std::fs::write(
        &extension_path,
        format!(
            r#"import {{ appendFileSync }} from "node:fs";

const LOG = {log:?};

export default function flagObserver(pi) {{
	pi.registerFlag("marker", {{
		description: "Lifecycle marker flag",
		type: "string",
		default: "unset",
	}});
	pi.on("session_start", (event) => {{
		appendFileSync(LOG, `session_start:${{pi.getFlag("marker")}}:${{event.reason}}\n`);
	}});
	pi.on("resources_discover", (event) => {{
		appendFileSync(LOG, `resources_discover:${{event.reason}}\n`);
		return {{ skillPaths: [], promptPaths: [], themePaths: [] }};
	}});
}}
"#,
            log = log_path.to_string_lossy()
        ),
    )?;

    let spec = pi_ext::host::HostSpec {
        source: pi_ext::host::HostSource::Env(host_path.clone()),
        program: host_path,
        args: Vec::new(),
    };
    let runner =
        HostExtensionRunner::spawn_from(&spec, vec![extension_path.to_string_lossy().into_owned()])
            .await?;

    // CLI flag application (flags.set) happens before any bind.
    runner
        .apply_flag_values(&BTreeMap::from([(
            "marker".to_owned(),
            FlagValueWire::String("cli-value".to_owned()),
        )]))
        .await?;

    let mut config = crate::core::agent_session::AgentSessionConfig::test_config(
        Arc::new(StubProvider),
        pi_agent::state::default_model(),
    )?;
    config.extension_runner = Some(Arc::clone(&runner) as Arc<dyn ExtensionRunner>);
    let session = crate::core::agent_session::AgentSession::new(config)?;

    session
        .bind_extensions(crate::core::agent_session::ExtensionBindings::default())
        .await?;

    let log = std::fs::read_to_string(&log_path)?;
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(
        lines,
        vec![
            "session_start:cli-value:startup",
            "resources_discover:startup"
        ],
        "flags.set → session_start → resources_discover order must hold"
    );

    runner.shutdown_once().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Theme bridge (theme.set / theme.update)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn theme_set_event_forwards_typed_and_keeps_runner_alive() -> R {
    use crate::core::extension_host::{ExtensionThemeRequest, ExtensionUiEvent};

    let (runner, host) = make_runner(json!({})).await?;
    let mut ui = runner.subscribe_ui();

    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "theme.set".to_owned(),
        payload: json!({"name": "classic-light", "persist": true}),
    })
    .await;

    let event = tokio::time::timeout(Duration::from_millis(500), ui.recv()).await??;
    let ExtensionUiEvent::ThemeSet(ExtensionThemeRequest::Named { name, persist }) = event else {
        return Err(format!("expected named ThemeSet, got {event:?}").into());
    };
    assert_eq!(name, "classic-light");
    assert!(persist);

    // An unknown open method would have tripped the fatal Raw path and shut
    // the pump down; prove the pump is still routing by receiving a second
    // typed event after the first.
    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "theme.set".to_owned(),
        payload: json!({"persist": false, "theme": {
            "colorMode": "truecolor", "fg": {"text": "#ffffff"}, "bg": {}
        }}),
    })
    .await;
    let event = tokio::time::timeout(Duration::from_millis(500), ui.recv()).await??;
    let ExtensionUiEvent::ThemeSet(ExtensionThemeRequest::Instance(wire)) = event else {
        return Err(format!("expected object-form ThemeSet, got {event:?}").into());
    };
    assert_eq!(wire.color_mode, "truecolor");

    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn push_theme_update_sends_event_and_records_generation() -> R {
    use pi_ext::protocol::{ThemeColorValue, ThemeUpdate, ThemeWire};
    use std::collections::BTreeMap as Map;

    let (runner, host) = make_runner(json!({})).await?;
    assert_eq!(runner.theme_generation(), 0);

    let mut fg = Map::new();
    fg.insert(
        "text".to_owned(),
        ThemeColorValue::Text("#ededed".to_owned()),
    );
    let update = ThemeUpdate {
        theme: ThemeWire {
            name: Some("dark".to_owned()),
            source_path: None,
            color_mode: "truecolor".to_owned(),
            fg,
            bg: Map::new(),
        },
        terminal_theme: "dark".to_owned(),
        theme_mode: "auto".to_owned(),
        theme_generation: 3,
        themes: Vec::new(),
    };
    runner.push_theme_update(&update).await;

    host.wait_for_request("theme.update").await?;
    let frame = host
        .requests
        .lock()
        .ok()
        .and_then(|requests| {
            requests
                .iter()
                .find(|frame| frame.method == "theme.update")
                .cloned()
        })
        .ok_or("theme.update frame missing")?;
    assert_eq!(frame.kind, FrameKind::Event);
    assert_eq!(frame.id, 0);
    assert_eq!(frame.payload["theme"]["name"], "dark");
    assert_eq!(frame.payload["theme"]["fg"]["text"], "#ededed");
    assert_eq!(frame.payload["themeGeneration"], 3);
    assert_eq!(frame.payload["terminalTheme"], "dark");
    assert_eq!(runner.theme_generation(), 3);

    runner.shutdown_once().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Session-action bridge (session.command / session.setModel / session.update)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_command_and_set_model_route_through_claimed_bridge() -> R {
    use crate::core::extension_host::SessionBridgeEvent;

    let (runner, host) = make_runner(json!({})).await?;
    let mut bridge = runner
        .take_session_bridge()
        .ok_or("first claim must yield the bridge receiver")?;
    assert!(
        runner.take_session_bridge().is_none(),
        "second claim must be rejected"
    );

    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "session.command".to_owned(),
        payload: json!({"action": "setSessionName", "name": "Renamed"}),
    })
    .await;
    let event = tokio::time::timeout(Duration::from_millis(500), bridge.recv())
        .await?
        .ok_or("bridge closed")?;
    let SessionBridgeEvent::Command { envelope, .. } = event else {
        return Err(format!("expected SetSessionName, got {event:?}").into());
    };
    assert_eq!(envelope.replacement_token, None);
    let SessionCommand::SetSessionName { name } = envelope.command else {
        return Err("expected SetSessionName command".into());
    };
    assert_eq!(name, "Renamed");

    host.emit(Frame {
        id: 41,
        kind: FrameKind::Req,
        method: "session.setModel".to_owned(),
        payload: json!({"model": {"id": "gpt-x", "provider": "openai"}}),
    })
    .await;
    let event = tokio::time::timeout(Duration::from_millis(500), bridge.recv())
        .await?
        .ok_or("bridge closed")?;
    let SessionBridgeEvent::SetModel { id, request } = event else {
        return Err(format!("expected SetModel, got {event:?}").into());
    };
    assert_eq!(id, BridgeRequestId(41));
    assert_eq!(request.model["id"], "gpt-x");

    runner.respond_set_model(id, true).await?;
    host.wait_for_request("session.setModel").await?;
    let response = host
        .requests
        .lock()
        .ok()
        .and_then(|requests| {
            requests
                .iter()
                .find(|frame| frame.method == "session.setModel" && frame.kind == FrameKind::Res)
                .cloned()
        })
        .ok_or("setModel response missing")?;
    assert_eq!(response.id, 41);
    assert_eq!(response.payload["success"], true);

    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn inbound_ui_select_request_flows_through_take_ui_requests() -> R {
    use pi_ext::client::{DialogOutcome, HostUiRequest, HostUiResponse};

    let (runner, host) = make_runner(json!({})).await?;
    let mut dialogs = runner
        .take_ui_requests()
        .ok_or("first claim must yield the dialog receiver")?;
    assert!(
        runner.take_ui_requests().is_none(),
        "second claim must be rejected"
    );

    host.emit(Frame {
        id: 41,
        kind: FrameKind::Req,
        method: "select".to_owned(),
        payload: json!({"title": "Pick one", "options": ["opt-a", "opt-b"]}),
    })
    .await;
    let request = tokio::time::timeout(Duration::from_millis(500), dialogs.recv())
        .await?
        .ok_or("dialog receiver closed")?;
    assert_eq!(request.id(), 41);
    let HostUiRequest::Select {
        request: select, ..
    } = &request
    else {
        return Err(format!("expected Select dialog, got {request:?}").into());
    };
    assert_eq!(select.title, "Pick one");

    runner
        .respond_ui(HostUiResponse::Select {
            id: request.id(),
            outcome: DialogOutcome::Answered("opt-b".to_owned()),
        })
        .await?;
    host.wait_for_request("select").await?;
    let response = host
        .requests
        .lock()
        .ok()
        .and_then(|requests| {
            requests
                .iter()
                .find(|frame| frame.method == "select" && frame.kind == FrameKind::Res)
                .cloned()
        })
        .ok_or("select response missing")?;
    assert_eq!(response.id, 41);
    assert_eq!(response.payload["value"], "opt-b");

    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn scoped_command_precedes_replacement_ready_on_claimed_bridge() -> R {
    use crate::core::extension_host::SessionBridgeEvent;
    let (runner, host) = make_runner(json!({})).await?;
    let mut bridge = runner
        .take_session_bridge()
        .ok_or("first claim must yield the bridge receiver")?;

    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "session.command".to_owned(),
        payload: json!({
            "replacementToken": "ordered-token",
            "action": "setSessionName",
            "name": "Candidate"
        }),
    })
    .await;
    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "session.replacementReady".to_owned(),
        payload: json!({"token": "ordered-token"}),
    })
    .await;

    let command = tokio::time::timeout(Duration::from_millis(500), bridge.recv())
        .await?
        .ok_or("bridge closed before command")?;
    let ready = tokio::time::timeout(Duration::from_millis(500), bridge.recv())
        .await?
        .ok_or("bridge closed before readiness")?;
    assert!(matches!(
        command,
        SessionBridgeEvent::Command {
            envelope: super::super::agent_session::bridge_types::SessionCommandEnvelope {
                replacement_token: Some(token),
                ..
            },
            ..
        } if token == "ordered-token"
    ));
    assert!(matches!(
        ready,
        SessionBridgeEvent::ReplacementReady { token, .. }
            if token == "ordered-token"
    ));

    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn unclaimed_set_model_request_is_answered_failure() -> R {
    let (runner, host) = make_runner(json!({})).await?;

    host.emit(Frame {
        id: 7,
        kind: FrameKind::Req,
        method: "session.setModel".to_owned(),
        payload: json!({"model": {"id": "gpt-x", "provider": "openai"}}),
    })
    .await;

    host.wait_for_request("session.setModel").await?;
    let response = host
        .requests
        .lock()
        .ok()
        .and_then(|requests| {
            requests
                .iter()
                .find(|frame| frame.method == "session.setModel" && frame.kind == FrameKind::Res)
                .cloned()
        })
        .ok_or("setModel failure response missing")?;
    assert_eq!(response.id, 7);
    assert_eq!(response.payload["success"], false);

    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn dropped_replacement_ready_emits_diagnostic() -> R {
    let (runner, host) = make_runner(json!({})).await?;
    // Install a drop handler that records the token it receives.
    let received_token = Arc::new(Mutex::new(None::<String>));
    let handler_token = Arc::clone(&received_token);
    runner.set_replacement_drop_handler(Arc::new(
        move |token: &str, _origin: Option<EndpointId>| {
            *handler_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(token.to_owned());
        },
    ));
    // Claim the bridge, then drop the receiver to close the channel.
    let bridge = runner.take_session_bridge().ok_or("bridge missing")?;
    drop(bridge);
    let mut errors = runner.subscribe_errors();
    // Emit a replacementReady event — the channel is closed so try_send
    // fails, and the dropped frame must produce an immediate diagnostic
    // and invoke the drop handler with the token.
    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "session.replacementReady".to_owned(),
        payload: json!({"token": "tok-1"}),
    })
    .await;
    let error = next_error(&mut errors, Duration::from_secs(2)).await?;
    assert_eq!(
        error.code, "extension_replacement_dropped",
        "expected extension_replacement_dropped diagnostic, got: {error:?}"
    );
    // The drop handler must receive tok-1 synchronously.
    let token = received_token
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .ok_or("drop handler was not invoked")?;
    assert_eq!(token, "tok-1", "drop handler received the wrong token");
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn unexpected_direct_session_control_reports_and_continues() -> R {
    let (runner, _host) = make_runner(json!({})).await?;
    let received_token = Arc::new(Mutex::new(None::<String>));
    let handler_token = Arc::clone(&received_token);
    runner.set_replacement_drop_handler(Arc::new(
        move |token: &str, _origin: Option<EndpointId>| {
            *handler_token
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(token.to_owned());
        },
    ));
    let mut errors = runner.subscribe_errors();

    finish_direct_session_control_delivery(
        &runner.inner,
        Some(SessionBridgeEvent::Reload {
            id: BridgeRequestId(41),
        }),
    );
    let protocol_error = next_error(&mut errors, Duration::from_secs(2)).await?;
    assert_eq!(protocol_error.code, "extension_protocol");
    assert_eq!(
        protocol_error.message,
        "correlated event reached the direct session-control route"
    );

    finish_direct_session_control_delivery(
        &runner.inner,
        Some(SessionBridgeEvent::ReplacementReady {
            token: "after-diagnostic".to_owned(),
            origin: None,
        }),
    );
    let dropped_error = next_error(&mut errors, Duration::from_secs(2)).await?;
    assert_eq!(dropped_error.code, "extension_replacement_dropped");
    assert_eq!(
        received_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref(),
        Some("after-diagnostic")
    );

    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn ui_control_event_forwards_typed_to_ui_subscribers() -> R {
    use crate::core::extension_host::{ExtensionUiControl, ExtensionUiEvent};

    let (runner, host) = make_runner(json!({})).await?;
    let mut ui = runner.subscribe_ui();

    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "ui.control".to_owned(),
        payload: json!({"control": "setEditorText", "text": "draft"}),
    })
    .await;

    let event = tokio::time::timeout(Duration::from_millis(500), ui.recv()).await??;
    let ExtensionUiEvent::UiControl(ExtensionUiControl::SetEditorText { text }) = event else {
        return Err(format!("expected SetEditorText, got {event:?}").into());
    };
    assert_eq!(text, "draft");

    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn push_session_and_ui_state_send_mirror_events() -> R {
    use crate::core::agent_session::SessionState;
    use pi_ext::protocol::UiStateWire;

    let (runner, host) = make_runner(json!({})).await?;

    let state = SessionState {
        session_name: Some("s".to_owned()),
        thinking_level: pi_ai::ModelThinkingLevel::Medium,
        active_tools: vec!["read".to_owned()],
        is_idle: true,
        system_prompt: "p".to_owned(),
        ..SessionState::default()
    };
    runner.push_session_state(&state).await;
    host.wait_for_request("session.update").await?;
    let frame = host
        .requests
        .lock()
        .ok()
        .and_then(|requests| {
            requests
                .iter()
                .find(|frame| frame.method == "session.update")
                .cloned()
        })
        .ok_or("session.update frame missing")?;
    assert_eq!(frame.kind, FrameKind::Event);
    assert_eq!(frame.payload["sessionName"], "s");
    assert_eq!(frame.payload["isIdle"], true);
    assert_eq!(frame.payload["thinkingLevel"], "medium");

    runner
        .push_ui_state(&UiStateWire {
            editor_text: "draft".to_owned(),
            tools_expanded: true,
        })
        .await;
    host.wait_for_request("ui.state").await?;
    let frame = host
        .requests
        .lock()
        .ok()
        .and_then(|requests| {
            requests
                .iter()
                .find(|frame| frame.method == "ui.state")
                .cloned()
        })
        .ok_or("ui.state frame missing")?;
    assert_eq!(frame.payload["editorText"], "draft");
    assert_eq!(frame.payload["toolsExpanded"], true);

    runner.shutdown_once().await;
    Ok(())
}

/// The awaited initial `session.update` push must land BEFORE `session_start`
/// is emitted: a handler reading the mirrored synchronous getters inside its
/// `session_start` hook observes real session state (the pre-bind session
/// name), never the host defaults. The second half proves the reverse
/// direction: a bridged `pi.setSessionName` command mutates the real session.
#[tokio::test]
async fn real_host_session_start_observes_initial_snapshot_and_commands_mutate() -> R {
    use futures::stream::StreamExt;

    #[derive(Clone)]
    struct StubProvider;

    impl pi_ai::Provider for StubProvider {
        fn stream(
            &self,
            _model: &pi_ai::Model,
            _context: pi_ai::Context,
            _options: pi_ai::StreamOptions,
        ) -> futures::stream::BoxStream<
            'static,
            std::result::Result<AssistantMessageEvent, pi_ai::ProviderError>,
        > {
            futures::stream::empty().boxed()
        }
    }

    let host_path = real_host_path()?;
    assert!(
        host_path.is_file(),
        "compiled host artifact missing: {}",
        host_path.display()
    );

    let directory = tempfile::tempdir()?;
    let log_path = directory.path().join("bridge.log");
    let extension_path = directory.path().join("bridge-observer.ts");
    std::fs::write(
        &extension_path,
        format!(
            r#"import {{ appendFileSync }} from "node:fs";

const LOG = {log:?};

export default function bridgeObserver(pi) {{
	pi.on("session_start", () => {{
		appendFileSync(LOG, `session_start:${{pi.getSessionName()}}\n`);
	}});
	pi.registerCommand("renameSession", {{
		description: "Rename via the bridge",
		async handler() {{
			pi.setSessionName("renamed-by-ext");
		}},
	}});
}}
"#,
            log = log_path.to_string_lossy()
        ),
    )?;

    let spec = pi_ext::host::HostSpec {
        source: pi_ext::host::HostSource::Env(host_path.clone()),
        program: host_path,
        args: Vec::new(),
    };
    let runner =
        HostExtensionRunner::spawn_from(&spec, vec![extension_path.to_string_lossy().into_owned()])
            .await?;
    let runtime_set =
        ExtensionRuntimeSet::bind(vec![(EndpointKind::TsCompat, Arc::clone(&runner))]);

    let mut config = crate::core::agent_session::AgentSessionConfig::test_config(
        Arc::new(StubProvider),
        pi_agent::state::default_model(),
    )?;
    config.extension_runner = Some(Arc::clone(&runtime_set) as Arc<dyn ExtensionRunner>);
    config.host_extension_runner = Some(Arc::clone(&runtime_set));
    let session = crate::core::agent_session::AgentSession::new(config)?;

    // Real pre-bind state the mirror must carry into the session_start hook.
    session.set_session_name("witness").await?;

    session
        .bind_extensions(crate::core::agent_session::ExtensionBindings::default())
        .await?;

    let log = std::fs::read_to_string(&log_path)?;
    assert_eq!(
        log.lines().collect::<Vec<_>>(),
        vec!["session_start:witness"],
        "session_start handler must observe the awaited initial snapshot, not host defaults"
    );

    // Host → Rust: the bridged command mutates the real session.
    assert!(runtime_set.execute_command("renameSession", "").await?);
    let renamed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if session.session_name().await.as_deref() == Some("renamed-by-ext") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        renamed.is_ok(),
        "bridged setSessionName must reach the session"
    );

    runtime_set.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn runtime_set_preserves_first_owned_registry_and_hook_fold_order() -> R {
    let snapshot = |label: &str| {
        json!({
            "tools": [{
                "name": "sharedTool",
                "label": label,
                "description": label,
                "parameters": {"type": "object"}
            }],
            "commands": [{"name": "sharedCommand"}],
            "shortcuts": [{"key": "ctrl+x"}],
            "flags": [{"name": "sharedFlag", "type": "string", "default": label}],
            "renderers": [],
            "providers": [],
            "handlers": ["message_end"]
        })
    };
    let (first, first_host) = make_runner(snapshot("first")).await?;
    let (second, second_host) = make_runner(snapshot("second")).await?;
    first_host.set_response(
        "message_end",
        json!({"message": assistant_text("first replacement")}),
    );
    second_host.set_response(
        "message_end",
        json!({"message": assistant_text("second replacement")}),
    );
    let set = ExtensionRuntimeSet::bind(vec![
        (EndpointKind::TsCompat, first),
        (EndpointKind::TsCompat, second),
    ]);

    assert_eq!(set.registry().tools()[0].label, "first");
    assert_eq!(set.registry().commands().len(), 1);
    assert_eq!(
        set.get_registered_commands(),
        vec!["sharedCommand".to_owned()]
    );
    assert_eq!(set.raw_shortcuts().len(), 2);
    let result = set
        .emit_message_end(assistant_text("original"))
        .await?
        .ok_or("message_end replacement missing")?;
    assert_eq!(
        serde_json::to_value(result)?,
        serde_json::to_value(assistant_text("second replacement"))?
    );
    let second_request = second_host
        .requests
        .lock()
        .ok()
        .and_then(|requests| {
            requests
                .iter()
                .find(|frame| frame.method == "message_end")
                .cloned()
        })
        .ok_or("second message_end request missing")?;
    assert_eq!(
        second_request.payload,
        serde_json::to_value(assistant_text("first replacement"))?
    );
    set.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn runtime_set_correlated_routes_return_to_same_id_on_each_origin() -> R {
    use crate::core::extension_host::SessionBridgeEvent;

    const FIRST_LOCAL: u64 = 7;
    const FIRST_MODEL: &str = "model-alpha";
    const SECOND_LOCAL: u64 = 9;
    const SECOND_MODEL: &str = "model-beta";

    let snapshot = json!({
        "tools": [],
        "commands": [],
        "shortcuts": [],
        "flags": [],
        "renderers": [],
        "providers": [],
        "handlers": []
    });
    let (first, first_host) = make_runner(snapshot.clone()).await?;
    let (second, second_host) = make_runner(snapshot).await?;
    let set = ExtensionRuntimeSet::bind(vec![
        (EndpointKind::TsCompat, first),
        (EndpointKind::TsCompat, second),
    ]);
    let mut bridge = set.take_session_bridge().ok_or("session bridge missing")?;

    first_host
        .emit(Frame {
            id: FIRST_LOCAL,
            kind: FrameKind::Req,
            method: "session.setModel".to_owned(),
            payload: json!({"model": {"id": FIRST_MODEL, "provider": "openai"}}),
        })
        .await;
    second_host
        .emit(Frame {
            id: SECOND_LOCAL,
            kind: FrameKind::Req,
            method: "session.setModel".to_owned(),
            payload: json!({"model": {"id": SECOND_MODEL, "provider": "openai"}}),
        })
        .await;

    let mut expected = HashMap::new();
    for _ in 0..2 {
        let event = tokio::time::timeout(Duration::from_millis(500), bridge.recv())
            .await?
            .ok_or("session bridge closed")?;
        let SessionBridgeEvent::SetModel { id, request } = event else {
            return Err("unexpected session bridge event".into());
        };
        let model_id = request.model["id"]
            .as_str()
            .ok_or("setModel model id missing")?;
        let success = match model_id {
            FIRST_MODEL => true,
            SECOND_MODEL => false,
            other => return Err(format!("unexpected model id: {other}").into()),
        };
        expected.insert(id, success);
    }
    assert_eq!(expected.len(), 2, "routed ids must be distinct");

    for (id, success) in &expected {
        set.respond_set_model(*id, *success).await?;
    }

    for (host, local, expected_success) in [
        (&first_host, FIRST_LOCAL, true),
        (&second_host, SECOND_LOCAL, false),
    ] {
        host.wait_for_request("session.setModel").await?;
        let response = host
            .requests
            .lock()
            .ok()
            .and_then(|requests| {
                requests
                    .iter()
                    .find(|frame| {
                        frame.method == "session.setModel"
                            && frame.kind == FrameKind::Res
                            && frame.id == local
                    })
                    .cloned()
            })
            .ok_or("setModel response missing")?;
        assert_eq!(response.id, local);
        assert_eq!(response.payload["success"], expected_success);
    }

    set.shutdown_once().await;
    Ok(())
}
#[tokio::test]
async fn runtime_set_terminal_input_uses_completed_replies_under_one_deadline() -> R {
    let snapshot = json!({
        "tools": [],
        "commands": [],
        "shortcuts": [],
        "flags": [],
        "renderers": [],
        "providers": [],
        "handlers": [],
        "terminalInput": true
    });
    let (slow, slow_host) = make_runner(snapshot.clone()).await?;
    let (fast, fast_host) = make_runner(snapshot).await?;
    slow_host.drop_method("terminalInput");
    fast_host.set_response(
        "terminalInput",
        json!({"consume": false, "data": "rewritten"}),
    );
    let set = ExtensionRuntimeSet::bind(vec![
        (EndpointKind::TsCompat, slow),
        (EndpointKind::TsCompat, fast),
    ]);

    let result = set
        .terminal_input_within("original", Duration::from_millis(500))
        .await?;
    assert!(!result.consume);
    assert_eq!(result.data.as_deref(), Some("rewritten"));
    slow_host.wait_for_request("terminalInput").await?;
    fast_host.wait_for_request("terminalInput").await?;
    set.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn runtime_set_endpoint_failure_leaves_sibling_hooks_live() -> R {
    let (failed, failed_host) = make_runner(full_snapshot()).await?;
    let (live, live_host) = make_runner(full_snapshot()).await?;
    live_host.set_response(
        "message_end",
        json!({"message": assistant_text("live replacement")}),
    );
    let set = ExtensionRuntimeSet::bind(vec![
        (EndpointKind::TsCompat, failed),
        (EndpointKind::TsCompat, live),
    ]);
    let mut errors = set.subscribe_errors();
    failed_host.close().await;
    let error = next_error(&mut errors, Duration::from_millis(500)).await?;
    assert!(!error.retryable);

    let replacement = set
        .emit_message_end(assistant_text("original"))
        .await?
        .ok_or("live sibling replacement missing")?;
    assert_eq!(
        serde_json::to_value(replacement)?,
        serde_json::to_value(assistant_text("live replacement"))?
    );
    set.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn runtime_set_shutdown_event_reaches_every_endpoint_without_reap_duplication() -> R {
    let (first, first_host) = make_runner(full_snapshot()).await?;
    let (second, second_host) = make_runner(full_snapshot()).await?;
    let set = ExtensionRuntimeSet::bind(vec![
        (EndpointKind::TsCompat, first),
        (EndpointKind::TsCompat, second),
    ]);

    set.emit(AgentSessionEvent::SessionShutdown {
        reason: ShutdownReason::Quit,
        target_session_file: None,
    })
    .await?;
    set.shutdown_once().await;
    set.shutdown_once().await;

    for host in [&first_host, &second_host] {
        host.wait_for_request("session_shutdown").await?;
        let count = host.requests.lock().map_or(0, |requests| {
            requests
                .iter()
                .filter(|frame| frame.method == "session_shutdown")
                .count()
        });
        assert_eq!(count, 1);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Invariant: malformed theme.set publishes nothing, runner stays alive
// ---------------------------------------------------------------------------

#[tokio::test]
async fn theme_set_without_name_or_theme_publishes_no_ui_event() -> R {
    use crate::core::extension_host::ExtensionUiEvent;
    use tokio::sync::broadcast::error::TryRecvError;

    let (runner, host) = make_runner(json!({})).await?;
    let mut ui = runner.subscribe_ui();

    // Neither `name` nor `theme`: the pump's TryFrom rejects it and
    // publishes nothing on the UI bus.
    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "theme.set".to_owned(),
        payload: json!({"persist": true}),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(matches!(ui.try_recv(), Err(TryRecvError::Empty)));

    // The runner must still route a follow-up typed event — the malformed
    // drop must not have torn down the pump.
    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "theme.set".to_owned(),
        payload: json!({"name": "classic-light", "persist": false}),
    })
    .await;
    let event = tokio::time::timeout(Duration::from_millis(500), ui.recv()).await??;
    assert!(
        matches!(&event, ExtensionUiEvent::ThemeSet(_)),
        "expected ThemeSet after malformed drop, got {event:?}"
    );

    runner.shutdown_once().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Invariant: notify event forwards typed ExtensionNotice to UI subscribers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn notify_event_forwards_typed_to_ui_subscribers() -> R {
    use crate::core::extension_host::{ExtensionNotice, ExtensionNoticeLevel, ExtensionUiEvent};

    let (runner, host) = make_runner(json!({})).await?;
    let mut ui = runner.subscribe_ui();

    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "notify".to_owned(),
        payload: json!({"message": "hello world", "type": "info"}),
    })
    .await;

    let event = tokio::time::timeout(Duration::from_millis(500), ui.recv()).await??;
    let ExtensionUiEvent::Notify(ExtensionNotice { message, level }) = event else {
        return Err(format!("expected Notify, got {event:?}").into());
    };
    assert_eq!(message, "hello world");
    assert_eq!(level, ExtensionNoticeLevel::Info);

    // The pump must forward every severity through the conversion impl, not
    // a hardcoded level: emit warning and error and assert the forwarded
    // levels survive the seam.
    for (wire_type, expected) in [
        ("warning", ExtensionNoticeLevel::Warning),
        ("error", ExtensionNoticeLevel::Error),
    ] {
        host.emit(Frame {
            id: 0,
            kind: FrameKind::Event,
            method: "notify".to_owned(),
            payload: json!({"message": "leveled", "type": wire_type}),
        })
        .await;
        let event = tokio::time::timeout(Duration::from_millis(500), ui.recv()).await??;
        let ExtensionUiEvent::Notify(ExtensionNotice { level, .. }) = event else {
            return Err(format!("expected Notify for {wire_type}, got {event:?}").into());
        };
        assert_eq!(level, expected, "notify {wire_type} must forward its level");
    }
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn command_catalog_converts_wire_registry_to_product_entries() -> R {
    use crate::core::agent_session::CommandCatalogEntry;

    let (runner, host) = make_runner(json!({
        "commands": [{
            "name": "catalogCmd",
            "description": "does a thing",
            "sourceInfo": {
                "path": "/ext/main.ts",
                "source": "package",
                "origin": "package",
                "scope": "project"
            }
        }],
        "handlers": [],
    }))
    .await?;

    let catalog = runner.command_catalog();
    assert_eq!(
        catalog,
        vec![CommandCatalogEntry {
            name: "catalogCmd".to_owned(),
            description: "does a thing".to_owned(),
            source: None,
            source_info: Some(crate::core::resources::source_info::SourceInfo {
                path: "/ext/main.ts".to_owned(),
                source: "package".to_owned(),
                origin: crate::core::resources::source_info::SourceOrigin::Package,
                scope: crate::core::resources::source_info::SourceScope::Project,
                base_dir: None,
            }),
        }],
        "the wire registry entry must convert: name, description, and host-reported SourceInfo"
    );

    runner.shutdown_once().await;
    drop(host);
    Ok(())
}

#[tokio::test]
async fn fork_request_converts_position_and_entry_id() -> R {
    let (runner, host) = make_runner(json!({ "handlers": [] })).await?;
    let mut bridge = runner
        .take_session_bridge()
        .ok_or("session bridge receiver missing")?;

    host.emit(Frame {
        id: 51,
        kind: FrameKind::Req,
        method: pi_ext::protocol::SESSION_FORK_METHOD.to_owned(),
        payload: json!({"entryId": "e7", "position": "before"}),
    })
    .await;

    let event = tokio::time::timeout(Duration::from_millis(500), bridge.recv())
        .await?
        .ok_or("session bridge channel closed")?;
    let crate::core::extension_host::SessionBridgeEvent::Fork { id, request } = event else {
        return Err(format!("expected Fork, got {event:?}").into());
    };
    assert_eq!(id, BridgeRequestId(51));
    assert_eq!(request.entry_id, "e7");
    assert!(matches!(
        request.position,
        Some(crate::core::agent_session_runtime::ForkPosition::Before)
    ));

    runner.shutdown_once().await;
    Ok(())
}
