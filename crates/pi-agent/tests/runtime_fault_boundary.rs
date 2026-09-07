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
use pi_agent::context::Context;
use pi_agent::harness::api::{
    AcquireLaneOptions, AgentHarness, AgentHarnessBuilder, AgentHarnessOptions, AgentLane,
    HarnessModels, HarnessResources, OperationRequest, PromptInput,
};
use pi_agent::harness::event::HarnessEventType;
use pi_agent::harness::result::HarnessFault;
use pi_agent::pi_ai;
use pi_agent::session::{
    CommitResult, Entry, EntryId, EntryScan, EntryStructure, HarnessStreamOptions, LaneName,
    ListReadOptions, MemoryStorage, OperationId, RawAddress, RawListElement, RawStoredValue,
    SessionError, SessionMetadata, SessionReaderExt, SessionStats, Storage,
    StorageBackedSession, StorageBranchScan, StorageErrorCode, StorageFailure, UsageRow,
    UsageScan, UuidV7Generator, Write, address,
};
use pi_agent::ToolExecutionMode;

struct FailNextCommitStorage {
    inner: MemoryStorage,
    armed: AtomicBool,
    cause: Arc<std::io::Error>,
}

impl Storage for FailNextCommitStorage {
    fn commit<'a>(
        &'a self,
        writes: Vec<Write>,
        cx: &'a Context,
    ) -> BoxFuture<'a, Result<CommitResult, SessionError>> {
        if self.armed.swap(false, Ordering::SeqCst) {
            let cause: Arc<dyn Error + Send + Sync> = self.cause.clone();
            return Box::pin(async move {
                Err(SessionError::Backend(StorageFailure {
                    code: StorageErrorCode::Io,
                    message: "admission commit failed".to_owned(),
                    source: Some(cause),
                }))
            });
        }
        self.inner.commit(writes, cx)
    }

    fn get_entries<'a>(&'a self, ids: &'a [EntryId], cx: &'a Context) -> BoxFuture<'a, Result<HashMap<EntryId, Entry>, SessionError>> {
        self.inner.get_entries(ids, cx)
    }

    fn get_value<'a>(&'a self, address: &'a RawAddress, cx: &'a Context) -> BoxFuture<'a, Result<Option<RawStoredValue>, SessionError>> {
        self.inner.get_value(address, cx)
    }

    fn scan_values<'a>(&'a self, prefix: &'a RawAddress, cx: &'a Context) -> BoxFuture<'a, Result<Vec<RawStoredValue>, SessionError>> {
        self.inner.scan_values(prefix, cx)
    }

    fn read_list<'a>(&'a self, address: &'a RawAddress, options: Option<ListReadOptions>, cx: &'a Context) -> BoxFuture<'a, Result<Vec<RawListElement>, SessionError>> {
        self.inner.read_list(address, options, cx)
    }

    fn scan_branch<'a>(&'a self, query: &'a StorageBranchScan, cx: &'a Context) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        self.inner.scan_branch(query, cx)
    }

    fn scan_branch_structure<'a>(&'a self, query: &'a StorageBranchScan, cx: &'a Context) -> BoxFuture<'a, Result<Vec<EntryStructure>, SessionError>> {
        self.inner.scan_branch_structure(query, cx)
    }

    fn scan_entries<'a>(&'a self, query: &'a EntryScan, cx: &'a Context) -> BoxFuture<'a, Result<Vec<Entry>, SessionError>> {
        self.inner.scan_entries(query, cx)
    }

    fn scan_usage<'a>(&'a self, query: &'a UsageScan, cx: &'a Context) -> BoxFuture<'a, Result<Vec<UsageRow>, SessionError>> {
        self.inner.scan_usage(query, cx)
    }

    fn get_stats<'a>(&'a self, cx: &'a Context) -> BoxFuture<'a, Result<SessionStats, SessionError>> {
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
        (self.model.provider == provider && self.model.id == model_id)
            .then(|| self.model.clone())
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
    ).await?;
    let main_name = LaneName::from("main");
    let other_name = LaneName::from("other");
    let main = harness.lane(&main_name, AcquireLaneOptions::default(), cx).await?;
    let other = harness.lane(&other_name, AcquireLaneOptions::default(), cx).await?;

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
async fn admission_storage_fault_seals_all_lanes_and_preserves_one_fault() -> Result<(), Box<dyn Error>> {
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
