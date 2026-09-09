//! Regression test for mid-operation watcher baselines: a watcher attaching
//! while a run streams an assistant response or executes a tool must observe
//! the live streaming message and running tool list from durable lane state,
//! not an empty baseline that later events cannot repair.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::error::Error;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::{self, BoxStream, StreamExt};
use pi_agent::context::Context;
use pi_agent::harness::api::{
    AcquireLaneOptions, AgentHarness, AgentHarnessBuilder, AgentHarnessOptions, HarnessModels,
    HarnessResources, PromptInput,
};
use pi_agent::harness::bus::Unsubscribe;
use pi_agent::harness::event::HarnessEventType;
use pi_agent::harness::snapshot::LaneSnapshotTool;
use pi_agent::harness::tool::{HarnessTool, ToolContextValue, ToolInvocation, ToolUpdateSink};
use pi_agent::pi_ai;
use pi_agent::session::{
    HarnessStreamOptions, LaneName, MemoryStorage, SessionMetadata, StorageBackedSession,
    UuidV7Generator,
};
use pi_agent::{AgentToolResult, ToolError, ToolExecutionMode};
use serde_json::{Map, Value, json};
use tokio::sync::Notify;

type ScriptedStream =
    BoxStream<'static, Result<pi_ai::AssistantMessageEvent, pi_ai::ProviderError>>;

struct ScriptedModels {
    model: pi_ai::Model,
    streams: Mutex<VecDeque<ScriptedStream>>,
}
#[allow(clippy::panic, reason = "underflow means the fixture script is wrong")]
impl pi_ai::Provider for ScriptedModels {
    fn stream(
        &self,
        _model: &pi_ai::Model,
        _context: pi_ai::Context,
        _options: pi_ai::StreamOptions,
    ) -> ScriptedStream {
        let next = self
            .streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        next.unwrap_or_else(|| {
            panic!("scripted stream underflow: provider streamed more times than scripted")
        })
    }
}

impl HarnessModels for ScriptedModels {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<pi_ai::Model> {
        (self.model.provider == provider && self.model.id == model_id).then(|| self.model.clone())
    }
}

/// A tool whose execution parks until the test releases it, so a watcher can
/// attach while the call is durably `EffectPending`.
struct BlockingTool {
    schema: Value,
    release: Arc<Notify>,
}

impl HarnessTool for BlockingTool {
    fn name(&self) -> &'static str {
        "blocker"
    }
    fn description(&self) -> &'static str {
        "blocks until released"
    }
    fn parameters(&self) -> &Value {
        &self.schema
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }
    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        _params: Value,
        _on_update: &'a ToolUpdateSink,
        _tool_context: Option<&'a ToolContextValue>,
        _invocation: &'a dyn ToolInvocation,
        _cx: &'a Context,
    ) -> BoxFuture<'a, Result<AgentToolResult, ToolError>> {
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            release.notified().await;
            Ok(AgentToolResult {
                content: vec![pi_ai::ToolResultContent::Text(pi_ai::TextContent::new(
                    "tool-done",
                ))],
                ..AgentToolResult::default()
            })
        })
    }
}

fn fixture_model() -> pi_ai::Model {
    pi_ai::Model {
        id: "watch-model".to_owned(),
        name: "Watch fixture".to_owned(),
        api: "watch-api".to_owned(),
        provider: "watch-provider".to_owned(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: Vec::new(),
        cost: pi_ai::ModelCost::default(),
        context_window: 8192,
        max_tokens: 1024,
        headers: None,
        compat: None,
        extra: BTreeMap::default(),
    }
}

fn pending_message() -> pi_ai::AssistantMessage {
    let mut message = pi_ai::AssistantMessage::new("watch-api", "watch-provider", "watch-model", 1);
    message.stop_reason = pi_ai::StopReason::Pending;
    message
}

/// Registers a listener that notifies once the named event type is delivered;
/// observer events are emitted only after their durable writes commit, so the
/// notification implies the snapshot reads observe the staged state. The
/// returned guard must stay alive for the listener to keep firing.
fn on_event(
    harness: &Arc<dyn AgentHarness>,
    kind: HarnessEventType,
) -> Result<(Arc<Notify>, Unsubscribe), Box<dyn Error>> {
    let notify = Arc::new(Notify::new());
    let observed = Arc::clone(&notify);
    let listener = harness.events().on(
        kind,
        Arc::new(move |_, _| {
            observed.notify_one();
            Box::pin(async {})
        }),
    )?;
    Ok((notify, listener))
}
#[allow(
    clippy::panic,
    reason = "timeout branches panic with run-state diagnostics"
)]
#[allow(
    clippy::too_many_lines,
    reason = "end-to-end drive scripts one full run inline"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_attached_mid_operation_sees_streaming_and_running_tools()
-> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let cx = Context::background();
        let session = StorageBackedSession::new(
            SessionMetadata {
                id: "lane-snapshot-watch".to_owned(),
                created_at: 1,
                storage_version: MemoryStorage::STORAGE_VERSION,
                cwd: None,
                parent_session_id: None,
                legacy_parent_session_path: None,
            },
            Arc::new(MemoryStorage::new()),
            Arc::new(UuidV7Generator::new()),
            None,
        );

        let stream_release = Arc::new(Notify::new());
        let tool_release = Arc::new(Notify::new());

        // Stream one emits a text-start plus partial delta, then parks on a
        // channel so the test can attach a watcher mid-stream; on release it
        // finishes with a tool call so the run enters the tool phase. The
        // frame encoder requires TextStart before TextDelta.
        let mut text_started = pending_message();
        text_started
            .content
            .push(pi_ai::AssistantContent::Text(pi_ai::TextContent::new("")));
        let mut partial_text = pending_message();
        partial_text
            .content
            .push(pi_ai::AssistantContent::Text(pi_ai::TextContent::new(
                "hel",
            )));
        let mut tool_message = partial_text.clone();
        tool_message
            .content
            .push(pi_ai::AssistantContent::ToolCall(pi_ai::ToolCall::new(
                "call-1",
                "blocker",
                Map::new(),
            )));
        tool_message.stop_reason = pi_ai::StopReason::ToolUse;
        let first = stream::iter(vec![
            Ok(pi_ai::AssistantMessageEvent::Start {
                partial: Arc::new(pending_message()),
            }),
            Ok(pi_ai::AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: Arc::new(text_started),
            }),
            Ok(pi_ai::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "hel".to_owned(),
                partial: Arc::new(partial_text),
            }),
        ])
        .chain(stream::once({
            let release = Arc::clone(&stream_release);
            async move {
                release.notified().await;
                Ok(pi_ai::AssistantMessageEvent::Done {
                    reason: pi_ai::DoneReason::ToolUse,
                    message: tool_message,
                })
            }
        }))
        .boxed();
        // Stream two answers the follow-up turn after the tool result commits.
        // consume_stream rejects a terminal Done without a preceding Start.
        let mut final_message =
            pi_ai::AssistantMessage::new("watch-api", "watch-provider", "watch-model", 2);
        final_message
            .content
            .push(pi_ai::AssistantContent::Text(pi_ai::TextContent::new(
                "final",
            )));
        final_message.stop_reason = pi_ai::StopReason::Stop;
        let second = stream::iter(vec![
            Ok(pi_ai::AssistantMessageEvent::Start {
                partial: Arc::new(pi_ai::AssistantMessage::new(
                    "watch-api",
                    "watch-provider",
                    "watch-model",
                    2,
                )),
            }),
            Ok(pi_ai::AssistantMessageEvent::Done {
                reason: pi_ai::DoneReason::Stop,
                message: final_message,
            }),
        ])
        .boxed();

        let models = Arc::new(ScriptedModels {
            model: fixture_model(),
            streams: Mutex::new(VecDeque::from([first, second])),
        });
        let (harness, _) = AgentHarnessBuilder::create(
            AgentHarnessOptions {
                session: session.clone(),
                models: models.clone(),
                model: models.model.clone(),
                thinking_level: None,
                active_tool_names: Some(vec!["blocker".to_owned()]),
                tools: vec![Arc::new(BlockingTool {
                    schema: json!({"type": "object"}),
                    release: Arc::clone(&tool_release),
                })],
                tool_context: None,
                system_prompt: None,
                resources: HarnessResources::default(),
                stream_options: HarnessStreamOptions::default(),
                retry: None,
                compaction: None,
                steering_mode: None,
                follow_up_mode: None,
                tool_execution: ToolExecutionMode::default(),
                to_provider_messages: None,
                entry_projectors: HashMap::new(),
            },
            &cx,
        )
        .await?;
        // Compaction is on by default and would start a summary generation
        // that consumes a scripted stream before the assistant turn runs.
        let _ = harness
            .set_compaction_settings(
                pi_agent::session::CompactionSettings {
                    enabled: false,
                    ..pi_agent::session::CompactionSettings::default()
                },
                &cx,
            )
            .await;
        let (retry_scheduled, _retry_listener) =
            on_event(&harness, HarnessEventType::RetryScheduled)?;
        let main = harness
            .lane(&LaneName::from("main"), AcquireLaneOptions::default(), &cx)
            .await?;
        let (run_started, _run_listener) = on_event(&harness, HarnessEventType::RunStart)?;
        let (message_start, _start_listener) = on_event(&harness, HarnessEventType::MessageStart)?;
        let (message_update, _update_listener) =
            on_event(&harness, HarnessEventType::MessageUpdate)?;
        let (tool_start, _tool_listener) = on_event(&harness, HarnessEventType::ToolStart)?;

        let run_lane = Arc::clone(&main);
        let run_cx = cx.clone();
        let mut run = tokio::spawn(async move {
            run_lane
                .prompt(
                    PromptInput::Text {
                        text: "hello".to_owned(),
                        images: Vec::new(),
                    },
                    &run_cx,
                )
                .await
        });

        // Mid-stream attach: the staged frames must rebuild the partial
        // assistant message for the watcher's baseline.
        tokio::select! {
            () = run_started.notified() => {}
            result = &mut run => panic!("run finished before RunStart: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(3)) => panic!("no RunStart within 3s"),
            () = retry_scheduled.notified() => panic!("run entered retry backoff before streaming"),
        }
        tokio::select! {
        () = message_start.notified() => {}
        () = tokio::time::sleep(Duration::from_secs(3)) => {
            let stalled = main.watch(&cx).await?.snapshot();
            panic!(
                "no MessageStart within 3s; operation = {:?}",
                stalled.operation.as_ref().map(|operation| &operation.status)
            );
        }
            }
        tokio::select! {
            () = message_update.notified() => {}
            result = &mut run => panic!("run finished before MessageUpdate: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(3)) => panic!("no MessageUpdate within 3s"),
        }
        let streaming = main.watch(&cx).await?.snapshot();
        let streaming_operation = streaming
            .operation
            .as_ref()
            .ok_or("mid-stream snapshot must carry the open operation")?;
        let streaming_message = streaming_operation
            .streaming_message
            .as_ref()
            .ok_or("mid-stream watcher must see the in-flight assistant message")?;
        assert_eq!(
            streaming_message.stop_reason,
            pi_ai::StopReason::Pending,
            "baseline streaming message must still be pending"
        );
        assert!(
            streaming_message.content.iter().any(|content| matches!(
                content,
                pi_ai::AssistantContent::Text(text) if text.text.contains("hel")
            )),
            "baseline streaming message must carry the streamed text"
        );
        // Release the parked stream with the tool-call terminal event so the
        // run enters the tool phase.
        stream_release.notify_one();
        tokio::select! {
            () = tool_start.notified() => {}
            result = &mut run => panic!("run finished before ToolStart: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(3)) => panic!("no ToolStart within 3s"),
        }
        let running = main.watch(&cx).await?.snapshot();
        let running_operation = running
            .operation
            .as_ref()
            .ok_or("mid-tool snapshot must carry the open operation")?;
        let tool = running_operation
            .running_tools
            .iter()
            .find(|tool| tool.tool_call_id() == "call-1")
            .ok_or("mid-tool watcher must see the running tool call")?;
        assert!(
            matches!(
                tool,
                LaneSnapshotTool::Running { tool_name, .. } if tool_name == "blocker"
            ),
            "baseline tool entry must be the running blocker call"
        );

        tool_release.notify_one();
        run.await??;
        harness.close(&cx).await?;
        Ok::<(), Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}
