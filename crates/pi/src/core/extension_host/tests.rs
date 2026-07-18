//! Fake-host acceptance tests for [`HostExtensionRunner`].
//!
//! Drives an in-memory JSONL fake host (no real Bun) through every
//! [`ExtensionRunner`] hook family, the full 33-event handler-presence set,
//! host-owned transforms/merge, non-retryable error/crash/timeout isolation
//! with pending-close / no-replay, registry first-wins dedup, reload
//! generation + stale-slot invalidation, the HTML renderer, and exactly-once
//! shutdown.

use std::collections::{HashMap, HashSet};
use std::error::Error;
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
use pi_ai::{AssistantContent, AssistantMessage, AssistantMessageEvent, TextContent, ToolCall};
use pi_ext::client::HostClient;
#[cfg(unix)]
use pi_ext::host::{HostSource, HostSpec};
use pi_ext::protocol::{
    ExtensionErrorEvent, Frame, FrameKind, HelloAck, decode_frame_str, encode_frame,
};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::super::agent_session::events::AgentSessionEvent;
use super::super::agent_session::extension_runner::{ExtensionRunner, SessionHooks};
use super::super::model_runtime::{CreateModelRuntimeOptions, ModelRuntime};
use super::{
    ALL_EVENT_TYPES, HostExtensionRunner, HostStartError, ToolRenderPhase,
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

/// Handle to drive the fake host.
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
    } else {
        responses
            .lock()
            .ok()
            .and_then(|map| map.get(&req.method).cloned())
            .unwrap_or_else(|| Value::Object(Map::new()))
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
        StartupBehavior::RejectLoad | StartupBehavior::Ready => {
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
    let mut permissions = fs::metadata(&script_path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions)?;
    let spec = HostSpec {
        source: HostSource::Env(script_path.clone()),
        program: script_path,
        args: vec![
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
    ExtensionRunner::shutdown(runner.as_ref(), "quit").await?;
    assert!(!runner.is_running());
    assert_host_shutdown_and_reaped(&pid_path, &shutdown_path)
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

    let delta = compact_message_update_event(&AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "lo".to_owned(),
        partial: partial.clone(),
    });
    assert_eq!(delta["type"], "text_delta");
    assert_eq!(delta["delta"], "lo");
    assert!(delta.get("partial").is_none());
    assert!(delta.get("block").is_none());
    assert!(delta["meta"].get("content").is_none());

    let end = compact_message_update_event(&AssistantMessageEvent::TextEnd {
        content_index: 0,
        content: "hello".to_owned(),
        partial,
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
                Ok(replacement)
            },
        )
        .await?;

    let payloads = observed.lock().map_err(|_| "observed lock poisoned")?;
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["projectTrusted"], true);
    drop(payloads);
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
    let (first, repeated) = tokio::join!(
        ExtensionRunner::shutdown(runner.as_ref(), "quit"),
        ExtensionRunner::shutdown(runner.as_ref(), "quit"),
    );
    first?;
    repeated?;
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
    assert_eq!(request.payload["reason"], "quit");
    assert_eq!(lines[1], "shutdown");
    assert_host_reaped(&pid_path)
}

#[tokio::test]
async fn shutdown_reaps_transport_when_hook_times_out() -> R {
    let (runner, host) = make_runner(full_snapshot()).await?;
    host.drop_method("session_shutdown");
    ExtensionRunner::shutdown(runner.as_ref(), "quit").await?;
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
    ExtensionRunner::shutdown(runner.as_ref(), "quit").await?;
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
