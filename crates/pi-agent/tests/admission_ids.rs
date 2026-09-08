//! Regression coverage for operation-ID admission uniqueness and key safety.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::sync::Arc;

use futures::stream::{self, BoxStream, StreamExt};
use pi_agent::ToolExecutionMode;
use pi_agent::context::Context;
use pi_agent::harness::api::{
    AcquireLaneOptions, AgentHarness, AgentHarnessBuilder, AgentHarnessOptions, AgentLane,
    DriveOptions, HarnessModels, HarnessResources, OperationRequest, PromptInput,
};
use pi_agent::harness::result::{DriveOutcome, HarnessError};
use pi_agent::pi_ai;
use pi_agent::session::{
    HarnessStreamOptions, LaneName, MemoryStorage, OperationId, SessionMetadata,
    StorageBackedSession, UuidV7Generator,
};

fn fixture_model() -> pi_ai::Model {
    pi_ai::Model {
        id: "admission-id-model".to_owned(),
        name: "Admission ID fixture".to_owned(),
        api: "admission-id-api".to_owned(),
        provider: "admission-id-provider".to_owned(),
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

struct StopModels {
    model: pi_ai::Model,
}

impl pi_ai::Provider for StopModels {
    fn stream(
        &self,
        _model: &pi_ai::Model,
        _context: pi_ai::Context,
        _options: pi_ai::StreamOptions,
    ) -> BoxStream<'static, Result<pi_ai::AssistantMessageEvent, pi_ai::ProviderError>> {
        let mut message = pi_ai::AssistantMessage::new(
            "admission-id-api",
            "admission-id-provider",
            "admission-id-model",
            1,
        );
        message
            .content
            .push(pi_ai::AssistantContent::Text(pi_ai::TextContent::new(
                "done",
            )));
        message.stop_reason = pi_ai::StopReason::Stop;
        stream::iter(vec![Ok(pi_ai::AssistantMessageEvent::Start {
            partial: Arc::new(message.clone()),
        })])
        .chain(stream::once(async move {
            Ok(pi_ai::AssistantMessageEvent::Done {
                reason: pi_ai::DoneReason::Stop,
                message,
            })
        }))
        .boxed()
    }
}

impl HarnessModels for StopModels {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<pi_ai::Model> {
        (self.model.provider == provider && self.model.id == model_id).then(|| self.model.clone())
    }
}

async fn fixture(
    cx: &Context,
) -> Result<
    (
        Arc<dyn AgentHarness>,
        Arc<dyn AgentLane>,
        Arc<dyn AgentLane>,
    ),
    Box<dyn Error>,
> {
    let session = StorageBackedSession::new(
        SessionMetadata {
            id: "admission-ids".to_owned(),
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
    let models = Arc::new(StopModels {
        model: fixture_model(),
    });
    let (harness, _) = AgentHarnessBuilder::create(
        AgentHarnessOptions {
            session,
            models: models.clone(),
            model: models.model.clone(),
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
        cx,
    )
    .await?;
    let main = harness
        .lane(&LaneName::from("main"), AcquireLaneOptions::default(), cx)
        .await?;
    let other = harness
        .lane(&LaneName::from("other"), AcquireLaneOptions::default(), cx)
        .await?;
    Ok((harness, main, other))
}

fn prompt(operation_id: OperationId) -> OperationRequest {
    OperationRequest::Prompt {
        operation_id: Some(operation_id),
        prompt: PromptInput::Text {
            text: "hello".to_owned(),
            images: Vec::new(),
        },
    }
}

#[tokio::test(flavor = "current_thread")]
async fn operation_id_reuse_across_lanes_is_rejected() -> Result<(), Box<dyn Error>> {
    let cx = Context::background();
    let (harness, main, other) = fixture(&cx).await?;
    let operation_id = OperationId::from("shared-operation");

    let admitted = main.accept(prompt(operation_id.clone()), &cx).await?;
    assert_eq!(admitted.operation_id, operation_id);
    assert!(
        matches!(
            other.accept(prompt(operation_id), &cx).await,
            Err(HarnessError::InvalidMessage { reason, message, .. })
                if reason == "operation_id_reuse" && message.contains("active")
        ),
        "an operation id already reserved on another lane must be rejected"
    );

    harness.close(&cx).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn operation_id_reuse_after_settlement_is_rejected() -> Result<(), Box<dyn Error>> {
    let cx = Context::background();
    let (harness, main, _) = fixture(&cx).await?;
    let operation_id = OperationId::from("settled-operation");

    let admitted = main.accept(prompt(operation_id.clone()), &cx).await?;
    let outcome = main
        .drive(
            DriveOptions {
                operation_id: admitted.operation_id,
                wait_for_retry: true,
                poll_deferred: false,
            },
            &cx,
        )
        .await?;
    assert!(matches!(
        outcome,
        DriveOutcome::Settled(record) if record.operation_id == operation_id
    ));

    assert!(
        matches!(
            main.accept(prompt(operation_id), &cx).await,
            Err(HarnessError::InvalidMessage { reason, message, .. })
                if reason == "operation_id_reuse" && message.contains("settled")
        ),
        "a settled operation id must not be admitted again"
    );

    harness.close(&cx).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn operation_id_key_separator_is_rejected_at_admission() -> Result<(), Box<dyn Error>> {
    let cx = Context::background();
    let (harness, main, _) = fixture(&cx).await?;
    assert!(
        matches!(
            main.accept(prompt(OperationId::from("operation:with-separator")), &cx)
                .await,
            Err(HarnessError::InvalidMessage { reason, message, .. })
                if reason == "operation_id" && message.contains("reserved ':' key separator")
        ),
        "':' is reserved by operation composite keys"
    );

    harness.close(&cx).await?;
    Ok(())
}
