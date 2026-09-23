//! Regression coverage for admission concurrency: the pending-admission
//! reservation (competing admissions must observe an in-flight commit as
//! busy), the lane-data/session-mutation lock order (a queue write must not
//! deadlock against or clobber an in-flight admission commit), and usage
//! event totals (cumulative after applying the row, not the raw row).

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::{self, BoxStream, StreamExt};
use pi_agent::ToolExecutionMode;
use pi_agent::context::Context;
use pi_agent::harness::api::{
    AcquireLaneOptions, AgentHarness, AgentHarnessBuilder, AgentHarnessOptions, AgentLane,
    DriveOptions, HarnessModels, HarnessResources, OperationRequest, PromptInput, QueueInput,
    RecordUsageOptions,
};
use pi_agent::harness::event::{HarnessEventPayload, HarnessEventType};
use pi_agent::harness::result::{DriveOutcome, HarnessError};
use pi_agent::harness::snapshot::LaneQueuedItem;
use pi_agent::pi_ai;
use pi_agent::session::{
    CommitResult, Entry, EntryId, EntryScan, EntryStructure, HarnessStreamOptions, LaneName,
    ListReadOptions, MemoryStorage, OperationId, RawAddress, RawListElement, RawStoredValue,
    SessionError, SessionMetadata, SessionStats, Storage, StorageBackedSession, StorageBranchScan,
    UsageRow, UsageScan, UuidV7Generator, Write,
};

fn fixture_model() -> pi_ai::Model {
    pi_ai::Model {
        id: "admission-concurrency-model".to_owned(),
        name: "Admission concurrency fixture".to_owned(),
        api: "admission-concurrency-api".to_owned(),
        provider: "admission-concurrency-provider".to_owned(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: Vec::new(),
        input_limits: None,
        cost: pi_ai::ModelCost::default(),
        prompt_cache: None,
        sampling_params: None,
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
            "admission-concurrency-api",
            "admission-concurrency-provider",
            "admission-concurrency-model",
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

/// Storage wrapper that parks the first commit issued after the gate closes,
/// so a test can observe the lane while an admission commit is in flight.
struct GatedStorage {
    inner: MemoryStorage,
    open: AtomicBool,
    entered: AtomicBool,
    entered_signal: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl GatedStorage {
    fn new() -> Self {
        Self {
            inner: MemoryStorage::new(),
            open: AtomicBool::new(true),
            entered: AtomicBool::new(false),
            entered_signal: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }

    fn close_gate(&self) {
        self.entered.store(false, Ordering::SeqCst);
        self.open.store(false, Ordering::SeqCst);
    }

    fn open_gate(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.release.notify_waiters();
    }

    async fn wait_until_gated(&self) -> Result<(), Box<dyn Error>> {
        let entered = &self.entered_signal;
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .map_err(|_| "admission commit never reached the gated storage".to_string())?;
        Ok(())
    }
}

impl Storage for GatedStorage {
    fn commit<'a>(
        &'a self,
        writes: Vec<Write>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<CommitResult, SessionError>> {
        Box::pin(async move {
            if !self.open.load(Ordering::SeqCst) {
                self.entered.store(true, Ordering::SeqCst);
                self.entered_signal.notify_one();
                while !self.open.load(Ordering::SeqCst) {
                    self.release.notified().await;
                }
            }
            self.inner.commit(writes, cx).await
        })
    }

    fn get_entries<'a>(
        &'a self,
        ids: &'a [EntryId],
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<HashMap<EntryId, Entry>, SessionError>> {
        Box::pin(async move { self.inner.get_entries(ids, cx).await })
    }

    fn get_value<'a>(
        &'a self,
        address: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<RawStoredValue>, SessionError>> {
        Box::pin(async move { self.inner.get_value(address, cx).await })
    }

    fn scan_values<'a>(
        &'a self,
        prefix: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<RawStoredValue>, SessionError>> {
        Box::pin(async move { self.inner.scan_values(prefix, cx).await })
    }

    fn read_list<'a>(
        &'a self,
        address: &'a RawAddress,
        options: Option<ListReadOptions>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<RawListElement>, SessionError>> {
        Box::pin(async move { self.inner.read_list(address, options, cx).await })
    }

    fn scan_branch<'a>(
        &'a self,
        query: &'a StorageBranchScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        Box::pin(async move { self.inner.scan_branch(query, cx).await })
    }

    fn scan_branch_structure<'a>(
        &'a self,
        query: &'a StorageBranchScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<EntryStructure>, SessionError>> {
        Box::pin(async move { self.inner.scan_branch_structure(query, cx).await })
    }

    fn scan_entries<'a>(
        &'a self,
        query: &'a EntryScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        Box::pin(async move { self.inner.scan_entries(query, cx).await })
    }

    fn scan_usage<'a>(
        &'a self,
        query: &'a UsageScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<UsageRow>, SessionError>> {
        Box::pin(async move { self.inner.scan_usage(query, cx).await })
    }

    fn get_stats<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<SessionStats, SessionError>> {
        Box::pin(async move { self.inner.get_stats(cx).await })
    }

    fn close<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move { self.inner.close(cx).await })
    }
}

async fn harness_fixture(
    cx: &Context,
    storage: Arc<GatedStorage>,
) -> Result<(Arc<dyn AgentHarness>, Arc<dyn AgentLane>), Box<dyn Error>> {
    let session = StorageBackedSession::new(
        SessionMetadata {
            id: "admission-concurrency".to_owned(),
            created_at: 1,
            storage_version: MemoryStorage::STORAGE_VERSION,
            cwd: None,
            parent_session_id: None,
            legacy_parent_session_path: None,
        },
        storage,
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
    Ok((harness, main))
}

async fn plain_fixture(
    cx: &Context,
) -> Result<(Arc<dyn AgentHarness>, Arc<dyn AgentLane>), Box<dyn Error>> {
    let session = StorageBackedSession::new(
        SessionMetadata {
            id: "admission-concurrency-plain".to_owned(),
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
    Ok((harness, main))
}

fn prompt(operation_id: &str) -> OperationRequest {
    OperationRequest::Prompt {
        operation_id: Some(OperationId::from(operation_id)),
        prompt: PromptInput::Text {
            text: "hello".to_owned(),
            images: Vec::new(),
        },
    }
}

fn queued_entry_id(item: &LaneQueuedItem) -> &EntryId {
    match item {
        LaneQueuedItem::Message { entry_id, .. } | LaneQueuedItem::Custom { entry_id, .. } => {
            entry_id
        }
    }
}

/// T48: while the first admission's durable commit is in flight, a competing
/// admission must observe the pending reservation as busy — not slip through
/// the idle check and orphan an already-persisted operation.
#[tokio::test(flavor = "current_thread")]
async fn second_admission_observes_pending_reservation() -> Result<(), Box<dyn Error>> {
    let cx = Context::background();
    let gated = Arc::new(GatedStorage::new());
    let (harness, main) = harness_fixture(&cx, Arc::clone(&gated)).await?;
    gated.close_gate();

    let first = tokio::spawn({
        let main = Arc::clone(&main);
        let cx = cx.clone();
        async move { main.accept(prompt("first-run"), &cx).await }
    });
    gated.wait_until_gated().await?;

    // The first admission published its reservation before dropping the lane
    // data guard, so this competing admission is rejected as busy instead of
    // committing a second operation against an idle-looking lane.
    let second = tokio::spawn({
        let main = Arc::clone(&main);
        let cx = cx.clone();
        async move { main.accept(prompt("second-run"), &cx).await }
    });
    let second = tokio::time::timeout(Duration::from_secs(5), second).await??;
    assert!(
        matches!(
            second,
            Err(HarnessError::LaneBusy { ref operation_id, .. })
                if operation_id == &OperationId::from("first-run")
        ),
        "competing admission must observe the pending reservation as busy, got {second:?}"
    );

    gated.open_gate();
    let admitted = tokio::time::timeout(Duration::from_secs(5), first).await??;
    let admitted = admitted?;
    assert_eq!(admitted.operation_id, OperationId::from("first-run"));

    // The reservation was undone cleanly and the second id never reached
    // durable state, so it is still admissible after the first run settles.
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
        DriveOutcome::Settled(record) if record.operation_id == OperationId::from("first-run")
    ));
    let replay = main.accept(prompt("second-run"), &cx).await?;
    assert_eq!(replay.operation_id, OperationId::from("second-run"));

    harness.close(&cx).await?;
    Ok(())
}

/// T33/T48 interleaving: a queue write racing an in-flight admission commit
/// must neither deadlock against it nor clobber the committed lane state —
/// the queued item survives and the admitted operation stays current.
#[tokio::test(flavor = "current_thread")]
async fn queue_write_during_admission_commit_preserves_both() -> Result<(), Box<dyn Error>> {
    let cx = Context::background();
    let gated = Arc::new(GatedStorage::new());
    let (harness, main) = harness_fixture(&cx, Arc::clone(&gated)).await?;
    gated.close_gate();

    let first = tokio::spawn({
        let main = Arc::clone(&main);
        let cx = cx.clone();
        async move { main.accept(prompt("first-run"), &cx).await }
    });
    gated.wait_until_gated().await?;

    let steer = tokio::spawn({
        let main = Arc::clone(&main);
        let cx = cx.clone();
        async move {
            main.steer(
                QueueInput::Text {
                    text: "while-admitting".to_owned(),
                    images: Vec::new(),
                },
                &cx,
            )
            .await
        }
    });
    // Let the queue write reach its mutation and park on the in-flight
    // admission commit before releasing the gate.
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    gated.open_gate();
    let admitted = tokio::time::timeout(Duration::from_secs(5), first).await??;
    assert_eq!(admitted?.operation_id, OperationId::from("first-run"));
    let queued = tokio::time::timeout(Duration::from_secs(5), steer).await??;
    let entry_id = queued?;

    let snapshot = main.watch(&cx).await?.snapshot();
    assert_eq!(
        snapshot.operation.as_ref().map(|op| op.id.clone()),
        Some(OperationId::from("first-run")),
        "the admitted operation must stay current after the concurrent queue write"
    );
    assert!(
        snapshot
            .queues
            .iter()
            .any(|item| queued_entry_id(item) == &entry_id),
        "the queued item must survive the concurrent admission commit"
    );

    harness.close(&cx).await?;
    Ok(())
}

/// T49: usage event `totals` carry the cumulative session usage after
/// applying the row, matching authoritative session statistics — not the
/// raw submitted row.
#[tokio::test(flavor = "current_thread")]
async fn usage_event_totals_are_cumulative_session_totals() -> Result<(), Box<dyn Error>> {
    let cx = Context::background();
    let (harness, main) = plain_fixture(&cx).await?;

    let observed: Arc<Mutex<Vec<pi_ai::Usage>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&observed);
    let _listener = harness.events().on(
        HarnessEventType::Usage,
        Arc::new(move |event, _| {
            if let HarnessEventPayload::Usage { totals, .. } = &event.payload {
                sink.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(totals.clone());
            }
            Box::pin(async {})
        }),
    )?;

    let first = pi_ai::Usage {
        input: 100,
        output: 40,
        total_tokens: 140,
        ..pi_ai::Usage::default()
    };
    let second = pi_ai::Usage {
        input: 30,
        output: 12,
        total_tokens: 42,
        ..pi_ai::Usage::default()
    };
    main.record_usage(
        first.clone(),
        RecordUsageOptions {
            entry_id: None,
            details: None,
        },
        &cx,
    )
    .await?;
    main.record_usage(
        second.clone(),
        RecordUsageOptions {
            entry_id: None,
            details: None,
        },
        &cx,
    )
    .await?;

    let totals = observed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(totals.len(), 2, "both usage rows must emit one event each");
    assert_eq!(totals[0], first, "first event's totals apply the first row");

    // The authoritative aggregate after the second row: row one plus row two.
    let mut expected = first;
    expected.input += second.input;
    expected.output += second.output;
    expected.total_tokens += second.total_tokens;
    assert_eq!(
        totals[1], expected,
        "second event's totals must be the cumulative session usage, not the submitted row"
    );

    // The event-reduced snapshot must agree with a freshly loaded one.
    let live = main.watch(&cx).await?.snapshot();
    assert_eq!(live.stats.usage, expected);

    harness.close(&cx).await?;
    Ok(())
}
