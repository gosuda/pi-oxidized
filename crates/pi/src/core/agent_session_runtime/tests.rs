//! Tests for `agent_session_runtime`.

use super::*;
use crate::core::agent_session::AgentSessionConfig;
use futures::stream::{self, BoxStream, StreamExt};
use pi_ai::{
    AssistantMessageEvent, Context, Model, ModelCost, ModelInput, Provider, ProviderError,
    StreamOptions,
};
use std::fs;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn failure(message: &'static str) -> io::Error {
    io::Error::other(message)
}

fn test_model() -> Model {
    Model {
        id: "m".to_owned(),
        name: "m".to_owned(),
        api: "test-api".to_owned(),
        provider: "test-provider".to_owned(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![ModelInput::Text],
        cost: ModelCost::default(),
        context_window: 8_192,
        max_tokens: 1_024,
        headers: None,
        compat: None,
        extra: std::collections::BTreeMap::new(),
    }
}

#[derive(Clone)]
struct StubProvider;

impl Provider for StubProvider {
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> BoxStream<'static, Result<AssistantMessageEvent, ProviderError>> {
        stream::empty().boxed()
    }
}

/// Factory that produces a fresh in-memory session per call.
struct TestFactory {
    calls: Arc<AtomicUsize>,
}

impl TestFactory {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl CreateAgentSessionRuntimeFactory for TestFactory {
    fn create(
        &self,
        options: CreateAgentSessionRuntimeOptions,
    ) -> BoxFuture<'_, Result<CreateAgentSessionRuntimeResult, AgentSessionRuntimeError>>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let mut config =
                AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())
                    .map_err(|e| AgentSessionRuntimeError::Factory(e.to_string()))?;
            config.session_manager = options.session_manager;
            let session = AgentSession::new(config)
                .map_err(|e| AgentSessionRuntimeError::Factory(e.to_string()))?;
            Ok(CreateAgentSessionRuntimeResult {
                session,
                services: AgentSessionRuntimeServices {
                    cwd: PathBuf::from(&options.cwd),
                    agent_dir: PathBuf::from(&options.agent_dir),
                },
                diagnostics: Vec::new(),
                model_fallback_message: None,
            })
        })
    }
}

/// Extension runner recording lifecycle `emit` calls (shared across the
/// sessions a recording factory creates).
struct EmitRecordingRunner {
    log: Mutex<Vec<String>>,
}

impl EmitRecordingRunner {
    fn new() -> Self {
        Self {
            log: Mutex::new(Vec::new()),
        }
    }

    fn log_clone(&self) -> Vec<String> {
        self.log
            .lock()
            .map_or_else(|p| p.into_inner().clone(), |g| g.clone())
    }
}

impl crate::core::agent_session::ExtensionRunner for EmitRecordingRunner {
    fn has_handlers(&self, _event: &str) -> bool {
        true
    }

    fn emit(
        &self,
        event: AgentSessionEvent,
    ) -> BoxFuture<
        '_,
        Result<
            Option<crate::core::agent_session::CancelResult>,
            crate::core::agent_session::ExtensionRunnerError,
        >,
    > {
        let entry = match &event {
            AgentSessionEvent::SessionStart {
                reason,
                previous_session_file,
            } => format!(
                "session_start:{}:{}",
                reason.as_str(),
                previous_session_file.as_deref().unwrap_or("-")
            ),
            AgentSessionEvent::SessionShutdown {
                reason,
                target_session_file,
            } => format!(
                "session_shutdown:{}:{}",
                reason.as_str(),
                target_session_file.as_deref().unwrap_or("-")
            ),
            other => other.type_name().to_owned(),
        };
        if let Ok(mut g) = self.log.lock() {
            g.push(entry);
        }
        Box::pin(async { Ok(None) })
    }

    fn emit_message_end(
        &self,
        message: pi_agent::AgentMessage,
    ) -> BoxFuture<
        '_,
        Result<
            Option<pi_agent::AgentMessage>,
            crate::core::agent_session::ExtensionRunnerError,
        >,
    > {
        Box::pin(async move { Ok(Some(message)) })
    }

    fn emit_tool_call(
        &self,
        _tool_name: &str,
        _tool_call_id: &str,
        _input: serde_json::Map<String, serde_json::Value>,
    ) -> BoxFuture<
        '_,
        Result<
            Option<pi_agent::BeforeToolCallResult>,
            crate::core::agent_session::ExtensionRunnerError,
        >,
    > {
        Box::pin(async { Ok(None) })
    }

    fn emit_tool_result(
        &self,
        _tool_name: &str,
        _tool_call_id: &str,
        _input: serde_json::Map<String, serde_json::Value>,
        _content: Vec<pi_ai::ToolResultContent>,
        _details: serde_json::Value,
        _is_error: bool,
    ) -> BoxFuture<
        '_,
        Result<
            Option<pi_agent::AfterToolCallResult>,
            crate::core::agent_session::ExtensionRunnerError,
        >,
    > {
        Box::pin(async { Ok(None) })
    }

    fn emit_input(
        &self,
        _text: &str,
        _images: Option<serde_json::Value>,
        _source: &str,
        _streaming_behavior: Option<&str>,
    ) -> BoxFuture<
        '_,
        Result<
            crate::core::agent_session::InputTransformResult,
            crate::core::agent_session::ExtensionRunnerError,
        >,
    > {
        Box::pin(async { Ok(crate::core::agent_session::InputTransformResult::default()) })
    }

    fn emit_before_agent_start(
        &self,
        _prompt: &str,
        _images: Option<serde_json::Value>,
    ) -> BoxFuture<
        '_,
        Result<
            Option<crate::core::agent_session::BeforeAgentStartResult>,
            crate::core::agent_session::ExtensionRunnerError,
        >,
    > {
        Box::pin(async { Ok(None) })
    }

    fn emit_resources_discover(
        &self,
        _cwd: &str,
        _reason: &str,
    ) -> BoxFuture<
        '_,
        Result<
            crate::core::resources::ResourceExtensionPaths,
            crate::core::agent_session::ExtensionRunnerError,
        >,
    > {
        Box::pin(async { Ok(crate::core::resources::ResourceExtensionPaths::default()) })
    }

    fn get_registered_commands(&self) -> Vec<String> {
        Vec::new()
    }

    fn execute_command<'a>(
        &'a self,
        _name: &'a str,
        _args: &'a str,
    ) -> BoxFuture<'a, Result<bool, crate::core::agent_session::ExtensionRunnerError>> {
        Box::pin(async { Ok(false) })
    }

    fn get_all_registered_tools(
        &self,
    ) -> std::collections::HashMap<String, Arc<dyn pi_agent::AgentTool>> {
        std::collections::HashMap::new()
    }

    fn get_flag_values(&self) -> std::collections::HashMap<String, serde_json::Value> {
        std::collections::HashMap::new()
    }

    fn invalidate(&self) {}

    fn emit_error(&self, _message: String) {}
}

/// Factory recording every `start_reason` and installing a shared
/// recording runner on each created session.
struct RecordingFactory {
    reasons: Mutex<Vec<SessionStartReason>>,
    runner: Arc<EmitRecordingRunner>,
}

impl RecordingFactory {
    fn new(runner: Arc<EmitRecordingRunner>) -> Self {
        Self {
            reasons: Mutex::new(Vec::new()),
            runner,
        }
    }

    fn reasons_clone(&self) -> Vec<SessionStartReason> {
        self.reasons
            .lock()
            .map_or_else(|p| p.into_inner().clone(), |g| g.clone())
    }
}

impl CreateAgentSessionRuntimeFactory for RecordingFactory {
    fn create(
        &self,
        options: CreateAgentSessionRuntimeOptions,
    ) -> BoxFuture<'_, Result<CreateAgentSessionRuntimeResult, AgentSessionRuntimeError>>
    {
        if let Ok(mut g) = self.reasons.lock() {
            g.push(options.start_reason);
        }
        Box::pin(async move {
            let mut config =
                AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())
                    .map_err(|e| AgentSessionRuntimeError::Factory(e.to_string()))?;
            config.session_manager = options.session_manager;
            config.extension_runner = Some(Arc::clone(&self.runner)
                as Arc<dyn crate::core::agent_session::ExtensionRunner>);
            let session = AgentSession::new(config)
                .map_err(|e| AgentSessionRuntimeError::Factory(e.to_string()))?;
            Ok(CreateAgentSessionRuntimeResult {
                session,
                services: AgentSessionRuntimeServices {
                    cwd: PathBuf::from(&options.cwd),
                    agent_dir: PathBuf::from(&options.agent_dir),
                },
                diagnostics: Vec::new(),
                model_fallback_message: None,
            })
        })
    }
}

struct GatedTestFactory {
    calls: AtomicUsize,
    active_replacements: AtomicUsize,
    entered: tokio::sync::mpsc::Sender<usize>,
    gates: [Arc<tokio::sync::Semaphore>; 2],
}

impl GatedTestFactory {
    fn new(
        entered: tokio::sync::mpsc::Sender<usize>,
        gates: [Arc<tokio::sync::Semaphore>; 2],
    ) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            active_replacements: AtomicUsize::new(0),
            entered,
            gates,
        }
    }
}

impl CreateAgentSessionRuntimeFactory for GatedTestFactory {
    fn create(
        &self,
        options: CreateAgentSessionRuntimeOptions,
    ) -> BoxFuture<'_, Result<CreateAgentSessionRuntimeResult, AgentSessionRuntimeError>>
    {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if call > 0 {
                self.entered.try_send(call).map_err(|error| {
                    AgentSessionRuntimeError::Factory(format!(
                        "failed to report replacement factory entry {call}: {error}"
                    ))
                })?;
                if self.active_replacements.swap(1, Ordering::SeqCst) != 0 {
                    return Err(AgentSessionRuntimeError::Factory(
                        "replacement factories overlapped".to_owned(),
                    ));
                }
                let gate = self.gates.get(call - 1).ok_or_else(|| {
                    AgentSessionRuntimeError::Factory(format!(
                        "unexpected replacement factory call {call}"
                    ))
                })?;
                gate.acquire()
                    .await
                    .map_err(|error| {
                        AgentSessionRuntimeError::Factory(format!(
                            "replacement factory gate {call} closed: {error}"
                        ))
                    })?
                    .forget();
            }

            let result = (|| {
                let mut config =
                    AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())
                        .map_err(|e| AgentSessionRuntimeError::Factory(e.to_string()))?;
                config.session_manager = options.session_manager;
                let session = AgentSession::new(config)
                    .map_err(|e| AgentSessionRuntimeError::Factory(e.to_string()))?;
                Ok(CreateAgentSessionRuntimeResult {
                    session,
                    services: AgentSessionRuntimeServices {
                        cwd: PathBuf::from(&options.cwd),
                        agent_dir: PathBuf::from(&options.agent_dir),
                    },
                    diagnostics: Vec::new(),
                    model_fallback_message: None,
                })
            })();
            if call > 0 {
                self.active_replacements.store(0, Ordering::SeqCst);
            }
            result
        })
    }
}

struct PreparationGatedFactory {
    calls: AtomicUsize,
    prepared: tokio::sync::mpsc::Sender<()>,
    publish_gate: Arc<tokio::sync::Semaphore>,
}

impl PreparationGatedFactory {
    fn new(
        prepared: tokio::sync::mpsc::Sender<()>,
        publish_gate: Arc<tokio::sync::Semaphore>,
    ) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            prepared,
            publish_gate,
        }
    }
}

impl CreateAgentSessionRuntimeFactory for PreparationGatedFactory {
    fn create(
        &self,
        options: CreateAgentSessionRuntimeOptions,
    ) -> BoxFuture<'_, Result<CreateAgentSessionRuntimeResult, AgentSessionRuntimeError>>
    {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let mut config =
                AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())
                    .map_err(|e| AgentSessionRuntimeError::Factory(e.to_string()))?;
            config.session_manager = options.session_manager;
            let session = AgentSession::new(config)
                .map_err(|e| AgentSessionRuntimeError::Factory(e.to_string()))?;
            let result = CreateAgentSessionRuntimeResult {
                session,
                services: AgentSessionRuntimeServices {
                    cwd: PathBuf::from(&options.cwd),
                    agent_dir: PathBuf::from(&options.agent_dir),
                },
                diagnostics: Vec::new(),
                model_fallback_message: None,
            };
            if call > 0 {
                self.prepared.try_send(()).map_err(|error| {
                    AgentSessionRuntimeError::Factory(format!(
                        "failed to report prepared replacement: {error}"
                    ))
                })?;
                self.publish_gate
                    .acquire()
                    .await
                    .map_err(|error| {
                        AgentSessionRuntimeError::Factory(format!(
                            "replacement publication gate closed: {error}"
                        ))
                    })?
                    .forget();
            }
            Ok(result)
        })
    }
}

/// Extension runner recording lifecycle `emit` calls (shared across the
/// sessions a recording factory creates).
struct FailingReplacementFactory;

impl CreateAgentSessionRuntimeFactory for FailingReplacementFactory {
    fn create(
        &self,
        _options: CreateAgentSessionRuntimeOptions,
    ) -> BoxFuture<'_, Result<CreateAgentSessionRuntimeResult, AgentSessionRuntimeError>>
    {
        Box::pin(async {
            Err(AgentSessionRuntimeError::Factory(
                "injected replacement failure".to_owned(),
            ))
        })
    }
}

async fn make_persistent_runtime(session_dir: &Path) -> TestResult<AgentSessionRuntime> {
    let factory = Arc::new(TestFactory::new());
    let session_manager = SessionManager::create(
        ".",
        Some(
            session_dir
                .to_str()
                .ok_or("session directory is not UTF-8")?,
        ),
        None,
    )?;
    Ok(create_agent_session_runtime(factory, ".".into(), ".".into(), session_manager).await?)
}

fn write_import_fixture(path: &Path) -> TestResult {
    fs::write(
        path,
        concat!(
            r#"{"type":"session","version":3,"id":"imported-session","timestamp":"2026-01-01T00:00:00.000Z","cwd":"."}"#,
            "\n",
        ),
    )?;
    Ok(())
}

async fn make_runtime() -> TestResult<AgentSessionRuntime> {
    let factory = Arc::new(TestFactory::new());
    let session_manager = SessionManager::in_memory(Some("."), None)?;
    Ok(create_agent_session_runtime(factory, ".".into(), ".".into(), session_manager).await?)
}

#[tokio::test]
async fn runtime_returns_session_and_cwd() -> TestResult {
    let runtime = make_runtime().await?;
    let session = runtime.session();
    assert!(!session.session_id().await.is_empty());
    assert_eq!(runtime.cwd(), ".");
    assert_eq!(runtime.agent_dir(), ".");
    Ok(())
}

#[tokio::test]
async fn new_session_replaces_session_and_invokes_rebind() -> TestResult {
    let runtime = Arc::new(make_runtime().await?);
    let rebind_calls = Arc::new(AtomicUsize::new(0));
    let rebind_calls_clone = Arc::clone(&rebind_calls);
    runtime.set_rebind_session(Some(Arc::new(move |_session| {
        let counter = Arc::clone(&rebind_calls_clone);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
        })
    })));

    let first_session = runtime.session();
    let outcome = runtime.new_session(NewSessionOptions::default()).await?;
    assert!(!outcome.cancelled);
    let second_session = runtime.session();
    assert!(
        !Arc::ptr_eq(&first_session, &second_session),
        "session should have been replaced"
    );
    assert_eq!(rebind_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn switch_session_to_new_path_succeeds() -> TestResult {
    let runtime = Arc::new(make_runtime().await?);
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("switch-target.jsonl");
    let path_str = path.to_string_lossy().into_owned();
    let outcome = runtime
        .switch_session(&path_str, SwitchSessionOptions::default())
        .await?;
    assert!(!outcome.cancelled);
    Ok(())
}

#[tokio::test]
async fn fork_at_clones_branch_and_returns_no_selected_text() -> TestResult {
    let runtime = Arc::new(make_runtime().await?);
    let entry_id = {
        let session = runtime.session();
        let sm = session.session_manager();
        let mut sm = sm.lock().await;
        sm.append_message(&pi_agent::AgentMessage::Llm(Box::new(
            pi_ai::Message::Assistant({
                let mut a = pi_ai::AssistantMessage::new(
                    "test-api",
                    "test-provider",
                    "m",
                    pi_agent::now_millis(),
                );
                a.stop_reason = pi_ai::StopReason::Stop;
                a
            }),
        )))?
    };
    let outcome = runtime.fork(&entry_id, ForkPosition::At).await?;
    assert!(!outcome.cancelled);
    assert!(outcome.selected_text.is_none());
    Ok(())
}

#[tokio::test]
async fn fork_before_user_message_returns_selected_text() -> TestResult {
    let runtime = Arc::new(make_runtime().await?);
    let entry_id = {
        let session = runtime.session();
        let sm = session.session_manager();
        let mut sm = sm.lock().await;
        sm.append_message(&pi_agent::AgentMessage::Llm(Box::new(
            pi_ai::Message::User(pi_ai::UserMessage::new(
                pi_ai::UserMessageContent::Text("hello world".into()),
                0,
            )),
        )))?
    };
    let outcome = runtime.fork(&entry_id, ForkPosition::Before).await?;
    assert!(!outcome.cancelled);
    assert_eq!(outcome.selected_text.as_deref(), Some("hello world"));
    Ok(())
}

#[tokio::test]
async fn fork_before_non_user_entry_errors() -> TestResult {
    let runtime = Arc::new(make_runtime().await?);
    let entry_id = {
        let session = runtime.session();
        let sm = session.session_manager();
        let mut sm = sm.lock().await;
        sm.append_message(&pi_agent::AgentMessage::Llm(Box::new(
            pi_ai::Message::Assistant({
                let mut a = pi_ai::AssistantMessage::new(
                    "test-api",
                    "test-provider",
                    "m",
                    pi_agent::now_millis(),
                );
                a.stop_reason = pi_ai::StopReason::Stop;
                a
            }),
        )))?
    };
    let Err(err) = runtime.fork(&entry_id, ForkPosition::Before).await else {
        return Err(failure("forking before a non-user entry must fail").into());
    };
    assert!(matches!(err, AgentSessionRuntimeError::InvalidForkEntry));
    Ok(())
}

#[tokio::test]
async fn fork_unknown_entry_errors() -> TestResult {
    let runtime = Arc::new(make_runtime().await?);
    let Err(err) = runtime.fork("missing", ForkPosition::At).await else {
        return Err(failure("forking an unknown entry must fail").into());
    };
    assert!(matches!(err, AgentSessionRuntimeError::InvalidForkEntry));
    Ok(())
}

#[tokio::test]
async fn import_from_jsonl_missing_file_errors() -> TestResult {
    let runtime = Arc::new(make_runtime().await?);
    let Err(err) = runtime
        .import_from_jsonl("/nonexistent/path.jsonl", None)
        .await
    else {
        return Err(failure("importing a missing JSONL file must fail").into());
    };
    assert!(matches!(err, AgentSessionRuntimeError::ImportNotFound(_)));
    Ok(())
}

#[test]
fn import_source_open_failure_removes_new_stage() -> TestResult {
    let root = tempfile::tempdir()?;
    let session_dir = root.path().join("sessions");
    fs::create_dir_all(&session_dir)?;

    let result = stage_import_file(
        session_dir.to_str().ok_or("session dir is not UTF-8")?,
        "missing.jsonl",
        &root.path().join("missing.jsonl"),
    );
    assert!(matches!(result, Err(AgentSessionRuntimeError::Transfer(_))));
    assert_eq!(fs::read_dir(&session_dir)?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn invalid_import_preserves_typed_session_error() -> TestResult {
    let root = tempfile::tempdir()?;
    let session_dir = root.path().join("sessions");
    let source_dir = root.path().join("external");
    fs::create_dir_all(&source_dir)?;
    let source = source_dir.join("invalid.jsonl");
    fs::write(&source, b"not json\n")?;
    let runtime = Arc::new(make_persistent_runtime(&session_dir).await?);

    let Err(error) = runtime
        .import_from_jsonl(source.to_str().ok_or("source path is not UTF-8")?, None)
        .await
    else {
        return Err("invalid import unexpectedly succeeded".into());
    };
    assert!(matches!(error, AgentSessionRuntimeError::Session(_)));
    assert!(
        fs::read_dir(&session_dir)?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .all(|entry| {
                entry
                    .path()
                    .extension()
                    .is_none_or(|extension| !extension.eq_ignore_ascii_case("tmp"))
            })
    );
    Ok(())
}

#[test]
fn import_publication_falls_back_when_hard_links_are_unsupported() -> TestResult {
    let root = tempfile::tempdir()?;
    let staged = root.path().join("staged.tmp");
    let destination = root.path().join("session.jsonl");
    fs::write(&staged, b"session bytes")?;

    let method = publish_no_replace_with(
        &staged,
        &destination,
        |_, _| Err(io::Error::new(io::ErrorKind::Unsupported, "no hard links")),
        |source, target| {
            if target.exists() {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            fs::rename(source, target)
        },
    )?;

    assert_eq!(method, ImportPublication::Moved);
    assert!(!staged.exists());
    assert_eq!(fs::read(&destination)?, b"session bytes");
    Ok(())
}

#[test]
fn staged_import_drop_reports_cleanup_failure() -> TestResult {
    let root = tempfile::tempdir()?;
    let staged = root.path().join("orphan.tmp");
    fs::write(&staged, b"stage")?;
    let diagnostics = Arc::new(RwLock::new(Vec::new()));
    let staged_text = staged.to_string_lossy().into_owned();
    drop(StagedImport::new(
        staged,
        Arc::new(|_| Err(io::Error::other("injected cleanup failure"))),
        Arc::clone(&diagnostics),
    ));

    let diagnostics = diagnostics
        .read()
        .map_err(|_| failure("diagnostics lock poisoned"))?;
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.message.contains(&staged_text)
            && diagnostic.message.contains("injected cleanup failure")
    }));
    Ok(())
}

#[tokio::test]
async fn cancelled_import_removes_staged_copy() -> TestResult {
    let root = tempfile::tempdir()?;
    let session_dir = root.path().join("sessions");
    let source_dir = root.path().join("external");
    fs::create_dir_all(&source_dir)?;
    let source = source_dir.join("cancelled.jsonl");
    let destination = session_dir.join("cancelled.jsonl");
    write_import_fixture(&source)?;

    let (prepared, mut prepared_rx) = tokio::sync::mpsc::channel(1);
    let publish_gate = Arc::new(tokio::sync::Semaphore::new(0));
    let factory = Arc::new(PreparationGatedFactory::new(prepared, publish_gate));
    let session_manager = SessionManager::create(
        ".",
        Some(
            session_dir
                .to_str()
                .ok_or("session directory is not UTF-8")?,
        ),
        None,
    )?;
    let runtime = Arc::new(
        create_agent_session_runtime(factory, ".".into(), ".".into(), session_manager).await?,
    );
    let original = runtime.session();
    let import_runtime = Arc::clone(&runtime);
    let source_text = source
        .to_str()
        .ok_or("source path is not UTF-8")?
        .to_owned();
    let mut import =
        tokio::spawn(async move { import_runtime.import_from_jsonl(&source_text, None).await });

    tokio::select! {
        prepared = prepared_rx.recv() => {
            prepared.ok_or("replacement factory did not prepare")?;
        }
        completed = &mut import => {
            let _ = completed?;
            return Err("import completed before replacement preparation".into());
        }
    }
    assert!(
        fs::read_dir(&session_dir)?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|entry| entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "tmp"))
    );

    import.abort();
    let Err(join_error) = import.await else {
        return Err("aborted import task unexpectedly completed".into());
    };
    assert!(join_error.is_cancelled());
    assert!(!destination.exists());
    assert!(
        fs::read_dir(&session_dir)?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .all(|entry| entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "tmp"))
    );
    assert!(Arc::ptr_eq(&original, &runtime.session()));
    Ok(())
}

#[tokio::test]
async fn post_publish_commit_survives_caller_cancellation() -> TestResult {
    let root = tempfile::tempdir()?;
    let session_dir = root.path().join("sessions");
    let source_dir = root.path().join("external");
    fs::create_dir_all(&source_dir)?;
    let source = source_dir.join("commit-cancel.jsonl");
    let destination = session_dir.join("commit-cancel.jsonl");
    write_import_fixture(&source)?;
    let runtime = Arc::new(make_persistent_runtime(&session_dir).await?);
    let original = runtime.session();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    runtime.set_import_commit_gate(Arc::clone(&gate));

    let import_runtime = Arc::clone(&runtime);
    let source_text = source
        .to_str()
        .ok_or("source path is not UTF-8")?
        .to_owned();
    let import =
        tokio::spawn(async move { import_runtime.import_from_jsonl(&source_text, None).await });

    runtime.wait_for_import_commit_started().await;
    assert!(destination.exists());
    import.abort();
    let Err(join_error) = import.await else {
        return Err("aborted import task unexpectedly completed".into());
    };
    assert!(join_error.is_cancelled());

    gate.add_permits(1);
    runtime.wait_for_import_commit_finished().await;
    assert!(!Arc::ptr_eq(&original, &runtime.session()));
    assert_eq!(
        runtime.session().session_file().await,
        Some(destination.to_string_lossy().into_owned())
    );
    assert!(
        fs::read_dir(&session_dir)?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .all(|entry| entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "tmp"))
    );
    Ok(())
}

#[tokio::test]
async fn import_collision_never_overwrites_existing_session() -> TestResult {
    let root = tempfile::tempdir()?;
    let session_dir = root.path().join("sessions");
    let source_dir = root.path().join("external");
    fs::create_dir_all(&source_dir)?;
    let source = source_dir.join("collision.jsonl");
    let destination = session_dir.join("collision.jsonl");
    write_import_fixture(&source)?;
    fs::create_dir_all(&session_dir)?;
    fs::write(&destination, b"existing session bytes")?;
    let runtime = Arc::new(make_persistent_runtime(&session_dir).await?);
    let original = runtime.session();

    let Err(error) = runtime
        .import_from_jsonl(source.to_str().ok_or("source path is not UTF-8")?, None)
        .await
    else {
        return Err(failure("basename collision must reject import").into());
    };

    assert!(matches!(
        error,
        AgentSessionRuntimeError::ImportCollision(_)
    ));
    assert_eq!(fs::read(&destination)?, b"existing session bytes");
    assert!(Arc::ptr_eq(&original, &runtime.session()));
    Ok(())
}

#[tokio::test]
async fn import_publish_race_never_overwrites_destination() -> TestResult {
    let root = tempfile::tempdir()?;
    let session_dir = root.path().join("sessions");
    let source_dir = root.path().join("external");
    fs::create_dir_all(&source_dir)?;
    let source = source_dir.join("raced.jsonl");
    let destination = session_dir.join("raced.jsonl");
    write_import_fixture(&source)?;

    let (prepared, mut prepared_rx) = tokio::sync::mpsc::channel(1);
    let publish_gate = Arc::new(tokio::sync::Semaphore::new(0));
    let factory = Arc::new(PreparationGatedFactory::new(
        prepared,
        Arc::clone(&publish_gate),
    ));
    let session_manager = SessionManager::create(
        ".",
        Some(
            session_dir
                .to_str()
                .ok_or("session directory is not UTF-8")?,
        ),
        None,
    )?;
    let runtime = Arc::new(
        create_agent_session_runtime(factory, ".".into(), ".".into(), session_manager).await?,
    );
    let original = runtime.session();
    let original_file = original.session_file().await;
    let import_runtime = Arc::clone(&runtime);
    let source_text = source
        .to_str()
        .ok_or("source path is not UTF-8")?
        .to_owned();
    let destination_text = destination.to_string_lossy().into_owned();
    let mut import =
        tokio::spawn(async move { import_runtime.import_from_jsonl(&source_text, None).await });

    tokio::select! {
        prepared = prepared_rx.recv() => {
            prepared.ok_or("replacement factory did not prepare")?;
        }
        completed = &mut import => {
            let _ = completed?;
            return Err("import completed before replacement preparation".into());
        }
    }
    fs::write(&destination, b"racing session bytes")?;
    publish_gate.add_permits(1);

    let result = import.await?;
    let Err(error) = result else {
        return Err(failure("raced destination must reject import").into());
    };
    assert!(matches!(
        error,
        AgentSessionRuntimeError::ImportCollision(path) if path == destination_text
    ));
    assert_eq!(fs::read(&destination)?, b"racing session bytes");
    assert!(Arc::ptr_eq(&original, &runtime.session()));
    assert_eq!(runtime.session().session_file().await, original_file);
    assert!(
        fs::read_dir(&session_dir)?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .all(|entry| entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "tmp"))
    );
    Ok(())
}

#[tokio::test]
async fn import_factory_failure_removes_stage_and_preserves_existing_session() -> TestResult {
    let root = tempfile::tempdir()?;
    let session_dir = root.path().join("sessions");
    let source_dir = root.path().join("external");
    fs::create_dir_all(&source_dir)?;
    let source = source_dir.join("factory-failure.jsonl");
    write_import_fixture(&source)?;
    let session_manager = SessionManager::create(
        ".",
        Some(
            session_dir
                .to_str()
                .ok_or("session directory is not UTF-8")?,
        ),
        None,
    )?;
    let mut config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())?;
    config.session_manager = session_manager;
    let original = AgentSession::new(config)?;
    let runtime = Arc::new(AgentSessionRuntime::new(
        Arc::clone(&original),
        AgentSessionRuntimeServices {
            cwd: PathBuf::from("."),
            agent_dir: PathBuf::from("."),
        },
        Arc::new(FailingReplacementFactory),
        Vec::new(),
        None,
    ));

    let Err(error) = runtime
        .import_from_jsonl(source.to_str().ok_or("source path is not UTF-8")?, None)
        .await
    else {
        return Err(failure("injected factory failure must reject import").into());
    };

    assert!(matches!(error, AgentSessionRuntimeError::Factory(_)));
    assert!(Arc::ptr_eq(&original, &runtime.session()));
    assert!(!session_dir.join("factory-failure.jsonl").exists());
    let session_entries = fs::read_dir(&session_dir)?.collect::<Result<Vec<_>, _>>()?;
    assert!(session_entries.iter().all(|entry| {
        entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "tmp")
    }));
    Ok(())
}

#[tokio::test]
async fn import_stage_unlink_failure_keeps_published_session_and_reports_warning() -> TestResult
{
    let root = tempfile::tempdir()?;
    let session_dir = root.path().join("sessions");
    let source_dir = root.path().join("external");
    fs::create_dir_all(&source_dir)?;
    let source = source_dir.join("stage-unlink.jsonl");
    let destination = session_dir.join("stage-unlink.jsonl");
    write_import_fixture(&source)?;
    let runtime = Arc::new(make_persistent_runtime(&session_dir).await?);
    let original = runtime.session();
    let rollback_attempts = Arc::new(AtomicUsize::new(0));
    let rollback_attempts_clone = Arc::clone(&rollback_attempts);
    let destination_for_remove = destination.clone();

    runtime.set_remove_file_for_test(Arc::new(move |path| {
        if path == destination_for_remove {
            rollback_attempts_clone.fetch_add(1, Ordering::SeqCst);
            return Err(io::Error::other("destination rollback must not run"));
        }
        if path.extension().is_some_and(|extension| extension == "tmp") {
            return Err(io::Error::other("injected stage unlink failure"));
        }
        fs::remove_file(path)
    }));

    let outcome = runtime
        .import_from_jsonl(source.to_str().ok_or("source path is not UTF-8")?, None)
        .await?;
    assert!(!outcome.cancelled);
    assert_eq!(rollback_attempts.load(Ordering::SeqCst), 0);
    assert_eq!(
        fs::read_to_string(&destination)?,
        fs::read_to_string(&source)?
    );
    let staged_paths = fs::read_dir(&session_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "tmp"))
        .collect::<Vec<_>>();
    assert_eq!(staged_paths.len(), 1);
    assert_eq!(
        fs::read_to_string(&staged_paths[0])?,
        fs::read_to_string(&source)?
    );
    let staged_path_text = staged_paths[0].to_string_lossy().into_owned();
    assert!(!Arc::ptr_eq(&original, &runtime.session()));
    assert_eq!(
        runtime.session().session_file().await,
        Some(destination.to_string_lossy().into_owned())
    );
    assert!(runtime.diagnostics().iter().any(|diagnostic| {
        diagnostic.kind
            == crate::core::agent_session_services::AgentSessionRuntimeDiagnosticKind::Warning
            && diagnostic.message.contains(&staged_path_text)
            && diagnostic.message.contains("injected stage unlink failure")
    }));
    Ok(())
}

#[tokio::test]
async fn successful_import_atomically_publishes_and_rebinds_session_file() -> TestResult {
    let root = tempfile::tempdir()?;
    let session_dir = root.path().join("sessions");
    let source_dir = root.path().join("external");
    fs::create_dir_all(&source_dir)?;
    let source = source_dir.join("published.jsonl");
    write_import_fixture(&source)?;
    let runtime = Arc::new(make_persistent_runtime(&session_dir).await?);

    let outcome = runtime
        .import_from_jsonl(source.to_str().ok_or("source path is not UTF-8")?, None)
        .await?;

    assert!(!outcome.cancelled);
    let destination = session_dir.join("published.jsonl");
    let session = runtime.session();
    let manager = session.session_manager();
    let mut manager = manager.lock().await;
    assert_eq!(manager.get_session_file(), destination.to_str());
    manager.append_message(&pi_agent::AgentMessage::Llm(Box::new(
        pi_ai::Message::User(pi_ai::UserMessage::new(
            pi_ai::UserMessageContent::Text("after import".into()),
            1,
        )),
    )))?;
    drop(manager);

    assert!(fs::read_to_string(&destination)?.contains("after import"));
    let session_entries = fs::read_dir(&session_dir)?.collect::<Result<Vec<_>, _>>()?;
    assert!(session_entries.iter().all(|entry| {
        entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "tmp")
    }));
    Ok(())
}

#[tokio::test]
async fn dispose_tears_down_session_without_replacing() -> TestResult {
    let runtime = Arc::new(make_runtime().await?);
    let session = runtime.session();
    runtime.dispose().await;
    assert!(Arc::ptr_eq(&runtime.session(), &session));
    Ok(())
}

#[tokio::test]
async fn rebind_callback_runs_after_apply_on_new_session() -> TestResult {
    // Regression 2860: withSession must run on the NEW session.
    let runtime = Arc::new(make_runtime().await?);
    let bound_session_ids = Arc::new(std::sync::Mutex::new(Vec::new()));
    let bound_ids_clone = Arc::clone(&bound_session_ids);
    runtime.set_rebind_session(Some(Arc::new(move |session| {
        let ids = Arc::clone(&bound_ids_clone);
        Box::pin(async move {
            let id = session.session_id().await;
            if let Ok(mut ids) = ids.lock() {
                ids.push(id);
            }
        })
    })));
    runtime.new_session(NewSessionOptions::default()).await?;
    let captured = bound_session_ids
        .lock()
        .map_err(|_| failure("bound session ID mutex poisoned"))?
        .clone();
    assert_eq!(captured.len(), 1, "rebind should fire once");
    assert_eq!(captured[0], runtime.session().session_id().await);
    Ok(())
}

#[tokio::test]
async fn set_before_session_invalidate_invoked_during_teardown() -> TestResult {
    let runtime = Arc::new(make_runtime().await?);
    let called = Arc::new(AtomicUsize::new(0));
    let called_clone = Arc::clone(&called);
    runtime.set_before_session_invalidate(Some(Arc::new(move || {
        called_clone.fetch_add(1, Ordering::SeqCst);
    })));
    runtime.new_session(NewSessionOptions::default()).await?;
    assert_eq!(called.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn replacement_serialized_concurrent_new_sessions() -> TestResult {
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel(2);
    let gates = [
        Arc::new(tokio::sync::Semaphore::new(0)),
        Arc::new(tokio::sync::Semaphore::new(0)),
    ];
    let factory = Arc::new(GatedTestFactory::new(
        entered_tx,
        [Arc::clone(&gates[0]), Arc::clone(&gates[1])],
    ));
    let session_manager = SessionManager::in_memory(Some("."), None)?;
    let runtime = Arc::new(
        create_agent_session_runtime(factory, ".".into(), ".".into(), session_manager).await?,
    );
    let start = Arc::new(tokio::sync::Barrier::new(3));

    let first_runtime = Arc::clone(&runtime);
    let first_start = Arc::clone(&start);
    let first = tokio::spawn(async move {
        first_start.wait().await;
        first_runtime
            .new_session(NewSessionOptions::default())
            .await
    });
    let second_runtime = Arc::clone(&runtime);
    let second_start = Arc::clone(&start);
    let second = tokio::spawn(async move {
        second_start.wait().await;
        second_runtime
            .new_session(NewSessionOptions::default())
            .await
    });
    start.wait().await;

    let first_call = tokio::time::timeout(std::time::Duration::from_secs(1), entered_rx.recv())
        .await
        .map_err(|_| io::Error::other("timed out waiting for first replacement factory entry"))?
        .ok_or_else(|| io::Error::other("replacement factory entry channel closed early"))?;
    assert_eq!(first_call, 1);

    match tokio::time::timeout(std::time::Duration::from_millis(100), entered_rx.recv()).await {
        Ok(Some(call)) => {
            return Err(io::Error::other(format!(
                "replacement factory call {call} entered before call {first_call} was released"
            ))
            .into());
        }
        Ok(None) => {
            return Err(io::Error::other(
                "replacement factory entry channel closed while first gate was held",
            )
            .into());
        }
        Err(_) => {}
    }

    gates[first_call - 1].add_permits(1);
    let second_call =
        tokio::time::timeout(std::time::Duration::from_secs(1), entered_rx.recv())
            .await
            .map_err(|_| {
                io::Error::other("timed out waiting for second replacement factory entry")
            })?
            .ok_or_else(|| {
                io::Error::other("replacement factory entry channel closed early")
            })?;
    assert_eq!(second_call, 2);
    gates[second_call - 1].add_permits(1);

    let first_result = tokio::time::timeout(std::time::Duration::from_secs(1), first)
        .await
        .map_err(|_| io::Error::other("timed out joining first new-session task"))??;
    let second_result = tokio::time::timeout(std::time::Duration::from_secs(1), second)
        .await
        .map_err(|_| io::Error::other("timed out joining second new-session task"))??;
    first_result?;
    second_result?;
    Ok(())
}

async fn make_recording_runtime() -> TestResult<(
    AgentSessionRuntime,
    Arc<RecordingFactory>,
    Arc<EmitRecordingRunner>,
)> {
    let runner = Arc::new(EmitRecordingRunner::new());
    let factory = Arc::new(RecordingFactory::new(Arc::clone(&runner)));
    let session_manager = SessionManager::in_memory(Some("."), None)?;
    let runtime = create_agent_session_runtime(
        Arc::clone(&factory) as Arc<dyn CreateAgentSessionRuntimeFactory>,
        ".".into(),
        ".".into(),
        session_manager,
    )
    .await?;
    Ok((runtime, factory, runner))
}

#[tokio::test]
async fn new_session_passes_new_reason_and_emits_typed_shutdown() -> TestResult {
    let (runtime, factory, runner) = make_recording_runtime().await?;
    runtime.new_session(NewSessionOptions::default()).await?;
    assert_eq!(
        factory.reasons_clone(),
        vec![SessionStartReason::Startup, SessionStartReason::New],
        "replacement factory must receive start_reason = New"
    );
    let log = runner.log_clone();
    assert!(
        log.iter().any(|e| e == "session_shutdown:new:-"),
        "old session must receive typed session_shutdown{{new}} (in-memory: no target), got {log:?}"
    );
    Ok(())
}

#[tokio::test]
async fn fork_passes_fork_reason_and_emits_typed_shutdown() -> TestResult {
    let (runtime, factory, runner) = make_recording_runtime().await?;
    let entry_id = {
        let session = runtime.session();
        let sm = session.session_manager();
        let mut sm = sm.lock().await;
        sm.append_message(&pi_agent::AgentMessage::Llm(Box::new(
            pi_ai::Message::Assistant({
                let mut a = pi_ai::AssistantMessage::new(
                    "test-api",
                    "test-provider",
                    "m",
                    pi_agent::now_millis(),
                );
                a.stop_reason = pi_ai::StopReason::Stop;
                a
            }),
        )))?
    };
    runtime.fork(&entry_id, ForkPosition::At).await?;
    assert_eq!(
        factory.reasons_clone(),
        vec![SessionStartReason::Startup, SessionStartReason::Fork],
        "fork factory must receive start_reason = Fork"
    );
    let log = runner.log_clone();
    assert!(
        log.iter().any(|e| e == "session_shutdown:fork:-"),
        "old session must receive typed session_shutdown{{fork}}, got {log:?}"
    );
    Ok(())
}

#[tokio::test]
async fn switch_session_emits_shutdown_with_target_session_file() -> TestResult {
    let (runtime, factory, runner) = make_recording_runtime().await?;
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("switch-target.jsonl");
    let path_str = path.to_string_lossy().into_owned();
    runtime
        .switch_session(&path_str, SwitchSessionOptions::default())
        .await?;
    assert_eq!(
        factory.reasons_clone(),
        vec![SessionStartReason::Startup, SessionStartReason::Resume],
    );
    let expected = format!("session_shutdown:resume:{path_str}");
    let log = runner.log_clone();
    assert!(
        log.contains(&expected),
        "switch must carry the new session file as targetSessionFile: want {expected}, got {log:?}"
    );
    Ok(())
}

#[tokio::test]
async fn dispose_emits_quit_shutdown_without_target() -> TestResult {
    let (runtime, _factory, runner) = make_recording_runtime().await?;
    runtime.dispose().await;
    let log = runner.log_clone();
    assert!(
        log.iter().any(|e| e == "session_shutdown:quit:-"),
        "dispose must emit typed session_shutdown{{quit}} with no target, got {log:?}"
    );
    Ok(())
}
