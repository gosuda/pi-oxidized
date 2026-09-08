//! Regression test: a cancel requested while a deferred poll is in flight
//! must settle the operation as aborted instead of publishing the deferred
//! terminal response and its tool transition.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::stream::{self, BoxStream, StreamExt};
use pi_agent::ToolExecutionMode;
use pi_agent::context::Context;
use pi_agent::harness::api::{
    AcquireLaneOptions, AgentHarnessBuilder, AgentHarnessOptions, DriveOptions, HarnessModels,
    HarnessResources, OperationRequest, PromptInput,
};
use pi_agent::harness::event::HarnessEventType;
use pi_agent::harness::hooks::{AfterResponse, AfterResponseEvent, AfterResponseResult, HookError};
use pi_agent::harness::result::DriveOutcome;
use pi_agent::pi_ai;
use pi_agent::session::{
    HarnessStreamOptions, LaneName, MemoryStorage, OperationId, SessionMetadata,
    StorageBackedSession, TerminalStatus, UuidV7Generator,
};
use serde_json::{Map, Value, json};

type ScriptedStream =
    BoxStream<'static, Result<pi_ai::AssistantMessageEvent, pi_ai::ProviderError>>;

fn fixture_model() -> pi_ai::Model {
    pi_ai::Model {
        id: "deferred-model".to_owned(),
        name: "Deferred fixture".to_owned(),
        api: "deferred-api".to_owned(),
        provider: "deferred-provider".to_owned(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: Vec::new(),
        cost: pi_ai::ModelCost::default(),
        context_window: 8192,
        max_tokens: 1024,
        headers: None,
        compat: None,
        extra: BTreeMap::new(),
    }
}

fn terminal_stream(message: pi_ai::AssistantMessage, reason: pi_ai::DoneReason) -> ScriptedStream {
    stream::iter(vec![Ok(pi_ai::AssistantMessageEvent::Start {
        partial: Arc::new(message.clone()),
    })])
    .chain(stream::once(async move {
        Ok(pi_ai::AssistantMessageEvent::Done { reason, message })
    }))
    .boxed()
}

/// Provider whose initial stream defers and whose deferred fetch records the
/// `wait` option it was polled with before emitting a tool-call response.
struct DeferredModels {
    model: pi_ai::Model,
    callbacks: pi_ai::DeferredCallbacks,
}

impl pi_ai::Provider for DeferredModels {
    fn stream(
        &self,
        model: &pi_ai::Model,
        _context: pi_ai::Context,
        _options: pi_ai::StreamOptions,
    ) -> ScriptedStream {
        let mut message = pi_ai::AssistantMessage::new(
            model.api.clone(),
            model.provider.clone(),
            model.id.clone(),
            1,
        );
        message.stop_reason = pi_ai::StopReason::Deferred;
        message.deferred = Some(pi_ai::DeferredHandle {
            provider: model.provider.clone(),
            model_id: model.id.clone(),
            api: model.api.clone(),
            id: "deferred-1".to_owned(),
            expires_at: None,
            poll_after_ms: None,
            data: None,
        });
        terminal_stream(message, pi_ai::DoneReason::Deferred)
    }

    fn deferred(&self) -> Option<&pi_ai::DeferredCallbacks> {
        Some(&self.callbacks)
    }
}

impl HarnessModels for DeferredModels {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<pi_ai::Model> {
        (self.model.provider == provider && self.model.id == model_id).then(|| self.model.clone())
    }
}

/// The deferred poll answers with a tool call, so publishing it would commit
/// the response entry and emit a tool-turn transition.
fn recording_fetch(
    model: pi_ai::Model,
    observed_waits: Arc<Mutex<Vec<Option<Value>>>>,
) -> pi_ai::FetchDeferredFn {
    Arc::new(move |_model, _handle, options| {
        if let Ok(mut waits) = observed_waits.lock() {
            waits.push(options.extra_value(pi_ai::StreamOptionKey::WAIT).cloned());
        }
        let mut message = pi_ai::AssistantMessage::new(
            model.api.clone(),
            model.provider.clone(),
            model.id.clone(),
            2,
        );
        message.content = vec![pi_ai::AssistantContent::ToolCall(pi_ai::ToolCall::new(
            "call-1",
            "blocked_tool",
            Map::new(),
        ))];
        message.stop_reason = pi_ai::StopReason::ToolUse;
        terminal_stream(message, pi_ai::DoneReason::ToolUse)
    })
}

#[allow(clippy::too_many_lines, reason = "abort race scripts one full deferred run inline")]
#[tokio::test(flavor = "current_thread")]
async fn abort_during_deferred_poll_publishes_interrupted_not_the_response()
-> Result<(), Box<dyn Error>> {
    let cx = Context::background();
    let session = StorageBackedSession::new(
        SessionMetadata {
            id: "deferred-poll-abort".to_owned(),
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
    let observed_waits = Arc::new(Mutex::new(Vec::new()));
    let model = fixture_model();
    let models = Arc::new(DeferredModels {
        model: model.clone(),
        callbacks: pi_ai::DeferredCallbacks {
            fetch: Some(recording_fetch(model.clone(), Arc::clone(&observed_waits))),
            cancel: None,
        },
    });
    let (harness, _) = AgentHarnessBuilder::create(
        AgentHarnessOptions {
            session,
            models: models.clone(),
            model,
            thinking_level: None,
            active_tool_names: None,
            tools: Vec::new(),
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
    let lane = harness
        .lane(&LaneName::from("main"), AcquireLaneOptions::default(), &cx)
        .await?;

    let turn_starts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&turn_starts);
    let _turn_guard = harness.events().on(
        HarnessEventType::TurnStart,
        Arc::new(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }),
    )?;

    let admitted = lane
        .accept(
            OperationRequest::Prompt {
                operation_id: Some(OperationId::from("deferred-abort")),
                prompt: PromptInput::Text {
                    text: "hello".to_owned(),
                    images: Vec::new(),
                },
            },
            &cx,
        )
        .await?;

    // Request the abort from inside `after_response`: the deferred poll has
    // already produced its terminal response, so the cancel lands in the
    // window between stream completion and response publication.
    let abort_armed = Arc::new(AtomicBool::new(false));
    let hook_lane = Arc::clone(&lane);
    let hook_operation = admitted.operation_id.clone();
    let hook_armed = Arc::clone(&abort_armed);
    let _hook_guard = harness.hooks().on::<AfterResponse>(
        Arc::new(move |_event: AfterResponseEvent, _cx: Context| {
            let lane = Arc::clone(&hook_lane);
            let operation_id = hook_operation.clone();
            let armed = Arc::clone(&hook_armed);
            async move {
                if armed.load(Ordering::SeqCst) {
                    let abort_cx = Context::background();
                    lane.request_abort(&operation_id, &abort_cx).await?;
                }
                Ok::<Option<AfterResponseResult>, HookError>(None)
            }
        }),
        None,
    )?;

    let first = lane
        .drive(
            DriveOptions {
                operation_id: admitted.operation_id.clone(),
                wait_for_retry: true,
                poll_deferred: false,
            },
            &cx,
        )
        .await?;
    assert!(matches!(first, DriveOutcome::WaitingDeferred { .. }));

    abort_armed.store(true, Ordering::SeqCst);
    let second = lane
        .drive(
            DriveOptions {
                operation_id: admitted.operation_id.clone(),
                wait_for_retry: true,
                poll_deferred: true,
            },
            &cx,
        )
        .await?;

    let DriveOutcome::Settled(record) = second else {
        return Err("deferred poll should have settled the operation".into());
    };
    assert_eq!(record.status, TerminalStatus::Aborted);
    // The deferred response carried a tool call: publishing it would have
    // committed the entry and emitted a tool-turn transition.
    assert_eq!(turn_starts.load(Ordering::SeqCst), 0);
    // Deferred polls are one-shot non-waiting fetches.
    assert_eq!(
        observed_waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_slice(),
        &[Some(json!(0))]
    );

    harness.close(&cx).await?;
    Ok(())
}
