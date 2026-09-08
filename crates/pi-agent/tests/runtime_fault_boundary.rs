//! Regression tests for the runtime fault boundary: a storage failure while
//! admitting a prompt must seal every lane against one shared fault, preserve
//! the original IO cause, and leave closing the session to the harness owner.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use pi_agent::ToolExecutionMode;
use pi_agent::context::Context;
use pi_agent::harness::api::{
    AcquireLaneOptions, AgentHarness, AgentHarnessBuilder, AgentHarnessOptions, AgentLane,
    HarnessModels, HarnessResources, NavigateOptions, OperationRequest, PromptInput, QueueInput,
};
use pi_agent::harness::event::HarnessEventType;
use pi_agent::harness::result::HarnessFault;
use pi_agent::pi_ai;
use pi_agent::session::{
    CommitResult, Entry, EntryId, EntryScan, EntryStructure, HarnessStreamOptions, LaneName,
    ListReadOptions, MemoryStorage, OperationId, RawAddress, RawListElement, RawStoredValue,
    SessionError, SessionMetadata, SessionReaderExt, SessionStats, Storage, StorageBackedSession,
    StorageBranchScan, StorageErrorCode, StorageFailure, UsageRow, UsageScan, UuidV7Generator,
    Write, address,
};

struct FailNextCommitStorage {
    inner: MemoryStorage,
    armed: AtomicBool,
    /// Armed commits to let through before the failure fires.
    skip: AtomicUsize,
    cause: Arc<std::io::Error>,
}

impl Storage for FailNextCommitStorage {
    fn commit<'a>(
        &'a self,
        writes: Vec<Write>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<CommitResult, SessionError>> {
        if self.armed.load(Ordering::SeqCst) {
            if self.skip.load(Ordering::SeqCst) > 0 {
                self.skip.fetch_sub(1, Ordering::SeqCst);
            } else if self.armed.swap(false, Ordering::SeqCst) {
                let cause: Arc<dyn Error + Send + Sync> = self.cause.clone();
                return Box::pin(async move {
                    Err(SessionError::Backend(StorageFailure {
                        code: StorageErrorCode::Io,
                        message: "admission commit failed".to_owned(),
                        source: Some(cause),
                    }))
                });
            }
        }
        self.inner.commit(writes, cx)
    }

    fn get_entries<'a>(
        &'a self,
        ids: &'a [EntryId],
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<HashMap<EntryId, Entry>, SessionError>> {
        self.inner.get_entries(ids, cx)
    }

    fn get_value<'a>(
        &'a self,
        address: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Option<RawStoredValue>, SessionError>> {
        self.inner.get_value(address, cx)
    }

    fn scan_values<'a>(
        &'a self,
        prefix: &'a RawAddress,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<RawStoredValue>, SessionError>> {
        self.inner.scan_values(prefix, cx)
    }

    fn read_list<'a>(
        &'a self,
        address: &'a RawAddress,
        options: Option<ListReadOptions>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<RawListElement>, SessionError>> {
        self.inner.read_list(address, options, cx)
    }

    fn scan_branch<'a>(
        &'a self,
        query: &'a StorageBranchScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        self.inner.scan_branch(query, cx)
    }

    fn scan_branch_structure<'a>(
        &'a self,
        query: &'a StorageBranchScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<EntryStructure>, SessionError>> {
        self.inner.scan_branch_structure(query, cx)
    }

    fn scan_entries<'a>(
        &'a self,
        query: &'a EntryScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        self.inner.scan_entries(query, cx)
    }

    fn scan_usage<'a>(
        &'a self,
        query: &'a UsageScan,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<Vec<UsageRow>, SessionError>> {
        self.inner.scan_usage(query, cx)
    }

    fn get_stats<'a>(
        &'a self,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<SessionStats, SessionError>> {
        self.inner.get_stats(cx)
    }

    fn close<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<(), SessionError>> {
        self.inner.close(cx)
    }
}

struct AdmissionModels {
    model: pi_ai::Model,
    streams: AtomicUsize,
}

impl pi_ai::Provider for AdmissionModels {
    fn stream(
        &self,
        _model: &pi_ai::Model,
        _context: pi_ai::Context,
        _options: pi_ai::StreamOptions,
    ) -> BoxStream<'static, Result<pi_ai::AssistantMessageEvent, pi_ai::ProviderError>> {
        self.streams.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::empty())
    }
}

impl HarnessModels for AdmissionModels {
    fn get_model(&self, provider: &str, model_id: &str) -> Option<pi_ai::Model> {
        (self.model.provider == provider && self.model.id == model_id).then(|| self.model.clone())
    }
}

fn cause_in_chain<'a, T: Error + 'static>(mut error: &'a (dyn Error + 'static)) -> Option<&'a T> {
    // Bound the walk so a broken error-source cycle fails rather than hanging.
    for _ in 0..32 {
        if let Some(cause) = error.downcast_ref::<T>() {
            return Some(cause);
        }
        error = error.source()?;
    }
    None
}

struct AdmissionFixture {
    storage: Arc<FailNextCommitStorage>,
    session: Arc<StorageBackedSession>,
    models: Arc<AdmissionModels>,
    harness: Arc<dyn AgentHarness>,
    main: Arc<dyn AgentLane>,
    other: Arc<dyn AgentLane>,
    main_name: LaneName,
}

/// Builds the bootstrapped two-lane harness fixture; the failing commit
/// stays disarmed until the caller arms it.
async fn arm_admission_fixture(cx: &Context) -> Result<AdmissionFixture, Box<dyn Error>> {
    let storage = Arc::new(FailNextCommitStorage {
        inner: MemoryStorage::new(),
        armed: AtomicBool::new(false),
        skip: AtomicUsize::new(0),
        cause: Arc::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
    });
    let session = StorageBackedSession::new(
        SessionMetadata {
            id: "runtime-fault-boundary".to_owned(),
            created_at: 1,
            storage_version: MemoryStorage::STORAGE_VERSION,
            cwd: None,
            parent_session_id: None,
            legacy_parent_session_path: None,
        },
        storage.clone(),
        Arc::new(UuidV7Generator::new()),
        None,
    );
    let models = Arc::new(AdmissionModels {
        model: pi_ai::Model {
            id: "admission-only".to_owned(),
            name: "Admission-only fixture".to_owned(),
            api: "admission-only".to_owned(),
            provider: "admission-only".to_owned(),
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
        },
        streams: AtomicUsize::new(0),
    });
    let (harness, _) = AgentHarnessBuilder::create(
        AgentHarnessOptions {
            session: session.clone(),
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
    let main_name = LaneName::from("main");
    let other_name = LaneName::from("other");
    let main = harness
        .lane(&main_name, AcquireLaneOptions::default(), cx)
        .await?;
    let other = harness
        .lane(&other_name, AcquireLaneOptions::default(), cx)
        .await?;

    Ok(AdmissionFixture {
        storage,
        session,
        models,
        harness,
        main,
        other,
        main_name,
    })
}

#[tokio::test(flavor = "current_thread")]
async fn admission_storage_fault_seals_all_lanes_and_preserves_one_fault()
-> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let cx = Context::background();
        let fixture = arm_admission_fixture(&cx).await?;
        let AdmissionFixture {
            storage,
            session,
            models,
            harness,
            main,
            other,
            main_name,
        } = fixture;

        let fault_events = Arc::new(AtomicUsize::new(0));
        let observed_faults = fault_events.clone();
        let _fault_listener = harness.events().on(HarnessEventType::Fault, Arc::new(move |_, _| {
            observed_faults.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }))?;
        let main_run_starts = Arc::new(AtomicUsize::new(0));
        let observed_starts = main_run_starts.clone();
        let observed_lane = main_name.clone();
        let _start_listener = harness.events().on(HarnessEventType::RunStart, Arc::new(move |event, _| {
            if event.lane.as_ref() == Some(&observed_lane) {
                observed_starts.fetch_add(1, Ordering::SeqCst);
            }
            Box::pin(async {})
        }))?;

        // Both lanes are bootstrapped. Only the next real admission commit fails.
        storage.armed.store(true, Ordering::SeqCst);
        let operation_id = OperationId::from("failed-admission");
        let first = main.accept(OperationRequest::Prompt {
            operation_id: Some(operation_id.clone()),
            prompt: PromptInput::Text { text: "hello".to_owned(), images: Vec::new() },
        }, &cx).await;
        let later = other.accept(OperationRequest::Prompt {
            operation_id: Some(OperationId::from("later-admission")),
            prompt: PromptInput::Text { text: "later".to_owned(), images: Vec::new() },
        }, &cx).await;

        // Fault sealing belongs to the harness owner and must not close the
        // session; only the explicit owner close below drains the session and
        // closes the real backend. Gather every observation before closing so
        // the RED path still reports the full picture.
        let main_state = session.get_value(&address::lane_state(&main_name), &cx).await;
        let operation = session.get_value(&address::operation_meta(&operation_id), &cx).await;
        let main_tip = session.get_value(&address::branch_tip(main_name.as_str()), &cx).await;
        let backend_before_close = storage.get_value(&address::lane_state(&main_name).erase(), &cx).await;
        let close = harness.close(&cx).await;
        let after_close = session.get_value(&address::lane_state(&main_name), &cx).await;
        let backend_after_close = storage.get_value(&address::lane_state(&main_name).erase(), &cx).await;

        assert!(first.is_err(), "failed admission must reject");
        assert!(later.is_err(), "storage fault must reject admission on another lane");
        assert!(!storage.armed.load(Ordering::SeqCst), "admission must reach the storage boundary");
        assert_eq!(models.streams.load(Ordering::SeqCst), 0, "admission must not invoke a provider");
        assert_eq!(fault_events.load(Ordering::SeqCst), 1, "initial and subsequent faulted calls admit exactly one fault event");
        assert_eq!(main_run_starts.load(Ordering::SeqCst), 0, "failed admission must not publish run_start");
        assert!(matches!(main_state?, Some(state) if state.value.current_operation_id.is_none()), "failed admission must leave durable lane state idle");
        assert!(operation?.is_none(), "failed admission must not publish operation metadata");
        assert!(matches!(main_tip?, Some(tip) if tip.value.is_none()), "failed admission must not publish its prompt tip");

        let first_error = first.err().ok_or("missing initial rejection")?;
        let later_error = later.err().ok_or("missing subsequent rejection")?;
        let first_fault = cause_in_chain::<HarnessFault>(&first_error)
            .ok_or("initial rejection must retain a typed HarnessFault")?;
        let later_fault = cause_in_chain::<HarnessFault>(&later_error)
            .ok_or("subsequent rejection must retain the same typed HarnessFault")?;
        assert!(std::ptr::eq(first_fault, later_fault), "all faulted calls must reject the same fault object");
        let session_cause = cause_in_chain::<SessionError>(&first_error)
            .ok_or("fault must retain the typed session failure")?;
        assert!(matches!(session_cause, SessionError::Backend(failure) if failure.code == StorageErrorCode::Io));
        let io_cause = cause_in_chain::<std::io::Error>(&first_error)
            .ok_or("fault must retain the original IO cause")?;
        assert!(std::ptr::eq(io_cause, storage.cause.as_ref()), "fault must preserve the original cause object");
        assert_eq!(io_cause.kind(), std::io::ErrorKind::PermissionDenied);

        // The fault seals admissions but leaves the backend open; the explicit
        // owner close is what actually drains the session and closes storage.
        assert!(backend_before_close.is_ok(), "fault sealing must not close the storage backend before explicit close");
        close?;
        assert!(matches!(after_close, Err(SessionError::Backend(failure)) if failure.code == StorageErrorCode::Closed), "explicit close must reject later session reads");
        assert!(matches!(backend_after_close, Err(SessionError::Backend(failure)) if failure.code == StorageErrorCode::Closed), "explicit close must actually close the storage backend");
        Ok::<(), Box<dyn Error>>(())
    }).await??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn navigation_storage_fault_seals_all_lanes() -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let cx = Context::background();
        let fixture = arm_admission_fixture(&cx).await?;
        let AdmissionFixture {
            storage,
            session,
            harness,
            main,
            other,
            main_name,
            ..
        } = fixture;
        let faults = Arc::new(AtomicUsize::new(0));
        let observed = faults.clone();
        let _listener = harness.events().on(
            HarnessEventType::Fault,
            Arc::new(move |_, _| {
                observed.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {})
            }),
        )?;
        // No target and no summary: the only storage touch is the commit.
        storage.armed.store(true, Ordering::SeqCst);
        let operation_id = OperationId::from("failed-navigation");
        let first = main
            .accept(
                OperationRequest::Navigation {
                    operation_id: Some(operation_id.clone()),
                    target_id: None,
                    options: NavigateOptions::default(),
                },
                &cx,
            )
            .await;
        let later = other
            .accept(
                OperationRequest::Prompt {
                    operation_id: Some(OperationId::from("later-admission")),
                    prompt: PromptInput::Text {
                        text: "later".to_owned(),
                        images: Vec::new(),
                    },
                },
                &cx,
            )
            .await;
        let main_state = session
            .get_value(&address::lane_state(&main_name), &cx)
            .await;
        let operation = session
            .get_value(&address::operation_meta(&operation_id), &cx)
            .await;
        let close = harness.close(&cx).await;
        assert!(first.is_err(), "failed navigation must reject");
        assert!(
            later.is_err(),
            "storage fault must reject admission on another lane"
        );
        assert!(
            !storage.armed.load(Ordering::SeqCst),
            "navigation must reach the storage boundary"
        );
        assert_eq!(
            faults.load(Ordering::SeqCst),
            1,
            "faulted navigation admits one fault event"
        );
        assert!(
            matches!(main_state?, Some(state) if state.value.current_operation_id.is_none()),
            "failed navigation must leave durable lane state idle"
        );
        assert!(
            operation?.is_none(),
            "failed navigation must not publish operation metadata"
        );
        let first_error = first.err().ok_or("missing initial rejection")?;
        let later_error = later.err().ok_or("missing subsequent rejection")?;
        let first_fault = cause_in_chain::<HarnessFault>(&first_error)
            .ok_or("initial rejection must retain a typed HarnessFault")?;
        let later_fault = cause_in_chain::<HarnessFault>(&later_error)
            .ok_or("subsequent rejection must retain the same typed HarnessFault")?;
        assert!(
            std::ptr::eq(first_fault, later_fault),
            "all faulted calls must reject the same fault object"
        );
        let session_cause = cause_in_chain::<SessionError>(&first_error)
            .ok_or("fault must retain the typed session failure")?;
        assert!(matches!(
            session_cause, SessionError::Backend(failure) if failure.code == StorageErrorCode::Io
        ));
        close?;
        Ok::<(), Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}

struct StubTool {
    name: &'static str,
    parameters: serde_json::Value,
}

impl pi_agent::harness::tool::HarnessTool for StubTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &'static str {
        "stub tool"
    }

    fn parameters(&self) -> &serde_json::Value {
        &self.parameters
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::default()
    }

    fn execute<'a>(
        &'a self,
        _tool_call_id: &'a str,
        _params: serde_json::Value,
        _on_update: &'a pi_agent::harness::tool::ToolUpdateSink,
        _tool_context: Option<&'a pi_agent::harness::tool::ToolContextValue>,
        _invocation: &'a dyn pi_agent::harness::tool::ToolInvocation,
        _cx: &'a Context,
    ) -> BoxFuture<'a, Result<pi_agent::AgentToolResult, pi_agent::ToolError>> {
        Box::pin(async { Ok(pi_agent::AgentToolResult::default()) })
    }
}

/// A `LaneCreated` listener that re-enters the harness must not deadlock:
/// `emit` resolves only after delivery on the serialized drain worker, so
/// the lane registry lock cannot be held across it.
#[tokio::test(flavor = "current_thread")]
async fn lane_created_listener_may_list_lanes() -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let cx = Context::background();
        let fixture = arm_admission_fixture(&cx).await?;
        let harness = fixture.harness;

        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = observed.clone();
        let listener_harness = harness.clone();
        let _listener = harness.events().on(
            HarnessEventType::LaneCreated,
            Arc::new(move |_, context| {
                let harness = listener_harness.clone();
                let recorded = recorded.clone();
                Box::pin(async move {
                    if let Ok(lanes) = harness.lanes(&context).await
                        && let Ok(mut slot) = recorded.lock()
                    {
                        *slot = lanes.into_iter().map(|lane| lane.name).collect();
                    }
                })
            }),
        )?;

        let created = LaneName::from("created");
        harness
            .lane(&created, AcquireLaneOptions::default(), &cx)
            .await?;

        // `lane()` awaited the event's delivery, so the listener has run.
        {
            let names = observed.lock().ok().ok_or("observed lock poisoned")?;
            assert!(
                names.contains(&created),
                "LaneCreated listener must observe the new lane through lanes()"
            );
            assert_eq!(names.len(), 3, "listener must see main, other, and created");
        }
        harness.close(&cx).await?;
        Ok::<(), Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}

/// A failed lane-metadata commit leaves a durable branch with no lane
/// metadata; the harness must seal against one shared fault and a later
/// restore must skip the orphan instead of failing the session.
#[tokio::test(flavor = "current_thread")]
async fn lane_metadata_commit_failure_seals_and_restore_skips_orphan() -> Result<(), Box<dyn Error>>
{
    tokio::time::timeout(Duration::from_secs(5), async {
        let cx = Context::background();
        let fixture = arm_admission_fixture(&cx).await?;
        let AdmissionFixture {
            storage,
            session,
            models,
            harness,
            main_name,
            ..
        } = fixture;

        let faults = Arc::new(AtomicUsize::new(0));
        let observed = faults.clone();
        let _listener = harness.events().on(
            HarnessEventType::Fault,
            Arc::new(move |_, _| {
                observed.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {})
            }),
        )?;

        // Let the branch-tip commit through, then fail the lane metadata
        // commit: the durable branch is left with no lane metadata.
        storage.skip.store(1, Ordering::SeqCst);
        storage.armed.store(true, Ordering::SeqCst);
        let orphan = LaneName::from("orphan");
        let created = harness
            .lane(&orphan, AcquireLaneOptions::default(), &cx)
            .await;

        assert!(
            created.is_err(),
            "metadata commit failure must reject lane creation"
        );
        let error = created.err().ok_or("missing lane creation rejection")?;
        cause_in_chain::<HarnessFault>(&error).ok_or("rejection must retain the sealing fault")?;
        assert_eq!(
            faults.load(Ordering::SeqCst),
            1,
            "metadata failure must emit one fault event"
        );
        assert!(
            harness.lanes(&cx).await.is_err(),
            "sealed harness must reject later calls"
        );

        // The orphan branch is durable but has no lane metadata.
        let tip = session
            .get_value(&address::branch_tip(orphan.as_str()), &cx)
            .await?;
        assert!(
            tip.is_some(),
            "failed creation leaves the durable branch tip"
        );
        let config = session
            .get_value(&address::lane_config(&orphan), &cx)
            .await?;
        assert!(
            config.is_none(),
            "failed creation leaves no lane configuration"
        );

        // Restore must skip the orphan instead of failing the session.
        let (restored, _) = AgentHarnessBuilder::create(
            AgentHarnessOptions {
                session: session.clone(),
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
            &cx,
        )
        .await?;
        let names: Vec<LaneName> = restored
            .lanes(&cx)
            .await?
            .into_iter()
            .map(|lane| lane.name)
            .collect();
        assert_eq!(names.len(), 2, "restore must recover only the valid lanes");
        assert!(names.contains(&main_name));
        assert!(
            !names.contains(&orphan),
            "restore must skip the metadata-less branch"
        );

        restored.close(&cx).await?;
        let _ = harness.close(&cx).await;
        Ok::<(), Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}

/// `set_tools` must reject removing a tool a lane still activates and accept
/// removals no lane references.
#[tokio::test(flavor = "current_thread")]
async fn set_tools_rejects_in_use_removals() -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let cx = Context::background();
        let fixture = arm_admission_fixture(&cx).await?;
        let AdmissionFixture { harness, main, .. } = fixture;

        let alpha: Arc<dyn pi_agent::harness::tool::HarnessTool> = Arc::new(StubTool {
            name: "alpha",
            parameters: serde_json::json!({}),
        });
        let beta: Arc<dyn pi_agent::harness::tool::HarnessTool> = Arc::new(StubTool {
            name: "beta",
            parameters: serde_json::json!({}),
        });
        harness
            .set_tools(vec![alpha.clone(), beta.clone()], &cx)
            .await?;
        main.set_active_tools(vec!["alpha".to_owned()], &cx).await?;

        // Removing a tool a lane still activates must be rejected.
        let rejected = harness.set_tools(vec![beta.clone()], &cx).await;
        let error = rejected.err().ok_or("in-use removal must be rejected")?;
        assert!(
            error.to_string().contains("alpha"),
            "rejection must name the in-use tool: {error}"
        );

        // Removing only unreferenced tools still swaps the registry.
        harness.set_tools(vec![alpha.clone()], &cx).await?;
        let remaining = harness.get_tools(&cx).await?;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name(), "alpha");
        assert_eq!(
            main.get_active_tools(&cx).await?,
            vec!["alpha".to_owned()],
            "the lane's active set must survive the disjoint swap"
        );

        harness.close(&cx).await?;
        Ok::<(), Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn lane_commit_storage_fault_seals_all_lanes() -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let cx = Context::background();
        let fixture = arm_admission_fixture(&cx).await?;
        let AdmissionFixture {
            storage,
            harness,
            main,
            other,
            ..
        } = fixture;
        let faults = Arc::new(AtomicUsize::new(0));
        let observed = faults.clone();
        let _listener = harness.events().on(
            HarnessEventType::Fault,
            Arc::new(move |_, _| {
                observed.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {})
            }),
        )?;

        // The queue path holds the lane data guard across the durable commit,
        // so the fault broadcast must not re-lock it while sealing.
        storage.armed.store(true, Ordering::SeqCst);
        let queued = main
            .next_run(
                QueueInput::Text {
                    text: "queued".to_owned(),
                    images: Vec::new(),
                },
                &cx,
            )
            .await;
        let later = other
            .accept(
                OperationRequest::Prompt {
                    operation_id: Some(OperationId::from("later-admission")),
                    prompt: PromptInput::Text {
                        text: "later".to_owned(),
                        images: Vec::new(),
                    },
                },
                &cx,
            )
            .await;
        let close = harness.close(&cx).await;

        assert!(queued.is_err(), "failed queue commit must reject");
        assert!(
            later.is_err(),
            "storage fault must reject admission on another lane"
        );
        assert!(
            !storage.armed.load(Ordering::SeqCst),
            "queue must reach the storage boundary"
        );
        assert_eq!(
            faults.load(Ordering::SeqCst),
            1,
            "commit fault admits one fault event"
        );
        let queued_error = queued.err().ok_or("missing queue rejection")?;
        let later_error = later.err().ok_or("missing subsequent rejection")?;
        let queued_fault = cause_in_chain::<HarnessFault>(&queued_error)
            .ok_or("queue rejection must retain a typed HarnessFault")?;
        let later_fault = cause_in_chain::<HarnessFault>(&later_error)
            .ok_or("subsequent rejection must retain the same typed HarnessFault")?;
        assert!(
            std::ptr::eq(queued_fault, later_fault),
            "all faulted calls must reject the same fault object"
        );
        let session_cause = cause_in_chain::<SessionError>(&queued_error)
            .ok_or("fault must retain the typed session failure")?;
        assert!(matches!(
            session_cause,
            SessionError::Backend(failure) if failure.code == StorageErrorCode::Io
        ));
        close?;
        Ok::<(), Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}
