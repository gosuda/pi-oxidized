//! Fake-host acceptance tests for [`HostExtensionRunner`].
//!
//! Drives an in-memory JSONL fake host (no real Bun) through every
//! [`ExtensionRunner`] hook family, the full 33-event handler-presence set,
//! host-owned transforms/merge, non-retryable error/crash/timeout isolation
//! with pending-close / no-replay, registry first-wins dedup, reload
//! generation + stale-slot invalidation, the HTML renderer, and exactly-once
//! shutdown.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
#[cfg(unix)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(unix)]
use std::fmt::Write as _;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Stdio;

use pi_agent::{
    AfterToolCallContext, AgentContext, AgentMessage, AgentToolResult, BeforeToolCallContext,
    CustomAgentMessage,
};
use pi_ai::{
    AssistantContent, AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent,
    StopReason, TextContent, ToolCall, Usage, UsageCost,
};
use pi_ext::client::{HostClient, HostUiRequest, HostUiResponse};
#[cfg(unix)]
use pi_ext::host::{HostError, HostSource, HostSpec};
use pi_ext::protocol::{
    ExtensionErrorEvent, FlagValueWire, Frame, FrameKind, HelloAck, KeyEventKindWire,
    KeyModifiersWire, UiEventRequest, UiEventWire, decode_frame_str, encode_frame,
};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::super::agent_session::events::{
    AgentSessionEvent, SessionShutdownReason as ShutdownReason,
};
use super::super::agent_session::extension_runner::{ExtensionRunner, SessionHooks};
use super::super::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use super::{
    ALL_EVENT_TYPES, ExtensionMode, HostExtensionRunner, HostStartError, RegistrySnapshotWire,
    SessionBridgeEvent, ToolRenderPhase, classify_endpoint_plans, compact_assistant_meta,
    compact_message_update_event, sanitize_html,
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

    async fn emit(&self, frame: Frame) {
        let _ = self.cmd_tx.send(FakeCmd::Emit(frame)).await;
    }

    async fn close(&self) {
        let _ = self.cmd_tx.send(FakeCmd::Close).await;
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
}

/// Build a client + fake host pair and a ready runner loaded with `snapshot`.
async fn make_runner(snapshot: Value) -> Result<(Arc<HostExtensionRunner>, FakeHost), BoxErr> {
    make_runner_with_trust(snapshot, false).await
}

async fn make_runner_with_trust(
    snapshot: Value,
    project_trusted: bool,
) -> Result<(Arc<HostExtensionRunner>, FakeHost), BoxErr> {
    let (client, host) = make_fake_client(snapshot);
    let runner = HostExtensionRunner::connect_with_cwd_and_trust(
        Arc::clone(&client),
        vec![],
        "/workspace",
        project_trusted,
        FAST_TIMEOUT,
    )
    .await?;
    Ok((runner, host))
}

fn make_fake_client(snapshot: Value) -> (Arc<HostClient>, FakeHost) {
    let (client_to_host, host_read) = tokio::io::duplex(64 * 1024);
    let (host_write, client_read) = tokio::io::duplex(64 * 1024);
    let (err_write, _err_read) = tokio::io::duplex(4096);
    let client = Arc::new(HostClient::connect_boxed(
        Box::new(client_to_host),
        Box::new(client_read),
        Box::new(err_write),
        None,
    ));
    let responses = Arc::new(Mutex::new(HashMap::new()));
    let drop_methods = Arc::new(Mutex::new(HashSet::new()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    tokio::spawn(fake_host_task(
        host_read,
        host_write,
        snapshot,
        Arc::clone(&responses),
        Arc::clone(&drop_methods),
        Arc::clone(&requests),
        cmd_rx,
    ));
    (
        client,
        FakeHost {
            cmd_tx,
            responses,
            drop_methods,
            requests,
        },
    )
}

async fn make_aggregate_runner(
    endpoints: Vec<(u64, Value, &str)>,
) -> Result<(Arc<HostExtensionRunner>, Vec<FakeHost>), BoxErr> {
    let mut clients = Vec::with_capacity(endpoints.len());
    let mut hosts = Vec::with_capacity(endpoints.len());
    for (id, snapshot, path) in endpoints {
        let (client, host) = make_fake_client(snapshot);
        clients.push((id, client, vec![path.to_owned()]));
        hosts.push(host);
    }
    let runner = HostExtensionRunner::connect_test_endpoints(clients, FAST_TIMEOUT).await?;
    Ok((runner, hosts))
}

/// Three-endpoint runner whose hosts all handle the given mutable hooks.
async fn make_mutable_hook_runner(
    handlers: &[&str],
) -> Result<(Arc<HostExtensionRunner>, Vec<FakeHost>), BoxErr> {
    let snapshot = || json!({ "handlers": handlers });
    make_aggregate_runner(vec![
        (0, snapshot(), "compat-a"),
        (1, snapshot(), "lean-b"),
        (2, snapshot(), "compat-c"),
    ])
    .await
}

/// Two-endpoint runner with empty registry snapshots.
async fn make_pair_runner() -> Result<(Arc<HostExtensionRunner>, Vec<FakeHost>), BoxErr> {
    make_aggregate_runner(vec![(0, json!({}), "compat-a"), (1, json!({}), "lean-b")]).await
}

/// Last request payload a fake host recorded for `method`.
fn last_request_payload(host: &FakeHost, method: &str) -> Result<Value, BoxErr> {
    host.requests
        .lock()
        .map_err(|_| "request lock poisoned")?
        .iter()
        .rev()
        .find(|frame| frame.method == method)
        .map(|frame| frame.payload.clone())
        .ok_or_else(|| format!("{method} request missing").into())
}

/// Whether the host recorded any request for `method`.
fn host_received(host: &FakeHost, method: &str) -> Result<bool, BoxErr> {
    Ok(host
        .requests
        .lock()
        .map_err(|_| "request lock poisoned")?
        .iter()
        .any(|frame| frame.method == method))
}

/// Receive one mpsc item within 500ms or fail with `label`.
async fn recv_labeled<T>(rx: &mut mpsc::Receiver<T>, label: &str) -> Result<T, BoxErr> {
    tokio::time::timeout(Duration::from_millis(500), rx.recv())
        .await
        .map_err(|_| format!("{label} timed out"))?
        .ok_or_else(|| format!("{label} missing").into())
}

/// Extract the frame id from a select UI request.
fn select_id(request: &HostUiRequest) -> Result<u64, BoxErr> {
    let HostUiRequest::Select { id, .. } = request else {
        return Err("expected select".into());
    };
    Ok(*id)
}

/// Extract the frame id from a set-model bridge event.
fn set_model_id(event: &SessionBridgeEvent) -> Result<u64, BoxErr> {
    let SessionBridgeEvent::SetModel { id, .. } = event else {
        return Err("expected setModel".into());
    };
    Ok(*id)
}

/// Emit a colliding select + set-model request pair from every host.
async fn emit_colliding_requests(hosts: &[FakeHost]) {
    for host in hosts {
        host.emit(Frame {
            id: 7,
            kind: FrameKind::Req,
            method: "select".to_owned(),
            payload: json!({"title":"Pick","options":["a"]}),
        })
        .await;
        host.emit(Frame {
            id: 41,
            kind: FrameKind::Req,
            method: "session.setModel".to_owned(),
            payload: json!({"model":{"id":"m","provider":"p"}}),
        })
        .await;
    }
}

/// Wait until the host recorded the local responses for the colliding ids.
async fn wait_for_local_responses(host: &FakeHost) -> R {
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            let complete = host.requests.lock().is_ok_and(|requests| {
                requests.iter().any(|frame| {
                    frame.kind == FrameKind::Res && frame.method == "select" && frame.id == 7
                }) && requests.iter().any(|frame| {
                    frame.kind == FrameKind::Res
                        && frame.method == "session.setModel"
                        && frame.id == 41
                })
            });
            if complete {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| "local response routing timed out")?;
    Ok(())
}

/// The endpoint-namespaced aggregate keys for one colliding local slot key.
fn namespaced_same_keys() -> HashSet<String> {
    HashSet::from([
        "endpoint-0:same-key".to_owned(),
        "endpoint-1:same-key".to_owned(),
    ])
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
    if req.kind != FrameKind::Req {
        return None;
    }
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

/// Snapshot carrying every registered surface + all 33 handler types.
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
// Load + 33-event handler presence + trait hook families
// ===========================================================================

#[tokio::test]
async fn load_reports_all_33_handlers_and_registry_surfaces() -> R {
    let (runner, _host) = make_runner(full_snapshot()).await?;

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

#[tokio::test]
async fn hook_tool_call_maps_block_and_mutated_input() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    host.set_response(
        "tool_call",
        json!({
            "block": true,
            "reason": "denied by policy",
            "input": {"path": "rewritten"}
        }),
    );
    let result = runner.emit_tool_call("read", "tc1", Map::new()).await?;
    let mapped = result.map(|r| (r.block, r.reason, r.arguments));
    assert_eq!(
        mapped,
        Some((
            true,
            Some("denied by policy".to_owned()),
            Some(Map::from_iter([(
                "path".to_owned(),
                Value::String("rewritten".to_owned())
            )]))
        ))
    );
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

#[test]
fn registry_snapshot_rejects_non_cli_flag_values() {
    for snapshot in [
        json!({"flags": [{"name": "bad", "type": "boolean", "default": 1}]}),
        json!({"flags": [{"name": "bad", "type": "boolean", "value": 1}]}),
    ] {
        let result = serde_json::from_value::<RegistrySnapshotWire>(snapshot);
        assert!(result.is_err(), "numeric flag values must be rejected");
    }
}

#[tokio::test]
async fn flags_set_acks_before_updating_local_values_and_rejection_preserves_state() -> R {
    let snapshot = json!({
        "flags": [{
            "name": "verbose",
            "type": "boolean",
            "default": false,
            "extensionPath": "/ext/a.ts"
        }]
    });
    let (runner, host) = make_runner(snapshot).await?;
    assert_eq!(
        runner.get_flag_values().get("verbose"),
        Some(&Value::Bool(false)),
        "boolean defaults must survive the registry snapshot"
    );
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
async fn aggregate_flag_rejection_does_not_skip_later_endpoints() -> R {
    let (runner, hosts) = make_aggregate_runner(vec![
        (
            0,
            json!({
                "flags": [{"name": "first", "type": "boolean", "value": false}]
            }),
            "first",
        ),
        (
            1,
            json!({
                "flags": [{"name": "second", "type": "boolean", "value": false}]
            }),
            "second",
        ),
    ])
    .await?;
    hosts[0].set_response(pi_ext::protocol::FLAGS_SET_METHOD, json!({"ok": false}));
    hosts[1].set_response(pi_ext::protocol::FLAGS_SET_METHOD, json!({"ok": true}));

    runner
        .apply_flag_values(&BTreeMap::from([
            ("first".to_owned(), FlagValueWire::Boolean(true)),
            ("second".to_owned(), FlagValueWire::Boolean(true)),
        ]))
        .await?;

    let values = runner.get_flag_values();
    assert_eq!(values.get("first"), Some(&Value::Bool(false)));
    assert_eq!(values.get("second"), Some(&Value::Bool(true)));
    let later_requests = hosts[1]
        .requests
        .lock()
        .map_err(|_| "request lock poisoned")?;
    assert!(later_requests.iter().any(|request| {
        request.method == pi_ext::protocol::FLAGS_SET_METHOD
            && request.payload == json!({"values": {"second": true}})
    }));
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
// Ordered multi-endpoint aggregation
// ===========================================================================

#[test]
fn classified_runs_preserve_compat_lean_compat_interleaving() -> R {
    let temp = tempfile::tempdir()?;
    let compat_a = temp.path().join("compat-a");
    let lean_b = temp.path().join("lean-b");
    let compat_c = temp.path().join("compat-c");
    for directory in [&compat_a, &lean_b, &compat_c] {
        std::fs::create_dir_all(directory)?;
        std::fs::write(directory.join("index.ts"), "export default {}")?;
    }
    std::fs::write(
        lean_b.join("pi-extension.json"),
        format!(
            r#"{{"$schema":"pi.extension.v1","name":"lean-b","version":"1.0.0","runtime":"ts-lean","entry":"index.ts","protocolVersion":{}}}"#,
            pi_ext::protocol::PROTOCOL_VERSION
        ),
    )?;
    let (plans, errors) = classify_endpoint_plans(vec![
        compat_a.to_string_lossy().into_owned(),
        lean_b.to_string_lossy().into_owned(),
        compat_c.to_string_lossy().into_owned(),
    ]);
    assert!(
        errors.is_empty(),
        "unexpected classification errors: {errors:?}"
    );
    assert_eq!(plans.len(), 3);
    assert_eq!(
        plans.iter().map(|plan| plan.mode).collect::<Vec<_>>(),
        vec![
            ExtensionMode::Compat,
            ExtensionMode::Lean,
            ExtensionMode::Compat
        ]
    );
    assert_eq!(
        plans.iter().map(|plan| plan.id).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn native_only_startup_skips_bun_and_isolates_run_failure() -> R {
    let temp = tempfile::tempdir()?;
    let good = temp.path().join("native-good");
    let bad = temp.path().join("native-bad");
    std::fs::create_dir_all(&good)?;
    std::fs::create_dir_all(&bad)?;
    let script = good.join("native-host.sh");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"hello"'*)
      printf '%s\n' '{{"id":1,"kind":"res","method":"hello","payload":{{"protocolVersion":{},"compatibilityVersion":"native"}}}}'
      ;;
    *'"method":"extensions.load"'*)
      printf '%s\n' '{{"id":2,"kind":"res","method":"extensions.load","payload":{{}}}}'
      ;;
    *'"method":"shutdown"'*) exit 0 ;;
  esac
done
"#,
            pi_ext::protocol::PROTOCOL_VERSION
        ),
    )?;
    let mut permissions = std::fs::metadata(&script)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&script, permissions)?;
    let bad_entry = bad.join("native-host.sh");
    std::fs::write(&bad_entry, "not executable")?;
    for (directory, name) in [(&good, "good"), (&bad, "bad")] {
        std::fs::write(
            directory.join("pi-extension.json"),
            format!(
                r#"{{"$schema":"pi.extension.v1","name":"{name}","version":"1.0.0","runtime":"native","entry":"native-host.sh","protocolVersion":{}}}"#,
                pi_ext::protocol::PROTOCOL_VERSION
            ),
        )?;
    }
    let bun_resolutions = Arc::new(AtomicUsize::new(0));
    let runner = HostExtensionRunner::start_classified_with_resolver(
        vec![
            good.to_string_lossy().into_owned(),
            bad.to_string_lossy().into_owned(),
        ],
        "/workspace".to_owned(),
        false,
        {
            let bun_resolutions = Arc::clone(&bun_resolutions);
            move || {
                bun_resolutions.fetch_add(1, Ordering::Relaxed);
                Err(HostError::NotAFile(PathBuf::from("bun-must-not-resolve")))
            }
        },
    )
    .await?;
    assert_eq!(
        bun_resolutions.load(Ordering::Relaxed),
        0,
        "native-only startup must not resolve Bun"
    );
    assert!(runner.is_active(), "good native sibling must remain active");
    let errors = runner.load_errors();
    assert_eq!(errors.len(), 1, "unexpected run diagnostics: {errors:?}");
    assert!(errors[0].0.contains("native-bad"));
    assert!(errors[0].1.contains("Permission denied"));
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn aggregate_registry_and_command_collisions_are_first_wins() -> R {
    let (runner, hosts) = make_aggregate_runner(vec![
        (
            0,
            json!({
                "commands":[{"name":"same"}],
                "shortcuts":[{"key":"ctrl+x"}],
                "tools":[{"name":"sameTool","label":"Same","description":"d","parameters":{}}],
                "handlers":[]
            }),
            "compat-a",
        ),
        (
            1,
            json!({
                "commands":[{"name":"same"},{"name":"only-b"}],
                "shortcuts":[{"key":"ctrl+x"}],
                "tools":[{"name":"sameTool","label":"Same B","description":"d","parameters":{}}],
                "handlers":[]
            }),
            "lean-b",
        ),
    ])
    .await?;
    hosts[0].set_response("command.execute", json!({"ok": true}));
    hosts[1].set_response("command.execute", json!({"ok": true}));
    hosts[1].set_response(
        pi_ext::protocol::SHORTCUT_EXECUTE_METHOD,
        json!({"handled": true}),
    );
    hosts[1].set_response("tool.renderHtml", json!({"html": "wrong owner"}));
    assert_eq!(runner.get_registered_commands(), vec!["same", "only-b"]);
    let collision_errors = runner
        .load_errors()
        .into_iter()
        .filter(|(_, message)| message.contains("duplicate command \"same\""))
        .collect::<Vec<_>>();
    assert_eq!(collision_errors.len(), 1);
    assert!(runner.execute_command("same", "args").await?);
    hosts[0].wait_for_request("command.execute").await?;
    assert!(
        !hosts[1]
            .requests
            .lock()
            .map_err(|_| "request lock poisoned")?
            .iter()
            .any(|frame| frame.method == "command.execute")
    );
    let mut errors = runner.subscribe_errors();
    hosts[0].close().await;
    let closed = next_error(&mut errors, Duration::from_millis(500)).await?;
    assert_eq!(closed.code, "extension_closed");
    assert!(
        !runner.execute_command("same", "after-crash").await?,
        "a command owned by a dead first endpoint must not be swallowed"
    );
    assert!(matches!(
        runner.execute_shortcut("ctrl+x").await,
        Err(pi_ext::client::HostClientError::NotRunning)
    ));
    assert!(
        runner
            .render_extension_tool_html(ToolRenderPhase::Result, "sameTool", &json!({}))
            .await
            .is_none()
    );
    assert!(
        !hosts[1]
            .requests
            .lock()
            .map_err(|_| "request lock poisoned")?
            .iter()
            .any(|frame| {
                matches!(
                    frame.method.as_str(),
                    "command.execute" | "shortcut.execute" | "tool.renderHtml"
                )
            }),
        "a lower-precedence duplicate must not receive command, shortcut, or render requests"
    );
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn aggregate_message_end_threads_replacements_in_order() -> R {
    let (runner, hosts) = make_mutable_hook_runner(&["message_end"]).await?;
    hosts[0].set_response("message_end", json!({"message": assistant_text("A")}));
    hosts[1].set_response("message_end", json!({"message": assistant_text("B")}));
    hosts[2].set_response("message_end", Value::Null);
    let message = runner
        .emit_message_end(assistant_text("raw"))
        .await?
        .ok_or("message_end should replace")?;
    assert_eq!(message, assistant_text("B"));
    assert_eq!(
        last_request_payload(&hosts[1], "message_end")?,
        serde_json::to_value(assistant_text("A"))?
    );
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn aggregate_tool_call_threads_inputs_and_short_circuits_on_block() -> R {
    let (runner, hosts) = make_mutable_hook_runner(&["tool_call"]).await?;
    hosts[0].set_response("tool_call", json!({"input":{"stage":"A"}}));
    hosts[1].set_response("tool_call", json!({"input":{"stage":"B"}}));
    hosts[2].set_response("tool_call", json!({"block":true,"reason":"stop"}));
    let tool_call = runner
        .emit_tool_call("read", "tc", Map::new())
        .await?
        .ok_or("tool_call result missing")?;
    assert!(tool_call.block);
    assert_eq!(tool_call.reason.as_deref(), Some("stop"));
    assert_eq!(
        tool_call
            .arguments
            .as_ref()
            .and_then(|args| args.get("stage")),
        Some(&json!("B"))
    );
    for (index, expected) in [(1, "A"), (2, "B")] {
        assert_eq!(
            last_request_payload(&hosts[index], "tool_call")?["input"]["stage"],
            expected
        );
    }
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn aggregate_tool_call_block_without_reason_does_not_inherit_earlier_reason() -> R {
    let (runner, hosts) = make_mutable_hook_runner(&["tool_call"]).await?;
    hosts[0].set_response("tool_call", json!({"reason":"early"}));
    hosts[1].set_response("tool_call", json!({}));
    hosts[2].set_response("tool_call", json!({"block":true}));
    let tool_call = runner
        .emit_tool_call("read", "tc", Map::new())
        .await?
        .ok_or("tool_call result missing")?;
    assert!(tool_call.block);
    assert_eq!(
        tool_call.reason, None,
        "a blocking endpoint without a reason must not inherit an earlier endpoint's reason"
    );
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn aggregate_tool_result_threads_content_details_and_error_flag() -> R {
    let (runner, hosts) = make_mutable_hook_runner(&["tool_result"]).await?;
    hosts[0].set_response(
        "tool_result",
        json!({"content":[{"type":"text","text":"A"}]}),
    );
    hosts[1].set_response(
        "tool_result",
        json!({"details":{"stage":"B"},"isError":true}),
    );
    hosts[2].set_response("tool_result", json!({"terminate":true}));
    let tool_result = runner
        .emit_tool_result("read", "tc", Map::new(), Vec::new(), Value::Null, false)
        .await?
        .ok_or("tool_result missing")?;
    assert_eq!(
        serde_json::to_value(tool_result.content)?,
        json!([{"type":"text","text":"A"}])
    );
    assert_eq!(tool_result.details, Some(json!({"stage":"B"})));
    assert_eq!(tool_result.is_error, Some(true));
    assert_eq!(tool_result.terminate, Some(true));
    assert_eq!(
        last_request_payload(&hosts[1], "tool_result")?["content"],
        json!([{"type":"text","text":"A"}])
    );
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn aggregate_input_transform_threads_and_short_circuits_on_handled() -> R {
    let (runner, hosts) = make_mutable_hook_runner(&["input"]).await?;
    hosts[0].set_response(
        "input",
        json!({"action":"transform","text":"A","images":{"stage":"A"}}),
    );
    hosts[1].set_response("input", json!({"action":"handled"}));
    hosts[2].set_response("input", json!({"action":"transform","text":"must-not-run"}));
    let input = runner.emit_input("raw", None, "user", None).await?;
    assert!(input.handled);
    assert_eq!(input.text.as_deref(), Some("A"));
    assert_eq!(input.images, Some(json!({"stage":"A"})));
    let b_input = last_request_payload(&hosts[1], "input")?;
    assert_eq!(b_input["text"], "A");
    assert_eq!(b_input["images"], json!({"stage":"A"}));
    assert!(
        !host_received(&hosts[2], "input")?,
        "a handled input transform must short-circuit later endpoints"
    );
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn aggregate_before_agent_start_threads_prompt_and_messages() -> R {
    let (runner, hosts) = make_mutable_hook_runner(&["before_agent_start"]).await?;
    hosts[0].set_response(
        "before_agent_start",
        json!({
            "systemPrompt":"system-A",
            "messages":[assistant_text("injected-A")]
        }),
    );
    hosts[1].set_response("before_agent_start", json!({"systemPrompt":"system-B"}));
    hosts[2].set_response("before_agent_start", Value::Null);
    let before = runner
        .emit_before_agent_start("prompt", None)
        .await?
        .ok_or("before_agent_start missing")?;
    assert_eq!(before.system_prompt.as_deref(), Some("system-B"));
    assert_eq!(before.messages, vec![assistant_text("injected-A")]);
    let b_before = last_request_payload(&hosts[1], "before_agent_start")?;
    assert_eq!(b_before["systemPrompt"], "system-A");
    assert_eq!(b_before["messages"], json!([assistant_text("injected-A")]));
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn aggregate_before_agent_start_accumulates_messages_across_endpoints() -> R {
    let (runner, hosts) = make_mutable_hook_runner(&["before_agent_start"]).await?;
    hosts[0].set_response(
        "before_agent_start",
        json!({"messages":[assistant_text("injected-A")]}),
    );
    hosts[1].set_response(
        "before_agent_start",
        json!({"messages":[assistant_text("injected-B")]}),
    );
    hosts[2].set_response("before_agent_start", json!({"systemPrompt":"sys"}));
    let before = runner
        .emit_before_agent_start("prompt", None)
        .await?
        .ok_or("before_agent_start missing")?;
    assert_eq!(
        before.messages,
        vec![assistant_text("injected-A"), assistant_text("injected-B")],
        "injected messages from every endpoint accumulate in endpoint order"
    );
    assert_eq!(before.system_prompt.as_deref(), Some("sys"));
    let c_before = last_request_payload(&hosts[2], "before_agent_start")?;
    assert_eq!(c_before["prompt"], "prompt");
    assert_eq!(
        c_before["messages"],
        json!([assistant_text("injected-A"), assistant_text("injected-B")]),
        "later endpoints observe the accumulated message list"
    );
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn aggregate_colliding_ui_and_session_ids_route_to_owning_clients() -> R {
    let (runner, hosts) = make_pair_runner().await?;
    let mut ui = runner.take_ui_requests().ok_or("ui claim failed")?;
    let mut bridge = runner.take_session_bridge().ok_or("bridge claim failed")?;
    emit_colliding_requests(&hosts).await;
    let first_ui_id = select_id(&recv_labeled(&mut ui, "first ui request").await?)?;
    let second_ui_id = select_id(&recv_labeled(&mut ui, "second ui request").await?)?;
    assert_ne!(first_ui_id, second_ui_id);
    runner
        .respond_ui(HostUiResponse::Select {
            id: first_ui_id,
            value: Some("a".to_owned()),
        })
        .await?;
    runner
        .respond_ui(HostUiResponse::Select {
            id: second_ui_id,
            value: None,
        })
        .await?;

    let first_session_id = set_model_id(&recv_labeled(&mut bridge, "first setModel").await?)?;
    let second_session_id = set_model_id(&recv_labeled(&mut bridge, "second setModel").await?)?;
    assert_ne!(first_session_id, second_session_id);
    runner.respond_set_model(first_session_id, true).await?;
    runner.respond_set_model(second_session_id, false).await?;

    for host in &hosts {
        wait_for_local_responses(host).await?;
    }
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn aggregate_colliding_slot_keys_namespace_and_route_to_owners() -> R {
    let (runner, hosts) = make_pair_runner().await?;
    hosts[0].emit(ui_slot_frame("same-key", 1, "A")).await;
    hosts[1].emit(ui_slot_frame("same-key", 1, "B")).await;
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if runner.current_slots().len() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| format!("colliding slots timed out: {:?}", runner.slot_keys()))?;
    assert_eq!(
        runner.slot_keys().into_iter().collect::<HashSet<_>>(),
        namespaced_same_keys()
    );
    for (index, key) in [(0, "endpoint-0:same-key"), (1, "endpoint-1:same-key")] {
        hosts[index].set_response("uiEvent", json!({"delivered": true}));
        assert!(
            runner
                .send_ui_event(UiEventRequest {
                    key: key.to_owned(),
                    generation: 1,
                    event: UiEventWire::Key {
                        code: "x".to_owned(),
                        modifiers: KeyModifiersWire::default(),
                        kind: KeyEventKindWire::Press,
                    },
                    data: None,
                })
                .await?
                .delivered
        );
        hosts[index].wait_for_request("uiEvent").await?;
        let request = hosts[index]
            .requests
            .lock()
            .map_err(|_| "request lock poisoned")?
            .iter()
            .find(|frame| frame.method == "uiEvent")
            .cloned()
            .ok_or("routed uiEvent missing")?;
        assert_eq!(request.payload["key"], "same-key");
    }
    hosts[0].emit(ui_slot_frame("same-key", 2, "A2")).await;
    hosts[1].emit(ui_slot_frame("same-key", 2, "B2")).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        runner.slot_keys().into_iter().collect::<HashSet<_>>(),
        namespaced_same_keys()
    );
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn aggregate_stale_session_route_fails_after_reload() -> R {
    let (runner, hosts) = make_pair_runner().await?;
    let mut bridge = runner.take_session_bridge().ok_or("bridge claim failed")?;
    hosts[0]
        .emit(Frame {
            id: 42,
            kind: FrameKind::Req,
            method: "session.setModel".to_owned(),
            payload: json!({"model":{"id":"stale","provider":"p"}}),
        })
        .await;
    let stale_id = set_model_id(&recv_labeled(&mut bridge, "stale setModel").await?)?;
    assert_eq!(runner.reload().await, 2);
    assert!(matches!(
        runner.respond_set_model(stale_id, true).await,
        Err(pi_ext::client::HostClientError::NotRunning)
    ));
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn aggregate_host_crash_disables_only_that_endpoint() -> R {
    let snapshot = json!({"handlers":["message_end"]});
    let (runner, hosts) = make_aggregate_runner(vec![
        (0, snapshot.clone(), "compat-a"),
        (1, snapshot, "lean-b"),
    ])
    .await?;
    hosts[1].set_response("message_end", json!({"message":assistant_text("survivor")}));
    let mut errors = runner.subscribe_errors();
    hosts[0].close().await;
    let closed = next_error(&mut errors, Duration::from_millis(500)).await?;
    assert_eq!(closed.code, "extension_closed");
    assert!(runner.is_active());
    let result = runner
        .emit_message_end(assistant_text("raw"))
        .await?
        .ok_or("surviving endpoint did not run")?;
    assert_eq!(result, assistant_text("survivor"));
    hosts[1].wait_for_request("message_end").await?;
    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn one_endpoint_keeps_local_frame_ids_and_slot_keys() -> R {
    let (runner, host) = make_runner(json!({})).await?;
    let mut bridge = runner.take_session_bridge().ok_or("bridge claim failed")?;
    host.emit(Frame {
        id: 41,
        kind: FrameKind::Req,
        method: "session.setModel".to_owned(),
        payload: json!({"model":{"id":"m","provider":"p"}}),
    })
    .await;
    let event = tokio::time::timeout(Duration::from_millis(500), bridge.recv())
        .await?
        .ok_or("setModel missing")?;
    let SessionBridgeEvent::SetModel { id, .. } = event else {
        return Err("expected setModel".into());
    };
    assert_eq!(id, 41);
    host.emit(ui_slot_frame("plain-key", 1, "slot")).await;
    tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if runner.slot_keys().iter().any(|key| key == "plain-key") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert_eq!(runner.slot_keys(), vec!["plain-key"]);
    runner.respond_set_model(id, true).await?;
    runner.shutdown_once().await;
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

#[tokio::test]
async fn stale_dispose_after_invalidate_does_not_duplicate_disposal() -> R {
    use crate::core::extension_host::ExtensionUiEvent;

    let (runner, host) = make_runner(full_snapshot()).await?;
    let mut ui = runner.subscribe_ui();

    host.emit(ui_slot_frame("w", 1, "live")).await;
    let first = tokio::time::timeout(Duration::from_millis(500), ui.recv()).await??;
    assert!(
        matches!(&first, ExtensionUiEvent::Slot(slot) if slot.key == "w"),
        "expected live slot first: {first:?}"
    );

    runner.invalidate();
    // A dispose frame buffered before teardown arrives after it; the stale
    // gate must drop it instead of emitting a second Dispose.
    host.emit(Frame {
        id: 0,
        kind: FrameKind::Event,
        method: "disposeSlot".to_owned(),
        payload: json!({"key":"w","generation":1}),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut disposals = 0;
    let mut unexpected = Vec::new();
    while let Ok(event) = ui.try_recv() {
        match event {
            ExtensionUiEvent::Dispose { key } => {
                assert_eq!(key, "w");
                disposals += 1;
            }
            other => unexpected.push(format!("{other:?}")),
        }
    }
    assert_eq!(disposals, 1, "stale dispose must not duplicate disposal");
    assert!(
        unexpected.is_empty(),
        "no event may follow the teardown dispose: {unexpected:?}"
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
// Invalidate + shutdown exactly once
// ===========================================================================

#[cfg(unix)]
#[derive(Clone, Copy)]
enum StartupBehavior {
    RejectHandshake,
    RejectLoad,
    Ready,
    /// Answer hello + load, then exit immediately (stdout EOF mid-session).
    ExitAfterLoad,
}

#[cfg(unix)]
fn write_startup_host(
    directory: &Path,
    behavior: StartupBehavior,
) -> Result<(HostSpec, PathBuf, PathBuf), BoxErr> {
    let script_path = directory.join("startup-host");
    let pid_path = directory.join("host.pid");
    let shutdown_path = directory.join("shutdown");
    let hello_payload = match behavior {
        StartupBehavior::RejectHandshake => {
            r#"{"protocolVersion":999,"compatibilityVersion":"rejected"}"#
        }
        StartupBehavior::RejectLoad | StartupBehavior::Ready | StartupBehavior::ExitAfterLoad => {
            r#"{"protocolVersion":1,"compatibilityVersion":"0.80.10"}"#
        }
    };
    let mut script = String::from(
        "#!/bin/sh\n\
         pid_file=\"$1\"\n\
         shutdown_file=\"$2\"\n\
         printf '%s\\n' \"$$\" > \"$pid_file\"\n\
         IFS= read -r request || exit 10\n",
    );
    writeln!(
        script,
        "printf '%s\\n' '{{\"id\":1,\"kind\":\"res\",\"method\":\"hello\",\"payload\":{hello_payload}}}'"
    )?;
    match behavior {
        StartupBehavior::RejectHandshake => {}
        StartupBehavior::RejectLoad => script.push_str(
            "IFS= read -r request || exit 11\n\
             printf '%s\\n' '{\"id\":2,\"kind\":\"res\",\"method\":\"extensions.load\",\"payload\":null}'\n",
        ),
        StartupBehavior::Ready => script.push_str(
            "IFS= read -r request || exit 11\n\
             printf '%s\\n' '{\"id\":2,\"kind\":\"res\",\"method\":\"extensions.load\",\"payload\":{\"handlers\":[\"session_shutdown\"]}}'\n",
        ),
        StartupBehavior::ExitAfterLoad => {
            script.push_str(
                "IFS= read -r request || exit 11\n\
                 printf '%s\\n' '{\"id\":2,\"kind\":\"res\",\"method\":\"extensions.load\",\"payload\":{\"handlers\":[\"session_shutdown\"]}}'\n\
                 exit 0\n",
            );
        }
    }
    script.push_str(
        "while IFS= read -r request; do\n\
           case \"$request\" in\n\
             *'\"method\":\"session_shutdown\"'*)\n\
               printf '%s\\n' \"$request\" >> \"$shutdown_file\"\n\
               printf '%s\\n' '{\"id\":3,\"kind\":\"res\",\"method\":\"session_shutdown\",\"payload\":{}}'\n\
               ;;\n\
           esac\n\
         done\n\
         printf '%s\\n' shutdown >> \"$shutdown_file\"\n",
    );
    fs::write(&script_path, script)?;
    let spec = HostSpec {
        source: HostSource::Env(script_path.clone()),
        program: PathBuf::from("/bin/sh"),
        args: vec![
            script_path.to_string_lossy().into_owned(),
            pid_path.to_string_lossy().into_owned(),
            shutdown_path.to_string_lossy().into_owned(),
        ],
    };
    Ok((spec, pid_path, shutdown_path))
}

#[cfg(unix)]
fn assert_host_shutdown_and_reaped(pid_path: &Path, shutdown_path: &Path) -> R {
    assert_eq!(fs::read_to_string(shutdown_path)?, "shutdown\n");
    assert_host_reaped(pid_path)
}

#[cfg(unix)]
fn assert_host_reaped(pid_path: &Path) -> R {
    let pid = fs::read_to_string(pid_path)?;
    let status = std::process::Command::new("/bin/kill")
        .args(["-0", pid.trim()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    assert!(!status.success(), "host process {pid:?} was not reaped");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn failed_handshake_startup_shuts_down_and_reaps() -> R {
    let directory = tempfile::tempdir()?;
    let (spec, pid_path, shutdown_path) =
        write_startup_host(directory.path(), StartupBehavior::RejectHandshake)?;
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
    assert_host_shutdown_and_reaped(&pid_path, &shutdown_path)
}

#[cfg(unix)]
#[tokio::test]
async fn failed_load_startup_shuts_down_and_reaps() -> R {
    let directory = tempfile::tempdir()?;
    let (spec, pid_path, shutdown_path) =
        write_startup_host(directory.path(), StartupBehavior::RejectLoad)?;
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
    assert_host_shutdown_and_reaped(&pid_path, &shutdown_path)
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_process_shutdown_reaps_and_remains_idempotent() -> R {
    let directory = tempfile::tempdir()?;
    let (spec, pid_path, shutdown_path) =
        write_startup_host(directory.path(), StartupBehavior::Ready)?;
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
    assert_host_shutdown_and_reaped(&pid_path, &shutdown_path)
}

#[cfg(unix)]
#[tokio::test]
async fn pump_eof_shuts_down_and_reaps_exactly_once() -> R {
    let directory = tempfile::tempdir()?;
    let (spec, pid_path, shutdown_path) =
        write_startup_host(directory.path(), StartupBehavior::ExitAfterLoad)?;
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
    // Reap is asynchronous relative to the error broadcast; poll kill -0.
    let pid = fs::read_to_string(&pid_path)?.trim().to_owned();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let alive = std::process::Command::new("/bin/kill")
                .args(["-0", &pid])
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
    .await?;

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
fn compact_message_updates_match_full_metadata_without_content() -> R {
    let mut partial = AssistantMessage::new("test-api", "test-provider", "m", 1);
    partial
        .content
        .push(AssistantContent::Text(TextContent::new("hello")));
    partial.response_model = Some("response-model".to_owned());
    partial.response_id = Some("response-id".to_owned());
    partial.diagnostics = Some(vec![AssistantMessageDiagnostic {
        kind: "retry".to_owned(),
        timestamp: 2,
        error: None,
        details: Some(Map::from_iter([("attempt".to_owned(), json!(1))])),
    }]);
    partial.usage = Usage {
        input: 3,
        output: 4,
        cache_read: 5,
        cache_write: 6,
        cache_write1h: Some(7),
        reasoning: Some(8),
        total_tokens: 9,
        cost: UsageCost {
            input: 1.0,
            output: 2.0,
            cache_read: 3.0,
            cache_write: 4.0,
            total: 10.0,
        },
    };
    partial.stop_reason = StopReason::Length;
    partial.error_message = Some("diagnostic".to_owned());

    let mut expected = serde_json::to_value(&partial)?;
    expected
        .as_object_mut()
        .ok_or("assistant message must serialize as an object")?
        .remove("content");
    assert_eq!(compact_assistant_meta(&partial), expected);

    let delta = compact_message_update_event(&AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "lo".to_owned(),
        partial: partial.clone(),
    });
    assert_eq!(delta["type"], "text_delta");
    assert_eq!(delta["delta"], "lo");
    assert!(delta.get("partial").is_none());
    assert!(delta.get("block").is_none());
    assert_eq!(delta["meta"], expected);

    let end = compact_message_update_event(&AssistantMessageEvent::TextEnd {
        content_index: 0,
        content: "hello".to_owned(),
        partial,
    });
    assert_eq!(end["block"]["type"], "text");
    assert_eq!(end["block"]["text"], "hello");
    assert_eq!(end["meta"], expected);
    Ok(())
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
    let (spec, pid_path, shutdown_path) =
        write_startup_host(directory.path(), StartupBehavior::Ready)?;
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
    assert_host_reaped(&pid_path)
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

/// RAII guard to ensure the child process and tasks are killed/aborted on exit.
struct HostGuard {
    child: tokio::process::Child,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl HostGuard {
    async fn teardown(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        for task in self.tasks.drain(..) {
            task.abort();
            let _ = task.await;
        }
    }
}

/// In-memory frame collector driven by a channel.
struct FrameCollector {
    rx: tokio::sync::mpsc::UnboundedReceiver<Frame>,
    pending: Vec<Frame>,
}

impl FrameCollector {
    fn new(stdout: tokio::process::ChildStdout) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(frame) = decode_frame_str(&line)
                    && tx.send(frame).is_err()
                {
                    break;
                }
            }
        });
        (
            Self {
                rx,
                pending: Vec::new(),
            },
            task,
        )
    }

    async fn await_frame_timeout(
        &mut self,
        predicate: impl Fn(&Frame) -> bool,
        timeout: Duration,
    ) -> std::result::Result<Frame, BoxErr> {
        if let Some(idx) = self.pending.iter().position(&predicate) {
            return Ok(self.pending.remove(idx));
        }
        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);

        loop {
            tokio::select! {
                () = &mut sleep => return Err("frame waiter timed out".into()),
                opt = self.rx.recv() => match opt {
                    Some(frame) => {
                        if predicate(&frame) {
                            return Ok(frame);
                        }
                        self.pending.push(frame);
                    }
                    None => return Err("frame stream closed".into()),
                }
            }
        }
    }
}

/// Write one frame and read lines until a matcher returns Some.
async fn round_trip(
    child_stdin: &mut tokio::process::ChildStdin,
    collector: &mut FrameCollector,
    request: &Frame,
) -> std::result::Result<Frame, BoxErr> {
    let bytes = encode_frame(request)?;
    child_stdin.write_all(&bytes).await?;
    child_stdin.flush().await?;
    collector
        .await_frame_timeout(
            |f| f.id == request.id && matches!(f.kind, FrameKind::Res | FrameKind::Error),
            Duration::from_secs(5),
        )
        .await
}

/// Phase 1: hello handshake — protocol + compatibility version round-trip.
async fn assert_hello_handshake(
    stdin: &mut tokio::process::ChildStdin,
    collector: &mut FrameCollector,
) -> R {
    let req = Frame {
        id: 1,
        kind: FrameKind::Req,
        method: "hello".to_owned(),
        payload: json!({
            "protocolVersion": pi_ext::protocol::PROTOCOL_VERSION,
            "compatibilityVersion": pi_ext::protocol::COMPATIBILITY_VERSION,
        }),
    };
    let resp = round_trip(stdin, collector, &req).await?;
    assert_eq!(resp.kind, FrameKind::Res);
    assert_eq!(resp.method, "hello");
    let payload = resp.payload.as_object().ok_or("hello payload not object")?;
    assert_eq!(
        payload.get("protocolVersion").and_then(Value::as_u64),
        Some(u64::from(pi_ext::protocol::PROTOCOL_VERSION))
    );
    assert_eq!(
        payload.get("compatibilityVersion").and_then(Value::as_str),
        Some(pi_ext::protocol::COMPATIBILITY_VERSION)
    );
    Ok(())
}

/// Phase 2: extensions.load — hostile + tool fixtures register and report errors.
async fn assert_extensions_load(
    stdin: &mut tokio::process::ChildStdin,
    collector: &mut FrameCollector,
    workspace_root: &std::path::Path,
) -> R {
    let fixtures_root = workspace_root
        .join("packages")
        .join("extension-host")
        .join("fixtures")
        .join("extensions");
    let req = Frame {
        id: 2,
        kind: FrameKind::Req,
        method: "extensions.load".to_owned(),
        payload: json!({
            "extensionPaths": [
                fixtures_root.join("hostile.ts").to_string_lossy(),
                fixtures_root.join("all-events.ts").to_string_lossy(),
                fixtures_root.join("hooks.ts").to_string_lossy(),
                fixtures_root.join("tool.ts").to_string_lossy(),
            ],
            "cwd": workspace_root.to_string_lossy(),
        }),
    };
    let resp = round_trip(stdin, collector, &req).await?;
    assert_eq!(resp.kind, FrameKind::Res);
    assert_eq!(resp.method, "extensions.load");
    let payload = resp.payload.as_object().ok_or("load payload not object")?;
    let extensions_loaded = payload
        .get("extensions")
        .and_then(Value::as_u64)
        .ok_or("extensions.load payload missing `extensions: number`")?;
    let load_errors = payload
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
    Ok(())
}

/// Phases 3–5: `session_start` uiSlot push, sanitized render, integer measure.
async fn assert_ui_slot_lifecycle(
    stdin: &mut tokio::process::ChildStdin,
    collector: &mut FrameCollector,
) -> R {
    let session_req = Frame {
        id: 3,
        kind: FrameKind::Req,
        method: "session_start".to_owned(),
        payload: json!({"type": "session_start", "reason": "startup"}),
    };
    let resp = round_trip(stdin, collector, &session_req).await?;
    assert_eq!(resp.kind, FrameKind::Res);
    assert_eq!(resp.method, "session_start");

    let slot = collector
        .await_frame_timeout(
            |f| {
                f.method == "uiSlot"
                    && f.payload.get("key").and_then(Value::as_str) == Some("widget.hostile")
            },
            Duration::from_secs(3),
        )
        .await?;
    let slot_payload = slot
        .payload
        .as_object()
        .ok_or("uiSlot payload not object")?;
    assert_eq!(
        slot_payload.get("placement").and_then(Value::as_str),
        Some("aboveEditor")
    );
    assert!(
        slot_payload
            .get("generation")
            .and_then(Value::as_u64)
            .is_some()
    );
    let runs = slot_payload
        .get("runs")
        .and_then(Value::as_array)
        .ok_or("runs missing")?;
    assert!(
        !runs.is_empty(),
        "hostile widget must push at least one sanitized run"
    );

    let render_req = Frame {
        id: 4,
        kind: FrameKind::Req,
        method: "render".to_owned(),
        payload: json!({"key": "widget.hostile", "width": 80}),
    };
    let render_resp = round_trip(stdin, collector, &render_req).await?;
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

    let measure_req = Frame {
        id: 5,
        kind: FrameKind::Req,
        method: "measure".to_owned(),
        payload: json!({"key": "widget.hostile", "width": 80}),
    };
    let measure_resp = round_trip(stdin, collector, &measure_req).await?;
    assert_eq!(measure_resp.kind, FrameKind::Res);
    let height = measure_resp
        .payload
        .as_object()
        .and_then(|o| o.get("height"))
        .and_then(Value::as_u64)
        .ok_or("measure missing height: u64")?;
    assert!(height > 0, "measure height must be > 0");
    Ok(())
}

/// Phases 6–10: `input`, `resources_discover`, `command.execute`, `message_end`, `agent_start`.
async fn assert_hook_sequence(
    stdin: &mut tokio::process::ChildStdin,
    collector: &mut FrameCollector,
) -> R {
    let input_req = Frame {
        id: 6,
        kind: FrameKind::Req,
        method: "input".to_owned(),
        payload: json!({"text": "hi", "source": "interactive"}),
    };
    let input_resp = round_trip(stdin, collector, &input_req).await?;
    assert_eq!(input_resp.kind, FrameKind::Res);
    let input_payload = input_resp
        .payload
        .as_object()
        .ok_or("input payload not object")?;
    assert_eq!(
        input_payload.get("action").and_then(Value::as_str),
        Some("continue")
    );

    let res_req = Frame {
        id: 7,
        kind: FrameKind::Req,
        method: "resources_discover".to_owned(),
        payload: json!({"cwd": "/tmp", "reason": "startup"}),
    };
    let res_resp = round_trip(stdin, collector, &res_req).await?;
    assert_eq!(res_resp.kind, FrameKind::Res);
    let res_payload = res_resp
        .payload
        .as_object()
        .ok_or("resources payload not object")?;
    assert!(res_payload.contains_key("skillPaths"));
    assert!(res_payload.contains_key("promptPaths"));
    assert!(res_payload.contains_key("themePaths"));

    let cmd_req = Frame {
        id: 8,
        kind: FrameKind::Req,
        method: "command.execute".to_owned(),
        payload: json!({"command": "does-not-exist", "args": ""}),
    };
    let cmd_resp = round_trip(stdin, collector, &cmd_req).await?;
    assert_eq!(cmd_resp.kind, FrameKind::Error);
    assert_eq!(cmd_resp.method, "command.execute");
    let cmd_err = cmd_resp
        .payload
        .as_object()
        .ok_or("command error not object")?;
    assert_eq!(
        cmd_err.get("code").and_then(Value::as_str),
        Some("not_found")
    );
    assert_eq!(
        cmd_err.get("retryable").and_then(Value::as_bool),
        Some(false)
    );

    let msg_req = Frame {
        id: 9,
        kind: FrameKind::Req,
        method: "message_end".to_owned(),
        payload: json!({"type": "message_end", "message": {"role": "assistant", "content": []}}),
    };
    let msg_resp = round_trip(stdin, collector, &msg_req).await?;
    assert_eq!(msg_resp.kind, FrameKind::Res);
    let msg_payload = msg_resp
        .payload
        .as_object()
        .ok_or("message_end payload not object")?;
    assert!(msg_payload.contains_key("message"));

    let agent_req = Frame {
        id: 10,
        kind: FrameKind::Req,
        method: "agent_start".to_owned(),
        payload: json!({"type": "agent_start"}),
    };
    let agent_resp = round_trip(stdin, collector, &agent_req).await?;
    assert_eq!(agent_resp.kind, FrameKind::Res);
    assert_eq!(
        agent_resp
            .payload
            .as_object()
            .and_then(|o| o.get("ok"))
            .and_then(Value::as_bool),
        Some(true)
    );
    Ok(())
}

/// Spawn the compiled JS host against an empty extension set and verify the
/// real cross-language lifecycle: hello handshake, extensions.load, registry
/// surfaces via `getAllRegisteredTools` (the load payload itself is an empty
/// snapshot), plus the specialized hook shapes that the host maps from
/// `extensions.load`'s `extensionPaths` (the fixtures register handlers).
#[tokio::test]
async fn real_host_lifecycle_and_dispatch() -> R {
    use tokio::process::Command;

    let host_path = real_host_path()?;
    assert!(
        host_path.is_file(),
        "compiled host artifact missing: {}",
        host_path.display()
    );

    let mut command = Command::new(&host_path);
    command.arg("--cwd").arg(".");
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| -> BoxErr { format!("spawn: {e}").into() })?;
    let mut stdin = child.stdin.take().ok_or("missing stdin")?;
    let stdout = child.stdout.take().ok_or("missing stdout")?;
    let stderr = child.stderr.take().ok_or("missing stderr")?;

    let (mut collector, collector_task) = FrameCollector::new(stdout);
    let stderr_task = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stderr).lines();
        // Drain host stderr without echoing: no logging framework is available
        // in this crate's test build, and `eprintln!` is disallowed in lib code.
        while let Ok(Some(_line)) = reader.next_line().await {}
    });

    let guard = HostGuard {
        child,
        tasks: vec![collector_task, stderr_task],
    };

    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .ok_or("workspace root")?;

    assert_hello_handshake(&mut stdin, &mut collector).await?;
    assert_extensions_load(&mut stdin, &mut collector, workspace_root).await?;
    assert_ui_slot_lifecycle(&mut stdin, &mut collector).await?;
    assert_hook_sequence(&mut stdin, &mut collector).await?;

    // Teardown.
    guard.teardown().await;

    Ok(())
}

/// Minimal in-repo reproduction of the E2E `rust-extension-flag-session-start`
/// marker bug: a real extension must observe the CLI-applied flag value inside
/// its `session_start` handler, and `resources_discover` must follow it.
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
    use crate::core::extension_host::ExtensionUiEvent;

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
    let ExtensionUiEvent::ThemeSet(set) = event else {
        return Err(format!("expected ThemeSet, got {event:?}").into());
    };
    assert_eq!(set.name.as_deref(), Some("classic-light"));
    assert!(set.persist);
    assert_eq!(set.theme, None);

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
    let ExtensionUiEvent::ThemeSet(object_form) = event else {
        return Err(format!("expected object-form ThemeSet, got {event:?}").into());
    };
    assert!(!object_form.persist);
    let wire = object_form.theme.ok_or("object form must carry a theme")?;
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
    use pi_ext::protocol::SessionCommand;

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
    let SessionBridgeEvent::Command(SessionCommand::SetSessionName { name }) = event else {
        return Err(format!("expected SetSessionName, got {event:?}").into());
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
    assert_eq!(id, 41);
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
async fn ui_control_event_forwards_typed_to_ui_subscribers() -> R {
    use crate::core::extension_host::ExtensionUiEvent;
    use pi_ext::protocol::UiControl;

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
    let ExtensionUiEvent::UiControl(UiControl::SetEditorText { text }) = event else {
        return Err(format!("expected SetEditorText, got {event:?}").into());
    };
    assert_eq!(text, "draft");

    runner.shutdown_once().await;
    Ok(())
}

#[tokio::test]
async fn push_session_and_ui_state_send_mirror_events() -> R {
    use pi_ext::protocol::{SessionStateWire, UiStateWire};

    let (runner, host) = make_runner(json!({})).await?;

    let state = SessionStateWire {
        session_name: Some("s".to_owned()),
        thinking_level: "medium".to_owned(),
        active_tools: vec!["read".to_owned()],
        is_idle: true,
        system_prompt: "p".to_owned(),
        ..SessionStateWire::default()
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
    assert_eq!(frame.payload["activeTools"][0], "read");

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

    let mut config = crate::core::agent_session::AgentSessionConfig::test_config(
        Arc::new(StubProvider),
        pi_agent::state::default_model(),
    )?;
    config.extension_runner = Some(Arc::clone(&runner) as Arc<dyn ExtensionRunner>);
    config.host_extension_runner = Some(Arc::clone(&runner));
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
    assert!(runner.execute_command("renameSession", "").await?);
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

    runner.shutdown_once().await;
    Ok(())
}
