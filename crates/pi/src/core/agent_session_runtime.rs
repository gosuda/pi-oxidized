//! `AgentSessionRuntime` — owns the current [`AgentSession`] and drives the
//! new / switch / fork / import replacement pipeline.
//!
//! Ports `coding-agent/src/core/agent-session-runtime.ts`. The runtime is
//! single-owner: each replacement operation (under a serializing async mutex)
//! creates the replacement session, tears down the current one, atomically
//! swaps, and rebinds listeners on the new session.
//!
//! Replacement order (matches TS regression 2860: `withSession` runs AFTER
//! rebind on the NEW session):
//! 1. `emit_before_switch` / `emit_before_fork` (cancellable).
//! 2. Build the new session manager.
//! 3. Call the factory → new session + services + diagnostics.
//! 4. `teardown_current`: typed `session_shutdown{reason, targetSessionFile}`
//!    emit, then `before_session_invalidate`, then `session.dispose()`.
//! 5. `apply(result)`: swap session + services + diagnostics.
//! 6. `finish_session_replacement`: `rebind_session(new_session)` then
//!    optional `with_session(ctx)`. The mode's rebind calls `bind_extensions`
//!    on the new session, which emits the stored
//!    `session_start{new|resume|fork}` after the old host received its
//!    `session_shutdown` in step 4.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use futures::future::BoxFuture;
use tokio::sync::Mutex as AsyncMutex;

use crate::core::agent_session::events::{
    AgentSessionEvent, SessionBeforeForkPosition, SessionBeforeSwitchReason, SessionShutdownReason,
    SessionStartReason,
};
use crate::core::agent_session::{AgentSession, ReplacedSessionContext};
use crate::core::session_transfer::SessionImportFileNotFoundError;
use crate::core::sessions::{
    NewSessionOptions as SessionManagerNewSessionOptions, SessionError, SessionManager,
    assert_session_cwd_exists,
};

// ---------------------------------------------------------------------------
// Services carried by the runtime
// ---------------------------------------------------------------------------

/// Cwd-bound service handles the runtime needs to drive replacements.
///
/// This is a intentionally narrow view of [`crate::core::agent_session_services::AgentSessionServices`]:
/// the runtime only needs `cwd`, `agent_dir`, and accumulated diagnostics.
/// The full services object (with `ModelRuntime`, `DefaultResourceLoader`)
/// is owned by the app layer; the factory extracts what the runtime needs.
#[derive(Clone, Debug)]
pub struct AgentSessionRuntimeServices {
    /// Effective working directory.
    pub cwd: PathBuf,
    /// Agent directory (preserved across replacements).
    pub agent_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Inputs to [`CreateAgentSessionRuntimeFactory::create`].
#[derive(Debug)]
pub struct CreateAgentSessionRuntimeOptions {
    /// Effective cwd for the new runtime.
    pub cwd: String,
    /// Agent directory (preserved across replacements).
    pub agent_dir: String,
    /// Session manager to bind (already constructed for the target session).
    pub session_manager: SessionManager,
    /// Replacement reason (carried into `session_start`).
    pub start_reason: SessionStartReason,
    /// Previous session file (for `session_start.previousSessionFile`).
    pub previous_session_file: Option<String>,
}

/// Result returned by [`CreateAgentSessionRuntimeFactory::create`].
pub struct CreateAgentSessionRuntimeResult {
    /// New session.
    pub session: Arc<AgentSession>,
    /// Services snapshot.
    pub services: AgentSessionRuntimeServices,
    /// Diagnostics collected during creation.
    pub diagnostics: Vec<crate::core::agent_session_services::AgentSessionRuntimeDiagnostic>,
    /// Optional model-fallback warning.
    pub model_fallback_message: Option<String>,
}

/// Factory that builds a fresh session + services from a session manager.
///
/// Implemented by the real services factory (which composes
/// `create_agent_session_services` + `create_agent_session_from_services`)
/// and fake test factories.
pub trait CreateAgentSessionRuntimeFactory: Send + Sync {
    /// Build a new session + services from the given options.
    ///
    /// # Errors
    ///
    /// Returns [`AgentSessionRuntimeError`] when services construction or
    /// session creation fails.
    fn create(
        &self,
        options: CreateAgentSessionRuntimeOptions,
    ) -> BoxFuture<'_, Result<CreateAgentSessionRuntimeResult, AgentSessionRuntimeError>>;
}

// ---------------------------------------------------------------------------
// Callbacks
// ---------------------------------------------------------------------------

/// Async callback invoked on the new session after teardown + apply, before
/// `with_session`. Re-binds listeners / event subscriptions.
pub type RebindSessionCallback =
    Arc<dyn Fn(Arc<AgentSession>) -> BoxFuture<'static, ()> + Send + Sync>;

/// Synchronous callback invoked after `session_shutdown` handlers finish but
/// before the old session is invalidated (host UI teardown that must not
/// yield to the event loop).
pub type BeforeSessionInvalidateCallback = Arc<dyn Fn() + Send + Sync>;

// ---------------------------------------------------------------------------
// Outcomes / options
// ---------------------------------------------------------------------------

/// Outcome of `new_session` / `switch_session` / `import_from_jsonl`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwitchOutcome {
    /// True when cancelled by a `before_switch` extension hook.
    pub cancelled: bool,
}

/// Outcome of `fork`.
#[derive(Clone, Debug, Default)]
pub struct ForkOutcome {
    /// True when cancelled by a `before_fork` extension hook.
    pub cancelled: bool,
    /// User-message text when forking before a user message (else `None`).
    pub selected_text: Option<String>,
}

/// Options for [`AgentSessionRuntime::new_session`].
#[derive(Clone, Debug, Default)]
pub struct NewSessionOptions {
    /// Optional parent session file (chained into the new session header).
    pub parent_session: Option<String>,
}

/// Options for [`AgentSessionRuntime::fork`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ForkPosition {
    /// Fork from the parent of the selected user message (default).
    #[default]
    Before,
    /// Fork at the selected entry (clone its subtree).
    At,
}

/// Options for [`AgentSessionRuntime::switch_session`].
#[derive(Clone, Debug, Default)]
pub struct SwitchSessionOptions {
    /// Optional cwd override when the imported session's stored cwd is missing.
    pub cwd_override: Option<String>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from runtime operations.
#[derive(Debug, thiserror::Error)]
pub enum AgentSessionRuntimeError {
    /// Session I/O error.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// Imported session cwd does not exist.
    #[error("Stored session working directory does not exist")]
    MissingSessionCwd,
    /// Imported file not found.
    #[error(transparent)]
    ImportNotFound(#[from] SessionImportFileNotFoundError),
    /// Persisted session missing its file path.
    #[error("Persisted session is missing a session file")]
    MissingSessionFile,
    /// Fork target is invalid (not a user message when `position = before`).
    #[error("Invalid entry ID for forking")]
    InvalidForkEntry,
    /// Unflushed persisted session cannot be forked.
    #[error(
        "This session has not been saved yet. Wait for the first assistant response before cloning or forking it."
    )]
    UnflushedSession,
    /// File transfer error during import.
    #[error("{0}")]
    Transfer(String),
    /// Factory failed to build the replacement runtime.
    #[error("runtime replacement failed: {0}")]
    Factory(String),
}

// ---------------------------------------------------------------------------
// AgentSessionRuntime
// ---------------------------------------------------------------------------

/// Owns the current [`AgentSession`] plus cwd-bound services, and drives the
/// replacement pipeline for new / switch / fork / import.
///
/// Not `Clone`. Callers hold it behind `Arc<AgentSessionRuntime>`; interior
/// mutability covers session + services so replacement operations can run
/// through a shared reference. A serializing async mutex prevents concurrent
/// replacement operations from interleaving teardown / apply / rebind.
pub struct AgentSessionRuntime {
    session: RwLock<Arc<AgentSession>>,
    services: RwLock<AgentSessionRuntimeServices>,
    factory: Arc<dyn CreateAgentSessionRuntimeFactory>,
    diagnostics: RwLock<Vec<crate::core::agent_session_services::AgentSessionRuntimeDiagnostic>>,
    model_fallback_message: RwLock<Option<String>>,
    rebind_session: RwLock<Option<RebindSessionCallback>>,
    before_session_invalidate: RwLock<Option<BeforeSessionInvalidateCallback>>,
    /// Serializes all replacement operations (new/switch/fork/import/dispose).
    replacement_lock: AsyncMutex<()>,
}

impl AgentSessionRuntime {
    /// Construct the runtime from an initial session + services + factory.
    #[must_use]
    pub fn new(
        session: Arc<AgentSession>,
        services: AgentSessionRuntimeServices,
        factory: Arc<dyn CreateAgentSessionRuntimeFactory>,
        diagnostics: Vec<crate::core::agent_session_services::AgentSessionRuntimeDiagnostic>,
        model_fallback_message: Option<String>,
    ) -> Self {
        Self {
            session: RwLock::new(session),
            services: RwLock::new(services),
            factory,
            diagnostics: RwLock::new(diagnostics),
            model_fallback_message: RwLock::new(model_fallback_message),
            rebind_session: RwLock::new(None),
            before_session_invalidate: RwLock::new(None),
            replacement_lock: AsyncMutex::new(()),
        }
    }

    /// Snapshot of the current session (cheap Arc clone).
    #[must_use]
    pub fn session(&self) -> Arc<AgentSession> {
        self.read_session()
    }

    /// Effective cwd.
    #[must_use]
    pub fn cwd(&self) -> String {
        self.services
            .read()
            .map(|g| g.cwd.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Agent directory (preserved across replacements).
    #[must_use]
    pub fn agent_dir(&self) -> String {
        self.services
            .read()
            .map(|g| g.agent_dir.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Snapshot of diagnostics.
    #[must_use]
    pub fn diagnostics(
        &self,
    ) -> Vec<crate::core::agent_session_services::AgentSessionRuntimeDiagnostic> {
        self.diagnostics
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Snapshot of the model-fallback warning.
    #[must_use]
    pub fn model_fallback_message(&self) -> Option<String> {
        self.model_fallback_message
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Set the rebind callback invoked after each replacement.
    pub fn set_rebind_session(&self, callback: Option<RebindSessionCallback>) {
        if let Ok(mut g) = self.rebind_session.write() {
            *g = callback;
        }
    }

    /// Set the synchronous pre-invalidate callback invoked during teardown.
    pub fn set_before_session_invalidate(&self, callback: Option<BeforeSessionInvalidateCallback>) {
        if let Ok(mut g) = self.before_session_invalidate.write() {
            *g = callback;
        }
    }

    // ----- Replacement operations ----------------------------------------

    /// Switch to a different session file (resume).
    ///
    /// # Errors
    ///
    /// See [`AgentSessionRuntimeError`].
    pub async fn switch_session(
        &self,
        session_path: &str,
        options: SwitchSessionOptions,
    ) -> Result<SwitchOutcome, AgentSessionRuntimeError> {
        let _guard = self.replacement_lock.lock().await;
        let before = self
            .emit_before_switch(SessionStartReason::Resume, Some(session_path))
            .await;
        if before.cancelled {
            return Ok(before);
        }

        let previous_session_file = self.session_file_for_teardown().await;
        let session_manager =
            SessionManager::open(session_path, None, options.cwd_override.as_deref())?;
        self.assert_cwd(&session_manager)?;
        let new_cwd = session_manager.get_cwd().to_owned();
        let target_session_file = session_manager.get_session_file().map(str::to_owned);
        let result = self
            .factory
            .create(CreateAgentSessionRuntimeOptions {
                cwd: new_cwd.clone(),
                agent_dir: self.agent_dir(),
                session_manager,
                start_reason: SessionStartReason::Resume,
                previous_session_file,
            })
            .await?;
        self.teardown_current(
            SessionShutdownReason::Resume,
            target_session_file.as_deref(),
        )
        .await;
        self.apply(result);
        self.finish_session_replacement(None).await;
        Ok(SwitchOutcome { cancelled: false })
    }

    /// Start a new session in the current cwd.
    ///
    /// # Errors
    ///
    /// See [`AgentSessionRuntimeError`].
    pub async fn new_session(
        &self,
        options: NewSessionOptions,
    ) -> Result<SwitchOutcome, AgentSessionRuntimeError> {
        let _guard = self.replacement_lock.lock().await;
        let before = self.emit_before_switch(SessionStartReason::New, None).await;
        if before.cancelled {
            return Ok(before);
        }

        let previous_session_file = self.session_file_for_teardown().await;
        let cwd = self.cwd();
        let session_manager = {
            let session = self.read_session();
            let sm = session.session_manager();
            let sm = sm.lock().await;
            if sm.is_persisted() {
                let session_dir = sm.get_session_dir().to_owned();
                let mut new_sm = SessionManager::create(&cwd, Some(&session_dir), None)?;
                if let Some(parent) = options.parent_session.as_deref() {
                    new_sm.new_session(Some(SessionManagerNewSessionOptions {
                        id: None,
                        parent_session: Some(parent.to_owned()),
                    }))?;
                }
                new_sm
            } else {
                let opts = options.parent_session.as_deref().map(|parent| {
                    SessionManagerNewSessionOptions {
                        id: None,
                        parent_session: Some(parent.to_owned()),
                    }
                });
                SessionManager::in_memory(Some(&cwd), opts)?
            }
        };
        let target_session_file = session_manager.get_session_file().map(str::to_owned);
        let result = self
            .factory
            .create(CreateAgentSessionRuntimeOptions {
                cwd: cwd.clone(),
                agent_dir: self.agent_dir(),
                session_manager,
                start_reason: SessionStartReason::New,
                previous_session_file,
            })
            .await?;
        self.teardown_current(SessionShutdownReason::New, target_session_file.as_deref())
            .await;
        self.apply(result);
        self.finish_session_replacement(None).await;
        Ok(SwitchOutcome { cancelled: false })
    }

    /// Fork the session at `entry_id`.
    ///
    /// `position = Before` (default): the entry must be a user message; the
    /// fork point is that message's parent and `selected_text` is returned.
    /// `position = At`: the fork point is the entry itself (clone subtree).
    ///
    /// For **persisted** sessions, the forked branch is written to a new file
    /// under the session directory. For **in-memory** sessions, a fresh
    /// in-memory manager is constructed (the mutated branch on the old
    /// manager is not carried over — until `AgentSession` accepts a shared
    /// `Arc<AsyncMutex<SessionManager>>`, matching the TS single-reference
    /// pattern).
    ///
    /// # Errors
    ///
    /// See [`AgentSessionRuntimeError`].
    pub async fn fork(
        &self,
        entry_id: &str,
        position: ForkPosition,
    ) -> Result<ForkOutcome, AgentSessionRuntimeError> {
        let _guard = self.replacement_lock.lock().await;
        let before = self.emit_before_fork(entry_id, position).await;
        if before.cancelled {
            return Ok(ForkOutcome {
                cancelled: true,
                selected_text: None,
            });
        }

        let (target_leaf_id, selected_text) = {
            let session = self.read_session();
            let sm = session.session_manager();
            let sm = sm.lock().await;
            let selected_entry = sm
                .get_entry(entry_id)
                .ok_or(AgentSessionRuntimeError::InvalidForkEntry)?;
            match position {
                ForkPosition::At => (selected_entry.id().map(str::to_owned), None),
                ForkPosition::Before => {
                    let is_user = matches!(
                        selected_entry,
                        crate::core::sessions::SessionEntry::Message(m) if m.message.role() == "user"
                    );
                    if !is_user {
                        return Err(AgentSessionRuntimeError::InvalidForkEntry);
                    }
                    let parent = selected_entry.parent_id().map(str::to_owned);
                    let text = extract_user_message_text_from_entry(selected_entry);
                    (parent, text)
                }
            }
        };

        let previous_session_file = self.session_file_for_teardown().await;
        let cwd = self.cwd();
        let agent_dir = self.agent_dir();

        // Build the new session manager.
        let session = self.read_session();
        let sm = session.session_manager();
        let sm_guard = sm.lock().await;
        let session_manager = if sm_guard.is_persisted() {
            let current_session_file = sm_guard
                .get_session_file()
                .map(str::to_owned)
                .ok_or(AgentSessionRuntimeError::MissingSessionFile)?;
            let session_dir = sm_guard.get_session_dir().to_owned();
            match target_leaf_id.as_deref() {
                None => {
                    let mut new_sm = SessionManager::create(&cwd, Some(&session_dir), None)?;
                    new_sm.new_session(Some(SessionManagerNewSessionOptions {
                        id: None,
                        parent_session: Some(current_session_file.clone()),
                    }))?;
                    new_sm
                }
                Some(leaf) => {
                    if !Path::new(&current_session_file).exists() {
                        return Err(AgentSessionRuntimeError::UnflushedSession);
                    }
                    let mut reopened =
                        SessionManager::open(&current_session_file, Some(&session_dir), None)?;
                    let forked_path = reopened.create_branched_session(leaf)?;
                    if forked_path.is_none() {
                        return Err(AgentSessionRuntimeError::InvalidForkEntry);
                    }
                    reopened
                }
            }
        } else {
            // In-memory: cannot extract the mutated manager from the live Arc.
            // Fall back to a fresh in-memory manager with parent linkage.
            let opts = match target_leaf_id.as_deref() {
                Some(_) => None, // branch state lost for in-memory (documented)
                None => Some(SessionManagerNewSessionOptions {
                    id: None,
                    parent_session: previous_session_file.clone(),
                }),
            };
            SessionManager::in_memory(Some(&cwd), opts)?
        };
        let new_cwd = session_manager.get_cwd().to_owned();
        let target_session_file = session_manager.get_session_file().map(str::to_owned);
        drop(sm_guard);
        drop(session);

        let result = self
            .factory
            .create(CreateAgentSessionRuntimeOptions {
                cwd: new_cwd,
                agent_dir,
                session_manager,
                start_reason: SessionStartReason::Fork,
                previous_session_file,
            })
            .await?;
        self.teardown_current(SessionShutdownReason::Fork, target_session_file.as_deref())
            .await;
        self.apply(result);
        self.finish_session_replacement(None).await;
        Ok(ForkOutcome {
            cancelled: false,
            selected_text,
        })
    }

    /// Import a JSONL session file and switch to it.
    ///
    /// # Errors
    ///
    /// Returns [`AgentSessionRuntimeError::ImportNotFound`] when the input
    /// path does not exist.
    pub async fn import_from_jsonl(
        &self,
        input_path: &str,
        cwd_override: Option<&str>,
    ) -> Result<SwitchOutcome, AgentSessionRuntimeError> {
        let _guard = self.replacement_lock.lock().await;
        let resolved = crate::core::config::resolve_path(input_path)
            .to_string_lossy()
            .into_owned();
        if !Path::new(&resolved).exists() {
            return Err(AgentSessionRuntimeError::ImportNotFound(
                SessionImportFileNotFoundError::new(&resolved),
            ));
        }

        let session_dir = {
            let session = self.read_session();
            let sm = session.session_manager();
            let sm = sm.lock().await;
            sm.get_session_dir().to_owned()
        };
        if !Path::new(&session_dir).exists() {
            std::fs::create_dir_all(&session_dir)
                .map_err(|e| AgentSessionRuntimeError::Transfer(e.to_string()))?;
        }

        let file_name = Path::new(&resolved).file_name().map_or_else(
            || "imported.jsonl".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        let destination = Path::new(&session_dir)
            .join(&file_name)
            .to_string_lossy()
            .into_owned();

        let before = self
            .emit_before_switch(SessionStartReason::Resume, Some(&destination))
            .await;
        if before.cancelled {
            return Ok(before);
        }

        let previous_session_file = self.session_file_for_teardown().await;
        let dest_canonical = std::fs::canonicalize(&destination).map_or_else(
            |_| destination.clone(),
            |path| path.to_string_lossy().into_owned(),
        );
        let resolved_canonical = std::fs::canonicalize(&resolved).map_or_else(
            |_| resolved.clone(),
            |path| path.to_string_lossy().into_owned(),
        );
        if dest_canonical != resolved_canonical {
            std::fs::copy(&resolved, &destination)
                .map_err(|e| AgentSessionRuntimeError::Transfer(e.to_string()))?;
        }

        let session_manager = SessionManager::open(&destination, Some(&session_dir), cwd_override)?;
        self.assert_cwd(&session_manager)?;
        let new_cwd = session_manager.get_cwd().to_owned();
        let target_session_file = session_manager.get_session_file().map(str::to_owned);
        let result = self
            .factory
            .create(CreateAgentSessionRuntimeOptions {
                cwd: new_cwd,
                agent_dir: self.agent_dir(),
                session_manager,
                start_reason: SessionStartReason::Resume,
                previous_session_file,
            })
            .await?;
        self.teardown_current(
            SessionShutdownReason::Resume,
            target_session_file.as_deref(),
        )
        .await;
        self.apply(result);
        self.finish_session_replacement(None).await;
        Ok(SwitchOutcome { cancelled: false })
    }

    /// Dispose the current session and the runtime.
    pub async fn dispose(&self) {
        let _guard = self.replacement_lock.lock().await;
        self.teardown_current(SessionShutdownReason::Quit, None)
            .await;
    }

    // ----- Internal helpers ----------------------------------------------

    fn read_session(&self) -> Arc<AgentSession> {
        self.session.read().map_or_else(
            |poisoned| Arc::clone(&*poisoned.into_inner()),
            |guard| Arc::clone(&*guard),
        )
    }

    async fn session_file_for_teardown(&self) -> Option<String> {
        self.read_session().session_file().await
    }

    /// Emit `session_before_switch` (cancellable) when handlers are registered.
    async fn emit_before_switch(
        &self,
        reason: SessionStartReason,
        target: Option<&str>,
    ) -> SwitchOutcome {
        let runner = self.read_session().extension_runner();
        if !runner.has_handlers("session_before_switch") {
            return SwitchOutcome { cancelled: false };
        }
        let reason = if reason == SessionStartReason::Resume {
            SessionBeforeSwitchReason::Resume
        } else {
            SessionBeforeSwitchReason::New
        };
        match runner
            .emit(AgentSessionEvent::SessionBeforeSwitch {
                reason,
                target_session_file: target.map(str::to_owned),
            })
            .await
        {
            Ok(result) => SwitchOutcome {
                cancelled: result.is_some_and(|result| result.cancel),
            },
            Err(error) => {
                runner.emit_error(error.to_string());
                SwitchOutcome { cancelled: false }
            }
        }
    }

    /// Emit `session_before_fork` (cancellable) when handlers are registered.
    async fn emit_before_fork(&self, entry_id: &str, position: ForkPosition) -> SwitchOutcome {
        let runner = self.read_session().extension_runner();
        if !runner.has_handlers("session_before_fork") {
            return SwitchOutcome { cancelled: false };
        }
        let position = match position {
            ForkPosition::Before => SessionBeforeForkPosition::Before,
            ForkPosition::At => SessionBeforeForkPosition::At,
        };
        match runner
            .emit(AgentSessionEvent::SessionBeforeFork {
                entry_id: entry_id.to_owned(),
                position,
            })
            .await
        {
            Ok(result) => SwitchOutcome {
                cancelled: result.is_some_and(|result| result.cancel),
            },
            Err(error) => {
                runner.emit_error(error.to_string());
                SwitchOutcome { cancelled: false }
            }
        }
    }

    /// Tear down the current session: typed `session_shutdown` emit →
    /// pre-invalidate → dispose.
    ///
    /// The emit self-gates on `session_shutdown` handler presence and host
    /// errors are isolated. Host process reap is always performed exactly
    /// once inside [`AgentSession::dispose`] when a concrete host is present,
    /// even without `session_shutdown` handlers.
    async fn teardown_current(
        &self,
        reason: SessionShutdownReason,
        target_session_file: Option<&str>,
    ) {
        let session = self.read_session();
        let runner = session.extension_runner();
        let _ = runner
            .emit(AgentSessionEvent::SessionShutdown {
                reason,
                target_session_file: target_session_file.map(str::to_owned),
            })
            .await;
        // Pre-invalidate callback (host UI teardown, sync). Upstream invokes
        // only the runtime-level callback here; the session-bound extension
        // shutdown handler is an extension-initiated "quit" request and must
        // NOT fire on session replacement (it would shut the RPC server down
        // after every fork/clone/new_session/switch_session).
        let runtime_cb = self
            .before_session_invalidate
            .read()
            .ok()
            .and_then(|g| g.clone());
        if let Some(cb) = runtime_cb {
            cb();
        }
        // dispose always awaits host process reap exactly once when bound.
        session.dispose().await;
    }

    /// Apply the factory result: swap session + services + diagnostics.
    fn apply(&self, result: CreateAgentSessionRuntimeResult) {
        if let Ok(mut g) = self.session.write() {
            *g = result.session;
        }
        if let Ok(mut g) = self.services.write() {
            *g = result.services;
        }
        if let Ok(mut g) = self.diagnostics.write() {
            *g = result.diagnostics;
        }
        if let Ok(mut g) = self.model_fallback_message.write() {
            *g = result.model_fallback_message;
        }
    }

    /// Invoke the rebind callback on the new session + optional `with_session`.
    ///
    /// Runs AFTER apply, so `self.session()` is the new session (regression 2860).
    async fn finish_session_replacement(
        &self,
        with_session: Option<
            Arc<dyn Fn(ReplacedSessionContext) -> BoxFuture<'static, ()> + Send + Sync>,
        >,
    ) {
        let session = self.read_session();
        let rebind = self.rebind_session.read().ok().and_then(|g| g.clone());
        if let Some(rebind) = rebind {
            rebind(Arc::clone(&session)).await;
        }
        if let Some(with) = with_session {
            let ctx = session.create_replaced_session_context().await;
            with(ctx).await;
        }
    }

    /// Assert the session cwd exists; fall back to the runtime cwd.
    fn assert_cwd(&self, session_manager: &SessionManager) -> Result<(), AgentSessionRuntimeError> {
        assert_session_cwd_exists(session_manager, &self.cwd())
            .map_err(|_| AgentSessionRuntimeError::MissingSessionCwd)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract concatenated text from a user-message entry (for `fork` `selected_text`).
fn extract_user_message_text_from_entry(
    entry: &crate::core::sessions::SessionEntry,
) -> Option<String> {
    use crate::core::sessions::SessionEntry;
    let SessionEntry::Message(m) = entry else {
        return None;
    };
    let text = crate::core::agent_session::tree::extract_user_message_text_pub(&m.message);
    if text.is_empty() { None } else { Some(text) }
}

/// Create the initial runtime from a factory and initial session manager.
///
/// # Errors
///
/// Returns [`AgentSessionRuntimeError`] when the factory fails.
pub async fn create_agent_session_runtime(
    factory: Arc<dyn CreateAgentSessionRuntimeFactory>,
    cwd: String,
    agent_dir: String,
    session_manager: SessionManager,
) -> Result<AgentSessionRuntime, AgentSessionRuntimeError> {
    let result = factory
        .create(CreateAgentSessionRuntimeOptions {
            cwd,
            agent_dir,
            session_manager,
            start_reason: SessionStartReason::Startup,
            previous_session_file: None,
        })
        .await?;
    Ok(AgentSessionRuntime::new(
        result.session,
        result.services,
        factory,
        result.diagnostics,
        result.model_fallback_message,
    ))
}

// ---------------------------------------------------------------------------
// Re-exports
// ---------------------------------------------------------------------------

pub use crate::core::session_transfer::SessionImportFileNotFoundError as SessionImportFileNotFound;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent_session::AgentSessionConfig;
    use futures::stream::{self, BoxStream, StreamExt};
    use pi_ai::{
        AssistantMessageEvent, Context, Model, ModelCost, ModelInput, Provider, ProviderError,
        StreamOptions,
    };
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
                let config = AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())
                    .map_err(|e| AgentSessionRuntimeError::Factory(e.to_string()))?;
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
                    let config =
                        AgentSessionConfig::test_config(Arc::new(StubProvider), test_model())
                            .map_err(|e| AgentSessionRuntimeError::Factory(e.to_string()))?;
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
    async fn teardown_without_pre_invalidate_leaves_shutdown_handler_alone() -> TestResult {
        // The extension shutdown handler is an extension-initiated quit
        // request; session replacement must not invoke it (upstream
        // teardownCurrent only calls beforeSessionInvalidate). Regression:
        // the RPC server exited after every fork/new_session/switch_session.
        let runtime = Arc::new(make_runtime().await?);
        let called = Arc::new(AtomicUsize::new(0));
        let called_clone = Arc::clone(&called);
        runtime
            .session()
            .bind_extensions(crate::core::agent_session::extension::ExtensionBindings {
                shutdown_handler: Some(Arc::new(move || {
                    called_clone.fetch_add(1, Ordering::SeqCst);
                })),
                ..Default::default()
            })
            .await?;
        runtime.new_session(NewSessionOptions::default()).await?;
        assert_eq!(called.load(Ordering::SeqCst), 0);
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
}
